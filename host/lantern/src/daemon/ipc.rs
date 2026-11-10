//! Daemon <-> window IPC over `$XDG_RUNTIME_DIR/lantern/daemon.sock`.
//!
//! Each `lantern open` process is one client. Its first frame is `Control::Open` on stream 0;
//! the daemon claims `pkg:user` in the [`Registry`]:
//! - New window `N`: the Open (minus `activation_token`) goes to the helper on stream `N`, and
//!   from then on frames on stream `N` pass through unchanged in both directions.
//! - Already open: the existing window's client gets `focus{activation_token}` on its stream,
//!   the new client gets `focus` (stream = existing id) and is closed.
//!
//! The helper side is never blocked by a client: each client has a bounded outbound queue, and a
//! client that overflows it is dropped exactly as if it had disconnected (close = quit).

use std::collections::HashMap;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use lantern_proto::{
    read_frame_async, write_frame_async, CompanionState, Control, DaemonCmd, Frame, InputEvent,
    Kind, SessionState, StatusReport, WindowInfo, MAX_CLIP, MAX_IME_TEXT,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use tokio::time::timeout;

use super::catalog::{AppEntry, Catalog};
use super::registry::{Claim, Registry};
use crate::clipboard::{ClipHandle, ClipSync, PcOffer};
use crate::settings::Settings;

/// Frames queued per client before it counts as stuck (~4 s of 60 fps video).
pub const CLIENT_QUEUE: usize = 240;
/// A single write to a client that takes longer than this ends that client.
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the daemon waits for room in the helper's outbound queue before declaring the
/// helper wedged (the session is then restarted).
const HELPER_SEND_TIMEOUT: Duration = Duration::from_secs(1);
/// A client must send its `open` within this long of connecting, or it is closed.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

pub const NOT_CONNECTED: &str = "phone not connected";
/// The open's app is in the menu for other phones only (known once this phone has synced).
pub const NOT_ON_THIS_PHONE: &str = "app not on this phone";
/// The user disconnected the phone in Lantern (tray or `lantern disconnect`).
pub const PAUSED: &str = "phone disconnected in Lantern";

pub fn socket_path_in(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("lantern").join("daemon.sock")
}

/// `$XDG_RUNTIME_DIR`, which must be set to an absolute path.
pub fn runtime_dir() -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .context("XDG_RUNTIME_DIR is not set")?;
    let dir = PathBuf::from(dir);
    if !dir.is_absolute() {
        bail!("XDG_RUNTIME_DIR {} is not absolute", dir.display());
    }
    Ok(dir)
}

/// umask is process-wide: concurrent binds must not interleave their set/restore, or one
/// could restore the old mask while another is still creating its socket.
static UMASK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Restores the previous umask on drop. Holds [`UMASK_LOCK`] meanwhile.
struct Umask {
    old: libc::mode_t,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Umask {
    fn set(mask: libc::mode_t) -> Self {
        let lock = UMASK_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: umask has no memory-safety preconditions.
        let old = unsafe { libc::umask(mask) };
        Self { old, _lock: lock }
    }
}

impl Drop for Umask {
    fn drop(&mut self) {
        // SAFETY: as above.
        unsafe { libc::umask(self.old) };
    }
}

/// Creates `<runtime_dir>/lantern` (mode 0700, owned by us) and listens on `daemon.sock` in it.
/// A leftover socket nobody listens on is replaced; a live one means another daemon is running.
pub fn bind(runtime_dir: &Path) -> Result<UnixListener> {
    let dir = runtime_dir.join("lantern");
    let sock = socket_path_in(runtime_dir);
    let _umask = Umask::set(0o077);

    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("create {}", dir.display())),
    }
    let meta =
        std::fs::symlink_metadata(&dir).with_context(|| format!("stat {}", dir.display()))?;
    if !meta.file_type().is_dir() {
        bail!("{} exists and is not a directory; remove it", dir.display());
    }
    // SAFETY: geteuid cannot fail.
    let me = unsafe { libc::geteuid() };
    if meta.uid() != me {
        bail!(
            "{} is owned by uid {}, not by us (uid {me}); refusing to use it",
            dir.display(),
            meta.uid()
        );
    }
    if meta.permissions().mode() & 0o7777 != 0o700 {
        tracing::warn!(dir = %dir.display(), mode = format!("{:o}", meta.permissions().mode() & 0o7777), "tightening socket dir to 0700");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 0700 {}", dir.display()))?;
    }

    match std::fs::symlink_metadata(&sock) {
        Ok(meta) => {
            if !meta.file_type().is_socket() {
                bail!("{} exists and is not a socket; remove it", sock.display());
            }
            if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
                bail!(
                    "lanternd is already running (socket {} is live)",
                    sock.display()
                );
            }
            tracing::info!(sock = %sock.display(), "removing stale socket");
            std::fs::remove_file(&sock)
                .with_context(|| format!("remove stale {}", sock.display()))?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("stat {}", sock.display())),
    }
    UnixListener::bind(&sock).with_context(|| format!("listen on {}", sock.display()))
}

