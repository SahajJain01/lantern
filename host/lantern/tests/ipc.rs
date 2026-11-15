//! Daemon <-> window IPC, against an in-process daemon (`serve`) whose phone session is a fake
//! helper on a local TCP socket. No phone and no adb are involved.

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use lantern::adb::{Adb, DeviceEvent};
use lantern::clipboard::{ClipHandle, PcClipboard, PcOffer};
use lantern::daemon::catalog::{Catalog, Dirs};
use lantern::daemon::ipc;
use lantern::daemon::session::{handshake, SessionConn};
use lantern::daemon::{serve, serve_with, Parts, RetryPolicy};
use lantern::settings::{Key, SettingsHandle};
use lantern_proto::{
    read_frame_async, write_frame_async, CloseReason, CompanionState, Control, Frame, ImeOp, Kind,
    SessionState, StatusReport, WindowInfo, MAX_IME_TEXT,
};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Instant};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const STEP: Duration = Duration::from_secs(2);

static DIRS: AtomicUsize = AtomicUsize::new(0);

/// A fresh, private stand-in for `$XDG_RUNTIME_DIR`.
fn runtime_dir() -> PathBuf {
    let n = DIRS.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("lantern-ipc-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn control(stream: u32, c: Control) -> Frame {
    Frame {
        kind: Kind::Control,
        stream,
        payload: c.encode(),
    }
}

fn open(pkg: &str, user: u32, token: Option<&str>) -> Frame {
    control(
        0,
        Control::Open {
            pkg: pkg.into(),
            user,
            w: 1280,
            h: 800,
            dpi: 160,
            activation_token: token.map(str::to_owned),
            phone: None,
        },
    )
}

fn decode(f: &Frame) -> Control {
    assert_eq!(f.kind, Kind::Control, "expected a control frame, got {f:?}");
    Control::decode(&f.payload).unwrap()
}

enum HelperCmd {
    Send(Frame),
    Die,
}

/// The fake phone side: handshakes every connection the daemon makes, reports every frame the
/// daemon sends (Hello excluded), writes what the test asks, and can drop the connection.
struct FakeHelper {
    from_daemon: mpsc::UnboundedReceiver<Frame>,
    cmds: mpsc::UnboundedSender<HelperCmd>,
    sessions: mpsc::UnboundedReceiver<usize>,
    /// Connections the daemon closed (the helper read EOF).
    eofs: Arc<AtomicUsize>,
}

impl FakeHelper {
    async fn spawn() -> (Self, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (frames_tx, from_daemon) = mpsc::unbounded_channel();
        let (cmds, mut cmd_rx) = mpsc::unbounded_channel::<HelperCmd>();
        let (sessions_tx, sessions) = mpsc::unbounded_channel();
        let eofs = Arc::new(AtomicUsize::new(0));
        let eof_count = eofs.clone();
        tokio::spawn(async move {
            let mut n = 0;
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let (mut rd, mut wr) = socket.into_split();
                let hello = read_frame_async(&mut rd).await.unwrap().unwrap();
                assert!(matches!(decode(&hello), Control::Hello { .. }));
                let reply = control(
                    0,
                    Control::Hello {
                        version: 1,
                        token: String::new(),
                        keyboard: true,
                        screen_off: true,
                    },
                );
                write_frame_async(&mut wr, &reply).await.unwrap();
                let _ = sessions_tx.send(n);
                n += 1;
                let frames_tx = frames_tx.clone();
                let eof_count = eof_count.clone();
                let reader = tokio::spawn(async move {
                    while let Ok(Some(frame)) = read_frame_async(&mut rd).await {
                        let _ = frames_tx.send(frame);
                    }
                    eof_count.fetch_add(1, Ordering::SeqCst);
                });
                loop {
                    match cmd_rx.recv().await {
                        Some(HelperCmd::Send(frame)) => {
                            if write_frame_async(&mut wr, &frame).await.is_err() {
                                break;
                            }
                        }
                        Some(HelperCmd::Die) => break,
                        None => return,
                    }
                }
                reader.abort();
                drop(wr);
            }
        });
        (
            Self {
                from_daemon,
                cmds,
                sessions,
                eofs,
            },
            port,
        )
    }

    fn send(&self, frame: Frame) {
        let _ = self.cmds.send(HelperCmd::Send(frame));
    }

    fn die(&self) {
        let _ = self.cmds.send(HelperCmd::Die);
    }

    /// The next frame the daemon sent to the helper.
    async fn next(&mut self) -> Frame {
        timeout(STEP, self.from_daemon.recv())
            .await
            .expect("helper received nothing from the daemon")
            .unwrap()
    }

    /// Asserts the daemon sends the helper nothing (other than a session Bye) for a while.
    async fn assert_quiet(&mut self) {
        if let Ok(Some(frame)) = timeout(Duration::from_millis(300), self.from_daemon.recv()).await
        {
            panic!(
                "helper unexpectedly received {frame:?} ({:?})",
                Control::decode(&frame.payload)
            );
        }
    }

    async fn next_session(&mut self) -> usize {
        timeout(STEP, self.sessions.recv())
            .await
            .expect("no helper session")
            .unwrap()
    }
}

fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        initial: Duration::from_millis(50),
        max: Duration::from_millis(200),
        stable_after: Duration::from_secs(60),
    }
}

struct Daemon {
    sock: PathBuf,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}

/// Starts `serve` on a private runtime dir. With `phone`, a device is online and every session
/// goes to the fake helper on `port`; without, no device ever appears.
fn start_daemon(port: Option<u16>) -> Daemon {
    start_daemon_with(port, None)
}

/// [`start_daemon`] with a menu catalogue.
fn start_daemon_with(port: Option<u16>, catalog: Option<Catalog>) -> Daemon {
    start_daemon_full(port, catalog, None)
}

/// [`start_daemon_with`] plus a (fake) PC clipboard.
fn start_daemon_full(
    port: Option<u16>,
    catalog: Option<Catalog>,
    clipboard: Option<PcClipboard>,
) -> Daemon {
    start_daemon_parts(
        port,
        Parts {
            catalog,
            clipboard,
            ..Default::default()
        },
    )
}

