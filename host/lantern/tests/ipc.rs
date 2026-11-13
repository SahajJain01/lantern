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
        let stream = handshake(stream, TOKEN, true).await?;
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
            activation_token: None
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
async fn session_end_eofs_clients_and_resets_registry() {
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
    assert_eq!(again, 1, "the new session starts a fresh registry");
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
                user: 0
            },
            WindowInfo {
                id: win2,
                pkg: "com.two".into(),
                user: 10
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

/// Plants the menu entry of `pkg` (user 0) as written by phone `serial`'s sync.
fn plant_entry(dirs: &Dirs, pkg: &str, serial: &str) {
    use lantern::daemon::catalog::{desktop_file, AppEntry};
    let e = AppEntry {
        pkg: pkg.into(),
        user: 0,
        label: pkg.into(),
        work: false,
        icon_png: Vec::new(),
    };
    std::fs::create_dir_all(&dirs.applications).unwrap();
    let text = desktop_file(&e, serial, &[serial.to_owned()], &dirs.exe, None);
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
    assert!(x.contains("X-Lantern-Serials=other;\n"), "{x}");

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

use lantern::daemon::control::{Command, Phone, UndoRunner, View};
use lantern_proto::DaemonCmd;
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
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}

/// Every session goes to the fake helper on `port`, whichever serial it is for. The phone
/// named "Pixel" is `dev`; other serials have no name.
fn start_ctl(port: u16, online: &[&str], undo: Option<UndoRunner>) -> Ctl {
    let rt = runtime_dir();
    let listener = ipc::bind(&rt).unwrap();
    let sock = ipc::socket_path_in(&rt);
    let (events, events_rx) = mpsc::unbounded_channel();
    for serial in online {
        events.send(DeviceEvent::Online((*serial).into())).unwrap();
    }
    let events_rx = futures_util::stream::unfold(events_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    });
    let (cmds, cmds_rx) = mpsc::channel(8);
    let (view_tx, view) = watch::channel(View {
        phone: Phone::None,
        name: None,
        locked: false,
    });
    let lookups = Arc::new(AtomicUsize::new(0));
    let counted = lookups.clone();
    let notes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let noted = notes.clone();
    let parts = Parts {
        commands: Some(cmds_rx),
        view: Some(view_tx),
        names: Some(Arc::new(move |serial: &str| {
            counted.fetch_add(1, Ordering::SeqCst);
            (serial == "dev").then(|| "Pixel".to_owned())
        })),
        undo,
        notify: Some(Arc::new(move |summary: &str, body: &str| {
            noted
                .lock()
                .unwrap()
                .push((summary.to_owned(), body.to_owned()));
        })),
        ..Default::default()
    };
    let starter = move |serial: String| async move {
        let stream = TcpStream::connect(("127.0.0.1", port)).await?;
        let stream = handshake(stream, TOKEN, true).await?;
        Ok(SessionConn::attach(stream, Adb::new(serial), "sha"))
    };
    let (stop, stop_rx) = oneshot::channel::<()>();
    let task = tokio::spawn(serve_with(
        listener,
        parts,
        None,
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
        stop: Some(stop),
        task,
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

    /// Waits until the view's phone matches.
    async fn view_until(&mut self, ok: impl Fn(&Phone) -> bool) -> View {
        let matched = timeout(STEP, self.view.wait_for(|v| ok(&v.phone)))
            .await
            .map(|view| view.unwrap().clone());
        matched.unwrap_or_else(|_| panic!("view never matched: {:?}", *self.view.borrow()))
    }

    async fn ready(&mut self, serial: &str) -> View {
        self.view_until(|p| matches!(p, Phone::Ready { serial: s, .. } if s == serial))
            .await
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

fn paused(serial: &str) -> Phone {
    Phone::Paused {
        serial: serial.into(),
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

#[tokio::test]
async fn disconnect_pauses_until_replug() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut ctl = start_ctl(port, &["dev"], None);
    assert_eq!(helper.next_session().await, 0);
    let ready = ctl.ready("dev").await;
    assert_eq!(ready.name.as_deref(), Some("Pixel"));

    ctl.command(Command::Disconnect).await;
    expect_bye(&mut helper).await;
    let view = ctl.view_until(|p| *p == paused("dev")).await;
    assert_eq!(view.name.as_deref(), Some("Pixel"));
    let report = status(&ctl.sock).await;
    assert_eq!(
        (report.state, report.serial.as_deref()),
        (SessionState::Paused, Some("dev"))
    );
    assert_no_session(&mut helper).await;
    assert_eq!(status(&ctl.sock).await.state, SessionState::Paused);

    // Unplug and replug: served again.
    ctl.offline("dev");
    ctl.view_until(|p| *p == Phone::None).await;
    ctl.online("dev");
    assert_eq!(helper.next_session().await, 1);
    ctl.ready("dev").await;
    // The name is cached, not read from the device record on every loop turn.
    for _ in 0..20 {
        status(&ctl.sock).await;
    }
    let lookups = ctl.lookups.load(Ordering::SeqCst);
    assert!(lookups <= 6, "names looked up {lookups} times");
    ctl.shutdown().await;
}

#[tokio::test]
async fn online_while_paused_starts_nothing() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut ctl = start_ctl(port, &["dev"], None);
    helper.next_session().await;
    ctl.ready("dev").await;
    ctl.command(Command::Disconnect).await;
    expect_bye(&mut helper).await;
    ctl.view_until(|p| *p == paused("dev")).await;
    // adb reports the phone again (e.g. track-devices reconnected) without an Offline.
    ctl.online("dev");
    assert_no_session(&mut helper).await;
    assert_eq!(
        *ctl.view.borrow(),
        View {
            phone: paused("dev"),
            name: Some("Pixel".into()),
            locked: false
        }
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
    ctl.command(Command::Disconnect).await;
    expect_bye(&mut helper).await;
    ctl.view_until(|p| *p == paused("dev")).await;
    ctl.command(Command::Connect).await;
    assert_eq!(helper.next_session().await, 1);
    ctl.ready("dev").await;
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

    ctl.command(Command::UndoSetup).await;
    assert_eq!(decode(&helper.next().await), Control::Bye);
    ctl.view_until(|p| matches!(p, Phone::Undoing { serial } if serial == "dev"))
        .await;
    let (serial, closed) = timeout(STEP, calls.recv()).await.unwrap().unwrap();
    assert_eq!(serial, "dev");
    assert!(closed, "undo ran while the helper connection was open");
    let report = status(&ctl.sock).await;
    assert_eq!(report.state, SessionState::Paused, "{report:?}");
    helper.die();

    // Ignored while undoing.
    ctl.command(Command::Connect).await;
    ctl.command(Command::Disconnect).await;
    ctl.command(Command::UndoSetup).await;
    assert_no_session(&mut helper).await;
    assert!(matches!(ctl.view.borrow().phone, Phone::Undoing { .. }));

    release.add_permits(1);
    ctl.view_until(|p| *p == paused("dev")).await;
    assert_no_session(&mut helper).await;
    assert!(calls.try_recv().is_err(), "undo ran once");
    assert_eq!(
        *ctl.notes.lock().unwrap(),
        [(
            "Lantern removed itself from Pixel".to_owned(),
            "helper --forget: forgot 1\ncompanion: uninstalled".to_owned()
        )]
    );
    // Connect afterwards sets the phone up again.
    ctl.command(Command::Connect).await;
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
    ctl.command(Command::UndoSetup).await;
    ctl.view_until(|p| matches!(p, Phone::Undoing { .. })).await;
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
    ctl.command(Command::Disconnect).await;
    expect_bye(&mut helper).await;
    ctl.view_until(|p| *p == paused("dev")).await;
    let mut client = UnixStream::connect(&ctl.sock).await.unwrap();
    send(&mut client, &open("com.x", 0, None)).await;
    let reply = recv(&mut client).await.expect("an error reply");
    assert_eq!(decode(&reply), Control::error(ipc::PAUSED));
    assert_eq!(ipc::PAUSED, "phone disconnected in Lantern");
    assert_eof(&mut client).await;
    ctl.shutdown().await;
}

/// `lantern connect|disconnect` over the socket.
async fn daemon_cmd(sock: &Path, cmd: DaemonCmd) -> StatusReport {
    let mut client = UnixStream::connect(sock).await.unwrap();
    send(&mut client, &control(0, Control::Daemon { cmd })).await;
    let reply = recv(&mut client).await.expect("a status report");
    let Control::StatusReport(report) = decode(&reply) else {
        panic!("expected status_report, got {:?}", decode(&reply));
    };
    assert_eof(&mut client).await;
    report
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
    ctl.view_until(|p| *p == paused("dev")).await;
    let report = status_until(&ctl.sock, |r| r.state == SessionState::Paused).await;
    assert_eq!(report.serial.as_deref(), Some("dev"));
    // The blocking client `lantern connect` uses.
    let sock = ctl.sock.clone();
    let reply =
        tokio::task::spawn_blocking(move || ipc::send_daemon_cmd(&sock, DaemonCmd::Connect))
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
            },
        ),
    )
    .await;
    assert_eof(&mut client).await;
    assert_eq!(decode(&helper.next().await), Control::Close);
    assert_eq!(status(&ctl.sock).await.state, SessionState::Ready);
    ctl.shutdown().await;
}

/// R4: the live phone goes away while another is online: the next one is served at once.
#[tokio::test]
async fn offline_of_live_phone_starts_the_next_online_phone() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut ctl = start_ctl(port, &["a", "b"], None);
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("a").await;
    ctl.offline("a");
    expect_bye(&mut helper).await;
    assert_eq!(helper.next_session().await, 1);
    let view = ctl.ready("b").await;
    assert_eq!(view.name, None, "b has no recorded name");
    ctl.shutdown().await;
}

/// A paused phone is never the next target: the next online phone that is not paused is.
#[tokio::test]
async fn offline_of_live_phone_skips_a_paused_phone() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut ctl = start_ctl(port, &["a"], None);
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("a").await;
    ctl.command(Command::Disconnect).await;
    expect_bye(&mut helper).await;
    ctl.view_until(|p| *p == paused("a")).await;
    // b comes online: a is paused, so b is served.
    ctl.online("b");
    assert_eq!(helper.next_session().await, 1);
    ctl.ready("b").await;
    ctl.online("c");
    ctl.offline("b");
    expect_bye(&mut helper).await;
    assert_eq!(helper.next_session().await, 2);
    ctl.ready("c").await;
    let report = status(&ctl.sock).await;
    assert_eq!(report.serial.as_deref(), Some("c"));
    ctl.shutdown().await;
}

