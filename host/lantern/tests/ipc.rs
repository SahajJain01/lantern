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
use lantern_proto::{
    read_frame_async, write_frame_async, CloseReason, Control, Frame, Kind, SessionState,
    StatusReport, WindowInfo,
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
            // The optional flag passes through the daemon unchanged.
            moved: true,
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