/// [`start_daemon`] with every optional part given.
fn start_daemon_parts(port: Option<u16>, parts: Parts) -> Daemon {
    let rt = runtime_dir();
    let listener = ipc::bind(&rt).unwrap();
    let sock = ipc::socket_path_in(&rt);
    let events = match port {
        Some(_) => stream::iter([DeviceEvent::Online("dev".into())])
            .chain(stream::pending())
            .boxed(),
        None => stream::pending().boxed(),
    };
    let starter = move |serial: String| async move {
        let port = port.expect("no phone in this test");
        let stream = TcpStream::connect(("127.0.0.1", port)).await?;
        let stream = handshake(stream, TOKEN, true, true).await?;
        Ok(SessionConn::attach(stream, Adb::new(serial), "sha"))
    };
    let (stop, stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(serve_with(
        listener,
        parts,
        None,
        events,
        starter,
        async move {
            let _ = stop_rx.await;
            "test-stop"
        },
        fast_policy(),
    ));
    Daemon {
        sock,
        stop: Some(stop),
        task,
    }
}

impl Daemon {
    async fn shutdown(mut self) {
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

async fn send(client: &mut UnixStream, frame: &Frame) {
    write_frame_async(client, frame).await.unwrap();
}

/// The next frame the daemon sent this client; `None` is EOF.
async fn recv(client: &mut UnixStream) -> Option<Frame> {
    timeout(STEP, read_frame_async(client))
        .await
        .expect("client got neither a frame nor EOF")
        .unwrap_or(None)
}

async fn assert_eof(client: &mut UnixStream) {
    let got = recv(client).await;
    assert!(got.is_none(), "expected EOF, got {got:?}");
}

/// Connects and sends `open`, retrying while the daemon has no phone session yet. Returns the
/// client and the daemon's first reply.
async fn connect_open(sock: &Path, frame: Frame) -> (UnixStream, Option<Frame>) {
    let deadline = Instant::now() + STEP;
    loop {
        let mut client = UnixStream::connect(sock).await.unwrap();
        send(&mut client, &frame).await;
        let reply = recv(&mut client).await;
        let not_connected = matches!(
            reply.as_ref().map(decode),
            Some(Control::Error { ref msg, .. }) if msg == "phone not connected"
        );
        if !not_connected {
            return (client, reply);
        }
        assert!(
            Instant::now() < deadline,
            "phone session never became active"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Opens a NEW window: the helper must see the Open on the assigned stream (activation token
/// stripped), then answers Opened, which reaches the client. Returns (client, win_id).
async fn open_window(
    sock: &Path,
    helper: &mut FakeHelper,
    pkg: &str,
    user: u32,
) -> (UnixStream, u32) {
    let mut client = {
        // The reply only arrives after the helper answers, so open without waiting for it.
        let deadline = Instant::now() + STEP;
        loop {
            let mut client = UnixStream::connect(sock).await.unwrap();
            send(&mut client, &open(pkg, user, Some("tok-first"))).await;
            match timeout(Duration::from_millis(200), read_frame_async(&mut client)).await {
                Err(_) => break client, // waiting on the helper: the open was accepted
                Ok(Ok(Some(f))) if matches!(decode(&f), Control::Error { ref msg, .. } if msg == "phone not connected") =>
                {
                    assert!(
                        Instant::now() < deadline,
                        "phone session never became active"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Ok(other) => panic!("unexpected reply to open: {other:?}"),
            }
        }
    };
    let fwd = helper.next().await;
    let win = fwd.stream;
    assert!(win > 0);
    assert_eq!(
        decode(&fwd),
        Control::Open {
            pkg: pkg.into(),
            user,
            w: 1280,
            h: 800,
            dpi: 160,
            activation_token: None,
            phone: None,
        },
        "helper must get the Open minus the activation token"
    );
    let opened = control(
        win,
        Control::Opened {
            display: 7,
            task: 9,
            codec: "h264".into(),
            w: 1280,
            h: 800,
            moved: false,
            label: None,
            work: false,
        },
    );
    helper.send(opened.clone());
    assert_eq!(recv(&mut client).await, Some(opened));
    (client, win)
}

// ---------------------------------------------------------------------------------------------

#[test]
fn socket_mode_holds_under_parallel_binds() {
    // umask is process-wide: concurrent binds must not undo each other's 077.
    let threads: Vec<_> = (0..32)
        .map(|_| {
            std::thread::spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let _enter = rt.enter();
                for _ in 0..300 {
                    let dir = runtime_dir();
                    let listener = ipc::bind(&dir).unwrap();
                    let sock = ipc::socket_path_in(&dir);
                    assert_eq!(mode(&sock) & 0o077, 0, "socket {} too open", sock.display());
                    drop(listener);
                    let _ = std::fs::remove_dir_all(&dir);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

#[tokio::test]
async fn socket_dir_is_0700() {
    let rt = runtime_dir();
    let _listener = ipc::bind(&rt).unwrap();
    let dir = rt.join("lantern");
    assert_eq!(mode(&dir), 0o700);
    let sock = ipc::socket_path_in(&rt);
    assert_eq!(sock, dir.join("daemon.sock"));
    assert!(std::fs::metadata(&sock).unwrap().file_type().is_socket());
    assert_eq!(
        mode(&sock) & 0o077,
        0,
        "socket must not be group/other accessible"
    );
}

#[tokio::test]
async fn existing_dir_with_loose_mode_is_tightened() {
    let rt = runtime_dir();
    let dir = rt.join("lantern");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _listener = ipc::bind(&rt).unwrap();
    assert_eq!(mode(&dir), 0o700);
}

#[tokio::test]
async fn stale_socket_is_replaced() {
    let rt = runtime_dir();
    let sock = ipc::socket_path_in(&rt);
    drop(ipc::bind(&rt).unwrap()); // leaves the socket file behind, nobody listening
    assert!(sock.exists());
    let listener = ipc::bind(&rt).expect("a stale socket must not block startup");
    let _client = UnixStream::connect(&sock).await.unwrap();
    let _ = listener.accept().await.unwrap();
}

#[tokio::test]
async fn live_socket_is_not_stolen() {
    let rt = runtime_dir();
    let _first = ipc::bind(&rt).unwrap();
    let error = format!(
        "{:#}",
        ipc::bind(&rt).expect_err("second daemon must refuse")
    );
    assert!(
        error.contains("already running"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn open_without_phone_is_an_error() {
    let daemon = start_daemon(None);
    let mut client = UnixStream::connect(&daemon.sock).await.unwrap();
    send(&mut client, &open("com.example", 0, None)).await;
    let reply = recv(&mut client).await.expect("an error reply");
    assert_eq!(reply.stream, 0);
    assert_eq!(decode(&reply), Control::error("phone not connected"));
    assert_eq!(recv(&mut client).await, None);
    daemon.shutdown().await;
}

#[tokio::test]
async fn window_frames_route_both_ways() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (mut client, win) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;
    assert_eq!(win, 1);

    // Client -> helper, unchanged.
    let resize = control(
        win,
        Control::Resize {
            w: 640,
            h: 480,
            dpi: 120,
        },
    );
    send(&mut client, &resize).await;
    assert_eq!(helper.next().await, resize);
    let input = Frame {
        kind: Kind::Input,
        stream: win,
        payload: vec![3, 0, 0, 0, 0, 29, 0, 0, 0, 1, 0, 0, 0, 0],
    };
    send(&mut client, &input).await;
    assert_eq!(helper.next().await, input);
    let focus = control(
        win,
        Control::Focus {
            activation_token: None,
        },
    );
    send(&mut client, &focus).await;
    assert_eq!(helper.next().await, focus);

    // Helper -> client, unchanged.
    let video = Frame {
        kind: Kind::Video,
        stream: win,
        payload: vec![0, 0, 0, 0, 0, 0, 0, 1, 2, 0, 0, 0, 1, 0x65],
    };
    helper.send(video.clone());
    assert_eq!(recv(&mut client).await, Some(video));
    let resized = control(win, Control::Resized { w: 640, h: 480 });
    helper.send(resized.clone());
    assert_eq!(recv(&mut client).await, Some(resized));
    daemon.shutdown().await;
}

#[tokio::test]
async fn second_open_same_pkg_gets_focus_not_opened() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (mut first, win) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;

    let (mut second, reply) =
        connect_open(&daemon.sock, open("com.example", 0, Some("tok-second"))).await;
    let reply = reply.expect("a focus reply");
    assert_eq!(reply.stream, win, "focus reply names the existing window");
    assert_eq!(
        decode(&reply),
        Control::Focus {
            activation_token: None
        }
    );
    assert_eq!(
        recv(&mut second).await,
        None,
        "the second client is closed after focus"
    );

    let focus = recv(&mut first)
        .await
        .expect("the existing window is told to focus");
    assert_eq!(focus.stream, win);
    assert_eq!(
        decode(&focus),
        Control::Focus {
            activation_token: Some("tok-second".into())
        }
    );
    helper.assert_quiet().await; // no second Open reaches the phone

    // Same package for another user is a different window.
    let (_other, other_win) = open_window(&daemon.sock, &mut helper, "com.example", 10).await;
    assert_eq!(other_win, win + 1);
    daemon.shutdown().await;
}

#[tokio::test]
async fn client_disconnect_sends_close_to_helper() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (client, win) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;
    drop(client); // abrupt death: no Close sent

    let close = helper.next().await;
    assert_eq!(close.stream, win);
    assert_eq!(decode(&close), Control::Close);
    // The id was released: the same package opens a NEW window.
    let (_client, again) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;
    assert_eq!(again, win + 1);
    daemon.shutdown().await;
}

#[tokio::test]
async fn helper_closed_is_forwarded_and_releases_window() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (mut client, win) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;

    let close = control(win, Control::Close);
    send(&mut client, &close).await;
    assert_eq!(helper.next().await, close);
    let closed = control(
        win,
        Control::Closed {
            reason: CloseReason::Closed,
        },
    );
    helper.send(closed.clone());
    assert_eq!(recv(&mut client).await, Some(closed));
    assert_eof(&mut client).await;
    helper.assert_quiet().await; // a closed window is not closed twice

    let (_client, again) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;
    assert_eq!(again, win + 1);
    daemon.shutdown().await;
}

/// An open the phone cannot do: the helper answers error{msg} then closed{error} on N with no
/// opened. Both must reach the window client (which prints msg and exits 1, window::outcome).
#[tokio::test]
async fn failed_open_error_and_closed_reach_the_client() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let deadline = Instant::now() + STEP;
    let mut client = loop {
        let mut client = UnixStream::connect(&daemon.sock).await.unwrap();
        send(&mut client, &open("com.foo", 0, None)).await;
        match timeout(Duration::from_millis(200), read_frame_async(&mut client)).await {
            Err(_) => break client, // forwarded; waiting on the helper
            Ok(_) => {
                assert!(
                    Instant::now() < deadline,
                    "phone session never became active"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    let fwd = helper.next().await;
    let win = fwd.stream;
    assert!(matches!(decode(&fwd), Control::Open { .. }));
    let error = control(win, Control::error("no launch intent for com.foo"));
    let closed = control(
        win,
        Control::Closed {
            reason: CloseReason::Error,
        },
    );
    helper.send(error.clone());
    helper.send(closed.clone());
    assert_eq!(recv(&mut client).await, Some(error));
    assert_eq!(recv(&mut client).await, Some(closed));
    assert_eof(&mut client).await;
    daemon.shutdown().await;
}

#[tokio::test]
async fn wrong_stream_drops_the_client() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (mut client, win) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;
    send(
        &mut client,
        &control(win + 5, Control::Resize { w: 1, h: 1, dpi: 1 }),
    )
    .await;
    assert_eof(&mut client).await;
    let close = helper.next().await;
    assert_eq!((close.stream, decode(&close)), (win, Control::Close));
    daemon.shutdown().await;
}

#[tokio::test]
async fn first_frame_must_be_open() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    helper.next_session().await;
    let mut client = UnixStream::connect(&daemon.sock).await.unwrap();
    send(&mut client, &control(0, Control::Close)).await;
    let reply = recv(&mut client).await.expect("an error reply");
    assert!(matches!(decode(&reply), Control::Error { .. }));
    assert_eof(&mut client).await;
    helper.assert_quiet().await;
    daemon.shutdown().await;
}

#[tokio::test]
async fn daemon_drop_eofs_clients() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (mut client, _) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;
    daemon.task.abort();
    let t0 = Instant::now();
    assert_eof(&mut client).await;
    assert!(t0.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn daemon_shutdown_eofs_clients() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (mut client, _) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;
    let sock = daemon.sock.clone();
    daemon.shutdown().await;
    assert_eof(&mut client).await;
    assert!(!sock.exists(), "socket file is removed on shutdown");
}

#[tokio::test]
async fn session_end_eofs_clients_and_never_reuses_ids() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    assert_eq!(helper.next_session().await, 0);
    let (mut client, win) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;
    assert_eq!(win, 1);
    helper.die();
    assert_eof(&mut client).await;

    assert_eq!(
        helper.next_session().await,
        1,
        "daemon restarts the session"
    );
    let (_client, again) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;
    // Decision 1: window ids are unique for lanternd's lifetime, across sessions and phones.
    assert_eq!(again, 2, "ids are never reused within the run");
    daemon.shutdown().await;
}

#[tokio::test]
async fn slow_client_is_dropped_without_stalling_others() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (_slow, slow_win) = open_window(&daemon.sock, &mut helper, "com.slow", 0).await;
    let (mut fast, fast_win) = open_window(&daemon.sock, &mut helper, "com.fast", 0).await;

    // Far more than the per-client queue plus the socket buffer; the slow client never reads.
    for _ in 0..600 {
        helper.send(Frame {
            kind: Kind::Video,
            stream: slow_win,
            payload: vec![0; 64 << 10],
        });
    }
    let video = Frame {
        kind: Kind::Video,
        stream: fast_win,
        payload: vec![1; 16],
    };
    helper.send(video.clone());
    assert_eq!(
        recv(&mut fast).await,
        Some(video),
        "a slow window stalled another"
    );

    let close = helper.next().await;
    assert_eq!(
        (close.stream, decode(&close)),
        (slow_win, Control::Close),
        "slow client treated as quit"
    );
    daemon.shutdown().await;
}

const VALID_KEY: [u8; 14] = [3, 0, 0, 0, 0, 29, 0, 0, 0, 1, 0, 0, 0, 0];

#[tokio::test]
async fn malformed_input_drops_only_that_client() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (mut bad, bad_win) = open_window(&daemon.sock, &mut helper, "com.bad", 0).await;
    let (mut good, good_win) = open_window(&daemon.sock, &mut helper, "com.good", 0).await;

    let garbage = Frame {
        kind: Kind::Input,
        stream: bad_win,
        payload: vec![9, 9, 9],
    };
    send(&mut bad, &garbage).await;
    assert_eof(&mut bad).await;
    let close = helper.next().await;
    assert_eq!(
        (close.stream, decode(&close)),
        (bad_win, Control::Close),
        "the helper gets only a close for the bad window, never the bad input"
    );
    helper.assert_quiet().await;

    // The other window and the session are untouched.
    let input = Frame {
        kind: Kind::Input,
        stream: good_win,
        payload: VALID_KEY.to_vec(),
    };
    send(&mut good, &input).await;
    assert_eq!(helper.next().await, input);
    let video = Frame {
        kind: Kind::Video,
        stream: good_win,
        payload: vec![1; 8],
    };
    helper.send(video.clone());
    assert_eq!(recv(&mut good).await, Some(video));
    daemon.shutdown().await;
}

#[tokio::test]
async fn truncated_and_trailing_input_are_rejected() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let mut trailing = VALID_KEY.to_vec();
    trailing.push(7);
    for (i, payload) in [VALID_KEY[..5].to_vec(), trailing].into_iter().enumerate() {
        let (mut client, win) =
            open_window(&daemon.sock, &mut helper, &format!("com.bad{i}"), 0).await;
        send(
            &mut client,
            &Frame {
                kind: Kind::Input,
                stream: win,
                payload,
            },
        )
        .await;
        assert_eof(&mut client).await;
        let close = helper.next().await;
        assert_eq!((close.stream, decode(&close)), (win, Control::Close));
    }
    daemon.shutdown().await;
}

#[tokio::test]
async fn client_focus_token_is_not_forwarded_to_helper() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (mut client, win) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;
    let focus = |activation_token: Option<&str>| {
        control(
            win,
            Control::Focus {
                activation_token: activation_token.map(str::to_owned),
            },
        )
    };
    send(&mut client, &focus(Some("secret-token"))).await;
    assert_eq!(helper.next().await, focus(None));
    daemon.shutdown().await;
}

/// `open.phone` is window -> lanternd only: the helper's `open` never carries the key.
#[tokio::test]
async fn open_phone_is_stripped_before_helper() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let with_phone = control(
        0,
        Control::Open {
            pkg: "com.example".into(),
            user: 10,
            w: 1280,
            h: 800,
            dpi: 160,
            activation_token: Some("tok".into()),
            phone: Some("dev".into()),
        },
    );
    let deadline = Instant::now() + STEP;
    let _client = loop {
        let mut client = UnixStream::connect(&daemon.sock).await.unwrap();
        send(&mut client, &with_phone).await;
        match timeout(Duration::from_millis(200), read_frame_async(&mut client)).await {
            Err(_) => break client, // forwarded; waiting on the helper
            Ok(_) => {
                assert!(
                    Instant::now() < deadline,
                    "phone session never became active"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    let fwd = helper.next().await;
    assert!(fwd.stream > 0);
    let raw: serde_json::Value = serde_json::from_slice(&fwd.payload).unwrap();
    assert!(raw.get("phone").is_none(), "{raw}");
    assert!(raw.get("activation_token").is_none(), "{raw}");
    assert_eq!(
        fwd.payload,
        br#"{"t":"open","pkg":"com.example","user":10,"w":1280,"h":800,"dpi":160}"#
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn reopen_when_owner_is_dead_opens_a_new_window() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (owner, win) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;

    // The owner dies in a way the daemon only learns of when it next writes: its read side is
    // shut, so the daemon's writer fails (EPIPE) and the owner's queue closes, while its write
    // side, and so the daemon's reader, stays open.
    // SAFETY: shutdown(2) on a valid fd owned by `owner`.
    let rc = unsafe { libc::shutdown(std::os::fd::AsRawFd::as_raw_fd(&owner), libc::SHUT_RD) };
    assert_eq!(rc, 0);
    helper.send(Frame {
        kind: Kind::Video,
        stream: win,
        payload: vec![0; 64],
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut second = UnixStream::connect(&daemon.sock).await.unwrap();
    send(&mut second, &open("com.example", 0, Some("tok-second"))).await;
    let close = helper.next().await;
    assert_eq!(
        (close.stream, decode(&close)),
        (win, Control::Close),
        "the dead owner is closed"
    );
    let fwd = helper.next().await;
    assert_ne!(fwd.stream, win, "a fresh window id");
    assert!(
        matches!(decode(&fwd), Control::Open { ref pkg, activation_token: None, .. } if pkg == "com.example"),
        "the reopen must become a new window, got {:?}",
        decode(&fwd)
    );
    let opened = control(
        fwd.stream,
        Control::Opened {
            display: 7,
            task: 9,
            codec: "h264".into(),
            w: 1280,
            h: 800,
            // The optional fields pass through the daemon unchanged.
            moved: true,
            label: Some("Example".into()),
            work: true,
        },
    );
    helper.send(opened.clone());
    assert_eq!(recv(&mut second).await, Some(opened));
    drop(owner);
    daemon.shutdown().await;
}

#[tokio::test]
async fn client_that_never_opens_is_closed() {
    let daemon = start_daemon(None);
    let mut idle = UnixStream::connect(&daemon.sock).await.unwrap();
    let t0 = Instant::now();
    let got = timeout(
        ipc::OPEN_TIMEOUT + Duration::from_secs(2),
        read_frame_async(&mut idle),
    )
    .await
    .expect("an idle client must be closed after the open deadline");
    assert!(matches!(got, Ok(None)), "expected EOF, got {got:?}");
    assert!(t0.elapsed() >= ipc::OPEN_TIMEOUT - Duration::from_millis(100));
    daemon.shutdown().await;
}

fn open_with(user: u32, w: u32, h: u32, dpi: u32) -> Frame {
    control(
        0,
        Control::Open {
            pkg: "com.example".into(),
            user,
            w,
            h,
            dpi,
            activation_token: None,
            phone: None,
        },
    )
}

/// The helper throws ProtocolError (ending every window) on an open outside its ranges
/// (Router.java open/range), so the daemon must reject it and never forward it.
#[tokio::test]
async fn out_of_range_open_is_rejected_and_not_forwarded() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    helper.next_session().await;
    let too_big_user = i32::MAX as u32 + 1;
    let bad = [
        open_with(too_big_user, 1280, 800, 160),
        open_with(u32::MAX, 1280, 800, 160),
        open_with(0, 0, 800, 160),
        open_with(0, 16385, 800, 160),
        open_with(0, 1280, 0, 160),
        open_with(0, 1280, 16385, 160),
        open_with(0, 1280, 800, 0),
        open_with(0, 1280, 800, 4097),
    ];
    for frame in bad {
        let (mut client, reply) = connect_open(&daemon.sock, frame.clone()).await;
        let reply = reply.unwrap_or_else(|| panic!("no error reply for {:?}", decode(&frame)));
        assert!(
            matches!(decode(&reply), Control::Error { .. }),
            "expected an error for {:?}, got {:?}",
            decode(&frame),
            decode(&reply)
        );
        assert_eof(&mut client).await;
    }
    helper.assert_quiet().await;

    // The inclusive upper bounds are accepted and forwarded unchanged.
    let edge = open_with(i32::MAX as u32, 16384, 16384, 4096);
    let mut client = UnixStream::connect(&daemon.sock).await.unwrap();
    send(&mut client, &edge).await;
    let fwd = helper.next().await;
    assert_eq!(decode(&fwd), decode(&edge));
    daemon.shutdown().await;
}

/// Same for resize (Router.java resize/range): an out-of-range one costs only that window.
#[tokio::test]
async fn out_of_range_resize_drops_only_that_client() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (mut good, good_win) = open_window(&daemon.sock, &mut helper, "com.good", 0).await;
    let bad = [
        (0, 800, 160),
        (16385, 800, 160),
        (1280, 0, 160),
        (1280, 16385, 160),
        (1280, 800, 0),
        (1280, 800, 4097),
    ];
    for (i, (w, h, dpi)) in bad.into_iter().enumerate() {
        let (mut client, win) =
            open_window(&daemon.sock, &mut helper, &format!("com.bad{i}"), 0).await;
        send(&mut client, &control(win, Control::Resize { w, h, dpi })).await;
        assert_eof(&mut client).await;
        let close = helper.next().await;
        assert_eq!(
            (close.stream, decode(&close)),
            (win, Control::Close),
            "resize {w}x{h}@{dpi} must close only its window, never reach the helper"
        );
    }
    helper.assert_quiet().await;

    for (w, h, dpi) in [(1, 1, 1), (16384, 16384, 4096)] {
        let resize = control(good_win, Control::Resize { w, h, dpi });
        send(&mut good, &resize).await;
        assert_eq!(helper.next().await, resize, "in-range resize is forwarded");
    }
    daemon.shutdown().await;
}

fn ime(win: u32, op: ImeOp, text: &str) -> Frame {
    control(
        win,
        Control::Ime {
            op,
            text: text.into(),
        },
    )
}

#[tokio::test]
async fn ipc_forwards_ime() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (mut client, win) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;
    let full = "ế".repeat(MAX_IME_TEXT / 3) + &"x".repeat(MAX_IME_TEXT % 3);
    assert_eq!(full.len(), MAX_IME_TEXT);
    for frame in [
        ime(win, ImeOp::Commit, "Tiếng Việt 👍"),
        ime(win, ImeOp::Compose, "việ"),
        ime(win, ImeOp::Compose, ""),
        ime(win, ImeOp::Finish, ""),
        ime(win, ImeOp::Commit, &full),
    ] {
        send(&mut client, &frame).await;
        assert_eq!(helper.next().await, frame, "ime is forwarded unchanged");
    }
    daemon.shutdown().await;
}

#[tokio::test]
async fn ipc_rejects_oversize_ime() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (mut good, good_win) = open_window(&daemon.sock, &mut helper, "com.good", 0).await;
    let over = "x".repeat(MAX_IME_TEXT + 1);
    // Multi-byte: fewer than MAX chars but more than MAX bytes.
    let over_utf8 = "ế".repeat(MAX_IME_TEXT / 3 + 1);
    for (i, (op, text)) in [
        (ImeOp::Commit, &over),
        (ImeOp::Compose, &over),
        (ImeOp::Commit, &over_utf8),
    ]
    .into_iter()
    .enumerate()
    {
        let (mut client, win) =
            open_window(&daemon.sock, &mut helper, &format!("com.bad{i}"), 0).await;
        send(&mut client, &ime(win, op, text)).await;
        assert_eof(&mut client).await;
        let close = helper.next().await;
        assert_eq!(
            (close.stream, decode(&close)),
            (win, Control::Close),
            "oversize ime must close only its window, never reach the helper"
        );
    }
    // An unknown op never decodes, so it is a protocol error too.
    let (mut client, win) = open_window(&daemon.sock, &mut helper, "com.badop", 0).await;
    let bogus = Frame {
        kind: Kind::Control,
        stream: win,
        payload: br#"{"t":"ime","op":"paste","text":"x"}"#.to_vec(),
    };
    send(&mut client, &bogus).await;
    assert_eof(&mut client).await;
    assert_eq!(decode(&helper.next().await), Control::Close);
    helper.assert_quiet().await;

    let ok = ime(good_win, ImeOp::Commit, "ok");
    send(&mut good, &ok).await;
    assert_eq!(helper.next().await, ok, "the good window is unaffected");
    daemon.shutdown().await;
}

// --- lantern status -------------------------------------------------------------------------

/// One `status` request: the daemon answers a `status_report` on stream 0 and closes.
async fn status(sock: &Path) -> StatusReport {
    let mut client = UnixStream::connect(sock).await.unwrap();
    send(&mut client, &control(0, Control::Status)).await;
    let reply = recv(&mut client).await.expect("a status report");
    assert_eq!(reply.stream, 0);
    let Control::StatusReport(report) = decode(&reply) else {
        panic!("expected status_report, got {:?}", decode(&reply));
    };
    assert_eof(&mut client).await;
    report
}

/// Polls `status` until `ok` holds (the session comes up asynchronously).
async fn status_until(sock: &Path, ok: impl Fn(&StatusReport) -> bool) -> StatusReport {
    let deadline = Instant::now() + STEP;
    loop {
        let report = status(sock).await;
        if ok(&report) {
            return report;
        }
        assert!(
            Instant::now() < deadline,
            "status never matched: {report:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn status_without_phone_is_idle() {
    let daemon = start_daemon(None);
    assert_eq!(
        status(&daemon.sock).await,
        StatusReport {
            state: SessionState::Idle,
            serial: None,
            helper: None,
            windows: vec![],
            companion: None,
            phones: vec![],
        }
    );
    daemon.shutdown().await;
}

#[tokio::test]
async fn status_reports_ready_session_and_open_windows() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let ready = status_until(&daemon.sock, |r| r.state == SessionState::Ready).await;
    assert_eq!(ready.serial.as_deref(), Some("dev"));
    assert_eq!(ready.helper.as_deref(), Some("sha"));
    assert!(ready.windows.is_empty());

    let (first, win1) = open_window(&daemon.sock, &mut helper, "com.one", 0).await;
    let (_second, win2) = open_window(&daemon.sock, &mut helper, "com.two", 10).await;
    let report = status(&daemon.sock).await;
    assert_eq!(
        report.windows,
        vec![
            WindowInfo {
                id: win1,
                pkg: "com.one".into(),
                user: 0,
                phone: Some("dev".into()),
            },
            WindowInfo {
                id: win2,
                pkg: "com.two".into(),
                user: 10,
                phone: Some("dev".into()),
            },
        ]
    );
    helper.assert_quiet().await; // status never reaches the phone

    drop(first);
    assert_eq!(decode(&helper.next().await), Control::Close);
    let report = status_until(&daemon.sock, |r| r.windows.len() == 1).await;
    assert_eq!(report.windows[0].id, win2);
    daemon.shutdown().await;
}

#[tokio::test]
async fn status_while_session_starts_is_starting() {
    let rt = runtime_dir();
    let listener = ipc::bind(&rt).unwrap();
    let sock = ipc::socket_path_in(&rt);
    let events = stream::iter([DeviceEvent::Online("dev".into())]).chain(stream::pending());
    // A start that never finishes (helper deploy stuck).
    let starter = |_serial: String| std::future::pending::<anyhow::Result<SessionConn>>();
    let (stop, stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(serve(
        listener,
        None,
        events,
        starter,
        async move {
            let _ = stop_rx.await;
            "test-stop"
        },
        fast_policy(),
    ));
    let report = status_until(&sock, |r| r.state == SessionState::Starting).await;
    assert_eq!(report.serial.as_deref(), Some("dev"));
    assert_eq!(report.helper, None);
    let _ = stop.send(());
    timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn status_after_open_is_a_protocol_error() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (mut client, win) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;
    send(&mut client, &control(win, Control::Status)).await;
    assert_eof(&mut client).await;
    assert_eq!(decode(&helper.next().await), Control::Close);
    daemon.shutdown().await;
}

#[tokio::test]
async fn single_instance_is_per_pkg_and_user() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let (mut owner0, win0) = open_window(&daemon.sock, &mut helper, "com.example", 0).await;
    let (mut owner10, win10) = open_window(&daemon.sock, &mut helper, "com.example", 10).await;
    assert_ne!(win0, win10, "user 10 gets its own window");

    // Opening user 10 again focuses user 10's window, never user 0's.
    let (_c, reply) = connect_open(&daemon.sock, open("com.example", 10, Some("tok-10"))).await;
    assert_eq!(reply.expect("a focus reply").stream, win10);
    let focus = recv(&mut owner10)
        .await
        .expect("user 10's window is focused");
    assert_eq!(focus.stream, win10);
    assert_eq!(
        decode(&focus),
        Control::Focus {
            activation_token: Some("tok-10".into())
        }
    );
    // And user 0 again focuses user 0's.
    let (_c, reply) = connect_open(&daemon.sock, open("com.example", 0, Some("tok-0"))).await;
    assert_eq!(reply.expect("a focus reply").stream, win0);
    let focus = recv(&mut owner0).await.expect("user 0's window is focused");
    assert_eq!(focus.stream, win0);
    helper.assert_quiet().await;

    // Closing user 10's window leaves user 0's registered; user 10 can then open anew.
    drop(owner10);
    assert_eq!(helper.next().await, control(win10, Control::Close));
    let report = status_until(&daemon.sock, |r| r.windows.len() == 1).await;
    assert_eq!((report.windows[0].id, report.windows[0].user), (win0, 0));
    let (_again, win10b) = open_window(&daemon.sock, &mut helper, "com.example", 10).await;
    assert!(win10b > win10);
    daemon.shutdown().await;
}

/// `companion` follows the live session's last `companion` message and is absent otherwise
/// (no message yet, and after the session ends).
#[tokio::test]
async fn status_reports_companion_state() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let daemon = start_daemon(Some(port));
    let ready = status_until(&daemon.sock, |r| r.state == SessionState::Ready).await;
    assert_eq!(ready.companion, None, "no companion message yet");

    let companion = |state, msg: Option<&str>| {
        control(
            0,
            Control::Companion {
                state,
                msg: msg.map(str::to_owned),
            },
        )
    };
    helper.send(companion(
        CompanionState::Unavailable,
        Some("not installed"),
    ));
    status_until(&daemon.sock, |r| {
        r.companion == Some(CompanionState::Unavailable)
    })
    .await;
    helper.send(companion(CompanionState::Ready, None));
    status_until(&daemon.sock, |r| r.companion == Some(CompanionState::Ready)).await;
    helper.assert_quiet().await; // never echoed to the phone

    // The session dies and a new one starts: the old state does not carry over.
    helper.die();
    assert_eq!(helper.next_session().await, 0);
    assert_eq!(helper.next_session().await, 1);
    let report = status_until(&daemon.sock, |r| r.state == SessionState::Ready).await;
    assert_eq!(report.companion, None, "{report:?}");
    daemon.shutdown().await;
}

fn pc_offer(text: &str, secret: bool) -> PcOffer {
    PcOffer {
        mimes: vec!["text/plain;charset=utf-8".into()],
        text: Some(text.into()),
        secret,
    }
}

/// The next text the daemon put on the (fake) PC clipboard.
async fn next_pc_set(rx: &std::sync::mpsc::Receiver<String>) -> String {
    let deadline = Instant::now() + STEP;
    loop {
        if let Ok(text) = rx.try_recv() {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "nothing was put on the PC clipboard"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// PC copies go to the phone as `clip` on stream 0, phone `clip`s land on the PC clipboard,
/// and neither side's text is echoed back; secrets stay on the PC.
#[tokio::test]
async fn clipboard_syncs_both_ways_without_echo() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let (handle, pc_sets) = ClipHandle::test_pair();
    let (offers, offers_rx) = mpsc::channel(8);
    let daemon = start_daemon_full(
        Some(port),
        None,
        Some(PcClipboard {
            handle,
            offers: offers_rx,
        }),
    );
    status_until(&daemon.sock, |r| r.state == SessionState::Ready).await;
    helper.assert_quiet().await; // nothing is pushed at session start

    offers.send(pc_offer("pc-1", false)).await.unwrap();
    let got = helper.next().await;
    assert_eq!(got.stream, 0);
    assert_eq!(
        decode(&got),
        Control::Clip {
            text: "pc-1".into()
        }
    );

    helper.send(control(
        0,
        Control::Clip {
            text: "phone-1".into(),
        },
    ));
    assert_eq!(next_pc_set(&pc_sets).await, "phone-1");
    // Our own selection comes back as a PC offer: not echoed to the phone.
    offers.send(pc_offer("phone-1", false)).await.unwrap();
    helper.assert_quiet().await;

    offers.send(pc_offer("hunter2", true)).await.unwrap();
    helper.assert_quiet().await;
    // After the unsyncable secret, the phone's text copied again on the PC is a real change.
    offers.send(pc_offer("phone-1", false)).await.unwrap();
    assert_eq!(
        decode(&helper.next().await),
        Control::Clip {
            text: "phone-1".into()
        }
    );

    offers.send(pc_offer("pc-2", false)).await.unwrap();
    assert_eq!(
        decode(&helper.next().await),
        Control::Clip {
            text: "pc-2".into()
        }
    );
    daemon.shutdown().await;
}

/// A PC copy while no phone session is live goes nowhere (and is not replayed later).
#[tokio::test]
async fn clipboard_offer_without_session_is_dropped() {
    let (handle, _pc_sets) = ClipHandle::test_pair();
    let (offers, offers_rx) = mpsc::channel(8);
    let daemon = start_daemon_full(
        None,
        None,
        Some(PcClipboard {
            handle,
            offers: offers_rx,
        }),
    );
    offers.send(pc_offer("nobody", false)).await.unwrap();
    let report = status(&daemon.sock).await;
    assert_eq!(report.state, SessionState::Idle);
    daemon.shutdown().await;
}

/// Nothing is put on the (fake) PC clipboard for a while.
async fn assert_no_pc_set(rx: &std::sync::mpsc::Receiver<String>) {
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rx.try_recv().is_err(), "the PC clipboard was set");
}

/// With `clipboard_sync = false` nothing crosses either way; turned back on, both directions
/// flow again (live, no restart).
#[tokio::test]
async fn clipboard_off_syncs_nothing_either_way() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let (handle, pc_sets) = ClipHandle::test_pair();
    let (offers, offers_rx) = mpsc::channel(8);
    let path = runtime_dir().join("config").join("settings.toml");
    let settings = SettingsHandle::new(Some(path));
    // A real settings file defaults to autoconnect off: switch it on so `dev` connects.
    settings.set(Key::Autoconnect, true).unwrap();
    let daemon = start_daemon_parts(
        Some(port),
        Parts {
            clipboard: Some(PcClipboard {
                handle,
                offers: offers_rx,
            }),
            settings: Some(settings.clone()),
            ..Default::default()
        },
    );
    status_until(&daemon.sock, |r| r.state == SessionState::Ready).await;
    helper.assert_quiet().await;

    settings.set(Key::ClipboardSync, false).unwrap();
    offers.send(pc_offer("pc-off", false)).await.unwrap();
    helper.assert_quiet().await;
    helper.send(control(
        0,
        Control::Clip {
            text: "phone-off".into(),
        },
    ));
    assert_no_pc_set(&pc_sets).await;

    settings.set(Key::ClipboardSync, true).unwrap();
    offers.send(pc_offer("pc-on", false)).await.unwrap();
    assert_eq!(
        decode(&helper.next().await),
        Control::Clip {
            text: "pc-on".into()
        }
    );
    helper.send(control(
        0,
        Control::Clip {
            text: "phone-on".into(),
        },
    ));
    assert_eq!(next_pc_set(&pc_sets).await, "phone-on");
    daemon.shutdown().await;
}

/// A catalogue in a private temp tree that never runs kbuildsycoca6.
fn temp_catalog() -> (PathBuf, Dirs, Catalog) {
    let root = runtime_dir();
    let dirs = Dirs {
        applications: root.join("applications"),
        icons: root.join("icons"),
        menus: root.join("menus"),
        directories: root.join("directories"),
        state: root.join("state"),
        exe: PathBuf::from("/usr/bin/lantern"),
    };
    let catalog = Catalog::with_rebuild(dirs.clone(), std::sync::Arc::new(|| {}));
    (root, dirs, catalog)
}

fn app(pkg: &str, user: u32, label: &str, icon: &str) -> Frame {
    control(
        0,
        Control::App {
            pkg: pkg.into(),
            user,
            label: label.into(),
            work: user != 0,
            icon: icon.into(),
        },
    )
}

/// Waits until `pred` holds for the file names in `dir`.
async fn wait_files(dir: &Path, what: &str, pred: impl Fn(&[String]) -> bool) {
    let deadline = Instant::now() + STEP;
    loop {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        if pred(&names) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: files are {names:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn catalogue_from_helper_becomes_menu_entries() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let (root, dirs, catalog) = temp_catalog();
    let daemon = start_daemon_with(Some(port), Some(catalog));
    helper.next_session().await;
    // "iVBORw0KGgo=" is the 8-byte PNG signature.
    helper.send(app("com.a", 0, "Alpha", "iVBORw0KGgo="));
    helper.send(app("com.b", 0, "Beta", "not base64!"));
    helper.send(app("com.w", 10, "Work", ""));
    helper.send(control(0, Control::AppsEnd { count: 3 }));
    let apps = dirs.applications.clone();
    wait_files(&apps, "first sync", |n| {
        n == [
            "lantern.com.a.desktop",
            "lantern.com.b.desktop",
            "lantern.com.w-u10.desktop",
        ]
    })
    .await;
    let a = std::fs::read_to_string(apps.join("lantern.com.a.desktop")).unwrap();
    assert!(
        a.contains("Name=Alpha\n") && a.contains("X-Lantern-Serial=dev\n"),
        "{a}"
    );
    assert!(dirs.icons.join("lantern.com.a.png").exists());
    assert!(!dirs.icons.join("lantern.com.b.png").exists());
    let w = std::fs::read_to_string(apps.join("lantern.com.w-u10.desktop")).unwrap();
    assert!(w.contains("Name=Work (work)\n"), "{w}");

    // A live removal outside a full sync is applied at once.
    helper.send(control(
        0,
        Control::AppRemoved {
            pkg: "com.b".into(),
            user: 0,
        },
    ));
    wait_files(&apps, "live removal", |n| {
        !n.iter().any(|f| f == "lantern.com.b.desktop")
    })
    .await;

    // The next full sync prunes what is gone (the removal above ended up in the window that
    // AppsEnd opened, so this sync is a mismatch...).
    helper.send(app("com.c", 0, "Gamma", ""));
    helper.send(control(0, Control::AppsEnd { count: 1 }));
    wait_files(&apps, "mismatched sync", |n| n.len() == 3).await;
    // ...and the one after it matches: com.a goes, the work profile (user 10) sent nothing and
    // keeps its entry.
    helper.send(app("com.c", 0, "Gamma", ""));
    helper.send(control(0, Control::AppsEnd { count: 1 }));
    wait_files(&apps, "full sync", |n| {
        n == ["lantern.com.c.desktop", "lantern.com.w-u10.desktop"]
    })
    .await;
    assert!(!dirs.icons.join("lantern.com.a.png").exists());

    // A partial sync prunes nothing.
    helper.send(app("com.d", 0, "Delta", ""));
    helper.send(control(0, Control::AppsEnd { count: 7 }));
    wait_files(&apps, "partial sync", |n| n.len() == 3).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    wait_files(&apps, "partial sync kept", |n| n.len() == 3).await;

    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// Plants the menu entry of `pkg` (user 0) as written by phone `serial`'s sync, under the
/// legacy name (as if `serial` were the primary phone).
fn plant_entry(dirs: &Dirs, pkg: &str, serial: &str) {
    plant_named_entry(dirs, pkg, serial, pkg);
}

/// [`plant_entry`] with the menu name `label`.
fn plant_named_entry(dirs: &Dirs, pkg: &str, serial: &str, label: &str) {
    use lantern::daemon::catalog::{desktop_file, AppEntry};
    use lantern::naming::Naming;
    let e = AppEntry {
        pkg: pkg.into(),
        user: 0,
        label: label.into(),
        work: false,
        icon_png: Vec::new(),
    };
    std::fs::create_dir_all(&dirs.applications).unwrap();
    let naming = Naming {
        primary: Some(serial.to_owned()),
    };
    let text = desktop_file(&e, serial, &naming, &dirs.exe, None);
    std::fs::write(
        dirs.applications.join(format!("lantern.{pkg}.desktop")),
        text,
    )
    .unwrap();
}

#[tokio::test]
async fn open_before_first_sync_is_forwarded() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let (root, dirs, catalog) = temp_catalog();
    plant_entry(&dirs, "com.x", "other");
    let daemon = start_daemon_with(Some(port), Some(catalog));
    helper.next_session().await;
    // Session `dev` is up but has sent no apps_end: its catalogue is unknown, so the open goes
    // to the phone.
    let (_client, _win) = open_window(&daemon.sock, &mut helper, "com.x", 0).await;
    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn open_of_other_phones_app_is_refused_after_sync() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let (root, dirs, catalog) = temp_catalog();
    plant_entry(&dirs, "com.x", "other");
    let daemon = start_daemon_with(Some(port), Some(catalog));
    helper.next_session().await;
    helper.send(app("com.y", 0, "Yankee", ""));
    helper.send(control(0, Control::AppsEnd { count: 1 }));
    // Helper frames are handled in order: once com.z's entry exists, apps_end has been too.
    helper.send(app("com.z", 0, "Zulu", ""));
    wait_files(&dirs.applications, "synced", |n| {
        n.iter().any(|f| f == "lantern.com.z.desktop")
    })
    .await;
    let x = std::fs::read_to_string(dirs.applications.join("lantern.com.x.desktop")).unwrap();
    assert!(x.contains("X-Lantern-Serial=other\n"), "{x}");

    let (mut client, reply) = connect_open(&daemon.sock, open("com.x", 0, None)).await;
    let reply = reply.expect("a reply to the open");
    assert_eq!(reply.stream, 0);
    assert_eq!(decode(&reply), Control::error("app not on this phone"));
    assert_eq!(ipc::NOT_ON_THIS_PHONE, "app not on this phone");
    assert_eq!(
        recv(&mut client).await,
        None,
        "the daemon closes the client"
    );
    helper.assert_quiet().await;

    // An app that lists this phone still opens.
    let (_client, _win) = open_window(&daemon.sock, &mut helper, "com.y", 0).await;
    daemon.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

// --- daemon control: Connect, Disconnect, Undo, Quit -----------------------------------------

use lantern::daemon::companion;
use lantern::daemon::control::{Command, ConnectEvent, Opener, PhoneView, UndoRunner, View};
use lantern::daemon::phones::Timing;
use lantern::settings::Settings;
use lantern_proto::{DaemonCmd, PhoneState};
use tokio::sync::watch;

/// An in-process daemon whose device events, commands and view the test drives and reads.
struct Ctl {
    sock: PathBuf,
    events: mpsc::UnboundedSender<DeviceEvent>,
    cmds: mpsc::Sender<Command>,
    view: watch::Receiver<View>,
    /// How often `names` was asked.
    lookups: Arc<AtomicUsize>,
    /// The (summary, body) of every notification.
    notes: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    /// What the connect prompt got.
    connect: mpsc::Receiver<ConnectEvent>,
    /// Every `(serial, pkg, user)` the opener was asked to open.
    opened: Arc<std::sync::Mutex<Vec<(String, String, u32)>>>,
    /// The daemon's settings (`autoconnect`), if it has any.
    settings: Option<SettingsHandle>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}

/// How a [`Ctl`] daemon is set up beyond its online phones.
#[derive(Default)]
struct Opts {
    undo: Option<UndoRunner>,
    /// The device records; `None`: flags in memory only.
    state_dir: Option<PathBuf>,
    /// `Some`: a settings part with this `autoconnect`; `None`: no settings (autoconnect on).
    autoconnect: Option<bool>,
    /// `None`: the default timing (10 s grace).
    timing: Option<Timing>,
    /// `$LANTERN_SERIAL`.
    configured: Option<String>,
    /// The test reports adb's first device list itself ([`Ctl::listed`]); otherwise the online
    /// phones are that list.
    unlisted: bool,
    /// R8: this serial's sessions own an adb forward that `close` removes through this fake adb,
    /// and each of its starts appends `start` to the fake's `order` log.
    slow_close: Option<(String, SlowAdb)>,
}

/// A 300 ms unplug grace and a 2 s connect-and-open window.
fn fast_timing() -> Timing {
    Timing {
        unplug_grace: Duration::from_millis(300),
        pending_for: Duration::from_secs(2),
        list_wait: Duration::from_secs(30),
    }
}

/// Autoconnect off, flags in memory: plugged-in phones wait until a Connect.
fn manual() -> Opts {
    Opts {
        autoconnect: Some(false),
        ..Default::default()
    }
}

/// Opts for the connect-rule tests: records in `state`, `autoconnect`, fast timing.
fn rules(state: &Path, autoconnect: bool) -> Opts {
    Opts {
        state_dir: Some(state.to_owned()),
        autoconnect: Some(autoconnect),
        timing: Some(fast_timing()),
        ..Default::default()
    }
}

/// A fresh scratch state dir (never the real `~/.local/state/lantern`).
fn scratch_state() -> PathBuf {
    let n = DIRS.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("lantern-ipc-state-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Every session goes to the fake helper on `port`, whichever serial it is for. The phone
/// named "Pixel" is `dev`; other serials have no name.
fn start_ctl(port: u16, online: &[&str], undo: Option<UndoRunner>) -> Ctl {
    start_ctl_with(port, online, undo, |_| {})
}

/// [`start_ctl`], with `more` filling in further parts.
fn start_ctl_with(
    port: u16,
    online: &[&str],
    undo: Option<UndoRunner>,
    more: impl FnOnce(&mut Parts),
) -> Ctl {
    start_ctl_opts(
        port,
        online,
        Opts {
            undo,
            ..Default::default()
        },
        more,
    )
}

fn start_ctl_opts(port: u16, online: &[&str], opts: Opts, more: impl FnOnce(&mut Parts)) -> Ctl {
    start_ctl_ports(Arc::new(move |_: &str| Some(port)), online, opts, more)
}

/// Each serial's sessions go to its own fake helper: `(serial, port)`. Every serial in `ports`
/// is online from the start. `dev` is "Pixel"; a serial not in `ports` never starts.
fn start_pair(ports: &[(&str, u16)], opts: Opts) -> Ctl {
    let online: Vec<&str> = ports.iter().map(|(serial, _)| *serial).collect();
    start_pair_online(ports, &online, opts)
}

/// [`start_pair`] with only `online` online from the start.
fn start_pair_online(ports: &[(&str, u16)], online: &[&str], opts: Opts) -> Ctl {
    let ports: std::collections::HashMap<String, u16> = ports
        .iter()
        .map(|(serial, port)| ((*serial).to_owned(), *port))
        .collect();
    start_ctl_ports(
        Arc::new(move |serial: &str| ports.get(serial).copied()),
        online,
        opts,
        |_| {},
    )
}

type PortOf = Arc<dyn Fn(&str) -> Option<u16> + Send + Sync>;

fn start_ctl_ports(
    port_of: PortOf,
    online: &[&str],
    opts: Opts,
    more: impl FnOnce(&mut Parts),
) -> Ctl {
    let rt = runtime_dir();
    let listener = ipc::bind(&rt).unwrap();
    let sock = ipc::socket_path_in(&rt);
    let (events, events_rx) = mpsc::unbounded_channel();
    for serial in online {
        events.send(DeviceEvent::Online((*serial).into())).unwrap();
    }
    if !opts.unlisted {
        events.send(DeviceEvent::Listed).unwrap();
    }
    let events_rx = futures_util::stream::unfold(events_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    });
    let (cmds, cmds_rx) = mpsc::channel(8);
    let (view_tx, view) = watch::channel(View::default());
    let lookups = Arc::new(AtomicUsize::new(0));
    let counted = lookups.clone();
    let notes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let noted = notes.clone();
    let (connect_tx, connect) = mpsc::channel(32);
    let opened = Arc::new(std::sync::Mutex::new(Vec::new()));
    let opens = opened.clone();
    let opener: Opener = Arc::new(move |serial: &str, pkg: &str, user: u32| {
        opens
            .lock()
            .unwrap()
            .push((serial.to_owned(), pkg.to_owned(), user));
    });
    let settings = opts.autoconnect.map(|autoconnect| {
        SettingsHandle::fixed(Settings {
            autoconnect,
            ..Settings::default()
        })
    });
    let mut parts = Parts {
        commands: Some(cmds_rx),
        view: Some(view_tx),
        names: Some(Arc::new(move |serial: &str| {
            counted.fetch_add(1, Ordering::SeqCst);
            (serial == "dev").then(|| "Pixel".to_owned())
        })),
        undo: opts.undo,
        notify: Some(Arc::new(move |summary: &str, body: &str| {
            noted
                .lock()
                .unwrap()
                .push((summary.to_owned(), body.to_owned()));
        })),
        settings: settings.clone(),
        state_dir: opts.state_dir,
        timing: opts.timing.unwrap_or_default(),
        connect_events: Some(connect_tx),
        opener: Some(opener),
        ..Default::default()
    };
    more(&mut parts);
    let slow_close = opts.slow_close;
    let starter = move |serial: String| {
        let port = port_of(&serial);
        let slow = slow_close
            .as_ref()
            .filter(|(slow, _)| *slow == serial)
            .map(|(_, adb)| {
                adb.note("start");
                adb.program.clone()
            });
        async move {
            let port = port.ok_or_else(|| anyhow::anyhow!("no fake helper for {serial}"))?;
            let stream = TcpStream::connect(("127.0.0.1", port)).await?;
            let stream = handshake(stream, TOKEN, true, true).await?;
            Ok(match slow {
                Some(program) => {
                    SessionConn::attach(stream, Adb::with_program(program, serial), "sha")
                        .with_forward(45999)
                }
                None => SessionConn::attach(stream, Adb::new(serial), "sha"),
            })
        }
    };
    let (stop, stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(serve_with(
        listener,
        parts,
        opts.configured,
        events_rx,
        starter,
        async move {
            let _ = stop_rx.await;
            "test-stop"
        },
        fast_policy(),
    ));
    Ctl {
        sock,
        events,
        cmds,
        view,
        lookups,
        notes,
        connect,
        opened,
        settings,
        stop: Some(stop),
        task,
    }
}

fn connect(serial: &str) -> Command {
    Command::Connect {
        serial: serial.into(),
        open: None,
    }
}

fn connect_and_open(serial: &str, pkg: &str, user: u32) -> Command {
    Command::Connect {
        serial: serial.into(),
        open: Some((pkg.into(), user)),
    }
}

fn disconnect(serial: &str) -> Command {
    Command::Disconnect {
        serial: serial.into(),
    }
}

fn undo_setup(serial: &str) -> Command {
    Command::UndoSetup {
        serial: serial.into(),
    }
}

fn plugged_in(serial: &str, name: &str) -> ConnectEvent {
    ConnectEvent::PluggedIn {
        serial: serial.into(),
        name: name.into(),
    }
}

impl Ctl {
    async fn command(&self, cmd: Command) {
        self.cmds.send(cmd).await.unwrap();
    }

    fn online(&self, serial: &str) {
        self.events
            .send(DeviceEvent::Online(serial.into()))
            .unwrap();
    }

    fn offline(&self, serial: &str) {
        self.events
            .send(DeviceEvent::Offline(serial.into()))
            .unwrap();
    }

    /// adb's device list is complete (its phones were reported with [`Ctl::online`]).
    fn listed(&self) {
        self.events.send(DeviceEvent::Listed).unwrap();
    }

    /// The view's entry for `serial` now.
    fn phone(&self, serial: &str) -> Option<PhoneView> {
        self.view.borrow().phone(serial).cloned()
    }

    /// Waits until the view matches.
    async fn view_until(&mut self, ok: impl Fn(&View) -> bool) -> View {
        let matched = timeout(STEP, self.view.wait_for(|v| ok(v)))
            .await
            .map(|view| view.unwrap().clone());
        matched.unwrap_or_else(|_| panic!("view never matched: {:?}", *self.view.borrow()))
    }

    /// Waits until `serial` is in `state`; returns its entry.
    async fn state_until(&mut self, serial: &str, state: PhoneState) -> PhoneView {
        let view = self
            .view_until(|v| v.phone(serial).is_some_and(|p| p.state == state))
            .await;
        view.phone(serial).unwrap().clone()
    }

    async fn ready(&mut self, serial: &str) -> PhoneView {
        self.state_until(serial, PhoneState::Ready).await
    }

    /// The next connect event.
    async fn event(&mut self) -> ConnectEvent {
        timeout(STEP, self.connect.recv())
            .await
            .expect("no connect event")
            .unwrap()
    }

    /// The next connect event that isn't a `Gone`.
    async fn prompt(&mut self) -> ConnectEvent {
        loop {
            match self.event().await {
                ConnectEvent::Gone { .. } => continue,
                event => return event,
            }
        }
    }

    /// Every connect event within 300 ms.
    async fn events_for_a_while(&mut self) -> Vec<ConnectEvent> {
        let mut got = Vec::new();
        while let Ok(Some(event)) = timeout(Duration::from_millis(300), self.connect.recv()).await {
            got.push(event);
        }
        got
    }

    /// No prompt (`PluggedIn`/`PlugIn`) arrives for a while; `Gone`s are fine.
    async fn assert_no_prompt(&mut self) {
        let events = self.events_for_a_while().await;
        assert!(
            events
                .iter()
                .all(|e| matches!(e, ConnectEvent::Gone { .. })),
            "unexpected prompt: {events:?}"
        );
    }

    fn opened(&self) -> Vec<(String, String, u32)> {
        self.opened.lock().unwrap().clone()
    }

    async fn shutdown(mut self) {
        let _ = self.stop.take().unwrap().send(());
        timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

/// `serial`'s record flags `(off, live)` (absent record: both false).
fn flags(state: &Path, serial: &str) -> (bool, bool) {
    companion::read_record(state, serial).map_or((false, false), |r| (r.off, r.live))
}

/// Polls the record until `ok` holds.
async fn record_until(state: &Path, serial: &str, ok: impl Fn((bool, bool)) -> bool) {
    let deadline = Instant::now() + STEP;
    while !ok(flags(state, serial)) {
        assert!(
            Instant::now() < deadline,
            "record never matched: {:?}",
            companion::read_record(state, serial)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Asserts no helper session starts for 3 × the fast policy's longest retry delay.
async fn assert_no_session(helper: &mut FakeHelper) {
    let quiet = 3 * fast_policy().max;
    if let Ok(Some(n)) = timeout(quiet, helper.sessions.recv()).await {
        panic!("a session ({n}) started while it must not");
    }
}

/// The daemon closed the session: the helper got `bye`. The fake helper is then told to drop
/// the connection so it can take the next one.
async fn expect_bye(helper: &mut FakeHelper) {
    assert_eq!(decode(&helper.next().await), Control::Bye);
    helper.die();
}

/// A Disconnect switches the phone off; it stays off after a replug (autoconnect on).
#[tokio::test]
async fn disconnect_pauses_until_replug() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut ctl = start_ctl(port, &["dev"], None);
    assert_eq!(helper.next_session().await, 0);
    let ready = ctl.ready("dev").await;
    assert_eq!(ready.name.as_deref(), Some("Pixel"));

    ctl.command(disconnect("dev")).await;
    expect_bye(&mut helper).await;
    let off = ctl.state_until("dev", PhoneState::Off).await;
    assert_eq!(off.name.as_deref(), Some("Pixel"));
    assert!(off.off);
    let report = status(&ctl.sock).await;
    assert_eq!(
        (report.state, report.serial.as_deref()),
        (SessionState::Paused, Some("dev"))
    );
    assert_no_session(&mut helper).await;

    // Unplug and replug: still off.
    ctl.offline("dev");
    ctl.state_until("dev", PhoneState::Unplugged).await;
    ctl.online("dev");
    ctl.state_until("dev", PhoneState::Off).await;
    assert_no_session(&mut helper).await;
    // Connect serves it again.
    ctl.command(connect("dev")).await;
    assert_eq!(helper.next_session().await, 1);
    ctl.ready("dev").await;
    // The name is cached, not read from the device record on every loop turn.
    for _ in 0..20 {
        status(&ctl.sock).await;
    }
    let lookups = ctl.lookups.load(Ordering::SeqCst);
    assert!(lookups <= 8, "names looked up {lookups} times");
    ctl.shutdown().await;
}

#[tokio::test]
async fn online_while_paused_starts_nothing() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut ctl = start_ctl(port, &["dev"], None);
    helper.next_session().await;
    ctl.ready("dev").await;
    ctl.command(disconnect("dev")).await;
    expect_bye(&mut helper).await;
    ctl.state_until("dev", PhoneState::Off).await;
    // adb reports the phone again (e.g. track-devices reconnected) without an Offline.
    ctl.online("dev");
    assert_no_session(&mut helper).await;
    assert_eq!(
        ctl.phone("dev"),
        Some(PhoneView {
            serial: "dev".into(),
            name: Some("Pixel".into()),
            state: PhoneState::Off,
            plugged: true,
            off: true,
            locked: false,
            companion: None,
        })
    );
    assert_eq!(status(&ctl.sock).await.state, SessionState::Paused);
    ctl.shutdown().await;
}

#[tokio::test]
async fn connect_resumes_after_disconnect() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut ctl = start_ctl(port, &["dev"], None);
    helper.next_session().await;
    ctl.ready("dev").await;
    ctl.command(disconnect("dev")).await;
    expect_bye(&mut helper).await;
    ctl.state_until("dev", PhoneState::Off).await;
    ctl.command(connect("dev")).await;
    assert_eq!(helper.next_session().await, 1);
    let ready = ctl.ready("dev").await;
    assert!(!ready.off);
    status_until(&ctl.sock, |r| r.state == SessionState::Ready).await;
    ctl.shutdown().await;
}

#[tokio::test]
async fn quit_command_ends_daemon_ok() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut ctl = start_ctl(port, &["dev"], None);
    helper.next_session().await;
    ctl.ready("dev").await;
    let sock = ctl.sock.clone();
    ctl.command(Command::Quit).await;
    timeout(Duration::from_secs(5), &mut ctl.task)
        .await
        .expect("the daemon ends on Quit")
        .unwrap()
        .expect("Quit is a clean exit");
    assert_eq!(decode(&helper.next().await), Control::Bye);
    assert!(!sock.exists(), "socket file is removed on Quit");
}

#[tokio::test]
async fn undo_runs_without_a_session() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let (calls_tx, mut calls) = mpsc::unbounded_channel::<(String, bool)>();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let eofs = helper.eofs.clone();
    let gate = release.clone();
    let undo: UndoRunner = Arc::new(move |serial: String| {
        let calls_tx = calls_tx.clone();
        let eofs = eofs.clone();
        let gate = gate.clone();
        Box::pin(async move {
            // The helper's connection is closed before the undo runs.
            let closed = timeout(Duration::from_secs(1), async {
                while eofs.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .is_ok();
            let _ = calls_tx.send((serial, closed));
            gate.acquire().await.unwrap().forget();
            (
                true,
                "helper --forget: forgot 1\ncompanion: uninstalled".to_owned(),
            )
        })
    });
    let mut ctl = start_ctl(port, &["dev"], Some(undo));
    helper.next_session().await;
    ctl.ready("dev").await;

    ctl.command(undo_setup("dev")).await;
    assert_eq!(decode(&helper.next().await), Control::Bye);
    ctl.state_until("dev", PhoneState::Undoing).await;
    let (serial, closed) = timeout(STEP, calls.recv()).await.unwrap().unwrap();
    assert_eq!(serial, "dev");
    assert!(closed, "undo ran while the helper connection was open");
    let report = status(&ctl.sock).await;
    assert_eq!(report.state, SessionState::Paused, "{report:?}");
    helper.die();

    // While undoing: a Connect is refused with a notice (decision 6); Disconnect and a second
    // Undo of the same phone change nothing.
    ctl.command(connect("dev")).await;
    ctl.command(disconnect("dev")).await;
    ctl.command(undo_setup("dev")).await;
    assert_no_session(&mut helper).await;
    assert_eq!(ctl.phone("dev").unwrap().state, PhoneState::Undoing);

    release.add_permits(1);
    ctl.state_until("dev", PhoneState::Off).await;
    assert_no_session(&mut helper).await;
    assert!(calls.try_recv().is_err(), "undo ran once");
    assert_eq!(
        *ctl.notes.lock().unwrap(),
        [
            (
                "Lantern is still being removed from Pixel".to_owned(),
                "Connect it again when that's done.".to_owned()
            ),
            (
                "Lantern removed itself from Pixel".to_owned(),
                "helper --forget: forgot 1\ncompanion: uninstalled".to_owned()
            )
        ]
    );
    // Connect afterwards sets the phone up again.
    ctl.command(connect("dev")).await;
    assert_eq!(helper.next_session().await, 1);
    ctl.ready("dev").await;
    ctl.shutdown().await;
}

#[tokio::test]
async fn quit_waits_for_a_running_undo() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let undo: UndoRunner = Arc::new(move |_serial: String| {
        let gate = gate.clone();
        Box::pin(async move {
            gate.acquire().await.unwrap().forget();
            (false, "helper --forget: FAILED: boom".to_owned())
        })
    });
    let mut ctl = start_ctl(port, &["dev"], Some(undo));
    helper.next_session().await;
    ctl.ready("dev").await;
    ctl.command(undo_setup("dev")).await;
    ctl.state_until("dev", PhoneState::Undoing).await;
    ctl.command(Command::Quit).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!ctl.task.is_finished(), "Quit must wait for the undo");
    release.add_permits(1);
    timeout(Duration::from_secs(5), &mut ctl.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        *ctl.notes.lock().unwrap(),
        [(
            "Undo phone setup failed on Pixel".to_owned(),
            "helper --forget: FAILED: boom".to_owned()
        )]
    );
}

#[tokio::test]
async fn open_while_paused_gets_paused_error() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut ctl = start_ctl(port, &["dev"], None);
    helper.next_session().await;
    ctl.ready("dev").await;
    ctl.command(disconnect("dev")).await;
    expect_bye(&mut helper).await;
    ctl.state_until("dev", PhoneState::Off).await;
    let mut client = UnixStream::connect(&ctl.sock).await.unwrap();
    send(&mut client, &open("com.x", 0, None)).await;
    let reply = recv(&mut client).await.expect("an error reply");
    assert_eq!(decode(&reply), Control::error(ipc::PAUSED));
    assert_eq!(ipc::PAUSED, "phone disconnected in Lantern");
    assert_eof(&mut client).await;
    ctl.shutdown().await;
}

/// `lantern connect|disconnect [<serial>]` over the socket: the daemon's reply.
async fn daemon_cmd_reply(sock: &Path, cmd: DaemonCmd, serial: Option<&str>) -> Control {
    let mut client = UnixStream::connect(sock).await.unwrap();
    send(
        &mut client,
        &control(
            0,
            Control::Daemon {
                cmd,
                serial: serial.map(str::to_owned),
            },
        ),
    )
    .await;
    let reply = recv(&mut client).await.expect("a reply");
    assert_eof(&mut client).await;
    decode(&reply)
}

/// `lantern connect|disconnect` for the only plugged-in phone.
async fn daemon_cmd(sock: &Path, cmd: DaemonCmd) -> StatusReport {
    match daemon_cmd_reply(sock, cmd, None).await {
        Control::StatusReport(report) => report,
        other => panic!("expected status_report, got {other:?}"),
    }
}

#[tokio::test]
async fn daemon_cmd_over_socket() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut ctl = start_ctl(port, &["dev"], None);
    helper.next_session().await;
    status_until(&ctl.sock, |r| r.state == SessionState::Ready).await;
    let report = daemon_cmd(&ctl.sock, DaemonCmd::Disconnect).await;
    assert_eq!(report.serial.as_deref(), Some("dev"));
    expect_bye(&mut helper).await;
    ctl.state_until("dev", PhoneState::Off).await;
    let report = status_until(&ctl.sock, |r| r.state == SessionState::Paused).await;
    assert_eq!(report.serial.as_deref(), Some("dev"));
    // The blocking client `lantern connect` uses.
    let sock = ctl.sock.clone();
    let reply =
        tokio::task::spawn_blocking(move || ipc::send_daemon_cmd(&sock, DaemonCmd::Connect, None))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        reply.state,
        SessionState::Paused,
        "the reply is the state before the command"
    );
    assert_eq!(helper.next_session().await, 1);
    status_until(&ctl.sock, |r| r.state == SessionState::Ready).await;
    // Only as the first frame: after an open it is a protocol error that drops the client.
    let (mut client, win) = open_window(&ctl.sock, &mut helper, "com.example", 0).await;
    send(
        &mut client,
        &control(
            win,
            Control::Daemon {
                cmd: DaemonCmd::Disconnect,
                serial: None,
            },
        ),
    )
    .await;
    assert_eof(&mut client).await;
    assert_eq!(decode(&helper.next().await), Control::Close);
    assert_eq!(status(&ctl.sock).await.state, SessionState::Ready);
    ctl.shutdown().await;
}

/// Review Focus 2: with autoconnect off a plugged-in phone waits and Lantern asks once.
#[tokio::test]
async fn autoconnect_off_waits_and_prompts() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &["dev"], rules(&state, false), |_| {});
    ctl.state_until("dev", PhoneState::Waiting).await;
    assert_eq!(ctl.event().await, plugged_in("dev", "Pixel"));
    assert_no_session(&mut helper).await;
    assert_eq!(ctl.events_for_a_while().await, [], "one prompt per plug-in");
    assert_eq!(status(&ctl.sock).await.state, SessionState::Idle);
    ctl.command(connect("dev")).await;
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("dev").await;
    assert_eq!(
        ctl.event().await,
        ConnectEvent::Gone {
            serial: "dev".into()
        }
    );
    assert_eq!(flags(&state, "dev"), (false, true));
    ctl.shutdown().await;
}

#[tokio::test]
async fn autoconnect_on_connects_unless_off() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &["dev"], rules(&state, true), |_| {});
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("dev").await;
    ctl.command(disconnect("dev")).await;
    expect_bye(&mut helper).await;
    ctl.state_until("dev", PhoneState::Off).await;
    assert_eq!(flags(&state, "dev"), (true, false));
    // Unplugged longer than the grace, then plugged in: nothing, not even a prompt.
    ctl.offline("dev");
    tokio::time::sleep(2 * fast_timing().unplug_grace).await;
    ctl.online("dev");
    ctl.state_until("dev", PhoneState::Off).await;
    assert_no_session(&mut helper).await;
    ctl.assert_no_prompt().await;
    ctl.shutdown().await;
    // A new daemon on the same records: still off.
    let mut ctl = start_ctl_opts(port, &["dev"], rules(&state, true), |_| {});
    ctl.state_until("dev", PhoneState::Off).await;
    assert_no_session(&mut helper).await;
    ctl.assert_no_prompt().await;
    ctl.command(connect("dev")).await;
    assert_eq!(helper.next_session().await, 1);
    ctl.ready("dev").await;
    assert_eq!(flags(&state, "dev"), (false, true));
    ctl.shutdown().await;
}

/// A restart (stop signal) keeps `live`: the phone reconnects without asking.
#[tokio::test]
async fn live_flag_reconnects_after_restart_without_prompt() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &["dev"], rules(&state, false), |_| {});
    assert_eq!(ctl.event().await, plugged_in("dev", "Pixel"));
    ctl.command(connect("dev")).await;
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("dev").await;
    ctl.shutdown().await;
    expect_bye(&mut helper).await;
    assert_eq!(flags(&state, "dev"), (false, true), "a stop keeps live");
    let mut ctl = start_ctl_opts(port, &["dev"], rules(&state, false), |_| {});
    assert_eq!(helper.next_session().await, 1);
    ctl.ready("dev").await;
    ctl.assert_no_prompt().await;
    ctl.shutdown().await;
}

#[tokio::test]
async fn quit_clears_live() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &["dev"], rules(&state, true), |_| {});
    helper.next_session().await;
    ctl.ready("dev").await;
    assert_eq!(flags(&state, "dev"), (false, true));
    ctl.command(Command::Quit).await;
    timeout(Duration::from_secs(5), &mut ctl.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(flags(&state, "dev"), (false, false));
}

/// Autoconnect is read at plug-in: switching it on connects nothing until the next one.
#[tokio::test]
async fn autoconnect_switched_on_connects_nothing_now() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &["dev"], rules(&state, false), |_| {});
    ctl.state_until("dev", PhoneState::Waiting).await;
    ctl.settings
        .as_ref()
        .unwrap()
        .set(lantern::settings::Key::Autoconnect, true)
        .unwrap();
    assert_no_session(&mut helper).await;
    assert_eq!(ctl.phone("dev").unwrap().state, PhoneState::Waiting);
    // Unplugged past the grace (a blip within it isn't a plug-in, R7).
    ctl.offline("dev");
    assert_eq!(ctl.prompt().await, plugged_in("dev", "Pixel"));
    assert_eq!(ctl.event().await, gone("dev"));
    ctl.online("dev");
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("dev").await;
    ctl.shutdown().await;
}

/// Review Focus 4: offline and back within the grace reconnects without asking.
#[tokio::test]
async fn blip_never_prompts() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &["dev"], rules(&state, false), |_| {});
    assert_eq!(ctl.event().await, plugged_in("dev", "Pixel"));
    ctl.command(connect("dev")).await;
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("dev").await;
    for round in 1..=3 {
        ctl.offline("dev");
        expect_bye(&mut helper).await;
        ctl.state_until("dev", PhoneState::Reconnecting).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        ctl.online("dev");
        assert_eq!(helper.next_session().await, round);
        ctl.ready("dev").await;
    }
    ctl.assert_no_prompt().await;
    assert_eq!(flags(&state, "dev"), (false, true));
    ctl.shutdown().await;
}

#[tokio::test]
async fn unplug_beyond_grace_clears_live() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &["dev"], rules(&state, false), |_| {});
    assert_eq!(ctl.event().await, plugged_in("dev", "Pixel"));
    ctl.command(connect("dev")).await;
    helper.next_session().await;
    ctl.ready("dev").await;
    ctl.offline("dev");
    expect_bye(&mut helper).await;
    ctl.state_until("dev", PhoneState::Reconnecting).await;
    ctl.state_until("dev", PhoneState::Unplugged).await;
    record_until(&state, "dev", |(_, live)| !live).await;
    while let Ok(event) = ctl.connect.try_recv() {
        assert!(matches!(event, ConnectEvent::Gone { .. }), "{event:?}");
    }
    ctl.online("dev");
    assert_eq!(ctl.event().await, plugged_in("dev", "Pixel"));
    assert_no_session(&mut helper).await;
    ctl.shutdown().await;
}