/// Every disconnected phone stays disconnected: pausing a second phone does not un-pause the
/// first, so unplugging the second starts nothing.
#[tokio::test]
async fn second_disconnect_keeps_the_first_phone_paused() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut ctl = start_ctl(port, &["a"], None);
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("a").await;
    ctl.command(Command::Disconnect).await;
    expect_bye(&mut helper).await;
    ctl.view_until(|p| *p == paused("a")).await;
    ctl.online("b");
    assert_eq!(helper.next_session().await, 1);
    ctl.ready("b").await;
    ctl.command(Command::Disconnect).await;
    expect_bye(&mut helper).await;
    // The most recent disconnected phone is shown.
    ctl.view_until(|p| *p == paused("b")).await;
    ctl.offline("b");
    ctl.view_until(|p| *p == paused("a")).await;
    assert_no_session(&mut helper).await;
    assert_eq!(ctl.view.borrow().phone, paused("a"));
    let report = status(&ctl.sock).await;
    assert_eq!(
        (report.state, report.serial.as_deref()),
        (SessionState::Paused, Some("a"))
    );
    // Connect resumes the phone shown.
    ctl.command(Command::Connect).await;
    assert_eq!(helper.next_session().await, 2);
    ctl.ready("a").await;
    ctl.shutdown().await;
}