/// `lantern status`: asks the daemon listening on `sock` for its [`StatusReport`]. Blocking.
pub fn query_status(sock: &Path) -> Result<StatusReport> {
    request_report(sock, Control::Status, "status")
}

/// `lantern connect|disconnect`: queues `cmd` in the daemon listening on `sock` and returns its
/// [`StatusReport`] from before the command applies. Blocking.
pub fn send_daemon_cmd(sock: &Path, cmd: DaemonCmd) -> Result<StatusReport> {
    request_report(sock, Control::Daemon { cmd }, "daemon command")
}

/// Sends `request` as the first frame and reads the `status_report` it gets.
fn request_report(sock: &Path, request: Control, what: &str) -> Result<StatusReport> {
    let mut stream = std::os::unix::net::UnixStream::connect(sock).with_context(|| {
        format!(
            "lanternd is not running (cannot connect to {})",
            sock.display()
        )
    })?;
    stream.set_read_timeout(Some(OPEN_TIMEOUT))?;
    stream.set_write_timeout(Some(OPEN_TIMEOUT))?;
    lantern_proto::write_frame(&mut stream, &control(0, request))
        .with_context(|| format!("send {what} request"))?;
    let reply = lantern_proto::read_frame(&mut stream)
        .with_context(|| format!("read {what} reply"))?
        .with_context(|| format!("lanternd closed the connection without a {what} reply"))?;
    match (reply.kind, Control::decode(&reply.payload)) {
        (Kind::Control, Ok(Control::StatusReport(report))) => Ok(report),
        (Kind::Control, Ok(Control::Error { msg, .. })) => bail!("lanternd: {msg}"),
        (kind, other) => bail!("unexpected {what} reply: {kind:?} {other:?}"),
    }
}

/// The helper stopped draining its queue for [`HELPER_SEND_TIMEOUT`]; the session should end.
#[derive(Debug)]
pub struct HelperWedged;

pub enum ClientEvent {
    Frame(u64, Frame),
    Gone(u64),
}

struct Client {
    out: mpsc::Sender<Frame>,
    /// The window this client owns, once its Open was accepted as new.
    win: Option<u32>,
    reader: AbortHandle,
    writer: AbortHandle,
}

impl Client {
    /// Closes the client after its queued frames are flushed (the writer ends when `out` drops).
    fn finish(self) {
        drop(self);
    }

