//! The hub with two live phones, `A` and `B`, over in-memory helper queues; window clients are
//! `UnixStream::pair()` ends added with `add_client`. No daemon, no socket, no phone.

use super::*;
use crate::clipboard::ClipHandle;
use lantern_proto::{LockReason, SecretPin};
use tokio::net::UnixStream;

const QUIET: Duration = Duration::from_millis(200);
const STEP: Duration = Duration::from_secs(2);

/// A hub with links `A` then `B` (so `B` is the default phone).
fn two() -> (Hub, mpsc::Receiver<Frame>, mpsc::Receiver<Frame>) {
    let mut hub = Hub::new(None);
    let (a_tx, a) = mpsc::channel(64);
    let (b_tx, b) = mpsc::channel(64);
    hub.session_up("A", a_tx, "shaA");
    hub.session_up("B", b_tx, "shaB");
    (hub, a, b)
}

fn open(pkg: &str, phone: Option<&str>) -> Frame {
    control(
        0,
        Control::Open {
            pkg: pkg.into(),
            user: 0,
            w: 800,
            h: 600,
            dpi: 160,
            activation_token: None,
            phone: phone.map(str::to_owned),
        },
    )
}

fn video(stream: u32, byte: u8) -> Frame {
    Frame {
        kind: Kind::Video,
        stream,
        payload: vec![byte; 4],
    }
}

fn decode(frame: &Frame) -> Control {
    assert_eq!(frame.kind, Kind::Control, "{frame:?}");
    Control::decode(&frame.payload).unwrap()
}

/// Hands the hub its next client event.
async fn pump(hub: &mut Hub) {
    let event = timeout(STEP, hub.next_event())
        .await
        .expect("no client event");
    hub.on_client(event).await.unwrap();
}

/// A client whose first frame is `first`, handled by the hub.
async fn client(hub: &mut Hub, first: Frame) -> UnixStream {
    let (mut ours, theirs) = UnixStream::pair().unwrap();
    hub.add_client(theirs);
    write_frame_async(&mut ours, &first).await.unwrap();
    pump(hub).await;
    ours
}

async fn recv(client: &mut UnixStream) -> Option<Frame> {
    timeout(STEP, read_frame_async(client))
        .await
        .expect("client got nothing")
        .unwrap()
}

async fn assert_client_quiet(client: &mut UnixStream) {
    if let Ok(read) = timeout(QUIET, read_frame_async(client)).await {
        panic!("unexpected client frame: {read:?}");
    }
}

fn assert_helper_quiet(helper: &mut mpsc::Receiver<Frame>, who: &str) {
    if let Ok(frame) = helper.try_recv() {
        panic!("{who}'s helper got {frame:?}");
    }
}

fn opened_on(helper: &mut mpsc::Receiver<Frame>) -> u32 {
    let frame = helper.try_recv().expect("no open");
    assert!(matches!(decode(&frame), Control::Open { .. }), "{frame:?}");
    frame.stream
}

#[tokio::test]
async fn same_app_on_two_phones_is_two_windows() {
    let (mut hub, mut a, mut b) = two();
    let mut on_a = client(&mut hub, open("com.x", Some("A"))).await;
    assert_eq!(opened_on(&mut a), 1);
    assert_helper_quiet(&mut b, "B");
    let _on_b = client(&mut hub, open("com.x", Some("B"))).await;
    assert_eq!(opened_on(&mut b), 2);
    assert_helper_quiet(&mut a, "A");
    // A second open of com.x on A focuses A's window 1.
    let mut again = client(&mut hub, open("com.x", Some("A"))).await;
    let focus = recv(&mut on_a).await.unwrap();
    assert_eq!(focus.stream, 1);
    assert!(matches!(decode(&focus), Control::Focus { .. }));
    let reply = recv(&mut again).await.unwrap();
    assert_eq!(reply.stream, 1);
    assert!(matches!(decode(&reply), Control::Focus { .. }));
    assert_eq!(recv(&mut again).await, None, "closed after the focus");
    assert_helper_quiet(&mut a, "A");
    assert_helper_quiet(&mut b, "B");
}

#[tokio::test]
async fn helper_frames_only_reach_their_own_phones_windows() {
    let (mut hub, mut a, mut b) = two();
    let mut on_a = client(&mut hub, open("com.x", Some("A"))).await;
    let mut on_b = client(&mut hub, open("com.x", Some("B"))).await;
    let (win_a, win_b) = (opened_on(&mut a), opened_on(&mut b));
    // B's helper names A's window: dropped.
    hub.on_helper("B", video(win_a, 7)).await.unwrap();
    assert_client_quiet(&mut on_a).await;
    assert_client_quiet(&mut on_b).await;
    // A's own frame on it arrives.
    hub.on_helper("A", video(win_a, 1)).await.unwrap();
    assert_eq!(recv(&mut on_a).await, Some(video(win_a, 1)));
    // Closed from the wrong phone doesn't release A's window either.
    let closed = control(
        win_a,
        Control::Closed {
            reason: lantern_proto::CloseReason::Error,
        },
    );
    hub.on_helper("B", closed).await.unwrap();
    hub.on_helper("A", video(win_a, 2)).await.unwrap();
    assert_eq!(recv(&mut on_a).await, Some(video(win_a, 2)));
    hub.on_helper("B", video(win_b, 3)).await.unwrap();
    assert_eq!(recv(&mut on_b).await, Some(video(win_b, 3)));
}