/// Disconnect with another phone online: that phone is served at once.
#[tokio::test]
async fn disconnect_starts_the_other_online_phone() {
    let (mut helper, port) = FakeHelper::spawn().await;
    let mut ctl = start_ctl(port, &["a", "b"], None);
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("a").await;
    ctl.command(Command::Disconnect).await;
    expect_bye(&mut helper).await;
    assert_eq!(helper.next_session().await, 1);
    ctl.ready("b").await;
    let report = status(&ctl.sock).await;
    assert_eq!(report.serial.as_deref(), Some("b"));
    ctl.shutdown().await;
}

/// When an undo ends with another phone online, that phone is served; the undone one stays
/// paused.
#[tokio::test]
async fn undo_end_starts_the_other_online_phone() {
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
    let mut ctl = start_ctl(port, &["a", "b"], Some(undo));
    assert_eq!(helper.next_session().await, 0);
    ctl.ready("a").await;
    ctl.command(Command::UndoSetup).await;
    expect_bye(&mut helper).await;
    ctl.view_until(|p| matches!(p, Phone::Undoing { serial } if serial == "a"))
        .await;
    // Nothing starts while the undo runs.
    assert_no_session(&mut helper).await;
    release.add_permits(1);
    assert_eq!(helper.next_session().await, 1);
    ctl.ready("b").await;
    // b goes away: a is still paused, so nothing starts.
    ctl.offline("b");
    expect_bye(&mut helper).await;
    ctl.view_until(|p| *p == paused("a")).await;
    assert_no_session(&mut helper).await;
    ctl.shutdown().await;
}