    /// Closes the client now, discarding anything queued.
    fn kill(self) {
        self.writer.abort();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

fn control(stream: u32, c: Control) -> Frame {
    Frame {
        kind: Kind::Control,
        stream,
        payload: c.encode(),
    }
}

/// Limits the helper enforces on host control messages. A value outside them makes the helper
/// throw `ProtocolError`, ending the whole session (every window), so the daemon checks them
/// first. Mirrors `Router.open`/`Router.resize`/`Router.range` in
/// android/helper/src/dev/lantern/helper/Router.java; keep the two in sync. `focus` and `close`
/// carry no range-checked fields.
pub mod limits {
    use std::ops::RangeInclusive;

    /// `open.w`, `open.h`, `resize.w`, `resize.h`: `range(m, "w"|"h", 1, 16384)`.
    pub const SIZE: RangeInclusive<u32> = 1..=16384;
    /// `open.dpi`, `resize.dpi`: `range(m, "dpi", 1, 4096)`.
    pub const DPI: RangeInclusive<u32> = 1..=4096;
    /// `open.user`: `range(m, "user", 0, Integer.MAX_VALUE)`.
    pub const USER: RangeInclusive<u32> = 0..=i32::MAX as u32;

    /// Whether a window geometry is within what the helper accepts.
    pub fn geometry_ok(w: u32, h: u32, dpi: u32) -> bool {
        SIZE.contains(&w) && SIZE.contains(&h) && DPI.contains(&dpi)
    }
}

/// Checks a client frame on its window's own stream and returns what to forward to the
/// helper, or `None` for a protocol error. A malformed input payload or a resize outside
/// [`limits`] would end the whole helper session (every window), so it is caught here and
/// costs only this client; so would an `ime` text over [`MAX_IME_TEXT`] bytes. The activation
/// token is daemon↔window only and is stripped from a client's `focus`.
fn client_frame_for_helper(frame: Frame) -> Option<Frame> {
    match frame.kind {
        Kind::Input => InputEvent::decode(&frame.payload).is_ok().then_some(frame),
        Kind::Video => None,
        Kind::Control => match Control::decode(&frame.payload).ok()? {
            Control::Focus { .. } => Some(control(
                frame.stream,
                Control::Focus {
                    activation_token: None,
                },
            )),
            Control::Resize { w, h, dpi } => limits::geometry_ok(w, h, dpi).then_some(frame),
            Control::Close => Some(frame),
            // The helper's Router rejects a text over MAX_IME_TEXT UTF-8 bytes (and an unknown
            // op, which never decodes here). Log the op and size only, never the typed text.
            Control::Ime { op, text } => {
                tracing::debug!(stream = frame.stream, ?op, len = text.len(), "ime");
                (text.len() <= MAX_IME_TEXT).then_some(frame)
            }
            _ => None,
        },
    }
}

/// Sends to the helper without ever blocking for long: a full queue gets
/// [`HELPER_SEND_TIMEOUT`] to drain. A closed queue means the session is already ending, which
/// the run loop notices on its own.
async fn to_helper(helper: &mpsc::Sender<Frame>, frame: Frame) -> Result<(), HelperWedged> {
    match helper.try_send(frame) {
        Ok(()) | Err(TrySendError::Closed(_)) => Ok(()),
        Err(TrySendError::Full(frame)) => {
            match timeout(HELPER_SEND_TIMEOUT, helper.send(frame)).await {
                Ok(_) => Ok(()),
                Err(_) => Err(HelperWedged),
            }
        }
    }
}

/// The `t` (message type) of a control payload, for logs that must not dump the payload.
fn control_type(payload: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| v.get("t").and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "?".to_owned())
}

/// One debug line per catalogue message: type, pkg, user, label and icon size, never the icon.
fn log_catalogue(c: &Control) {
    match c {
        Control::App {
            pkg,
            user,
            label,
            work,
            icon,
        } => tracing::debug!(pkg, user, label, work, icon_len = icon.len(), "helper app"),
        Control::AppsEnd { count } => tracing::debug!(count, "helper apps_end"),
        Control::AppRemoved { pkg, user } => tracing::debug!(pkg, user, "helper app_removed"),
        _ => {}
    }
}

/// Routes frames between window clients and the current helper session.
pub struct Hub {
    listener: Option<UnixListener>,
    clients: HashMap<u64, Client>,
    windows: HashMap<u32, u64>,
    /// What `lantern status` reports for each open window.
    info: HashMap<u32, WindowInfo>,
    registry: Registry,
    /// The session part of `lantern status`, kept current by the run loop.
    session: (SessionState, Option<String>, Option<String>),
    /// The live session's last `companion` state and its `msg`; cleared when the session ends.
    companion: Option<(CompanionState, Option<String>)>,
    next_cid: u64,
    events_tx: mpsc::Sender<ClientEvent>,
    events: mpsc::Receiver<ClientEvent>,
    /// The phone's launcher apps as menu entries, if the daemon keeps them.
    catalog: Option<Catalog>,
    /// Where phone `clip`s go (the PC clipboard), if the daemon syncs it.
    clip_out: Option<ClipHandle>,
    /// Echo/secret/size suppression for the clipboard; reset with each session.
    clip_sync: ClipSync,
    /// The user's settings (`clipboard_sync`), if the daemon has a settings file.
    settings: Option<watch::Receiver<Settings>>,
    /// `clipboard_sync` as of the last clipboard event, to reset `clip_sync` when it turns on.
    clip_was_on: bool,
    /// `lantern connect|disconnect` commands not yet taken by the run loop.
    commands: Vec<DaemonCmd>,
}

impl Hub {
    /// A hub without a listener only drains helper frames (used by `run_loop`).
    pub fn new(listener: Option<UnixListener>) -> Self {
        let (events_tx, events) = mpsc::channel(256);
        Self {
            listener,
            clients: HashMap::new(),
            windows: HashMap::new(),
            info: HashMap::new(),
            registry: Registry::default(),
            session: (SessionState::Idle, None, None),
            companion: None,
            next_cid: 0,
            events_tx,
            events,
            catalog: None,
            clip_out: None,
            clip_sync: ClipSync::default(),
            settings: None,
            clip_was_on: true,
            commands: Vec::new(),
        }
    }