/// R7 (a): A is not the default phone (B is), so a frame that went to the default phone instead
/// of its window's phone would show here.
#[tokio::test]
async fn client_frames_go_to_their_phones_helper() {
    let (mut hub, mut a, mut b) = two();
    assert_eq!(hub.default_phone(), Some("B"));
    let mut on_a = client(&mut hub, open("com.x", Some("A"))).await;
    let _on_b = client(&mut hub, open("com.x", Some("B"))).await;
    let win_a = opened_on(&mut a);
    opened_on(&mut b);
    let input = Frame {
        kind: Kind::Input,
        stream: win_a,
        payload: InputEvent::Scroll {
            x: 1.0,
            y: 2.0,
            h: 0.0,
            v: 1.0,
        }
        .encode(),
    };
    write_frame_async(&mut on_a, &input).await.unwrap();
    pump(&mut hub).await;
    assert_eq!(a.try_recv().unwrap(), input);
    assert_helper_quiet(&mut b, "B");
    // Its close goes to A too.
    drop(on_a);
    pump(&mut hub).await;
    let close = a.try_recv().unwrap();
    assert_eq!((close.stream, decode(&close)), (win_a, Control::Close));
    assert_helper_quiet(&mut b, "B");
    assert_helper_quiet(&mut a, "A");
}

#[tokio::test]
async fn session_down_closes_only_that_phones_windows() {
    let (mut hub, mut a, mut b) = two();
    let mut on_a = client(&mut hub, open("com.x", Some("A"))).await;
    let mut on_b = client(&mut hub, open("com.x", Some("B"))).await;
    opened_on(&mut a);
    let win_b = opened_on(&mut b);
    hub.session_down("A");
    assert_eq!(recv(&mut on_a).await, None, "A's window client is closed");
    hub.on_helper("B", video(win_b, 5)).await.unwrap();
    assert_eq!(recv(&mut on_b).await, Some(video(win_b, 5)));
    assert_eq!(hub.live_serials(), ["B"]);
    assert_eq!(hub.status().windows.len(), 1);
    // A comes back: its next window gets a new id, never 1 again.
    let (a_tx, mut a) = mpsc::channel(64);
    hub.session_up("A", a_tx, "shaA");
    let _again = client(&mut hub, open("com.x", Some("A"))).await;
    assert_eq!(opened_on(&mut a), 3);
    assert_helper_quiet(&mut b, "B");
}

#[tokio::test]
async fn open_without_phone_goes_to_the_default_phone() {
    let (mut hub, mut a, mut b) = two();
    assert_eq!(hub.default_phone(), Some("B"));
    assert_eq!(hub.live_serials(), ["A", "B"]);
    let _c = client(&mut hub, open("com.x", None)).await;
    assert_eq!(opened_on(&mut b), 1);
    assert_helper_quiet(&mut a, "A");
    assert_eq!(hub.status().windows[0].phone.as_deref(), Some("B"));
}

#[tokio::test]
async fn lock_state_is_per_phone() {
    let (mut hub, mut a, mut b) = two();
    let (tx, mut events) = mpsc::channel(16);
    hub.set_lock_events(tx);
    let locked = control(
        0,
        Control::Locked {
            reason: LockReason::Clipboard,
        },
    );
    hub.on_helper("B", locked.clone()).await.unwrap();
    hub.on_helper("B", locked).await.unwrap();
    assert!(hub.locked("B"));
    assert!(!hub.locked("A"));
    assert_eq!(
        events.try_recv(),
        Ok(LockEvent::Locked {
            serial: "B".into(),
            reason: LockReason::Clipboard
        })
    );
    assert!(events.try_recv().is_err(), "one event per stretch");
    // A blocked open on B becomes PHONE_LOCKED; one on A keeps the phone's error.
    let mut on_b = client(&mut hub, open("com.x", Some("B"))).await;
    let mut on_a = client(&mut hub, open("com.x", Some("A"))).await;
    let (win_b, win_a) = (opened_on(&mut b), opened_on(&mut a));
    hub.on_helper("B", control(win_b, Control::error("SecurityException")))
        .await
        .unwrap();
    assert_eq!(
        recv(&mut on_b).await.as_ref().map(decode),
        Some(Control::error(PHONE_LOCKED))
    );
    let error = control(win_a, Control::error("no launch intent"));
    hub.on_helper("A", error.clone()).await.unwrap();
    assert_eq!(recv(&mut on_a).await, Some(error));
    // B's session end ends B's stretch only.
    hub.session_down("B");
    assert_eq!(
        events.try_recv(),
        Ok(LockEvent::Unlocked { serial: "B".into() })
    );
    assert!(events.try_recv().is_err());
}