/// R6: a wanted phone that never got ready (its starts keep failing) and is unplugged past the
/// grace gets its notice closed: a `Gone` when the grace ends.
#[tokio::test]
async fn wanted_phone_gone_past_grace_closes_its_notice() {
    // Nothing listens on this port: every session start fails.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &["dev"], rules(&state, true), |_| {});
    ctl.state_until("dev", PhoneState::Retrying).await;
    ctl.offline("dev");
    ctl.state_until("dev", PhoneState::Reconnecting).await;
    ctl.state_until("dev", PhoneState::Unplugged).await;
    let events = ctl.events_for_a_while().await;
    assert_eq!(
        events,
        [ConnectEvent::Gone {
            serial: "dev".into()
        }]
    );
    ctl.shutdown().await;
}

/// R6: a loose cable on a phone that is only waiting doesn't re-pop "is plugged in": back
/// within the grace, its notice stays as it is; away past the grace, it is closed, and the
/// next plug-in asks again.
#[tokio::test]
async fn waiting_phone_blip_keeps_its_notice() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &["dev"], rules(&state, false), |_| {});
    assert_eq!(ctl.event().await, plugged_in("dev", "Pixel"));
    for _ in 0..3 {
        ctl.offline("dev");
        ctl.state_until("dev", PhoneState::Unplugged).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        ctl.online("dev");
        ctl.state_until("dev", PhoneState::Waiting).await;
    }
    assert_eq!(
        ctl.events_for_a_while().await,
        [],
        "no Gone, no second prompt"
    );
    ctl.offline("dev");
    assert_eq!(
        ctl.event().await,
        ConnectEvent::Gone {
            serial: "dev".into()
        }
    );
    ctl.online("dev");
    assert_eq!(ctl.event().await, plugged_in("dev", "Pixel"));
    // Connect while it is away within the grace: its "Plug in" notice is not closed when the
    // grace ends.
    ctl.offline("dev");
    ctl.state_until("dev", PhoneState::Unplugged).await;
    ctl.command(connect("dev")).await;
    assert!(matches!(ctl.event().await, ConnectEvent::PlugIn { .. }));
    tokio::time::sleep(2 * fast_timing().unplug_grace).await;
    assert_eq!(ctl.events_for_a_while().await, []);
    assert_no_session(&mut helper).await;
    ctl.shutdown().await;
}

