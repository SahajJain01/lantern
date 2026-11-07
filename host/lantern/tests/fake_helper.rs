use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::anyhow;
use futures_util::stream::{self, StreamExt};
use lantern::adb::{Adb, DeviceEvent};
use lantern::daemon::session::{handshake, HandshakeError, SessionConn};
use lantern::daemon::{run_loop, RetryPolicy};
use lantern_proto::{read_frame_async, write_frame_async, Control, Frame, Kind};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{timeout, Instant};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

/// Reads the daemon's Hello (asserting it speaks v1 with `token`) and answers with `reply`.
async fn answer_hello(socket: &mut TcpStream, token: &str, reply: Control) {
    let hello = read_frame_async(socket).await.unwrap().unwrap();
    assert_eq!(hello.kind, Kind::Control);
    assert_eq!(hello.stream, 0);
    match Control::decode(&hello.payload).unwrap() {
        Control::Hello {
            version,
            token: got,
        } => {
            assert_eq!(version, 1, "daemon must always send v1");
            assert_eq!(got, token);
        }
        other => panic!("expected Hello, got {other:?}"),
    }
    write_frame_async(
        socket,
        &Frame {
            kind: Kind::Control,
            stream: 0,
            payload: reply.encode(),
        },
    )
    .await
    .unwrap();
}

fn hello(version: u32, token: &str) -> Control {
    Control::Hello {
        version,
        token: token.to_owned(),
    }
}

async fn fake_helper(reply: Control) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        answer_hello(&mut socket, TOKEN, reply).await;
    });
    (port, task)
}

async fn fake_handshake_error(reply: Control) -> String {
    let (port, helper) = fake_helper(reply).await;
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let error = handshake(stream, TOKEN).await.unwrap_err().to_string();
    helper.await.unwrap();
    error
}

#[tokio::test]
async fn session_handshake_against_fake_helper() {
    let (port, helper) = fake_helper(hello(1, "")).await;
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    handshake(stream, TOKEN).await.unwrap();
    helper.await.unwrap();
}

#[tokio::test]
async fn session_handshake_rejects_wrong_version() {
    // The fake answers with a v2 Hello, so this exercises the daemon's own version check.
    let error = fake_handshake_error(hello(2, "")).await;
    assert_eq!(
        error,
        "helper speaks protocol v2, lantern expects v1; rebuild/redeploy helper"
    );
}

#[tokio::test]
async fn session_handshake_rejects_token_in_echo() {
    let error = fake_handshake_error(hello(1, "x")).await;
    assert!(
        error.contains("Hello echo carried a token"),
        "unexpected error: {error}"
    );
    assert!(!error.contains("version"), "unexpected error: {error}");
}

async fn handshake_error(reply: Control) -> anyhow::Error {
    let (port, helper) = fake_helper(reply).await;
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let error = handshake(stream, TOKEN).await.unwrap_err();
    helper.await.unwrap();
    error
}

/// Protocol failures are typed so the daemon can tell them apart from transient start failures.
#[tokio::test]
async fn handshake_protocol_failures_are_typed() {
    let version = handshake_error(hello(2, "")).await;
    assert!(matches!(
        version.downcast_ref::<HandshakeError>(),
        Some(HandshakeError::VersionMismatch {
            helper: 2,
            expected: 1
        })
    ));
    let echo = handshake_error(hello(1, "x")).await;
    assert!(matches!(
        echo.downcast_ref::<HandshakeError>(),
        Some(HandshakeError::TokenEcho)
    ));
    let rejected = handshake_error(Control::Error {
        msg: "bad token".into(),
    })
    .await;
    assert!(rejected.downcast_ref::<HandshakeError>().is_none());
}

/// Runs `run_loop` with a starter whose every helper answers `reply`; returns the loop's result
/// and how many starts it made. Shutdown never fires, so only a fatal error ends the loop.
async fn run_loop_against(reply: Control) -> (anyhow::Result<()>, usize) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            answer_hello(&mut socket, TOKEN, reply.clone()).await;
        }
    });
    let starts = Arc::new(AtomicUsize::new(0));
    let starter = {
        let starts = starts.clone();
        move |serial: String| {
            starts.fetch_add(1, Ordering::SeqCst);
            async move {
                let stream = TcpStream::connect(("127.0.0.1", port)).await?;
                // Extra context on top, as callers may add: the type must survive it.
                let stream = handshake(stream, TOKEN)
                    .await
                    .map_err(|error| error.context("start session"))?;
                Ok(SessionConn::attach(stream, Adb::new(serial), "sha"))
            }
        }
    };
    let result = timeout(
        Duration::from_secs(2),
        run_loop(
            None,
            online_then_idle("dev"),
            starter,
            std::future::pending(),
            fast_policy(),
        ),
    )
    .await
    .expect("run_loop kept retrying a handshake protocol failure");
    // Longer than several fast_policy backoffs: a stray retry would be counted here.
    tokio::time::sleep(Duration::from_millis(300)).await;
    (result, starts.load(Ordering::SeqCst))
}