fn offer(text: &str) -> PcOffer {
    PcOffer {
        mimes: vec!["text/plain;charset=utf-8".into()],
        text: Some(text.into()),
        secret: false,
    }
}

fn clip_text(frame: Frame) -> String {
    assert_eq!(frame.stream, 0);
    match decode(&frame) {
        Control::Clip { text } => text,
        other => panic!("expected clip, got {other:?}"),
    }
}

#[tokio::test]
async fn pc_copy_fans_out_and_phone_copy_skips_its_source() {
    let (mut hub, mut a, mut b) = two();
    let (handle, pc) = ClipHandle::test_pair();
    hub.set_clipboard(handle);
    hub.on_helper("A", control(0, Control::Clip { text: "x".into() }))
        .await
        .unwrap();
    assert_eq!(pc.try_recv().unwrap(), "x");
    // Its selection comes back: B gets it, A doesn't.
    assert!(hub.on_pc_clip(offer("x")).await.is_empty());
    assert_eq!(clip_text(b.try_recv().unwrap()), "x");
    assert_helper_quiet(&mut a, "A");
    // A PC copy goes to both.
    assert!(hub.on_pc_clip(offer("pc")).await.is_empty());
    assert_eq!(clip_text(a.try_recv().unwrap()), "pc");
    assert_eq!(clip_text(b.try_recv().unwrap()), "pc");
}

/// R7 (b): two wedged helpers share one 1 s wait; both are reported, the healthy one gets its
/// clip.
#[tokio::test]
async fn wedged_helper_is_reported_alone() {
    let mut hub = Hub::new(None);
    let (a_tx, _a) = mpsc::channel(1);
    a_tx.try_send(video(1, 0)).unwrap(); // full, never drained
    let (b_tx, mut b) = mpsc::channel(4);
    let (c_tx, _c) = mpsc::channel(1);
    c_tx.try_send(video(1, 0)).unwrap(); // full, never drained
    hub.session_up("A", a_tx, "shaA");
    hub.session_up("B", b_tx, "shaB");
    hub.session_up("C", c_tx, "shaC");
    let started = Instant::now();
    let wedged = hub.on_pc_clip(offer("y")).await;
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "one shared wait, not one per helper: {:?}",
        started.elapsed()
    );
    assert_eq!(
        wedged,
        [
            HelperWedged { serial: "A".into() },
            HelperWedged { serial: "C".into() }
        ]
    );
    assert_eq!(clip_text(b.try_recv().unwrap()), "y");
}

fn pin(digits: &str) -> Pin {
    Pin::parse(digits.to_owned()).unwrap()
}

#[tokio::test]
async fn send_unlock_only_while_that_phone_is_locked() {
    let (mut hub, mut a, mut b) = two();
    assert_eq!(hub.send_unlock("B", pin("1234")).await, Ok(false));
    assert_helper_quiet(&mut b, "B");
    assert_eq!(hub.send_unlock("C", pin("1234")).await, Ok(false));
    let locked = control(
        0,
        Control::Locked {
            reason: LockReason::Session,
        },
    );
    hub.on_helper("B", locked).await.unwrap();
    assert_eq!(hub.send_unlock("B", pin("2468")).await, Ok(true));
    let frame = b.try_recv().unwrap();
    assert_eq!(frame.stream, 0);
    match decode(&frame) {
        Control::Unlock { pin } => assert_eq!(pin, SecretPin::new("2468".into())),
        other => panic!("expected unlock, got {other:?}"),
    }
    assert_helper_quiet(&mut b, "B");
    assert_helper_quiet(&mut a, "A");
    // A isn't locked: nothing for it.
    assert_eq!(hub.send_unlock("A", pin("1234")).await, Ok(false));
    assert_helper_quiet(&mut a, "A");
    // The stretch ends: B gets no PIN either.
    hub.on_helper("B", control(0, Control::Unlocked))
        .await
        .unwrap();
    assert_eq!(hub.send_unlock("B", pin("1234")).await, Ok(false));
    assert_helper_quiet(&mut b, "B");
}

#[tokio::test]
async fn status_top_level_is_the_latest_ready_phone() {
    let (mut hub, mut a, _b) = two();
    let status = hub.status();
    assert_eq!(status.state, SessionState::Ready);
    assert_eq!(status.serial.as_deref(), Some("B"));
    assert_eq!(status.helper.as_deref(), Some("shaB"));
    let _c = client(&mut hub, open("com.x", Some("A"))).await;
    opened_on(&mut a);
    assert_eq!(
        hub.status().windows,
        [WindowInfo {
            id: 1,
            pkg: "com.x".into(),
            user: 0,
            phone: Some("A".into()),
        }]
    );
    hub.session_down("B");
    let status = hub.status();
    assert_eq!(
        (
            status.state,
            status.serial.as_deref(),
            status.helper.as_deref()
        ),
        (SessionState::Ready, Some("A"), Some("shaA"))
    );
    hub.session_down("A");
    hub.set_paused(Some("A".into()));
    let status = hub.status();
    assert_eq!(
        (status.state, status.serial.as_deref()),
        (SessionState::Paused, Some("A"))
    );
}