/// A port nothing listens on: every session start fails.
fn dead_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn gone(serial: &str) -> ConnectEvent {
    ConnectEvent::Gone {
        serial: serial.into(),
    }
}

/// R7: phone a is wanted and offline in its grace (its starts keep failing) when its `wanted`
/// is released; the grace carries over, so a's notice still closes when it ends.
async fn wanted_in_grace(ctl: &mut Ctl) {
    ctl.command(connect("a")).await;
    ctl.state_until("a", PhoneState::Retrying).await;
    ctl.offline("a");
    ctl.state_until("a", PhoneState::Reconnecting).await;
}

/// R7 (a): b's pending connect-and-open connects b while a is in its grace; a's grace runs on
/// and its end closes a's notice.
#[tokio::test]
async fn released_in_grace_by_switch_in_closes_its_notice() {
    let state = scratch_state();
    let mut ctl = start_ctl_opts(dead_port(), &["a"], rules(&state, false), |_| {});
    assert_eq!(ctl.prompt().await, plugged_in("a", "a"));
    wanted_in_grace(&mut ctl).await;
    ctl.command(connect_and_open("b", "com.x", 0)).await;
    assert!(matches!(ctl.event().await, ConnectEvent::PlugIn { .. }));
    ctl.online("b");
    ctl.state_until("a", PhoneState::Unplugged).await;
    tokio::time::sleep(2 * fast_timing().unplug_grace).await;
    assert_eq!(ctl.events_for_a_while().await, [gone("a")]);
    ctl.shutdown().await;
}