    /// The `lantern connect|disconnect` commands received since the last call, oldest first.
    pub fn take_commands(&mut self) -> Vec<DaemonCmd> {
        std::mem::take(&mut self.commands)
    }

    /// Follows the user's settings: while `clipboard_sync` is false nothing crosses either way.
    pub fn set_settings(&mut self, settings: watch::Receiver<Settings>) {
        self.clip_was_on = settings.borrow().clipboard_sync;
        self.settings = Some(settings);
    }

    /// Whether clipboard sync is on now. Turning it back on resets [`ClipSync`]: what it
    /// remembered may be stale after changes it never saw. (Checked at each clipboard event,
    /// which is equivalent: nothing reads `clip_sync` in between.)
    fn clip_on(&mut self) -> bool {
        let on = self
            .settings
            .as_ref()
            .is_none_or(|settings| settings.borrow().clipboard_sync);
        if on && !self.clip_was_on {
            tracing::debug!("clipboard sync back on");
            self.clip_sync = ClipSync::default();
        }
        self.clip_was_on = on;
        on
    }

    /// Phone `clip`s are put on the PC clipboard through `handle`.
    pub fn set_clipboard(&mut self, handle: ClipHandle) {
        self.clip_out = Some(handle);
    }

    /// The PC selection changed while a session is live: its text goes to the phone as
    /// `clip{text}` on stream 0 unless [`ClipSync::pc_changed`] suppresses it.
    pub async fn on_pc_clip(
        &mut self,
        offer: PcOffer,
        helper: &mpsc::Sender<Frame>,
    ) -> Result<(), HelperWedged> {
        if !self.clip_on() {
            let len = offer.text.as_ref().map_or(0, String::len);
            tracing::debug!("clipboard sync off; {len} chars not synced");
            return Ok(());
        }
        match self.clip_sync.pc_changed(&offer) {
            Some(text) => {
                tracing::debug!(len = text.len(), "PC clipboard sent to the phone");
                to_helper(helper, control(0, Control::Clip { text })).await
            }
            None => {
                tracing::debug!(
                    secret = offer.secret,
                    len = offer.text.as_ref().map(String::len),
                    "PC clipboard change not sent"
                );
                Ok(())
            }
        }
    }

    /// A phone `clip`: onto the PC clipboard (never echoed back by [`Self::on_pc_clip`]).
    fn on_phone_clip(&mut self, text: String) {
        let len = text.len();
        if !self.clip_on() {
            tracing::debug!("clipboard sync off; {len} chars not synced");
            return;
        }
        if text.is_empty() || len > MAX_CLIP {
            tracing::warn!(len, "phone clip empty or over MAX_CLIP; ignored");
            return;
        }
        let Some(out) = &self.clip_out else {
            tracing::debug!(len, "phone clip ignored: no PC clipboard");
            return;
        };
        tracing::debug!(len, "phone clipboard put on the PC");
        out.set(self.clip_sync.from_phone(text));
    }

