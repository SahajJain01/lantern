//! Daemon <-> window IPC, against an in-process daemon (`serve`) whose phone session is a fake
//! helper on a local TCP socket. No phone and no adb are involved.

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use lantern::adb::{Adb, DeviceEvent};
use lantern::daemon::ipc;
use lantern::daemon::session::{handshake, SessionConn};
use lantern::daemon::{serve, RetryPolicy};
use lantern_proto::{read_frame_async, write_frame_async, CloseReason, Control, Frame, Kind};
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
}

impl FakeHelper {
    async fn spawn() -> (Self, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (frames_tx, from_daemon) = mpsc::unbounded_channel();
        let (cmds, mut cmd_rx) = mpsc::unbounded_channel::<HelperCmd>();
        let (sessions_tx, sessions) = mpsc::unbounded_channel();
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
                    },
                );
                write_frame_async(&mut wr, &reply).await.unwrap();
                let _ = sessions_tx.send(n);
                n += 1;
                let frames_tx = frames_tx.clone();
                let reader = tokio::spawn(async move {
                    while let Ok(Some(frame)) = read_frame_async(&mut rd).await {
                        let _ = frames_tx.send(frame);
                    }
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
        let stream = handshake(stream, TOKEN).await?;
        Ok(SessionConn::attach(stream, Adb::new(serial), "sha"))
    };
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
            Some(Control::Error { ref msg }) if msg == "phone not connected"
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
                Ok(Ok(Some(f))) if matches!(decode(&f), Control::Error { ref msg } if msg == "phone not connected") =>
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
        },
    );
    helper.send(opened.clone());
    assert_eq!(recv(&mut client).await, Some(opened));
    (client, win)
}

// ---------------------------------------------------------------------------------------------

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
    assert_eq!(
        decode(&reply),
        Control::Error {
            msg: "phone not connected".into()
        }
    );
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