/// R7 (a): a Connect for another plugged phone while a is in its grace leaves a's grace
/// running; its end closes a's notice.
#[tokio::test]
async fn released_in_grace_by_connect_other_closes_its_notice() {
    let state = scratch_state();
    let mut ctl = start_ctl_opts(dead_port(), &["a", "b"], rules(&state, false), |_| {});
    let mut asked = vec![ctl.prompt().await, ctl.prompt().await];
    asked.sort_by_key(|e| format!("{e:?}"));
    assert_eq!(asked, [plugged_in("a", "a"), plugged_in("b", "b")]);
    wanted_in_grace(&mut ctl).await;
    ctl.command(connect("b")).await;
    ctl.state_until("a", PhoneState::Unplugged).await;
    tokio::time::sleep(2 * fast_timing().unplug_grace).await;
    assert_eq!(ctl.events_for_a_while().await, [gone("a")]);
    ctl.shutdown().await;
}

/// R7 (a), Disconnect of a during its grace; back within the grace it isn't asked.
#[tokio::test]
async fn released_in_grace_by_disconnect_closes_its_notice() {
    let state = scratch_state();
    let mut ctl = start_ctl_opts(dead_port(), &["a"], rules(&state, false), |_| {});
    assert_eq!(ctl.prompt().await, plugged_in("a", "a"));
    wanted_in_grace(&mut ctl).await;
    ctl.command(disconnect("a")).await;
    ctl.state_until("a", PhoneState::Unplugged).await;
    tokio::time::sleep(2 * fast_timing().unplug_grace).await;
    assert_eq!(ctl.events_for_a_while().await, [gone("a")]);
    // Again, and back within the grace: no prompt, and its notice isn't closed.
    ctl.online("a");
    assert_eq!(ctl.prompt().await, plugged_in("a", "a"));
    ctl.command(connect("a")).await;
    ctl.state_until("a", PhoneState::Retrying).await;
    ctl.offline("a");
    ctl.state_until("a", PhoneState::Reconnecting).await;
    ctl.command(disconnect("a")).await;
    ctl.state_until("a", PhoneState::Unplugged).await;
    ctl.online("a");
    ctl.state_until("a", PhoneState::Off).await;
    tokio::time::sleep(2 * fast_timing().unplug_grace).await;
    assert_eq!(ctl.events_for_a_while().await, []);
    ctl.shutdown().await;
}

/// R7 (b): a disconnected phone (autoconnect off, so its rule is Prompt) on a loose cable isn't
/// asked; away past the grace, the next plug-in asks.
#[tokio::test]
async fn disconnected_phone_blip_never_prompts() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &["dev"], rules(&state, false), |_| {});
    assert_eq!(ctl.event().await, plugged_in("dev", "Pixel"));
    ctl.command(connect("dev")).await;
    helper.next_session().await;
    ctl.ready("dev").await;
    assert_eq!(ctl.event().await, gone("dev"));
    ctl.command(disconnect("dev")).await;
    expect_bye(&mut helper).await;
    ctl.state_until("dev", PhoneState::Off).await;
    for _ in 0..3 {
        ctl.offline("dev");
        ctl.state_until("dev", PhoneState::Unplugged).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        ctl.online("dev");
        ctl.state_until("dev", PhoneState::Off).await;
    }
    assert_eq!(ctl.events_for_a_while().await, []);
    assert_eq!(flags(&state, "dev"), (true, false));
    ctl.offline("dev");
    assert_eq!(ctl.event().await, gone("dev"));
    ctl.online("dev");
    assert_eq!(ctl.event().await, plugged_in("dev", "Pixel"));
    assert_no_session(&mut helper).await;
    ctl.shutdown().await;
}

/// R7: with a zero grace (`LANTERN_UNPLUG_GRACE_MS=0`, the device items) every replug is a
/// plug-in, even when the replug is handled before the grace's expiry.
#[tokio::test]
async fn zero_grace_replug_prompts() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut opts = rules(&state, false);
    opts.timing = Some(Timing {
        unplug_grace: Duration::ZERO,
        pending_for: Duration::from_secs(2),
        ..fast_timing()
    });
    let mut ctl = start_ctl_opts(port, &["dev"], opts, |_| {});
    assert_eq!(ctl.event().await, plugged_in("dev", "Pixel"));
    for _ in 0..5 {
        ctl.offline("dev");
        ctl.online("dev");
        assert_eq!(ctl.prompt().await, plugged_in("dev", "Pixel"));
    }
    assert_no_session(&mut helper).await;
    ctl.shutdown().await;
}

/// R8: the wanted (live) phone back after its grace ended, before the expiry was handled (a
/// zero grace), is a new plug-in: it asks instead of reconnecting.
#[tokio::test]
async fn wanted_phone_back_after_grace_prompts() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut opts = rules(&state, false);
    opts.timing = Some(Timing {
        unplug_grace: Duration::ZERO,
        pending_for: Duration::from_secs(2),
        ..fast_timing()
    });
    let mut ctl = start_ctl_opts(port, &["dev"], opts, |_| {});
    assert_eq!(ctl.event().await, plugged_in("dev", "Pixel"));
    for n in 0..5 {
        ctl.command(connect("dev")).await;
        assert_eq!(helper.next_session().await, n);
        ctl.ready("dev").await;
        ctl.offline("dev");
        ctl.online("dev");
        expect_bye(&mut helper).await;
        assert_eq!(ctl.prompt().await, plugged_in("dev", "Pixel"));
        assert_no_session(&mut helper).await;
        assert_eq!(flags(&state, "dev"), (false, false));
    }
    ctl.shutdown().await;
}

/// R7: one phone away in its grace doesn't hush another phone's first plug-in.
#[tokio::test]
async fn first_plug_in_still_prompts() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &["a"], rules(&state, false), |_| {});
    assert_eq!(ctl.event().await, plugged_in("a", "a"));
    ctl.offline("a");
    ctl.state_until("a", PhoneState::Unplugged).await;
    ctl.online("new");
    assert_eq!(ctl.event().await, plugged_in("new", "new"));
    assert_no_session(&mut helper).await;
    ctl.shutdown().await;
}

/// Review Focus 2: a stale `live` flag (the phone stayed away for the grace after start) is
/// cleared, so plugging it in later asks.
#[tokio::test]
async fn stale_live_flag_cleared_when_absent_at_start() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    companion::set_live(&state, "dev", true).unwrap();
    let mut ctl = start_ctl_opts(port, &[], rules(&state, false), |_| {});
    ctl.state_until("dev", PhoneState::Unplugged).await;
    record_until(&state, "dev", |(_, live)| !live).await;
    ctl.online("dev");
    assert_eq!(ctl.event().await, plugged_in("dev", "Pixel"));
    assert_no_session(&mut helper).await;
    assert_eq!(ctl.phone("dev").unwrap().state, PhoneState::Waiting);
    ctl.shutdown().await;
}

/// The start-up stale-`live` check waits for adb's first device list: with a zero grace, a
/// live phone in that list (delivered after the grace has already passed) reconnects without
/// asking.
#[tokio::test]
async fn live_phone_in_first_device_list_reconnects_with_zero_grace() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    companion::set_live(&state, "dev", true).unwrap();
    let mut opts = rules(&state, false);
    opts.timing = Some(Timing {
        unplug_grace: Duration::ZERO,
        pending_for: Duration::from_secs(2),
        ..fast_timing()
    });
    opts.unlisted = true;
    let mut ctl = start_ctl_opts(port, &[], opts, |_| {});
    // adb track-devices answers a little after lanternd starts.
    tokio::time::sleep(Duration::from_millis(100)).await;
    ctl.online("dev");
    ctl.listed();
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("dev").await;
    ctl.assert_no_prompt().await;
    assert_eq!(flags(&state, "dev"), (false, true));
    ctl.shutdown().await;
}

/// `LANTERN_SERIAL`: that phone connects even off with autoconnect off; others are ignored.
#[tokio::test]
async fn configured_serial_bypasses_autoconnect_and_off() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    companion::set_off(&state, "dev", true).unwrap();
    let mut opts = rules(&state, false);
    opts.configured = Some("dev".into());
    let mut ctl = start_ctl_opts(port, &["dev"], opts, |_| {});
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("dev").await;
    ctl.online("b");
    assert_no_session(&mut helper).await;
    let events = ctl.events_for_a_while().await;
    assert!(
        events
            .iter()
            .all(|e| matches!(e, ConnectEvent::Gone { serial } if serial == "dev")),
        "{events:?}"
    );
    assert_eq!(ctl.phone("b"), None);
    assert_eq!(ctl.view.borrow().phones.len(), 1);
    // Commands for another phone are ignored too.
    ctl.command(connect("b")).await;
    helper.assert_quiet().await;
    assert_eq!(ctl.phone("dev").unwrap().state, PhoneState::Ready);
    ctl.shutdown().await;
}

/// Decision 4: connecting b leaves a live; a gets no `bye` and keeps its `live` flag.
#[tokio::test]
async fn connecting_b_keeps_a_live() {
    let (mut dev, dev_port) = FakeHelper::spawn().await;
    let (mut b, b_port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_pair(&[("dev", dev_port), ("b", b_port)], rules(&state, false));
    ctl.command(connect("dev")).await;
    assert_eq!(dev.next_session().await, 0);
    ctl.ready("dev").await;
    ctl.command(connect("b")).await;
    assert_eq!(b.next_session().await, 0);
    ctl.ready("b").await;
    dev.assert_quiet().await;
    assert_eq!(ctl.phone("dev").unwrap().state, PhoneState::Ready);
    assert_eq!(flags(&state, "dev"), (false, true));
    assert_eq!(flags(&state, "b"), (false, true));
    assert_no_session(&mut dev).await;
    ctl.shutdown().await;
}

/// The live phone unplugged for good: the other plugged phone (waiting, autoconnect off) is
/// not started.
#[tokio::test]
async fn live_phone_gone_starts_nothing() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &["a", "b"], rules(&state, false), |_| {});
    ctl.command(connect("a")).await;
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("a").await;
    ctl.offline("a");
    expect_bye(&mut helper).await;
    ctl.state_until("a", PhoneState::Unplugged).await;
    assert_no_session(&mut helper).await;
    assert_eq!(ctl.phone("b").unwrap().state, PhoneState::Waiting);
    assert_eq!(status(&ctl.sock).await.state, SessionState::Idle);
    ctl.shutdown().await;
}

/// Two-phone rule: each phone's `off` is its own; neither starts after both replug.
#[tokio::test]
async fn per_phone_off_flags_are_independent() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &["a"], rules(&state, true), |_| {});
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("a").await;
    ctl.command(disconnect("a")).await;
    expect_bye(&mut helper).await;
    ctl.state_until("a", PhoneState::Off).await;
    ctl.online("b");
    ctl.command(connect("b")).await;
    assert_eq!(helper.next_session().await, 1);
    ctl.ready("b").await;
    ctl.command(disconnect("b")).await;
    expect_bye(&mut helper).await;
    ctl.state_until("b", PhoneState::Off).await;
    assert_eq!(ctl.phone("a").unwrap().state, PhoneState::Off);
    ctl.offline("a");
    ctl.offline("b");
    tokio::time::sleep(2 * fast_timing().unplug_grace).await;
    ctl.online("a");
    ctl.online("b");
    assert_no_session(&mut helper).await;
    ctl.state_until("a", PhoneState::Off).await;
    ctl.state_until("b", PhoneState::Off).await;
    assert_eq!(flags(&state, "a"), (true, false));
    assert_eq!(flags(&state, "b"), (true, false));
    ctl.shutdown().await;
}

/// Review Focus 5: a connect-and-open opens its app once, on its phone, even after a blip.
#[tokio::test]
async fn pending_connect_opens_once() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &[], rules(&state, false), |_| {});
    ctl.command(connect_and_open("b", "com.x", 0)).await;
    assert_eq!(
        ctl.event().await,
        ConnectEvent::PlugIn {
            serial: "b".into(),
            name: "b".into(),
            app: Some("com.x".into()),
        }
    );
    ctl.online("b");
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("b").await;
    assert_eq!(ctl.opened(), [("b".into(), "com.x".into(), 0)]);
    // A blip and ready again: not opened again.
    ctl.offline("b");
    expect_bye(&mut helper).await;
    ctl.state_until("b", PhoneState::Reconnecting).await;
    ctl.online("b");
    assert_eq!(helper.next_session().await, 1);
    ctl.ready("b").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(ctl.opened().len(), 1);
    ctl.assert_no_prompt().await;
    ctl.shutdown().await;
}

/// Review Focus 5: a pending request for b never opens on a, which connects first; b's request
/// survives a's Connect (decision 5) and opens on b once b is ready.
#[tokio::test]
async fn pending_is_per_phone() {
    let (mut a, a_port) = FakeHelper::spawn().await;
    let (mut b, b_port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_pair_online(&[("a", a_port), ("b", b_port)], &[], rules(&state, false));
    ctl.command(connect_and_open("b", "com.x", 0)).await;
    assert!(matches!(ctl.event().await, ConnectEvent::PlugIn { .. }));
    ctl.online("a");
    assert_eq!(ctl.event().await, plugged_in("a", "a"));
    ctl.command(connect("a")).await;
    assert_eq!(a.next_session().await, 0);
    ctl.ready("a").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(ctl.opened(), [], "nothing opens on a");
    ctl.online("b");
    assert_eq!(b.next_session().await, 0);
    ctl.ready("b").await;
    assert_eq!(ctl.opened(), [("b".into(), "com.x".into(), 0)]);
    assert_eq!(ctl.phone("a").unwrap().state, PhoneState::Ready);
    a.assert_quiet().await;
    ctl.assert_no_prompt().await;
    ctl.shutdown().await;
}

/// Decision 5: a request for a, then Connect{b}: b connects, and a plugged in within the
/// request's window connects too and opens its app; b stays live.
#[tokio::test]
async fn connect_for_b_keeps_a_pending_request() {
    let (mut a, a_port) = FakeHelper::spawn().await;
    let (mut b, b_port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_pair_online(
        &[("a", a_port), ("b", b_port)],
        &["b"],
        rules(&state, false),
    );
    assert_eq!(ctl.event().await, plugged_in("b", "b"));
    ctl.command(connect_and_open("a", "com.x", 0)).await;
    assert!(matches!(ctl.event().await, ConnectEvent::PlugIn { .. }));
    ctl.command(connect("b")).await;
    assert_eq!(b.next_session().await, 0);
    ctl.ready("b").await;
    ctl.online("a");
    assert_eq!(a.next_session().await, 0);
    ctl.ready("a").await;
    assert_eq!(ctl.opened(), [("a".into(), "com.x".into(), 0)]);
    assert_eq!(ctl.phone("b").unwrap().state, PhoneState::Ready);
    b.assert_quiet().await;
    ctl.shutdown().await;
}

/// Decision 5: a request for a, then c connects through its own rule (autoconnect): a's
/// request stays, so a plugged in later connects and opens its app, and c stays live.
#[tokio::test]
async fn plug_connect_keeps_other_pending() {
    let (mut a, a_port) = FakeHelper::spawn().await;
    let (mut c, c_port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_pair_online(&[("a", a_port), ("c", c_port)], &[], rules(&state, true));
    ctl.command(connect_and_open("a", "com.x", 0)).await;
    assert!(matches!(ctl.event().await, ConnectEvent::PlugIn { .. }));
    ctl.online("c");
    assert_eq!(c.next_session().await, 0);
    ctl.ready("c").await;
    assert_eq!(ctl.opened(), []);
    ctl.online("a");
    assert_eq!(a.next_session().await, 0);
    ctl.ready("a").await;
    assert_eq!(ctl.opened(), [("a".into(), "com.x".into(), 0)]);
    assert_eq!(ctl.phone("c").unwrap().state, PhoneState::Ready);
    c.assert_quiet().await;
    ctl.shutdown().await;
}

/// Decision 4: every phone that was live when lanternd stopped reconnects, without asking.
#[tokio::test]
async fn every_live_phone_reconnects_after_restart() {
    let (mut a, a_port) = FakeHelper::spawn().await;
    let (mut x, x_port) = FakeHelper::spawn().await;
    let state = scratch_state();
    companion::set_live(&state, "a", true).unwrap();
    companion::set_live(&state, "x", true).unwrap();
    let mut ctl = start_pair(&[("a", a_port), ("x", x_port)], rules(&state, false));
    assert_eq!(a.next_session().await, 0);
    assert_eq!(x.next_session().await, 0);
    ctl.ready("a").await;
    ctl.ready("x").await;
    ctl.assert_no_prompt().await;
    assert_eq!(flags(&state, "a"), (false, true));
    assert_eq!(flags(&state, "x"), (false, true));
    ctl.shutdown().await;
}

/// `lantern connect|disconnect <serial>` for a phone neither plugged in nor known is refused
/// with the known phones named, and creates no record.
#[tokio::test]
async fn daemon_cmd_refuses_unknown_serial() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    companion::set_off(&state, "old", true).unwrap();
    let mut ctl = start_ctl_opts(port, &["dev"], rules(&state, true), |_| {});
    helper.next_session().await;
    ctl.ready("dev").await;
    for cmd in [DaemonCmd::Connect, DaemonCmd::Disconnect] {
        assert_eq!(
            daemon_cmd_reply(&ctl.sock, cmd, Some("typo")).await,
            Control::error(
                "no phone typo is plugged in or known; known phones: Pixel (dev), old (old)"
            )
        );
    }
    helper.assert_quiet().await;
    assert!(companion::read_record(&state, "typo").is_none());
    assert!(ctl.phone("typo").is_none());
    ctl.assert_no_prompt().await;
    // A known phone that isn't plugged in is accepted.
    let reply = daemon_cmd_reply(&ctl.sock, DaemonCmd::Connect, Some("old")).await;
    assert!(matches!(reply, Control::StatusReport(_)), "{reply:?}");
    assert!(matches!(ctl.prompt().await, ConnectEvent::PlugIn { .. }));
    ctl.shutdown().await;
}

/// Under `LANTERN_SERIAL` a CLI command naming another phone (plugged in, known from a record,
/// or a typo) is refused with the reason, not as unknown; the configured phone is accepted.
#[tokio::test]
async fn daemon_cmd_for_another_serial_under_lantern_serial_is_refused() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    companion::set_off(&state, "old", true).unwrap();
    let mut opts = rules(&state, false);
    opts.configured = Some("dev".into());
    let mut ctl = start_ctl_opts(port, &["dev", "b"], opts, |_| {});
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("dev").await;
    for serial in ["b", "old", "typo"] {
        for cmd in [DaemonCmd::Connect, DaemonCmd::Disconnect] {
            assert_eq!(
                daemon_cmd_reply(&ctl.sock, cmd, Some(serial)).await,
                Control::error(format!(
                    "phone {serial} is not served: lanternd serves only dev because \
                     LANTERN_SERIAL=dev is set"
                ))
            );
        }
    }
    helper.assert_quiet().await;
    assert!(companion::read_record(&state, "typo").is_none());
    assert!(companion::read_record(&state, "b").is_none());
    let reply = daemon_cmd_reply(&ctl.sock, DaemonCmd::Connect, Some("dev")).await;
    assert!(matches!(reply, Control::StatusReport(_)), "{reply:?}");
    ctl.shutdown().await;
}

/// With no phone plugged in or known, the refusal says so.
#[tokio::test]
async fn daemon_cmd_unknown_serial_with_no_phones() {
    let (_helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let ctl = start_ctl_opts(port, &[], rules(&state, false), |_| {});
    assert_eq!(
        daemon_cmd_reply(&ctl.sock, DaemonCmd::Connect, Some("typo")).await,
        Control::error("no phone typo is plugged in or known; no phone is known")
    );
    assert!(companion::read_record(&state, "typo").is_none());
    ctl.shutdown().await;
}

#[tokio::test]
async fn pending_expires() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut opts = rules(&state, false);
    opts.timing = Some(Timing {
        unplug_grace: Duration::from_millis(300),
        pending_for: Duration::from_millis(200),
        ..fast_timing()
    });
    let mut ctl = start_ctl_opts(port, &[], opts, |_| {});
    ctl.command(connect_and_open("b", "com.x", 0)).await;
    assert!(matches!(ctl.event().await, ConnectEvent::PlugIn { .. }));
    tokio::time::sleep(Duration::from_millis(400)).await;
    ctl.online("b");
    assert_eq!(ctl.event().await, plugged_in("b", "b"));
    assert_no_session(&mut helper).await;
    assert_eq!(ctl.opened(), []);
    ctl.shutdown().await;
}

/// R8: a Connect without an app for an unplugged phone is remembered ("It connects if you
/// plug it in within 2 minutes"): plugged in within the window it connects, autoconnect off,
/// without asking, and opens nothing.
#[tokio::test]
async fn plain_connect_for_unplugged_phone_is_remembered() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &[], rules(&state, false), |_| {});
    ctl.command(connect("b")).await;
    assert_eq!(
        ctl.event().await,
        ConnectEvent::PlugIn {
            serial: "b".into(),
            name: "b".into(),
            app: None,
        }
    );
    ctl.online("b");
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("b").await;
    ctl.assert_no_prompt().await;
    assert_eq!(ctl.opened(), [], "no app was asked for");
    ctl.shutdown().await;
}

