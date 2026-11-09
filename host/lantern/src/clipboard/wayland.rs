//! The PC clipboard over `ext_data_control_v1` (KDE Plasma 6 has it; no zwlr fallback).
//!
//! A dedicated std thread owns its own `wayland-client` connection and event queue, so the
//! tokio runtime never blocks on the compositor:
//! - Selections are read by a second thread (the "reader"): the Wayland thread only issues
//!   `receive` and hands the pipe over, so it keeps dispatching meanwhile (it must: reading our
//!   own selection needs it to answer the `send`). Each pipe read has a 1 s timeout and a
//!   `MAX_CLIP + 1` byte cap.
//! - Our own source's `send`s are written by short-lived threads with a 5 s timeout on a
//!   non-blocking fd, at most [`MAX_WRITERS`] at once, so a slow reader can't wedge anything.
//! - Only the regular selection; the primary selection is ignored.
//! - No compositor (`WAYLAND_DISPLAY` unset, socket missing, global missing, connection lost):
//!   one warning, then a retry every [`RETRY`].
//!
//! Clipboard text is never logged, only its length.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use lantern_proto::MAX_CLIP;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

use super::{
    decode_text, is_secret_hint, pick_mime, ClipHandle, PcOffer, SECRET_HINT_MIME, TEXT_MIMES,
};

/// How long to wait between connection attempts while there is no usable compositor.
pub const RETRY: Duration = Duration::from_secs(10);
/// How long one selection read may take.
pub const READ_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a write of our own selection to a reader may take.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Concurrent writes of our own selection; further `send`s are refused (fd closed).
pub const MAX_WRITERS: usize = 8;
/// The password hint is a single word.
const HINT_CAP: usize = 64;

/// Starts the clipboard thread. PC selections go to `tx`; the handle sets the PC selection.
/// The thread ends when every [`ClipHandle`] is dropped or `tx` is closed.
pub fn spawn(tx: tokio::sync::mpsc::Sender<PcOffer>) -> ClipHandle {
    let (cmd_tx, cmds) = std_mpsc::channel();
    let (wake_r, wake_w) = match pipe(true, true) {
        Ok(fds) => fds,
        Err(error) => {
            tracing::warn!(%error, "no PC clipboard sync: wake pipe failed");
            return ClipHandle {
                tx: cmd_tx,
                wake: None,
            };
        }
    };
    let (jobs_tx, jobs) = std_mpsc::channel::<Job>();
    let spawned = std::thread::Builder::new()
        .name("lantern-clip-read".into())
        .spawn(move || reader(jobs, tx))
        .and_then(|_| {
            std::thread::Builder::new()
                .name("lantern-clip".into())
                .spawn(move || run(cmds, wake_r, jobs_tx))
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "no PC clipboard sync: thread spawn failed");
    }
    ClipHandle {
        tx: cmd_tx,
        wake: Some(Arc::new(wake_w)),
    }
}

/// One selection to read: the pipes `receive` writes to.
struct Job {
    mimes: Vec<String>,
    hint: Option<OwnedFd>,
    text: Option<OwnedFd>,
}

/// The reader thread: reads each selection in order and passes it to the daemon.
fn reader(jobs: std_mpsc::Receiver<Job>, tx: tokio::sync::mpsc::Sender<PcOffer>) {
    for job in jobs {
        // A hint that can't be read counts as secret: never risk sending a password.
        let secret = job
            .hint
            .is_some_and(|fd| match read_pipe(fd, HINT_CAP, READ_TIMEOUT) {
                Ok(content) => is_secret_hint(&content),
                Err(error) => {
                    tracing::debug!(
                        error = format!("{error:#}"),
                        "password hint unreadable; treating as secret"
                    );
                    true
                }
            });
        let text = match job.text {
            // Dropped unread: a secret never even enters our memory.
            Some(_) if secret => None,
            Some(fd) => match read_pipe(fd, MAX_CLIP + 1, READ_TIMEOUT) {
                Ok(bytes) => decode_text(bytes),
                Err(error) => {
                    tracing::debug!(error = format!("{error:#}"), "PC selection unreadable");
                    None
                }
            },
            None => None,
        };
        tracing::debug!(
            secret,
            len = text.as_ref().map(String::len),
            "PC clipboard changed"
        );
        let offer = PcOffer {
            mimes: job.mimes,
            text,
            secret,
        };
        if tx.blocking_send(offer).is_err() {
            return;
        }
    }
}