#[tokio::test]
async fn daemon_exits_on_version_mismatch_without_retry() {
    let (result, starts) = run_loop_against(hello(2, "")).await;
    let error = format!("{:#}", result.expect_err("version mismatch must be fatal"));
    assert!(
        error.contains("helper speaks protocol v2, lantern expects v1; rebuild/redeploy helper"),
        "unexpected error: {error}"
    );
    assert_eq!(starts, 1, "version mismatch was retried");
}

#[tokio::test]
async fn daemon_exits_on_token_echo_without_retry() {
    let (result, starts) = run_loop_against(hello(1, "x")).await;
    let error = format!("{:#}", result.expect_err("token echo must be fatal"));
    assert!(
        error.contains("Hello echo carried a token"),
        "unexpected error: {error}"
    );
    assert_eq!(starts, 1, "token-echo refusal was retried");
}

#[tokio::test]
async fn session_handshake_reports_helper_error() {
    let error = fake_handshake_error(Control::Error {
        msg: "bad token".into(),
    })
    .await;
    assert_eq!(error, "helper rejected handshake: bad token");
}

/// A fake helper that handshakes and then dies (drops its socket) mid-session.
#[tokio::test]
async fn session_notices_helper_death() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (die_tx, die_rx) = oneshot::channel::<()>();
    let helper = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        answer_hello(&mut socket, TOKEN, hello(1, "")).await;
        die_rx.await.unwrap();
        drop(socket);
    });
    let stream = handshake(
        TcpStream::connect(("127.0.0.1", port)).await.unwrap(),
        TOKEN,
    )
    .await
    .unwrap();
    let mut conn = SessionConn::attach(stream, Adb::new("fake"), "sha");

    // Healthy session: not closed.
    assert!(timeout(Duration::from_millis(200), conn.closed())
        .await
        .is_err());
    die_tx.send(()).unwrap();
    let why = timeout(Duration::from_secs(2), conn.closed())
        .await
        .expect("daemon did not notice the helper dying");
    assert_eq!(why, "helper closed the connection");
    helper.await.unwrap();
    conn.close().await;
}

fn fast_policy() -> RetryPolicy {
    RetryPolicy {
        initial: Duration::from_millis(50),
        max: Duration::from_millis(200),
        stable_after: Duration::from_secs(60),
    }
}

fn online_then_idle(serial: &str) -> impl futures_core::Stream<Item = DeviceEvent> {
    stream::iter([DeviceEvent::Online(serial.into())]).chain(stream::pending())
}

/// Fake helper `n` of a series: every accepted connection is handshaken; connection 0 dies right
/// away, later ones stay up and report whether they received the daemon's Bye.
async fn helper_series(
    listener: TcpListener,
    accepted: mpsc::Sender<usize>,
    bye: mpsc::Sender<usize>,
) {
    let mut n = 0;
    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        answer_hello(&mut socket, TOKEN, hello(1, "")).await;
        accepted.send(n).await.unwrap();
        if n == 0 {
            drop(socket);
        } else {
            let bye = bye.clone();
            tokio::spawn(async move {
                while let Ok(Some(frame)) = read_frame_async(&mut socket).await {
                    if matches!(Control::decode(&frame.payload), Ok(Control::Bye)) {
                        let _ = bye.send(n).await;
                    }
                }
            });
        }
        n += 1;
    }
}

#[tokio::test]
async fn daemon_restarts_session_after_helper_death() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (accepted_tx, mut accepted) = mpsc::channel(8);
    let (bye_tx, mut bye) = mpsc::channel(8);
    tokio::spawn(helper_series(listener, accepted_tx, bye_tx));

    let starts = Arc::new(AtomicUsize::new(0));
    let (ready_tx, mut ready) = mpsc::channel::<usize>(8);
    let starter = {
        let starts = starts.clone();
        move |serial: String| {
            let n = starts.fetch_add(1, Ordering::SeqCst);
            let ready_tx = ready_tx.clone();
            async move {
                let stream = TcpStream::connect(("127.0.0.1", port)).await?;
                let stream = handshake(stream, TOKEN).await?;
                let _ = ready_tx.send(n).await;
                Ok(SessionConn::attach(stream, Adb::new(serial), "sha"))
            }
        }
    };
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let daemon = tokio::spawn(run_loop(
        Some("dev".into()),
        online_then_idle("dev"),
        starter,
        async move {
            let _ = stop_rx.await;
            "test-stop"
        },
        fast_policy(),
    ));

    assert_eq!(accepted.recv().await, Some(0));
    // Connection 0 dies; the daemon must notice and start a fresh session on its own.
    let second = timeout(Duration::from_secs(2), accepted.recv())
        .await
        .expect("daemon did not restart after helper death");
    assert_eq!(second, Some(1));
    assert_eq!(ready.recv().await, Some(0));
    assert_eq!(ready.recv().await, Some(1));
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    // Let run_loop take ownership of the second session before stopping.
    tokio::time::sleep(Duration::from_millis(50)).await;

    stop_tx.send(()).unwrap();
    timeout(Duration::from_secs(5), daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), bye.recv()).await.unwrap(),
        Some(1),
        "restarted session was not closed with Bye on shutdown"
    );
}