/// R8: past the window, a plain Connect for an unplugged phone is forgotten: plugging it in
/// asks.
#[tokio::test]
async fn plain_connect_for_unplugged_phone_expires() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut opts = rules(&state, false);
    opts.timing = Some(Timing {
        unplug_grace: Duration::from_millis(300),
        pending_for: Duration::from_millis(200),
        ..fast_timing()
    });
    let mut ctl = start_ctl_opts(port, &[], opts, |_| {});
    ctl.command(connect("b")).await;
    assert!(matches!(ctl.event().await, ConnectEvent::PlugIn { .. }));
    tokio::time::sleep(Duration::from_millis(400)).await;
    ctl.online("b");
    assert_eq!(ctl.event().await, plugged_in("b", "b"));
    assert_no_session(&mut helper).await;
    ctl.shutdown().await;
}

/// R8: a plain Connect after a connect-and-open for the same unplugged phone keeps the app.
#[tokio::test]
async fn plain_connect_keeps_the_requested_app() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &[], rules(&state, false), |_| {});
    ctl.command(connect_and_open("b", "com.x", 0)).await;
    ctl.command(connect("b")).await;
    ctl.online("b");
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("b").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(ctl.opened(), [("b".into(), "com.x".into(), 0)]);
    ctl.shutdown().await;
}

#[tokio::test]
async fn later_request_replaces() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let mut ctl = start_ctl_opts(port, &[], rules(&state, false), |_| {});
    ctl.command(connect_and_open("b", "com.x", 0)).await;
    ctl.command(connect_and_open("b", "com.y", 10)).await;
    ctl.online("b");
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("b").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(ctl.opened(), [("b".into(), "com.y".into(), 10)]);
    ctl.shutdown().await;
}

/// An undo that always succeeds at once.
fn instant_undo() -> (UndoRunner, mpsc::UnboundedReceiver<String>) {
    let (calls_tx, calls) = mpsc::unbounded_channel();
    let undo: UndoRunner = Arc::new(move |serial: String| {
        let _ = calls_tx.send(serial);
        Box::pin(async { (true, "companion: uninstalled".to_owned()) })
    });
    (undo, calls)
}

#[tokio::test]
async fn undo_keeps_record_and_sets_off() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let (undo, mut calls) = instant_undo();
    let mut opts = rules(&state, true);
    opts.undo = Some(undo);
    let mut ctl = start_ctl_opts(port, &["dev"], opts, |_| {});
    helper.next_session().await;
    ctl.ready("dev").await;
    ctl.command(undo_setup("dev")).await;
    expect_bye(&mut helper).await;
    assert_eq!(timeout(STEP, calls.recv()).await.unwrap().unwrap(), "dev");
    let view = ctl.state_until("dev", PhoneState::Off).await;
    assert!(view.off);
    assert_eq!(flags(&state, "dev"), (true, false));
    assert_no_session(&mut helper).await;
    ctl.shutdown().await;
}

#[tokio::test]
async fn undo_of_waiting_phone_leaves_live_session() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    let (undo, mut calls) = instant_undo();
    let mut opts = rules(&state, false);
    opts.undo = Some(undo);
    let mut ctl = start_ctl_opts(port, &["a", "b"], opts, |_| {});
    ctl.command(connect("a")).await;
    helper.next_session().await;
    ctl.ready("a").await;
    ctl.state_until("b", PhoneState::Waiting).await;
    ctl.command(undo_setup("b")).await;
    assert_eq!(timeout(STEP, calls.recv()).await.unwrap().unwrap(), "b");
    helper.assert_quiet().await;
    ctl.state_until("b", PhoneState::Off).await;
    assert_eq!(ctl.phone("a").unwrap().state, PhoneState::Ready);
    ctl.shutdown().await;
}

/// When an undo ends, nothing else starts: the other plugged phone keeps waiting.
#[tokio::test]
async fn undo_end_starts_nothing() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let undo: UndoRunner = Arc::new(move |_serial: String| {
        let gate = gate.clone();
        Box::pin(async move {
            gate.acquire().await.unwrap().forget();
            (true, "companion: uninstalled".to_owned())
        })
    });
    let state = scratch_state();
    let mut opts = rules(&state, false);
    opts.undo = Some(undo);
    let mut ctl = start_ctl_opts(port, &["a", "b"], opts, |_| {});
    ctl.command(connect("a")).await;
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("a").await;
    ctl.command(undo_setup("a")).await;
    expect_bye(&mut helper).await;
    ctl.state_until("a", PhoneState::Undoing).await;
    // Nothing starts while the undo runs.
    assert_no_session(&mut helper).await;
    release.add_permits(1);
    ctl.state_until("a", PhoneState::Off).await;
    assert_no_session(&mut helper).await;
    assert_eq!(ctl.phone("b").unwrap().state, PhoneState::Waiting);
    ctl.shutdown().await;
}

#[tokio::test]
async fn daemon_cmd_without_serial_needs_one_plugged() {
    let (mut dev, dev_port) = FakeHelper::spawn().await;
    let (mut b, b_port) = FakeHelper::spawn().await;
    let mut ctl = start_pair(&[("dev", dev_port), ("b", b_port)], Opts::default());
    dev.next_session().await;
    b.next_session().await;
    ctl.ready("dev").await;
    ctl.ready("b").await;
    assert_eq!(
        daemon_cmd_reply(&ctl.sock, DaemonCmd::Connect, None).await,
        Control::error(
            "several phones are plugged in: Pixel (dev), b (b); name one: lantern connect <serial>"
        )
    );
    dev.assert_quiet().await;
    b.assert_quiet().await;
    // An explicit serial works, on that phone only.
    let reply = daemon_cmd_reply(&ctl.sock, DaemonCmd::Disconnect, Some("dev")).await;
    assert!(matches!(reply, Control::StatusReport(_)), "{reply:?}");
    expect_bye(&mut dev).await;
    ctl.state_until("dev", PhoneState::Off).await;
    b.assert_quiet().await;
    // One plugged in: that phone (already live: nothing restarts).
    ctl.offline("dev");
    ctl.state_until("dev", PhoneState::Unplugged).await;
    daemon_cmd(&ctl.sock, DaemonCmd::Connect).await;
    assert_no_session(&mut b).await;
    assert_eq!(ctl.phone("b").unwrap().state, PhoneState::Ready);
    // None plugged in.
    ctl.offline("b");
    expect_bye(&mut b).await;
    ctl.view_until(|v| v.phones.iter().all(|p| !p.plugged))
        .await;
    assert_eq!(
        daemon_cmd_reply(&ctl.sock, DaemonCmd::Disconnect, None).await,
        Control::error("no phone is plugged in")
    );
    ctl.shutdown().await;
}

#[tokio::test]
async fn status_lists_every_phone() {
    use lantern_proto::PhoneReport;
    let (mut helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    companion::set_off(&state, "old", true).unwrap();
    let mut ctl = start_ctl_opts(port, &["dev", "b"], rules(&state, false), |_| {});
    ctl.command(connect("dev")).await;
    helper.next_session().await;
    ctl.ready("dev").await;
    ctl.state_until("b", PhoneState::Waiting).await;
    let report = status(&ctl.sock).await;
    assert_eq!(report.state, SessionState::Ready);
    let connected_at = report.phones[0].connected_at;
    assert!(connected_at.is_some(), "{report:?}");
    assert_eq!(
        report.phones,
        [
            PhoneReport {
                serial: "dev".into(),
                name: Some("Pixel".into()),
                state: PhoneState::Ready,
                plugged: true,
                off: false,
                connected_at,
            },
            PhoneReport {
                serial: "b".into(),
                name: None,
                state: PhoneState::Waiting,
                plugged: true,
                off: false,
                connected_at: None,
            },
            PhoneReport {
                serial: "old".into(),
                name: None,
                state: PhoneState::Unplugged,
                plugged: false,
                off: true,
                connected_at: None,
            },
        ]
    );
    ctl.command(disconnect("dev")).await;
    expect_bye(&mut helper).await;
    ctl.state_until("dev", PhoneState::Off).await;
    let report = status(&ctl.sock).await;
    assert_eq!(
        (report.state, report.serial.as_deref()),
        (SessionState::Paused, Some("dev"))
    );
    assert_eq!(report.phones[0].state, PhoneState::Off);
    assert!(report.phones[0].off);
    ctl.shutdown().await;
}

// --- the PIN lock: locked/unlocked, unlock{pin}, opens the lock blocked ---------------------

use lantern::unlock::{LockEvent, Pin};
use lantern_proto::LockReason;

/// The view's ready phone, if any.
fn ready_phone(view: &View) -> Option<&PhoneView> {
    view.phones.iter().find(|p| p.state == PhoneState::Ready)
}

/// A daemon with a view, a PIN channel and lock events, on the fake helper.
struct Locky {
    daemon: Daemon,
    view: watch::Receiver<View>,
    /// PINs, each with the serial of the phone it was asked for (the fake helper's is `dev`).
    pins: mpsc::Sender<(String, Pin)>,
    events: mpsc::Receiver<LockEvent>,
}

async fn start_locky(port: u16) -> Locky {
    let (view_tx, mut view) = watch::channel(View::default());
    let (pins, pins_rx) = mpsc::channel(1);
    let (events_tx, events) = mpsc::channel(16);
    let daemon = start_daemon_parts(
        Some(port),
        Parts {
            view: Some(view_tx),
            unlock: Some(pins_rx),
            lock_events: Some(events_tx),
            ..Default::default()
        },
    );
    timeout(STEP, view.wait_for(|v| ready_phone(v).is_some()))
        .await
        .expect("session never ready")
        .unwrap();
    Locky {
        daemon,
        view,
        pins,
        events,
    }
}

impl Locky {
    async fn locked_until(&mut self, locked: bool) {
        timeout(
            STEP,
            self.view
                .wait_for(|v| v.live_phones().next().is_some_and(|p| p.locked == locked)),
        )
        .await
        .unwrap_or_else(|_| panic!("view.locked never became {locked}"))
        .unwrap();
    }

    async fn event(&mut self) -> LockEvent {
        timeout(STEP, self.events.recv())
            .await
            .expect("no lock event")
            .unwrap()
    }
}

#[tokio::test]
async fn unlock_frame_reaches_helper_once() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut locky = start_locky(port).await;
    // Decision 11: a PIN goes only to a phone whose lock stretch is open.
    helper.send(control(
        0,
        Control::Locked {
            reason: LockReason::Clipboard,
        },
    ));
    locky.locked_until(true).await;
    locky
        .pins
        .send(("dev".into(), Pin::parse("24680\n".into()).unwrap()))
        .await
        .unwrap();
    let frame = helper.next().await;
    assert_eq!(frame.stream, 0);
    match decode(&frame) {
        Control::Unlock { pin } => assert_eq!(pin.expose(), "24680"),
        other => panic!("expected unlock, got {other:?}"),
    }
    assert_eq!(frame.payload, br#"{"t":"unlock","pin":"24680"}"#);
    helper.assert_quiet().await;
    locky.daemon.shutdown().await;
}

#[tokio::test]
async fn unlock_without_session_is_dropped_with_a_notice() {
    let rt = runtime_dir();
    let listener = ipc::bind(&rt).unwrap();
    let notes = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
    let noted = notes.clone();
    let (pins, pins_rx) = mpsc::channel(1);
    let (stop, stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(serve_with(
        listener,
        Parts {
            unlock: Some(pins_rx),
            notify: Some(Arc::new(move |summary: &str, body: &str| {
                noted
                    .lock()
                    .unwrap()
                    .push((summary.to_owned(), body.to_owned()));
            })),
            ..Default::default()
        },
        None,
        stream::pending::<DeviceEvent>(),
        |_serial: String| async move { anyhow::bail!("no phone in this test") },
        async move {
            let _ = stop_rx.await;
            "test-stop"
        },
        fast_policy(),
    ));
    pins.send(("dev".into(), Pin::parse("1357".into()).unwrap()))
        .await
        .unwrap();
    let deadline = Instant::now() + STEP;
    while notes.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "no notice");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let notes = notes.lock().unwrap().clone();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].0, "dev isn't connected");
    assert!(!notes[0].0.contains("1357") && !notes[0].1.contains("1357"));
    let _ = stop.send(());
    timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// Collects everything a `tracing` fmt subscriber writes.
#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn unlock_frame_not_logged() {
    const DIGITS: &str = "80417395";
    let captured = Captured::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    // A current-thread test runtime: every task logs on this thread.
    let _guard = tracing::subscriber::set_default(subscriber);
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut locky = start_locky(port).await;
    helper.send(control(
        0,
        Control::Locked {
            reason: LockReason::Clipboard,
        },
    ));
    locky.locked_until(true).await;
    locky
        .pins
        .send(("dev".into(), Pin::parse(format!("{DIGITS}\n")).unwrap()))
        .await
        .unwrap();
    let frame = helper.next().await;
    assert!(matches!(decode(&frame), Control::Unlock { .. }));
    helper.send(control(
        0,
        Control::UnlockResult {
            ok: false,
            reason: Some("wrong PIN".into()),
        },
    ));
    // A malformed unlock-shaped frame from the helper (a number where a string goes) is not
    // quoted in any decode error either.
    helper.send(Frame {
        kind: Kind::Control,
        stream: 0,
        payload: format!(r#"{{"t":"unlock","pin":{DIGITS}}}"#).into_bytes(),
    });
    loop {
        if let LockEvent::Result { ok: false, .. } = locky.event().await {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    locky.daemon.shutdown().await;
    let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(text.contains("unlock"), "nothing captured: {text}");
    assert!(
        !text.contains(DIGITS),
        "the PIN reached a log line:\n{text}"
    );
}

#[tokio::test]
async fn locked_sets_view_and_unlocked_clears() {
    let (helper, port) = FakeHelper::spawn().await;
    let mut locky = start_locky(port).await;
    assert!(!locky.view.borrow().live_phones().next().unwrap().locked);
    helper.send(control(
        0,
        Control::Locked {
            reason: LockReason::Clipboard,
        },
    ));
    locky.locked_until(true).await;
    assert_eq!(
        locky.event().await,
        LockEvent::Locked {
            serial: "dev".into(),
            reason: LockReason::Clipboard,
        }
    );
    helper.send(control(0, Control::Unlocked));
    locky.locked_until(false).await;
    assert_eq!(
        locky.event().await,
        LockEvent::Unlocked {
            serial: "dev".into(),
        }
    );

    // A successful unlock_result also ends it; a failed one does not.
    helper.send(control(
        0,
        Control::Locked {
            reason: LockReason::Open,
        },
    ));
    locky.locked_until(true).await;
    assert_eq!(
        locky.event().await,
        LockEvent::Locked {
            serial: "dev".into(),
            reason: LockReason::Open,
        }
    );
    let failed = LockEvent::Result {
        serial: "dev".into(),
        ok: false,
        reason: Some("wrong PIN".into()),
    };
    helper.send(control(
        0,
        Control::UnlockResult {
            ok: false,
            reason: Some("wrong PIN".into()),
        },
    ));
    assert_eq!(locky.event().await, failed);
    assert!(locky.view.borrow().live_phones().next().unwrap().locked);
    helper.send(control(
        0,
        Control::UnlockResult {
            ok: true,
            reason: None,
        },
    ));
    locky.locked_until(false).await;
    assert_eq!(
        locky.event().await,
        LockEvent::Result {
            serial: "dev".into(),
            ok: true,
            reason: None
        }
    );

    // The end of the session ends it too.
    helper.send(control(
        0,
        Control::Locked {
            reason: LockReason::Clipboard,
        },
    ));
    locky.locked_until(true).await;
    assert_eq!(
        locky.event().await,
        LockEvent::Locked {
            serial: "dev".into(),
            reason: LockReason::Clipboard,
        }
    );
    helper.die();
    locky.locked_until(false).await;
    assert_eq!(
        locky.event().await,
        LockEvent::Unlocked {
            serial: "dev".into(),
        }
    );
    locky.daemon.shutdown().await;
}

#[tokio::test]
async fn open_failed_while_locked_gets_phone_locked() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut locky = start_locky(port).await;
    let sock = locky.daemon.sock.clone();
    let open_blocked = |pkg: &'static str, user: u32| {
        let sock = sock.clone();
        async move {
            let mut client = UnixStream::connect(&sock).await.unwrap();
            send(&mut client, &open(pkg, user, None)).await;
            client
        }
    };
    let mut client = open_blocked("com.foo", 0).await;
    let fwd = helper.next().await;
    let win = fwd.stream;
    assert!(matches!(decode(&fwd), Control::Open { .. }));
    // The helper's order: locked{open}, then the window's error and closed.
    helper.send(control(
        0,
        Control::Locked {
            reason: LockReason::Open,
        },
    ));
    helper.send(control(
        win,
        Control::error("open com.foo failed: SecurityException"),
    ));
    let closed = control(
        win,
        Control::Closed {
            reason: CloseReason::Error,
        },
    );
    helper.send(closed.clone());
    assert_eq!(
        recv(&mut client).await.as_ref().map(decode),
        Some(Control::error(ipc::PHONE_LOCKED))
    );
    assert_eq!(recv(&mut client).await, Some(closed));
    assert_eof(&mut client).await;
    assert_eq!(
        locky.event().await,
        LockEvent::Locked {
            serial: "dev".into(),
            reason: LockReason::Open,
        }
    );

    // The same app blocked twice and another user's app: one entry each.
    for (pkg, user) in [("com.foo", 0), ("com.bar", 10)] {
        let mut client = open_blocked(pkg, user).await;
        let win = helper.next().await.stream;
        helper.send(control(win, Control::error("open failed")));
        helper.send(control(
            win,
            Control::Closed {
                reason: CloseReason::Error,
            },
        ));
        assert_eq!(
            recv(&mut client).await.as_ref().map(decode),
            Some(Control::error(ipc::PHONE_LOCKED))
        );
    }

    helper.send(control(0, Control::Unlocked));
    assert_eq!(
        locky.event().await,
        LockEvent::Unlocked {
            serial: "dev".into(),
        }
    );
    assert_eq!(
        locky.event().await,
        LockEvent::Reopen {
            serial: "dev".into(),
            apps: vec![("com.foo".into(), 0), ("com.bar".into(), 10)]
        }
    );

    // Unlocked now: a failed open keeps the phone's own error and is not reopened.
    let mut client = open_blocked("com.baz", 0).await;
    let win = helper.next().await.stream;
    let error = control(win, Control::error("no launch intent for com.baz"));
    helper.send(error.clone());
    assert_eq!(recv(&mut client).await, Some(error));
    helper.send(control(0, Control::Unlocked));
    assert_eq!(
        locky.event().await,
        LockEvent::Unlocked {
            serial: "dev".into(),
        }
    );
    assert!(
        timeout(Duration::from_millis(300), locky.events.recv())
            .await
            .is_err(),
        "nothing to reopen"
    );
    locky.daemon.shutdown().await;
}

