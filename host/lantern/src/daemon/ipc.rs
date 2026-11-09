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
    read_frame_async, write_frame_async, CompanionState, Control, Frame, InputEvent, Kind,
    SessionState, StatusReport, WindowInfo, MAX_IME_TEXT,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::AbortHandle;
use tokio::time::timeout;

use super::catalog::{AppEntry, Catalog};
use super::registry::{Claim, Registry};

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
    let mut stream = std::os::unix::net::UnixStream::connect(sock).with_context(|| {
        format!(
            "lanternd is not running (cannot connect to {})",
            sock.display()
        )
    })?;
    stream.set_read_timeout(Some(OPEN_TIMEOUT))?;
    stream.set_write_timeout(Some(OPEN_TIMEOUT))?;
    lantern_proto::write_frame(&mut stream, &control(0, Control::Status))
        .context("send status request")?;
    let reply = lantern_proto::read_frame(&mut stream)
        .context("read status reply")?
        .context("lanternd closed the connection without a status reply")?;
    match (reply.kind, Control::decode(&reply.payload)) {
        (Kind::Control, Ok(Control::StatusReport(report))) => Ok(report),
        (Kind::Control, Ok(Control::Error { msg, .. })) => bail!("lanternd: {msg}"),
        (kind, other) => bail!("unexpected status reply: {kind:?} {other:?}"),
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
    /// The live session's last `companion` state; cleared when the session ends.
    companion: Option<CompanionState>,
    next_cid: u64,
    events_tx: mpsc::Sender<ClientEvent>,
    events: mpsc::Receiver<ClientEvent>,
    /// The phone's launcher apps as menu entries, if the daemon keeps them.
    catalog: Option<Catalog>,
    /// The live session's serial, for re-opening the catalogue sync window after `apps_end`.
    catalog_serial: Option<String>,
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
            catalog_serial: None,
        }
    }

    /// Keeps `catalog` in step with the helper's catalogue messages.
    pub fn set_catalog(&mut self, catalog: Catalog) {
        self.catalog = Some(catalog);
    }

    /// A session with `serial` is up: the first catalogue sync window opens.
    pub fn session_started(&mut self, serial: &str) {
        self.catalog_serial = Some(serial.to_owned());
        if let Some(catalog) = self.catalog.as_mut() {
            catalog.begin_sync(serial);
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
            Control::AppsEnd { count } => {
                match catalog.end_sync(count) {
                    Ok(removed) => tracing::info!(count, removed, "catalogue synced"),
                    Err(error) => {
                        tracing::warn!(error = format!("{error:#}"), "catalogue prune failed")
                    }
                }
                // The next full sync (a companion relink, a profile change) gets its own window.
                if let Some(serial) = &self.catalog_serial {
                    catalog.begin_sync(serial);
                }
            }
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
        let companion = self.companion.filter(|_| state == SessionState::Ready);
        StatusReport {
            state,
            serial,
            helper,
            windows,
            companion,
        }
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
            self.reply_and_finish(cid, control(0, Control::error(NOT_CONNECTED)));
            return Ok(());
        };
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
                    self.companion = Some(state);
                }
                Ok(
                    c
                    @ (Control::App { .. } | Control::AppsEnd { .. } | Control::AppRemoved { .. }),
                ) => {
                    log_catalogue(&c);
                    self.on_catalogue(c);
                }
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
    }
}