/// The Wayland thread: connect, serve, and on any failure warn once and retry every [`RETRY`].
fn run(cmds: std_mpsc::Receiver<String>, wake: OwnedFd, jobs: std_mpsc::Sender<Job>) {
    let mut warned = false;
    loop {
        let mut connected = false;
        match serve(&cmds, &wake, &jobs, &mut connected) {
            Ok(()) => return, // every handle is gone
            Err(error) => {
                if connected {
                    warned = false;
                }
                if !warned {
                    tracing::warn!(
                        error = format!("{error:#}"),
                        retry_in = ?RETRY,
                        "PC clipboard unavailable; retrying"
                    );
                    warned = true;
                } else {
                    tracing::debug!(
                        error = format!("{error:#}"),
                        "PC clipboard still unavailable"
                    );
                }
            }
        }
        // Wait out RETRY; phone texts arriving meanwhile are dropped (only changes sync).
        let until = Instant::now() + RETRY;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            let _ = poll_in(&[wake.as_fd()], Some(left));
            drain(&wake);
            loop {
                match cmds.try_recv() {
                    Ok(_) => {}
                    Err(std_mpsc::TryRecvError::Empty) => break,
                    Err(std_mpsc::TryRecvError::Disconnected) => return,
                }
            }
            if left.is_zero() {
                break;
            }
        }
    }
}

/// The compositor socket: `$WAYLAND_DISPLAY`, or (when lanternd started before the session
/// exported it) the systemd user manager's `WAYLAND_DISPLAY`, under `$XDG_RUNTIME_DIR`.
fn socket_path() -> Result<PathBuf> {
    let name = std::env::var_os("WAYLAND_DISPLAY")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let out = std::process::Command::new("systemctl")
                .args(["--user", "show-environment"])
                .stderr(std::process::Stdio::null())
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .find_map(|l| l.strip_prefix("WAYLAND_DISPLAY="))
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .context("WAYLAND_DISPLAY is not set")?;
    if name.is_absolute() {
        return Ok(name);
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .context("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(dir).join(name))
}

/// One connection's lifetime. `Ok` only when every handle is dropped.
fn serve(
    cmds: &std_mpsc::Receiver<String>,
    wake: &OwnedFd,
    jobs: &std_mpsc::Sender<Job>,
    connected: &mut bool,
) -> Result<()> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path)
        .with_context(|| format!("connect to the compositor at {}", path.display()))?;
    let conn = Connection::from_socket(stream).context("wayland connection")?;
    let (globals, mut queue): (_, EventQueue<State>) =
        registry_queue_init(&conn).context("wayland registry")?;
    let qh = queue.handle();
    let manager: ExtDataControlManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("the compositor has no ext_data_control_manager_v1")?;
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).context("no wl_seat")?;
    let device = manager.get_data_device(&seat, &qh, ());
    let mut state = State {
        conn: conn.clone(),
        qh: qh.clone(),
        manager,
        device,
        current: None,
        source: None,
        jobs: jobs.clone(),
        finished: false,
        writers: Arc::new(AtomicUsize::new(0)),
        live: false,
    };
    // The selection already there when we connect is state, not a change: not read.
    queue.roundtrip(&mut state).context("wayland roundtrip")?;
    state.live = true;
    *connected = true;
    tracing::info!(socket = %path.display(), "PC clipboard sync on");
    loop {
        queue
            .dispatch_pending(&mut state)
            .context("wayland dispatch")?;
        queue.flush().context("wayland flush")?;
        if state.finished {
            bail!("the compositor ended the clipboard device");
        }
        let Some(guard) = queue.prepare_read() else {
            continue; // events already queued
        };
        let ready = poll_in(&[guard.connection_fd(), wake.as_fd()], None)?;
        if ready[0] {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error).context("wayland read"),
            }
        } else {
            drop(guard);
        }
        if ready[1] {
            drain(wake);
            let mut latest = None;
            loop {
                match cmds.try_recv() {
                    Ok(text) => latest = Some(text),
                    Err(std_mpsc::TryRecvError::Empty) => break,
                    Err(std_mpsc::TryRecvError::Disconnected) => return Ok(()),
                }
            }
            if let Some(text) = latest {
                state.set(text);
            }
        }
    }
}