/// Decision 11 with two live phones: a PIN for dev, which isn't locked, is dropped (no `unlock`
/// frame anywhere); b's PIN reaches b's helper only.
#[tokio::test]
async fn pin_for_an_unlocked_phone_is_dropped() {
    let (mut dev, dev_port) = FakeHelper::spawn().await;
    let (mut b, b_port) = FakeHelper::spawn().await;
    let (pins, pins_rx) = mpsc::channel::<(String, Pin)>(1);
    let mut ctl = start_ctl_ports(
        {
            let ports = [("dev", dev_port), ("b", b_port)];
            Arc::new(move |serial: &str| ports.iter().find(|(s, _)| *s == serial).map(|p| p.1))
        },
        &["dev", "b"],
        Opts::default(),
        |parts| parts.unlock = Some(pins_rx),
    );
    dev.next_session().await;
    b.next_session().await;
    ctl.ready("dev").await;
    ctl.ready("b").await;
    b.send(control(
        0,
        Control::Locked {
            reason: LockReason::Clipboard,
        },
    ));
    ctl.view_until(|v| v.phone("b").is_some_and(|p| p.locked))
        .await;
    pins.send(("dev".into(), Pin::parse("1357".into()).unwrap()))
        .await
        .unwrap();
    dev.assert_quiet().await;
    b.assert_quiet().await;
    assert!(
        ctl.notes.lock().unwrap().is_empty(),
        "dev is live: no notice"
    );
    pins.send(("b".into(), Pin::parse("2468".into()).unwrap()))
        .await
        .unwrap();
    match decode(&b.next().await) {
        Control::Unlock { pin } => assert_eq!(pin.expose(), "2468"),
        other => panic!("expected unlock, got {other:?}"),
    }
    dev.assert_quiet().await;
    ctl.shutdown().await;
}

/// R7: only the start of a locked stretch is forwarded, so a burst of `locked` cannot fill the
/// prompt's queue and push out the `unlocked`/`unlock_result` behind it.
#[tokio::test]
async fn locked_burst_is_one_event() {
    let (helper, port) = FakeHelper::spawn().await;
    let mut locky = start_locky(port).await;
    for _ in 0..50 {
        helper.send(control(
            0,
            Control::Locked {
                reason: LockReason::Clipboard,
            },
        ));
    }
    helper.send(control(0, Control::Unlocked));
    assert_eq!(
        locky.event().await,
        LockEvent::Locked {
            serial: "dev".into(),
            reason: LockReason::Clipboard,
        }
    );
    assert_eq!(
        locky.event().await,
        LockEvent::Unlocked {
            serial: "dev".into(),
        }
    );
    // A new stretch is forwarded again.
    helper.send(control(
        0,
        Control::Locked {
            reason: LockReason::Open,
        },
    ));
    assert_eq!(
        locky.event().await,
        LockEvent::Locked {
            serial: "dev".into(),
            reason: LockReason::Open,
        }
    );
    locky.daemon.shutdown().await;
}

/// R7: without the unlock prompt (no session bus) nothing tells the user to enter a PIN, so a
/// blocked open keeps the phone's own error and is not reopened.
#[tokio::test]
async fn without_unlock_prompt_open_errors_are_kept() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let (view_tx, mut view) = watch::channel(View::default());
    let daemon = start_daemon_parts(
        Some(port),
        Parts {
            view: Some(view_tx),
            ..Default::default()
        },
    );
    timeout(STEP, view.wait_for(|v| ready_phone(v).is_some()))
        .await
        .unwrap()
        .unwrap();
    helper.send(control(
        0,
        Control::Locked {
            reason: LockReason::Open,
        },
    ));
    timeout(
        STEP,
        view.wait_for(|v| v.live_phones().next().is_some_and(|p| p.locked)),
    )
    .await
    .unwrap()
    .unwrap();
    let mut client = UnixStream::connect(&daemon.sock).await.unwrap();
    send(&mut client, &open("com.foo", 0, None)).await;
    let win = helper.next().await.stream;
    let error = control(
        win,
        Control::error("open com.foo failed: SecurityException"),
    );
    helper.send(error.clone());
    assert_eq!(recv(&mut client).await, Some(error));
    daemon.shutdown().await;
}

// --- open.phone routing (Task 5) ---------------------------------------------------------------

/// A window's `open` naming `phone`.
fn open_on(pkg: &str, user: u32, phone: &str) -> Frame {
    control(
        0,
        Control::Open {
            pkg: pkg.into(),
            user,
            w: 1280,
            h: 800,
            dpi: 160,
            activation_token: None,
            phone: Some(phone.into()),
        },
    )
}

/// Sends `frame` as a new client's `open` and returns lanternd's reply (it must be lanternd's
/// own refusal on stream 0), after checking it then closes the client.
async fn refused(sock: &Path, frame: Frame) -> Control {
    let mut client = UnixStream::connect(sock).await.unwrap();
    send(&mut client, &frame).await;
    let reply = recv(&mut client).await.expect("a reply to the open");
    assert_eq!(reply.stream, 0);
    assert_eof(&mut client).await;
    decode(&reply)
}

/// Sends `dev`'s full catalogue (com.y only) and waits until it is in: from then on an app in
/// the menu for other phones only is "not on this phone".
async fn full_sync(helper: &FakeHelper, dirs: &Dirs) {
    helper.send(app("com.y", 0, "Yankee", ""));
    helper.send(control(0, Control::AppsEnd { count: 1 }));
    // Helper frames are handled in order: once com.z's entry exists, apps_end has been too.
    helper.send(app("com.z", 0, "Zulu", ""));
    wait_files(&dirs.applications, "synced", |n| {
        n.iter().any(|f| f == "lantern.com.z.desktop")
    })
    .await;
}

/// `open{phone}` naming the live phone goes to its helper with `phone` stripped, even for an
/// app the catalogue says is only on another phone (the window chose the phone).
#[tokio::test]
async fn open_naming_the_live_phone_is_forwarded() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let (root, dirs, catalog) = temp_catalog();
    plant_entry(&dirs, "com.x", "other");
    let mut ctl = start_ctl_with(port, &["dev"], None, |p| p.catalog = Some(catalog));
    helper.next_session().await;
    ctl.ready("dev").await;
    full_sync(&helper, &dirs).await;
    // Without a phone the Plan 3 rule still refuses it.
    assert_eq!(
        refused(&ctl.sock, open("com.x", 0, None)).await,
        Control::error(ipc::NOT_ON_THIS_PHONE)
    );
    let mut client = UnixStream::connect(&ctl.sock).await.unwrap();
    send(&mut client, &open_on("com.x", 0, "dev")).await;
    let fwd = helper.next().await;
    assert!(fwd.stream > 0);
    assert_eq!(
        fwd.payload,
        br#"{"t":"open","pkg":"com.x","user":0,"w":1280,"h":800,"dpi":160}"#
    );
    ctl.assert_no_prompt().await;
    ctl.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// With the connect prompt, `open{phone}` naming a phone that isn't live is refused with
/// CONNECT_OFFERED and the prompt gets `OpenRefused` (the app named as in its entry); the
/// helper sees nothing. An Off phone is offered too; a serial lanternd doesn't know is not.
#[tokio::test]
async fn open_naming_another_phone_is_offered() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let (root, dirs, catalog) = temp_catalog();
    plant_named_entry(&dirs, "com.x", "b", "Xray");
    let mut ctl = start_ctl_opts(port, &["dev", "b"], manual(), |p| p.catalog = Some(catalog));
    assert_eq!(ctl.prompt().await, plugged_in("dev", "Pixel"));
    assert_eq!(ctl.prompt().await, plugged_in("b", "b"));
    ctl.command(connect("dev")).await;
    helper.next_session().await;
    ctl.ready("dev").await;
    let offered = ConnectEvent::OpenRefused {
        serial: "b".into(),
        name: "b".into(),
        app: "Xray".into(),
        pkg: "com.x".into(),
        user: 0,
    };
    assert_eq!(
        refused(&ctl.sock, open_on("com.x", 0, "b")).await,
        Control::error(ipc::CONNECT_OFFERED)
    );
    assert_eq!(ctl.prompt().await, offered);
    helper.assert_quiet().await;
    // Switched off: still offered (Connect clears `off`).
    ctl.command(disconnect("b")).await;
    ctl.state_until("b", PhoneState::Off).await;
    assert_eq!(
        refused(&ctl.sock, open_on("com.x", 0, "b")).await,
        Control::error(ipc::CONNECT_OFFERED)
    );
    assert_eq!(ctl.prompt().await, offered);
    // A typo is no phone to connect: the old error, no prompt, no record.
    assert_eq!(
        refused(&ctl.sock, open_on("com.x", 0, "typo")).await,
        Control::error(ipc::NOT_CONNECTED)
    );
    ctl.assert_no_prompt().await;
    assert!(ctl.phone("typo").is_none());
    helper.assert_quiet().await;
    ctl.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// The ledger's Task 5 note: while the connect prompt's notifications don't get through (its
/// `Notify` failed, no server answers), lanternd doesn't offer Connect; the window gets the old
/// error and shows its own notice. Once the prompt is healthy again, opens are offered.
#[tokio::test]
async fn open_is_offered_only_while_the_prompt_is_healthy() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let health = lantern::connect::Health::new();
    let shared = health.clone();
    let mut ctl = start_ctl_opts(port, &["dev", "b"], manual(), |p| {
        p.connect_health = Some(shared)
    });
    assert_eq!(ctl.prompt().await, plugged_in("dev", "Pixel"));
    assert_eq!(ctl.prompt().await, plugged_in("b", "b"));
    ctl.command(connect("dev")).await;
    helper.next_session().await;
    ctl.ready("dev").await;
    assert_eq!(
        refused(&ctl.sock, open_on("com.x", 0, "b")).await,
        Control::error(ipc::NOT_CONNECTED)
    );
    ctl.command(disconnect("b")).await;
    ctl.state_until("b", PhoneState::Off).await;
    assert_eq!(
        refused(&ctl.sock, open_on("com.x", 0, "b")).await,
        Control::error(ipc::PAUSED)
    );
    ctl.assert_no_prompt().await;
    health.set(true);
    assert_eq!(
        refused(&ctl.sock, open_on("com.x", 0, "b")).await,
        Control::error(ipc::CONNECT_OFFERED)
    );
    assert!(matches!(
        ctl.prompt().await,
        ConnectEvent::OpenRefused { serial, .. } if serial == "b"
    ));
    health.set(false);
    assert_eq!(
        refused(&ctl.sock, open_on("com.x", 0, "b")).await,
        Control::error(ipc::PAUSED)
    );
    ctl.assert_no_prompt().await;
    helper.assert_quiet().await;
    ctl.shutdown().await;
}

/// Without the connect prompt (no session bus) the old errors stay: PAUSED for a phone
/// switched off in Lantern, NOT_CONNECTED otherwise.
#[tokio::test]
async fn open_naming_an_off_phone_without_prompt_is_paused_error() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut ctl = start_ctl_opts(port, &["dev", "b"], manual(), |p| p.connect_events = None);
    ctl.command(connect("dev")).await;
    helper.next_session().await;
    ctl.ready("dev").await;
    ctl.state_until("b", PhoneState::Waiting).await;
    assert_eq!(
        refused(&ctl.sock, open_on("com.x", 0, "b")).await,
        Control::error(ipc::NOT_CONNECTED)
    );
    ctl.command(disconnect("b")).await;
    ctl.state_until("b", PhoneState::Off).await;
    assert_eq!(
        refused(&ctl.sock, open_on("com.x", 0, "b")).await,
        Control::error(ipc::PAUSED)
    );
    assert_eq!(
        refused(&ctl.sock, open_on("com.x", 0, "typo")).await,
        Control::error(ipc::NOT_CONNECTED)
    );
    helper.assert_quiet().await;
    ctl.shutdown().await;
}

/// `open` without a phone keeps Plan 3's rules even with the connect prompt present: not on
/// this phone after a full sync, PAUSED after a Disconnect, and no prompt for either.
#[tokio::test]
async fn open_without_phone_keeps_plan3_rules() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let (root, dirs, catalog) = temp_catalog();
    plant_entry(&dirs, "com.x", "other");
    let mut ctl = start_ctl_with(port, &["dev"], None, |p| p.catalog = Some(catalog));
    helper.next_session().await;
    ctl.ready("dev").await;
    full_sync(&helper, &dirs).await;
    assert_eq!(
        refused(&ctl.sock, open("com.x", 0, None)).await,
        Control::error(ipc::NOT_ON_THIS_PHONE)
    );
    ctl.command(disconnect("dev")).await;
    expect_bye(&mut helper).await;
    ctl.state_until("dev", PhoneState::Off).await;
    assert_eq!(
        refused(&ctl.sock, open("com.y", 0, None)).await,
        Control::error(ipc::PAUSED)
    );
    ctl.assert_no_prompt().await;
    ctl.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

// --- several phones live at once (Plan 4c Task 5) ----------------------------------------------

/// Two fake helpers, `dev` ("Pixel") and `b`, both online; autoconnect on unless `opts` says
/// otherwise.
async fn two_live(opts: Opts) -> (Ctl, FakeHelper, FakeHelper) {
    let (mut dev, dev_port) = FakeHelper::spawn().await;
    let (mut b, b_port) = FakeHelper::spawn().await;
    let mut ctl = start_pair(&[("dev", dev_port), ("b", b_port)], opts);
    assert_eq!(dev.next_session().await, 0);
    assert_eq!(b.next_session().await, 0);
    ctl.ready("dev").await;
    ctl.ready("b").await;
    (ctl, dev, b)
}

/// Opens a NEW window on `phone` (open{phone}); its helper gets the Open with `phone` stripped
/// and answers Opened. Returns (client, win_id).
async fn open_window_on(
    sock: &Path,
    helper: &mut FakeHelper,
    pkg: &str,
    phone: &str,
) -> (UnixStream, u32) {
    let mut client = UnixStream::connect(sock).await.unwrap();
    send(&mut client, &open_on(pkg, 0, phone)).await;
    let fwd = helper.next().await;
    let win = fwd.stream;
    assert!(win > 0);
    assert!(
        matches!(decode(&fwd), Control::Open { pkg: ref got, phone: None, .. } if got == pkg),
        "{fwd:?}"
    );
    let opened = control(
        win,
        Control::Opened {
            display: 7,
            task: 9,
            codec: "h264".into(),
            w: 1280,
            h: 800,
            moved: false,
            label: None,
            work: false,
        },
    );
    helper.send(opened.clone());
    assert_eq!(recv(&mut client).await, Some(opened));
    (client, win)
}

/// A frame each way between a window and its own helper.
async fn routes_both_ways(client: &mut UnixStream, helper: &mut FakeHelper, win: u32, byte: u8) {
    let video = Frame {
        kind: Kind::Video,
        stream: win,
        payload: vec![byte; 8],
    };
    helper.send(video.clone());
    assert_eq!(recv(client).await, Some(video));
    let input = Frame {
        kind: Kind::Input,
        stream: win,
        payload: VALID_KEY.to_vec(),
    };
    send(client, &input).await;
    assert_eq!(helper.next().await, input);
}

/// The client is still open: nothing arrives, no EOF.
async fn assert_client_open(client: &mut UnixStream) {
    if let Ok(read) = timeout(Duration::from_millis(200), read_frame_async(client)).await {
        panic!("window client got {read:?}");
    }
}

#[tokio::test]
async fn two_phones_live_at_once() {
    let state = scratch_state();
    let (ctl, mut dev, mut b) = two_live(rules(&state, true)).await;
    assert_eq!(flags(&state, "dev"), (false, true));
    assert_eq!(flags(&state, "b"), (false, true));
    assert_no_session(&mut dev).await;
    assert_no_session(&mut b).await;
    dev.assert_quiet().await;
    b.assert_quiet().await;
    ctl.shutdown().await;
}

#[tokio::test]
async fn windows_of_two_phones_route_to_their_own_helper() {
    let (ctl, mut dev, mut b) = two_live(Opts::default()).await;
    let (mut on_dev, win_dev) = open_window_on(&ctl.sock, &mut dev, "com.x", "dev").await;
    let (mut on_b, win_b) = open_window_on(&ctl.sock, &mut b, "com.x", "b").await;
    assert_ne!(win_dev, win_b);
    routes_both_ways(&mut on_dev, &mut dev, win_dev, 1).await;
    routes_both_ways(&mut on_b, &mut b, win_b, 2).await;
    dev.assert_quiet().await;
    b.assert_quiet().await;
    let report = status(&ctl.sock).await;
    assert_eq!(
        report.windows,
        [
            WindowInfo {
                id: win_dev,
                pkg: "com.x".into(),
                user: 0,
                phone: Some("dev".into()),
            },
            WindowInfo {
                id: win_b,
                pkg: "com.x".into(),
                user: 0,
                phone: Some("b".into()),
            },
        ]
    );
    ctl.shutdown().await;
}