    /// Keeps `catalog` in step with the helper's catalogue messages.
    pub fn set_catalog(&mut self, catalog: Catalog) {
        self.catalog = Some(catalog);
    }

    /// A session with `serial` is up: the first catalogue sync window opens.
    pub fn session_started(&mut self, serial: &str) {
        if let Some(catalog) = self.catalog.as_mut() {
            catalog.start_session(serial);
        }
    }

    /// `app`/`apps_end`/`app_removed` from the helper. Catalogue file writes are small and
    /// synchronous; kbuildsycoca6 runs off this loop.
    fn on_catalogue(&mut self, c: Control) {
        let Some(catalog) = self.catalog.as_mut() else {
            return;
        };
        match c {
            Control::App {
                pkg,
                user,
                label,
                work,
                icon,
            } => {
                let entry = AppEntry::from_message(pkg, user, label, work, &icon);
                if let Err(error) = catalog.upsert(&entry) {
                    tracing::warn!(
                        pkg = entry.pkg,
                        user,
                        error = format!("{error:#}"),
                        "catalogue entry not written"
                    );
                }
            }
            Control::AppRemoved { pkg, user } => {
                if let Err(error) = catalog.remove(&pkg, user) {
                    tracing::warn!(
                        pkg,
                        user,
                        error = format!("{error:#}"),
                        "catalogue removal failed"
                    );
                }
            }
            // `end_sync` also opens the next window (a companion relink, a profile change).
            Control::AppsEnd { count } => match catalog.end_sync(count) {
                Ok(removed) => tracing::info!(count, removed, "catalogue synced"),
                Err(error) => {
                    tracing::warn!(error = format!("{error:#}"), "catalogue prune failed")
                }
            },
            _ => {}
        }
    }

    /// Records the session state `lantern status` reports: `serial` is the device being started
    /// or served, `helper` the live helper's SHA-256.
    pub fn set_session(
        &mut self,
        state: SessionState,
        serial: Option<String>,
        helper: Option<String>,
    ) {
        self.session = (state, serial, helper);
    }

    /// The `lantern status` answer: session state plus open windows by id.
    pub fn status(&self) -> StatusReport {
        let mut windows: Vec<WindowInfo> = self.info.values().cloned().collect();
        windows.sort_by_key(|w| w.id);
        let (state, serial, helper) = self.session.clone();
        let companion = self
            .companion
            .as_ref()
            .map(|(companion, _)| *companion)
            .filter(|_| state == SessionState::Ready);
        StatusReport {
            state,
            serial,
            helper,
            windows,
            companion,
        }
    }

    /// The live session's last `companion` state with its `msg` (e.g. "keyboard off in Lantern
    /// settings"), if the helper sent one.
    pub fn companion(&self) -> Option<(CompanionState, Option<String>)> {
        self.companion.clone()
    }

    /// Forgets window `win` (registry and status).
    fn release(&mut self, win: u32) {
        self.windows.remove(&win);
        self.info.remove(&win);
        self.registry.release(win);
    }

    /// The listener's socket path, if any.
    pub fn socket_path(&self) -> Option<PathBuf> {
        let addr = self.listener.as_ref()?.local_addr().ok()?;
        addr.as_pathname().map(Path::to_path_buf)
    }