#[tokio::test]
async fn daemon_retries_failed_start_with_backoff() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let helper = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        answer_hello(&mut socket, TOKEN, hello(1, "")).await;
        // Stay up until the daemon closes.
        while let Ok(Some(_)) = read_frame_async(&mut socket).await {}
    });

    let attempts = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let (ready_tx, ready_rx) = oneshot::channel::<()>();
    let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));
    let starter = {
        let attempts = attempts.clone();
        move |serial: String| {
            let attempt = {
                let mut attempts = attempts.lock().unwrap();
                attempts.push(Instant::now());
                attempts.len()
            };
            let ready_tx = ready_tx.clone();
            async move {
                if attempt <= 2 {
                    return Err(anyhow!("simulated start failure {attempt}"));
                }
                let stream = TcpStream::connect(("127.0.0.1", port)).await?;
                let stream = handshake(stream, TOKEN).await?;
                if let Some(tx) = ready_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                Ok(SessionConn::attach(stream, Adb::new(serial), "sha"))
            }
        }
    };
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let daemon = tokio::spawn(run_loop(
        None,
        online_then_idle("dev"),
        starter,
        async move {
            let _ = stop_rx.await;
            "test-stop"
        },
        fast_policy(),
    ));

    timeout(Duration::from_secs(3), ready_rx)
        .await
        .expect("daemon gave up after failed starts")
        .unwrap();
    let attempts = attempts.lock().unwrap().clone();
    assert_eq!(attempts.len(), 3);
    // 50 ms after the first failure, 100 ms after the second.
    assert!(attempts[1] - attempts[0] >= Duration::from_millis(50));
    assert!(attempts[2] - attempts[1] >= Duration::from_millis(100));

    stop_tx.send(()).unwrap();
    timeout(Duration::from_secs(5), daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(2), helper)
        .await
        .unwrap()
        .unwrap();
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn shutdown_during_start_cancels_it_promptly() {
    let dropped = Arc::new(AtomicBool::new(false));
    let (entered_tx, entered_rx) = oneshot::channel::<()>();
    let mut entered_tx = Some(entered_tx);
    let starter = {
        let dropped = dropped.clone();
        move |_serial: String| {
            let guard = DropFlag(dropped.clone());
            let entered = entered_tx.take();
            async move {
                let _guard = guard;
                if let Some(tx) = entered {
                    let _ = tx.send(());
                }
                // A start that would take far longer than any shutdown budget.
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Err(anyhow!("unreachable"))
            }
        }
    };
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let daemon = tokio::spawn(run_loop(
        None,
        online_then_idle("dev"),
        starter,
        async move {
            let _ = stop_rx.await;
            "SIGINT"
        },
        fast_policy(),
    ));
    entered_rx.await.unwrap();
    let t0 = Instant::now();
    stop_tx.send(()).unwrap();
    timeout(Duration::from_secs(1), daemon)
        .await
        .expect("shutdown blocked behind session start")
        .unwrap()
        .unwrap();
    assert!(t0.elapsed() < Duration::from_millis(500));
    assert!(
        dropped.load(Ordering::SeqCst),
        "start future was not dropped"
    );
}

#[tokio::test]
async fn offline_during_start_cancels_it() {
    let dropped = Arc::new(AtomicBool::new(false));
    let (offline_tx, offline_rx) = oneshot::channel::<()>();
    let mut offline_tx = Some(offline_tx);
    let starter = {
        let dropped = dropped.clone();
        move |_serial: String| {
            let guard = DropFlag(dropped.clone());
            let go = offline_tx.take();
            async move {
                let _guard = guard;
                if let Some(go) = go {
                    let _ = go.send(());
                }
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Err(anyhow!("unreachable"))
            }
        }
    };
    let events = stream::iter([DeviceEvent::Online("dev".into())])
        .chain(stream::once(async move {
            let _ = offline_rx.await;
            DeviceEvent::Offline("dev".into())
        }))
        .chain(stream::pending());
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let daemon = tokio::spawn(run_loop(
        None,
        events,
        starter,
        async move {
            let _ = stop_rx.await;
            "test-stop"
        },
        fast_policy(),
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !dropped.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "offline did not cancel the start"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stop_tx.send(()).unwrap();
    timeout(Duration::from_secs(1), daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