// --- the PIN lock: locked/unlocked, unlock{pin}, opens the lock blocked ---------------------

use lantern::unlock::{LockEvent, Pin};
use lantern_proto::LockReason;

/// A daemon with a view, a PIN channel and lock events, on the fake helper.
struct Locky {
    daemon: Daemon,
    view: watch::Receiver<View>,
    pins: mpsc::Sender<Pin>,
    events: mpsc::Receiver<LockEvent>,
}

async fn start_locky(port: u16) -> Locky {
    let (view_tx, mut view) = watch::channel(View {
        phone: Phone::None,
        name: None,
        locked: false,
    });
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
    timeout(
        STEP,
        view.wait_for(|v| matches!(v.phone, Phone::Ready { .. })),
    )
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
        timeout(STEP, self.view.wait_for(|v| v.locked == locked))
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
    let locky = start_locky(port).await;
    locky
        .pins
        .send(Pin::parse("24680\n".into()).unwrap())
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
    pins.send(Pin::parse("1357".into()).unwrap()).await.unwrap();
    let deadline = Instant::now() + STEP;
    while notes.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "no notice");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let notes = notes.lock().unwrap().clone();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].0, "Your phone isn't connected");
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
        .send(Pin::parse(format!("{DIGITS}\n")).unwrap())
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
    assert!(!locky.view.borrow().locked);
    helper.send(control(
        0,
        Control::Locked {
            reason: LockReason::Clipboard,
        },
    ));
    locky.locked_until(true).await;
    assert_eq!(
        locky.event().await,
        LockEvent::Locked(LockReason::Clipboard)
    );
    helper.send(control(0, Control::Unlocked));
    locky.locked_until(false).await;
    assert_eq!(locky.event().await, LockEvent::Unlocked);

    // A successful unlock_result also ends it; a failed one does not.
    helper.send(control(
        0,
        Control::Locked {
            reason: LockReason::Open,
        },
    ));
    locky.locked_until(true).await;
    assert_eq!(locky.event().await, LockEvent::Locked(LockReason::Open));
    let failed = LockEvent::Result {
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
    assert!(locky.view.borrow().locked);
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
        LockEvent::Locked(LockReason::Clipboard)
    );
    helper.die();
    locky.locked_until(false).await;
    assert_eq!(locky.event().await, LockEvent::Unlocked);
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
    assert_eq!(locky.event().await, LockEvent::Locked(LockReason::Open));

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
    assert_eq!(locky.event().await, LockEvent::Unlocked);
    assert_eq!(
        locky.event().await,
        LockEvent::Reopen(vec![("com.foo".into(), 0), ("com.bar".into(), 10)])
    );

    // Unlocked now: a failed open keeps the phone's own error and is not reopened.
    let mut client = open_blocked("com.baz", 0).await;
    let win = helper.next().await.stream;
    let error = control(win, Control::error("no launch intent for com.baz"));
    helper.send(error.clone());
    assert_eq!(recv(&mut client).await, Some(error));
    helper.send(control(0, Control::Unlocked));
    assert_eq!(locky.event().await, LockEvent::Unlocked);
    assert!(
        timeout(Duration::from_millis(300), locky.events.recv())
            .await
            .is_err(),
        "nothing to reopen"
    );
    locky.daemon.shutdown().await;
}