struct State {
    conn: Connection,
    qh: QueueHandle<State>,
    manager: ExtDataControlManagerV1,
    device: ExtDataControlDeviceV1,
    /// The current selection's offer, destroyed when the next one arrives.
    current: Option<ExtDataControlOfferV1>,
    /// Our own selection, while we hold it.
    source: Option<ExtDataControlSourceV1>,
    jobs: std_mpsc::Sender<Job>,
    finished: bool,
    writers: Arc<AtomicUsize>,
    /// False during the first roundtrip: selections then are the existing clipboard.
    live: bool,
}

/// Mime types an offer announced (its `offer` events precede the `selection` naming it).
#[derive(Default)]
struct OfferData(Mutex<Vec<String>>);

impl State {
    /// Puts `text` on the PC clipboard: a new source offering [`TEXT_MIMES`].
    fn set(&mut self, text: String) {
        let len = text.len();
        let source = self
            .manager
            .create_data_source(&self.qh, Arc::<[u8]>::from(text.into_bytes()));
        for mime in TEXT_MIMES {
            source.offer(mime.to_owned());
        }
        self.device.set_selection(Some(&source));
        // The old source gets `cancelled` and is destroyed then.
        self.source = Some(source);
        tracing::debug!(len, "phone clipboard put on the PC");
    }

    /// A new regular selection: ask for its text (and password hint) and hand the pipes to the
    /// reader.
    fn selection(&mut self, offer: Option<ExtDataControlOfferV1>) {
        if let Some(old) = self.current.take() {
            old.destroy();
        }
        let Some(offer) = offer else {
            return; // the clipboard was cleared: nothing to sync
        };
        if !self.live {
            self.current = Some(offer);
            return;
        }
        let mimes = offer
            .data::<OfferData>()
            .map(|d| d.0.lock().unwrap_or_else(|p| p.into_inner()).clone())
            .unwrap_or_default();
        if let Some(mime) = pick_mime(&mimes) {
            match self.receive(&offer, &mimes, mime) {
                Ok(job) => {
                    let _ = self.jobs.send(job);
                }
                Err(error) => tracing::warn!(error = format!("{error:#}"), "PC selection not read"),
            }
        }
        self.current = Some(offer);
    }

    fn receive(&self, offer: &ExtDataControlOfferV1, mimes: &[String], mime: &str) -> Result<Job> {
        // The hint first: a password manager answers in request order.
        let hint = if mimes.iter().any(|m| m == SECRET_HINT_MIME) {
            let (r, w) = pipe(true, false)?;
            offer.receive(SECRET_HINT_MIME.to_owned(), w.as_fd());
            Some((r, w))
        } else {
            None
        };
        let (r, w) = pipe(true, false)?;
        offer.receive(mime.to_owned(), w.as_fd());
        // Our write ends must be closed once sent, or the reads never see EOF.
        self.conn.flush().context("flush receive")?;
        drop(w);
        Ok(Job {
            mimes: mimes.to_vec(),
            hint: hint.map(|(r, w)| {
                drop(w);
                r
            }),
            text: Some(r),
        })
    }

    /// Someone (maybe us) reads our selection: write it from a short-lived thread.
    fn send(&self, data: Arc<[u8]>, mime: &str, fd: OwnedFd) {
        if !TEXT_MIMES.contains(&mime) {
            return; // drops (closes) fd
        }
        if self.writers.fetch_add(1, Ordering::SeqCst) >= MAX_WRITERS {
            self.writers.fetch_sub(1, Ordering::SeqCst);
            tracing::debug!("too many clipboard readers; refusing one");
            return;
        }
        let writers = self.writers.clone();
        let spawned = std::thread::Builder::new()
            .name("lantern-clip-write".into())
            .spawn(move || {
                if let Err(error) = write_fd(fd, &data, WRITE_TIMEOUT) {
                    tracing::debug!(
                        error = format!("{error:#}"),
                        "clipboard write to a reader failed"
                    );
                }
                writers.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            self.writers.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::Selection { id } => state.selection(id),
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                offer.destroy(); // primary selection: ignored
            }
            ext_data_control_device_v1::Event::Finished => state.finished = true,
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, OfferData::default()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, OfferData> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        data: &OfferData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            data.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, Arc<[u8]>> for State {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        data: &Arc<[u8]>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.send(data.clone(), &mime_type, fd)
            }
            ext_data_control_source_v1::Event::Cancelled => {
                if state.source.as_ref() == Some(source) {
                    state.source = None;
                }
                source.destroy();
            }
            _ => {}
        }
    }
}