    /// Accepts clients until one of them has something to say. Cancel-safe.
    pub async fn next_event(&mut self) -> ClientEvent {
        loop {
            let accepted = tokio::select! {
                event = self.events.recv() => return event.expect("hub holds a sender"),
                accepted = async { self.listener.as_ref().expect("guarded").accept().await },
                    if self.listener.is_some() => accepted,
            };
            match accepted {
                Ok((stream, _)) => self.add_client(stream),
                Err(error) => {
                    tracing::warn!(%error, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn add_client(&mut self, stream: UnixStream) {
        self.next_cid += 1;
        let cid = self.next_cid;
        let (mut rd, mut wr) = stream.into_split();
        let (out, mut queue) = mpsc::channel::<Frame>(CLIENT_QUEUE);
        let events = self.events_tx.clone();
        let reader = tokio::spawn(async move {
            let mut first = true;
            loop {
                let read = read_frame_async(&mut rd);
                let read = if first {
                    first = false;
                    match timeout(OPEN_TIMEOUT, read).await {
                        Ok(read) => read,
                        Err(_) => {
                            tracing::warn!(cid, "client sent no open in time; closing it");
                            break;
                        }
                    }
                } else {
                    read.await
                };
                match read {
                    Ok(Some(frame)) => {
                        if events.send(ClientEvent::Frame(cid, frame)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::debug!(cid, %error, "client read failed");
                        break;
                    }
                }
            }
            let _ = events.send(ClientEvent::Gone(cid)).await;
        });
        let writer = tokio::spawn(async move {
            while let Some(frame) = queue.recv().await {
                match timeout(CLIENT_WRITE_TIMEOUT, write_frame_async(&mut wr, &frame)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::debug!(cid, %error, "client write failed");
                        break;
                    }
                    Err(_) => {
                        tracing::warn!(cid, "client write timed out");
                        break;
                    }
                }
            }
            // Dropping the write half shuts it down: the client reads EOF.
        });
        tracing::debug!(cid, "client connected");
        self.clients.insert(
            cid,
            Client {
                out,
                win: None,
                reader: reader.abort_handle(),
                writer: writer.abort_handle(),
            },
        );
    }

    /// Sends `frame` to `cid` and closes it after the flush.
    fn reply_and_finish(&mut self, cid: u64, frame: Frame) {
        if let Some(client) = self.clients.remove(&cid) {
            let _ = client.out.try_send(frame);
            client.finish();
        }
    }

    /// Drops a client now. A window it owned is closed on the phone (close = quit) and released.
    async fn drop_client(
        &mut self,
        cid: u64,
        helper: Option<&mpsc::Sender<Frame>>,
    ) -> Result<(), HelperWedged> {
        let Some(client) = self.clients.remove(&cid) else {
            return Ok(());
        };
        let win = client.win;
        client.kill();
        if let Some(win) = win {
            self.release(win);
            tracing::info!(cid, win, "window client gone; closing window");
            if let Some(helper) = helper {
                return to_helper(helper, control(win, Control::Close)).await;
            }
        }
        Ok(())
    }

    /// Handles one client event. `helper` is the live session's outbound queue, if any.
    pub async fn on_client(
        &mut self,
        event: ClientEvent,
        helper: Option<&mpsc::Sender<Frame>>,
    ) -> Result<(), HelperWedged> {
        let (cid, frame) = match event {
            ClientEvent::Gone(cid) => return self.drop_client(cid, helper).await,
            ClientEvent::Frame(cid, frame) => (cid, frame),
        };
        let Some(client) = self.clients.get(&cid) else {
            return Ok(());
        };
        let Some(win) = client.win else {
            return self.on_open(cid, frame, helper).await;
        };
        let (stream, kind) = (frame.stream, frame.kind);
        match (stream == win)
            .then(|| client_frame_for_helper(frame))
            .flatten()
        {
            Some(frame) => match helper {
                Some(helper) => to_helper(helper, frame).await,
                None => Ok(()),
            },
            None => {
                tracing::warn!(
                    cid,
                    win,
                    stream,
                    ?kind,
                    "client protocol error; dropping it"
                );
                self.drop_client(cid, helper).await
            }
        }
    }

    async fn on_open(
        &mut self,
        cid: u64,
        frame: Frame,
        helper: Option<&mpsc::Sender<Frame>>,
    ) -> Result<(), HelperWedged> {
        let open = (frame.kind == Kind::Control && frame.stream == 0)
            .then(|| Control::decode(&frame.payload).ok())
            .flatten();
        if matches!(open, Some(Control::Status)) {
            tracing::debug!(cid, "status request");
            let report = Control::StatusReport(self.status());
            self.reply_and_finish(cid, control(0, report));
            return Ok(());
        }
        if let Some(Control::Daemon { cmd }) = open {
            tracing::info!(cid, ?cmd, "daemon command");
            self.commands.push(cmd);
            let report = Control::StatusReport(self.status());
            self.reply_and_finish(cid, control(0, report));
            return Ok(());
        }
        let Some(Control::Open {
            pkg,
            user,
            w,
            h,
            dpi,
            activation_token,
        }) = open
        else {
            tracing::warn!(cid, "first client frame was not open on stream 0");
            let msg = "first frame must be open on stream 0".to_owned();
            self.reply_and_finish(cid, control(0, Control::error(msg)));
            return Ok(());
        };
        if !limits::USER.contains(&user) || !limits::geometry_ok(w, h, dpi) {
            tracing::warn!(cid, pkg, user, w, h, dpi, "open out of range; rejecting it");
            let msg = format!(
                "open out of range: user={user} (0..={}), w={w} h={h} (1..={}), dpi={dpi} (1..={})",
                limits::USER.end(),
                limits::SIZE.end(),
                limits::DPI.end()
            );
            self.reply_and_finish(cid, control(0, Control::error(msg)));
            return Ok(());
        }
        let Some(helper) = helper else {
            let msg = match self.session.0 {
                SessionState::Paused => PAUSED,
                _ => NOT_CONNECTED,
            };
            self.reply_and_finish(cid, control(0, Control::error(msg)));
            return Ok(());
        };
        if self.not_on_this_phone(&pkg, user) {
            tracing::info!(cid, pkg, user, "app not on this phone; refusing the open");
            self.reply_and_finish(cid, control(0, Control::error(NOT_ON_THIS_PHONE)));
            return Ok(());
        }
        let key = format!("{pkg}:{user}");
        let mut claim = self.registry.claim(&key);
        if let Claim::Existing(win) = claim {
            tracing::info!(cid, win, pkg, user, "already open; focusing");
            if let Some(&owner) = self.windows.get(&win) {
                self.send_to_client(
                    owner,
                    control(win, Control::Focus { activation_token }),
                    Some(helper),
                )
                .await?;
                if !self.clients.contains_key(&owner) {
                    // The owner could not take the focus and was dropped (its window closed
                    // and released): this open becomes a new window instead.
                    tracing::info!(cid, win, "owner gone; opening a new window");
                    claim = self.registry.claim(&key);
                }
            }
        }
        match claim {
            Claim::New(win) => {
                tracing::info!(cid, win, pkg, user, "opening window");
                if let Some(client) = self.clients.get_mut(&cid) {
                    client.win = Some(win);
                }
                self.windows.insert(win, cid);
                self.info.insert(
                    win,
                    WindowInfo {
                        id: win,
                        pkg: pkg.clone(),
                        user,
                    },
                );
                let open = Control::Open {
                    pkg,
                    user,
                    w,
                    h,
                    dpi,
                    activation_token: None,
                };
                to_helper(helper, control(win, open)).await
            }
            Claim::Existing(win) => {
                self.reply_and_finish(
                    cid,
                    control(
                        win,
                        Control::Focus {
                            activation_token: None,
                        },
                    ),
                );
                Ok(())
            }
        }
    }

    /// Whether `pkg` for `user` is known to be only on other phones: this phone's full
    /// catalogue has arrived, the app has a menu entry, and the entry does not list the
    /// session's phone. Before the first sync nothing is refused.
    fn not_on_this_phone(&self, pkg: &str, user: u32) -> bool {
        let (Some(catalog), Some(serial)) = (&self.catalog, &self.session.1) else {
            return false;
        };
        if !catalog.synced() {
            return false;
        }
        let phones = catalog.phones(pkg, user);
        !phones.is_empty() && !phones.iter().any(|s| s == serial)
    }

    /// Queues `frame` for a window client; one that cannot keep up is dropped.
    async fn send_to_client(
        &mut self,
        cid: u64,
        frame: Frame,
        helper: Option<&mpsc::Sender<Frame>>,
    ) -> Result<(), HelperWedged> {
        let Some(client) = self.clients.get(&cid) else {
            return Ok(());
        };
        match client.out.try_send(frame) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                tracing::warn!(cid, win = ?client.win, "client queue overflowed; dropping it");
                self.drop_client(cid, helper).await
            }
            Err(TrySendError::Closed(_)) => self.drop_client(cid, helper).await,
        }
    }

    /// Handles one frame from the helper.
    pub async fn on_helper(
        &mut self,
        frame: Frame,
        helper: &mpsc::Sender<Frame>,
    ) -> Result<(), HelperWedged> {
        if frame.stream == 0 {
            match Control::decode(&frame.payload) {
                Ok(Control::Error { msg, .. }) => tracing::error!(msg, "helper session error"),
                Ok(Control::Companion { state, msg }) => {
                    match state {
                        CompanionState::Ready => tracing::info!("companion linked"),
                        CompanionState::Unavailable => {
                            tracing::warn!(
                                msg = msg.as_deref().unwrap_or(""),
                                "companion unavailable"
                            )
                        }
                    }
                    self.companion = Some((state, msg));
                }
                Ok(
                    c
                    @ (Control::App { .. } | Control::AppsEnd { .. } | Control::AppRemoved { .. }),
                ) => {
                    log_catalogue(&c);
                    self.on_catalogue(c);
                }
                Ok(Control::Clip { text }) => self.on_phone_clip(text),
                // Never dump whole frames: some carry icons or clipboard text.
                Ok(_) => tracing::debug!(
                    t = control_type(&frame.payload),
                    len = frame.payload.len(),
                    "helper stream-0 frame"
                ),
                Err(error) => {
                    tracing::debug!(%error, len = frame.payload.len(), "undecodable helper stream-0 frame")
                }
            }
            return Ok(());
        }
        let win = frame.stream;
        // Unknown or already-released window (e.g. the closed answering our close): ignore.
        let Some(&cid) = self.windows.get(&win) else {
            return Ok(());
        };
        let closed = frame.kind == Kind::Control
            && matches!(Control::decode(&frame.payload), Ok(Control::Closed { .. }));
        if closed {
            tracing::info!(cid, win, "window closed by helper");
            self.release(win);
            if let Some(client) = self.clients.get_mut(&cid) {
                client.win = None;
            }
            self.reply_and_finish(cid, frame);
            return Ok(());
        }
        self.send_to_client(cid, frame, Some(helper)).await
    }

    /// The helper session ended: every window client is closed (it exits) and the registry
    /// starts over for the next session. Clients still waiting to send Open are kept.
    pub fn reset(&mut self) {
        let windowed: Vec<u64> = self.windows.values().copied().collect();
        for cid in windowed {
            if let Some(client) = self.clients.remove(&cid) {
                client.finish();
            }
        }
        self.windows.clear();
        self.info.clear();
        self.registry = Registry::default();
        self.companion = None;
        self.clip_sync = ClipSync::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn companion(state: CompanionState, msg: Option<&str>) -> Frame {
        Frame {
            kind: Kind::Control,
            stream: 0,
            payload: Control::Companion {
                state,
                msg: msg.map(str::to_owned),
            }
            .encode(),
        }
    }

    #[tokio::test]
    async fn hub_keeps_the_companion_msg() {
        let mut hub = Hub::new(None);
        let (helper, _rx) = mpsc::channel(4);
        assert_eq!(hub.companion(), None);
        let off = "keyboard off in Lantern settings";
        hub.on_helper(companion(CompanionState::Ready, Some(off)), &helper)
            .await
            .unwrap();
        assert_eq!(
            hub.companion(),
            Some((CompanionState::Ready, Some(off.to_owned())))
        );
        hub.on_helper(companion(CompanionState::Ready, None), &helper)
            .await
            .unwrap();
        assert_eq!(hub.companion(), Some((CompanionState::Ready, None)));
        hub.reset();
        assert_eq!(hub.companion(), None, "cleared when the session ends");
    }
}