/// Review Focus 4: b's helper dies; b restarts after its backoff while dev's session, window
/// and view stay as they are.
#[tokio::test]
async fn helper_death_of_one_phone_keeps_the_other() {
    let (ctl, mut dev, mut b) = two_live(Opts::default()).await;
    let (mut on_dev, win_dev) = open_window_on(&ctl.sock, &mut dev, "com.x", "dev").await;
    let mut view = ctl.view.clone();
    view.borrow_and_update();
    b.die();
    let watched = timeout(STEP, async {
        let mut b_left = false;
        loop {
            view.changed().await.unwrap();
            let now = view.borrow_and_update().clone();
            let dev_state = now.phone("dev").unwrap().state;
            assert_eq!(dev_state, PhoneState::Ready, "dev changed: {now:?}");
            let b_state = now.phone("b").unwrap().state;
            if b_state != PhoneState::Ready {
                b_left = true;
            } else if b_left {
                return;
            }
        }
    });
    let ((), session) = tokio::join!(
        async { watched.await.expect("b never came back") },
        b.next_session()
    );
    assert_eq!(session, 1, "b got a new session");
    assert_client_open(&mut on_dev).await;
    dev.assert_quiet().await;
    routes_both_ways(&mut on_dev, &mut dev, win_dev, 3).await;
    assert_eq!(status(&ctl.sock).await.windows.len(), 1);
    ctl.shutdown().await;
}

#[tokio::test]
async fn offline_of_one_phone_keeps_the_other() {
    let (ctl, mut dev, mut b) = two_live(Opts::default()).await;
    let (mut on_dev, win_dev) = open_window_on(&ctl.sock, &mut dev, "com.x", "dev").await;
    let mut ctl = ctl;
    ctl.offline("b");
    expect_bye(&mut b).await;
    ctl.state_until("b", PhoneState::Reconnecting).await;
    routes_both_ways(&mut on_dev, &mut dev, win_dev, 4).await;
    assert_eq!(ctl.phone("dev").unwrap().state, PhoneState::Ready);
    dev.assert_quiet().await;
    ctl.shutdown().await;
}

#[tokio::test]
async fn disconnect_one_keeps_the_other() {
    let state = scratch_state();
    let (mut ctl, mut dev, mut b) = two_live(rules(&state, true)).await;
    let (mut on_dev, win_dev) = open_window_on(&ctl.sock, &mut dev, "com.x", "dev").await;
    ctl.command(disconnect("b")).await;
    expect_bye(&mut b).await;
    ctl.state_until("b", PhoneState::Off).await;
    assert_client_open(&mut on_dev).await;
    routes_both_ways(&mut on_dev, &mut dev, win_dev, 5).await;
    assert_eq!(ctl.phone("dev").unwrap().state, PhoneState::Ready);
    assert_eq!(flags(&state, "dev"), (false, true));
    assert_eq!(flags(&state, "b"), (true, false));
    assert_no_session(&mut b).await;
    dev.assert_quiet().await;
    ctl.shutdown().await;
}

#[tokio::test]
async fn undo_of_one_keeps_the_others_session_and_windows() {
    let (undo, mut calls) = instant_undo();
    let (mut ctl, mut dev, mut b) = two_live(Opts {
        undo: Some(undo),
        ..Default::default()
    })
    .await;
    let (mut on_dev, win_dev) = open_window_on(&ctl.sock, &mut dev, "com.x", "dev").await;
    ctl.command(undo_setup("b")).await;
    expect_bye(&mut b).await;
    assert_eq!(timeout(STEP, calls.recv()).await.unwrap().unwrap(), "b");
    ctl.state_until("b", PhoneState::Off).await;
    assert_client_open(&mut on_dev).await;
    routes_both_ways(&mut on_dev, &mut dev, win_dev, 6).await;
    assert_eq!(ctl.phone("dev").unwrap().state, PhoneState::Ready);
    dev.assert_quiet().await;
    ctl.shutdown().await;
}

/// An undo for `b` that waits for the returned semaphore.
fn gated_undo() -> (
    UndoRunner,
    Arc<tokio::sync::Semaphore>,
    mpsc::UnboundedReceiver<String>,
) {
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let gate = release.clone();
    let (calls_tx, calls) = mpsc::unbounded_channel();
    let undo: UndoRunner = Arc::new(move |serial: String| {
        let gate = gate.clone();
        let _ = calls_tx.send(serial);
        Box::pin(async move {
            gate.acquire().await.unwrap().forget();
            (true, "companion: uninstalled".to_owned())
        })
    });
    (undo, release, calls)
}

/// Decision 6: a Connect for the phone being undone is refused with a notice saying why.
#[tokio::test]
async fn connect_during_its_undo_says_so() {
    let (undo, release, _calls) = gated_undo();
    let (mut ctl, mut dev, mut b) = two_live(Opts {
        undo: Some(undo),
        ..Default::default()
    })
    .await;
    ctl.command(undo_setup("b")).await;
    expect_bye(&mut b).await;
    ctl.state_until("b", PhoneState::Undoing).await;
    ctl.command(connect("b")).await;
    let deadline = Instant::now() + STEP;
    while ctl.notes.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "no notice");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        ctl.notes.lock().unwrap()[0],
        (
            "Lantern is still being removed from b".to_owned(),
            "Connect it again when that's done.".to_owned()
        )
    );
    assert_no_session(&mut b).await;
    assert_eq!(ctl.phone("b").unwrap().state, PhoneState::Undoing);
    release.add_permits(1);
    ctl.state_until("b", PhoneState::Off).await;
    assert_no_session(&mut b).await;
    dev.assert_quiet().await;
    ctl.shutdown().await;
}

/// Decision 6: commands for another phone run while an undo runs.
#[tokio::test]
async fn commands_for_other_phones_run_during_an_undo() {
    let (undo, release, _calls) = gated_undo();
    let (mut ctl, mut dev, mut b) = two_live(Opts {
        undo: Some(undo),
        ..Default::default()
    })
    .await;
    ctl.command(undo_setup("b")).await;
    expect_bye(&mut b).await;
    ctl.state_until("b", PhoneState::Undoing).await;
    ctl.command(disconnect("dev")).await;
    expect_bye(&mut dev).await;
    ctl.state_until("dev", PhoneState::Off).await;
    ctl.command(connect("dev")).await;
    assert_eq!(dev.next_session().await, 1);
    ctl.ready("dev").await;
    assert_eq!(ctl.phone("b").unwrap().state, PhoneState::Undoing);
    release.add_permits(1);
    ctl.state_until("b", PhoneState::Off).await;
    ctl.shutdown().await;
}

/// Decision 6: one undo at a time; a second phone's Undo is refused with a notice and leaves
/// that phone live.
#[tokio::test]
async fn second_undo_is_refused_with_a_notice() {
    let (undo, release, mut calls) = gated_undo();
    let (mut ctl, mut dev, mut b) = two_live(Opts {
        undo: Some(undo),
        ..Default::default()
    })
    .await;
    ctl.command(undo_setup("b")).await;
    expect_bye(&mut b).await;
    ctl.state_until("b", PhoneState::Undoing).await;
    ctl.command(undo_setup("dev")).await;
    let deadline = Instant::now() + STEP;
    while ctl.notes.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "no notice");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        ctl.notes.lock().unwrap()[0],
        (
            "Lantern is busy removing itself from b".to_owned(),
            "Try Pixel again when that's done.".to_owned()
        )
    );
    dev.assert_quiet().await;
    assert_eq!(ctl.phone("dev").unwrap().state, PhoneState::Ready);
    assert!(!ctl.phone("dev").unwrap().off);
    release.add_permits(1);
    ctl.state_until("b", PhoneState::Off).await;
    assert_eq!(calls.recv().await.as_deref(), Some("b"));
    assert!(calls.try_recv().is_err(), "dev was never undone");
    ctl.shutdown().await;
}

/// Review Focus 5: Quit clears every live phone's `live` flag and closes every session.
#[tokio::test]
async fn quit_clears_live_of_every_phone() {
    let state = scratch_state();
    let (mut ctl, mut dev, mut b) = two_live(rules(&state, true)).await;
    ctl.command(Command::Quit).await;
    timeout(Duration::from_secs(5), &mut ctl.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(flags(&state, "dev"), (false, false));
    assert_eq!(flags(&state, "b"), (false, false));
    for helper in [&mut dev, &mut b] {
        assert_eq!(decode(&helper.next().await), Control::Bye);
        record_eof(helper).await;
    }
}

/// Review Focus 5: a stop signal closes every session and keeps every flag.
#[tokio::test]
async fn stop_ends_every_session_and_keeps_flags() {
    let state = scratch_state();
    let (ctl, mut dev, mut b) = two_live(rules(&state, true)).await;
    ctl.shutdown().await;
    for helper in [&mut dev, &mut b] {
        assert_eq!(decode(&helper.next().await), Control::Bye);
        record_eof(helper).await;
    }
    assert_eq!(flags(&state, "dev"), (false, true));
    assert_eq!(flags(&state, "b"), (false, true));
}

/// The helper read EOF on its connection.
async fn record_eof(helper: &FakeHelper) {
    let deadline = Instant::now() + STEP;
    while helper.eofs.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the helper never read EOF");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Review Focus 2 (G7): a phone copy reaches the PC, and through the PC's selection event the
/// other phone only; a PC copy reaches both.
#[tokio::test]
async fn phone_copy_reaches_pc_and_other_phone_only() {
    let (mut dev, dev_port) = FakeHelper::spawn().await;
    let (mut b, b_port) = FakeHelper::spawn().await;
    let (handle, pc_sets) = ClipHandle::test_pair();
    let (offers, offers_rx) = mpsc::channel(8);
    let mut ctl = start_ctl_ports(
        Arc::new(move |serial: &str| match serial {
            "dev" => Some(dev_port),
            "b" => Some(b_port),
            _ => None,
        }),
        &["dev", "b"],
        Opts::default(),
        |parts| {
            parts.clipboard = Some(PcClipboard {
                handle,
                offers: offers_rx,
            })
        },
    );
    dev.next_session().await;
    b.next_session().await;
    ctl.ready("dev").await;
    ctl.ready("b").await;
    dev.send(control(0, Control::Clip { text: "x".into() }));
    assert_eq!(next_pc_set(&pc_sets).await, "x");
    offers.send(pc_offer("x", false)).await.unwrap();
    assert_eq!(decode(&b.next().await), Control::Clip { text: "x".into() });
    dev.assert_quiet().await;
    offers.send(pc_offer("y", false)).await.unwrap();
    assert_eq!(
        decode(&dev.next().await),
        Control::Clip { text: "y".into() }
    );
    assert_eq!(decode(&b.next().await), Control::Clip { text: "y".into() });
    ctl.shutdown().await;
}

/// Decision 14: adb never sends its first device list; stale `live` flags are still cleared
/// after the grace plus `list_wait`.
#[tokio::test]
async fn stale_live_cleared_even_if_adb_never_lists() {
    let (_helper, port) = FakeHelper::spawn().await;
    let state = scratch_state();
    companion::set_live(&state, "dev", true).unwrap();
    let mut opts = rules(&state, false);
    opts.timing = Some(Timing {
        unplug_grace: Duration::from_millis(300),
        list_wait: Duration::from_millis(300),
        ..fast_timing()
    });
    opts.unlisted = true;
    let started = Instant::now();
    let ctl = start_ctl_opts(port, &[], opts, |_| {});
    let deadline = started + Duration::from_secs(1);
    while flags(&state, "dev").1 {
        assert!(Instant::now() < deadline, "live never cleared");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        started.elapsed() >= Duration::from_millis(550),
        "cleared too early"
    );
    ctl.shutdown().await;
}

/// Decision 2: the top level of `lantern status` is the phone that got ready last.
#[tokio::test]
async fn status_top_level_is_the_latest_ready_phone() {
    let (mut dev, dev_port) = FakeHelper::spawn().await;
    let (mut b, b_port) = FakeHelper::spawn().await;
    let mut ctl = start_pair(&[("dev", dev_port), ("b", b_port)], manual());
    ctl.command(connect("dev")).await;
    dev.next_session().await;
    ctl.ready("dev").await;
    assert_eq!(status(&ctl.sock).await.serial.as_deref(), Some("dev"));
    ctl.command(connect("b")).await;
    b.next_session().await;
    ctl.ready("b").await;
    let report = status_until(&ctl.sock, |r| r.serial.as_deref() == Some("b")).await;
    assert_eq!(report.state, SessionState::Ready);
    ctl.command(disconnect("b")).await;
    expect_bye(&mut b).await;
    let report = status_until(&ctl.sock, |r| r.serial.as_deref() == Some("dev")).await;
    assert_eq!(report.state, SessionState::Ready);
    ctl.shutdown().await;
}

/// A fake adb whose `forward --remove` takes 600 ms and then appends `removed` to `order`; the
/// test's starter appends `start` there too. Every other call succeeds at once.
#[derive(Clone)]
struct SlowAdb {
    program: PathBuf,
    order: PathBuf,
}

impl SlowAdb {
    fn new() -> Self {
        let n = DIRS.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("lantern-ipc-slow-adb-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("adb");
        let tmp = dir.join("adb.tmp");
        std::fs::write(
            &tmp,
            "#!/bin/sh\ncase \"$*\" in\n  *'forward --remove'*) sleep 0.6; echo removed >> \"$(dirname \"$0\")/order\" ;;\nesac\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, &program).unwrap();
        // A fork in another test's thread while the script was open for writing can hold its
        // write fd until that child execs (ETXTBSY): run it once until it execs cleanly.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match std::process::Command::new(&program).arg("probe").status() {
                Ok(status) => {
                    assert!(status.success());
                    break;
                }
                Err(error) if error.raw_os_error() == Some(26) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "fake adb busy: {error}"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fake adb: {error}"),
            }
        }
        Self {
            order: dir.join("order"),
            program,
        }
    }

    fn note(&self, what: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.order)
            .unwrap();
        writeln!(file, "{what}").unwrap();
    }

    fn order(&self) -> Vec<String> {
        std::fs::read_to_string(&self.order)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for SlowAdb {
    fn drop(&mut self) {
        if let Some(dir) = self.order.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// R8 (1): a phone's restart waits for its own previous session's close, so that close can't
/// remove the forward (same port) the new session made. b's helper dies; its close removes
/// the forward slowly (600 ms) while the retry (50 ms) is due long before.
#[tokio::test]
async fn restart_waits_for_its_own_close() {
    let (mut dev, dev_port) = FakeHelper::spawn().await;
    let (mut b, b_port) = FakeHelper::spawn().await;
    let adb = SlowAdb::new();
    let ports: std::collections::HashMap<String, u16> =
        [("dev".to_owned(), dev_port), ("b".to_owned(), b_port)].into();
    let mut ctl = start_ctl_ports(
        Arc::new(move |serial: &str| ports.get(serial).copied()),
        &["dev", "b"],
        Opts {
            slow_close: Some(("b".into(), adb.clone())),
            ..Default::default()
        },
        |_| {},
    );
    assert_eq!(dev.next_session().await, 0);
    assert_eq!(b.next_session().await, 0);
    ctl.ready("dev").await;
    ctl.ready("b").await;
    b.die();
    assert_eq!(b.next_session().await, 1);
    ctl.ready("b").await;
    assert_eq!(adb.order(), ["start", "removed", "start"]);
    dev.assert_quiet().await;
    assert_eq!(ctl.phone("dev").unwrap().state, PhoneState::Ready);
    ctl.shutdown().await;
}

/// R8 (2): b blips (back within its grace) while dev is live: b restarts at once with no
/// prompt, and dev is never touched.
#[tokio::test]
async fn blip_of_one_phone_restarts_it_and_keeps_the_other() {
    let (mut ctl, mut dev, mut b) = two_live(Opts {
        timing: Some(fast_timing()),
        ..Default::default()
    })
    .await;
    let (mut on_dev, win_dev) = open_window_on(&ctl.sock, &mut dev, "com.x", "dev").await;
    let mut view = ctl.view.clone();
    view.borrow_and_update();
    let watch_dev = tokio::spawn(async move {
        while view.changed().await.is_ok() {
            let now = view.borrow_and_update().clone();
            assert_eq!(
                now.phone("dev").unwrap().state,
                PhoneState::Ready,
                "dev changed: {now:?}"
            );
        }
    });
    ctl.offline("b");
    expect_bye(&mut b).await;
    ctl.state_until("b", PhoneState::Reconnecting).await;
    ctl.online("b");
    assert_eq!(b.next_session().await, 1);
    ctl.ready("b").await;
    ctl.assert_no_prompt().await;
    assert_client_open(&mut on_dev).await;
    dev.assert_quiet().await;
    routes_both_ways(&mut on_dev, &mut dev, win_dev, 7).await;
    assert!(!watch_dev.is_finished(), "dev's watcher failed");
    watch_dev.abort();
    ctl.shutdown().await;
}

/// A helper that answers the hello and then never reads another byte: its queue fills.
struct DeafHelper {
    sessions: mpsc::UnboundedReceiver<usize>,
}

impl DeafHelper {
    async fn spawn() -> (Self, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sessions_tx, sessions) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut held = Vec::new();
            for n in 0.. {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let hello = read_frame_async(&mut socket).await.unwrap().unwrap();
                assert!(matches!(decode(&hello), Control::Hello { .. }));
                let reply = control(
                    0,
                    Control::Hello {
                        version: 1,
                        token: String::new(),
                        keyboard: true,
                        screen_off: true,
                    },
                );
                write_frame_async(&mut socket, &reply).await.unwrap();
                let _ = sessions_tx.send(n);
                held.push(socket);
            }
        });
        (Self { sessions }, port)
    }
}

/// R8 (3): b's helper stops reading, so the PC clipboard fan-out can't reach it: only b's
/// session ends (and restarts); dev stays Ready and gets nothing but the clips.
#[tokio::test]
async fn wedged_helper_ends_only_its_own_session() {
    let (mut dev, dev_port) = FakeHelper::spawn().await;
    let (mut b, b_port) = DeafHelper::spawn().await;
    let (handle, _pc_sets) = ClipHandle::test_pair();
    let (offers, offers_rx) = mpsc::channel(8);
    let mut ctl = start_ctl_ports(
        Arc::new(move |serial: &str| match serial {
            "dev" => Some(dev_port),
            "b" => Some(b_port),
            _ => None,
        }),
        &["dev", "b"],
        Opts::default(),
        |parts| {
            parts.clipboard = Some(PcClipboard {
                handle,
                offers: offers_rx,
            })
        },
    );
    assert_eq!(dev.next_session().await, 0);
    assert_eq!(timeout(STEP, b.sessions.recv()).await.unwrap(), Some(0));
    ctl.ready("dev").await;
    ctl.ready("b").await;
    let mut view = ctl.view.clone();
    view.borrow_and_update();
    let watch_dev = tokio::spawn(async move {
        while view.changed().await.is_ok() {
            let now = view.borrow_and_update().clone();
            assert_eq!(
                now.phone("dev").unwrap().state,
                PhoneState::Ready,
                "dev changed: {now:?}"
            );
        }
    });
    let big = "x".repeat(256 * 1024);
    let mut sent = 0;
    for n in 0..400 {
        if ctl.phone("b").unwrap().state != PhoneState::Ready {
            break;
        }
        let offer = pc_offer(&format!("{n:04}{big}"), false);
        timeout(Duration::from_secs(5), offers.send(offer))
            .await
            .expect("the daemon stopped taking PC offers")
            .unwrap();
        sent += 1;
    }
    // b's session ended (helper stopped reading) and a new one starts. Its close and the
    // retry delay come first: 10 s keeps a loaded machine from failing it.
    assert_eq!(
        timeout(Duration::from_secs(10), b.sessions.recv())
            .await
            .expect("b never restarted"),
        Some(1)
    );
    ctl.ready("b").await;
    // dev got every clip and nothing else, on its first session.
    for _ in 0..sent {
        assert!(matches!(decode(&dev.next().await), Control::Clip { .. }));
    }
    dev.assert_quiet().await;
    assert_eq!(dev.eofs.load(Ordering::SeqCst), 0);
    assert!(
        dev.sessions.try_recv().is_err(),
        "dev must keep its session"
    );
    assert!(!watch_dev.is_finished(), "dev's watcher failed");
    watch_dev.abort();
    ctl.shutdown().await;
}