/// A pipe; `nonblock_read`/`nonblock_write` set `O_NONBLOCK` on that end only (the other end
/// may go to another client, which expects a normal blocking fd).
fn pipe(nonblock_read: bool, nonblock_write: bool) -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: fds is a valid 2-int buffer.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe2 just returned these two fds, owned by nobody else.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    if nonblock_read {
        set_nonblocking(r.as_fd())?;
    }
    if nonblock_write {
        set_nonblocking(w.as_fd())?;
    }
    Ok((r, w))
}

fn set_nonblocking(fd: BorrowedFd) -> std::io::Result<()> {
    // SAFETY: fcntl on a valid fd.
    unsafe {
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Waits until any of `fds` is readable (or `timeout`); which ones are. EINTR counts as none.
fn poll_in(fds: &[BorrowedFd], timeout: Option<Duration>) -> std::io::Result<Vec<bool>> {
    poll(fds, libc::POLLIN, timeout)
}

fn poll(
    fds: &[BorrowedFd],
    events: libc::c_short,
    timeout: Option<Duration>,
) -> std::io::Result<Vec<bool>> {
    let mut pfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|fd| libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        })
        .collect();
    let ms = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
    // SAFETY: pfds is a valid array of pfds.len() pollfds.
    let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, ms) };
    if n < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(vec![false; fds.len()]);
        }
        return Err(error);
    }
    Ok(pfds.iter().map(|p| p.revents != 0).collect())
}

/// Empties the (non-blocking) wake pipe.
fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 64];
    // SAFETY: reads into a valid buffer of buf.len() bytes.
    while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

/// Reads a non-blocking pipe to EOF, stopping at `cap` bytes; fails after `timeout`.
fn read_pipe(fd: OwnedFd, cap: usize, timeout: Duration) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    while out.len() < cap {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            bail!("read timed out after {timeout:?}");
        };
        poll_in(&[fd.as_fd()], Some(left))?;
        let want = buf.len().min(cap - out.len());
        // SAFETY: reads at most `want` <= buf.len() bytes into buf.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), want) };
        if n == 0 {
            return Ok(out);
        }
        if n < 0 {
            let error = std::io::Error::last_os_error();
            match error.kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted => continue,
                _ => return Err(error).context("read"),
            }
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    Ok(out)
}

/// Writes all of `data` to `fd` (made non-blocking), giving up after `timeout`.
fn write_fd(fd: OwnedFd, data: &[u8], timeout: Duration) -> Result<()> {
    set_nonblocking(fd.as_fd())?;
    let deadline = Instant::now() + timeout;
    let mut off = 0;
    while off < data.len() {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            bail!("write timed out after {timeout:?}");
        };
        poll(&[fd.as_fd()], libc::POLLOUT, Some(left))?;
        // SAFETY: writes from a valid slice.
        let n = unsafe {
            libc::write(
                fd.as_raw_fd(),
                data[off..].as_ptr().cast(),
                data.len() - off,
            )
        };
        if n < 0 {
            let error = std::io::Error::last_os_error();
            match error.kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted => continue,
                _ => return Err(error).context("write"),
            }
        }
        off += n as usize;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_pipe_reads_to_eof() {
        let (r, w) = pipe(true, false).unwrap();
        write_fd(w, b"hello", Duration::from_secs(1)).unwrap();
        assert_eq!(read_pipe(r, 100, Duration::from_secs(1)).unwrap(), b"hello");
    }

    #[test]
    fn read_pipe_caps_and_times_out() {
        let (r, w) = pipe(true, false).unwrap();
        write_fd(
            w.try_clone().unwrap(),
            b"0123456789",
            Duration::from_secs(1),
        )
        .unwrap();
        // Capped before EOF: the open writer doesn't matter.
        assert_eq!(read_pipe(r, 4, Duration::from_secs(1)).unwrap(), b"0123");
        let (r2, _w2) = pipe(true, false).unwrap();
        let t0 = Instant::now();
        assert!(read_pipe(r2, 4, Duration::from_millis(100)).is_err());
        assert!(t0.elapsed() < Duration::from_secs(1));
        drop(w);
    }

    #[test]
    fn write_fd_gives_up_on_a_stuck_reader() {
        let (_r, w) = pipe(false, false).unwrap();
        let big = vec![0u8; 1 << 20]; // more than a pipe holds
        let t0 = Instant::now();
        assert!(write_fd(w, &big, Duration::from_millis(100)).is_err());
        assert!(t0.elapsed() < Duration::from_secs(1));
    }
}
