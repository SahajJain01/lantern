//! The unlock prompt against a private `dbus-daemon` (never the real session bus): a fake
//! `org.freedesktop.Notifications` records `Notify`/`CloseNotification` and emits
//! `ActionInvoked`; the PIN dialog is `/bin/echo 1234`; the reopen `exe` is a script that
//! records its arguments.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{emit_action, wait_for, Bus, Call};
use lantern::daemon::control::{PhoneView, View};
use lantern::unlock::{self, LockEvent, Pin, Prompt};
use lantern_proto::{CompanionState, LockReason, PhoneState};
use tokio::sync::{mpsc, watch};
use zbus::Connection;

fn locked_view() -> View {
    View {
        phones: vec![PhoneView {
            serial: "s1".into(),
            name: Some("Test Phone".into()),
            state: PhoneState::Ready,
            plugged: true,
            off: false,
            locked: true,
            companion: Some((CompanionState::Ready, None)),
        }],
    }
}

/// `unlock::run` on a private bus, with or without a notification server.
struct Rig {
    bus: Bus,
    server: Option<Connection>,
    calls: Arc<Mutex<Vec<Call>>>,
    events: Option<mpsc::Sender<LockEvent>>,
    view: watch::Sender<View>,
    pins: mpsc::Receiver<(String, Pin)>,
    /// The tray's handle on the prompt.
    request: unlock::Request,
    /// Where the fake `exe` appends one line per run.
    reopened: PathBuf,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A fake `lantern`: appends its arguments to `<dir>/reopened`.
fn fake_exe(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("reopened");
    let exe = dir.join("fake-lantern");
    std::fs::write(
        &exe,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    (exe, log)
}

fn echo_prompt(text: &str) -> Prompt {
    Prompt {
        program: "/bin/echo".into(),
        args: vec![text.to_owned()],
    }
}

impl Rig {
    async fn start(with_server: bool, prompt: Prompt) -> Self {
        Self::start_full(with_server, locked_view(), prompt).await
    }

    /// With a notification server, showing `view`.
    async fn start_view(view: View, prompt: Prompt) -> Self {
        Self::start_full(true, view, prompt).await
    }

    async fn start_full(with_server: bool, view: View, prompt: Prompt) -> Self {
        let bus = Bus::start();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let server = if with_server {
            Some(bus.serve_notes(calls.clone()).await)
        } else {
            None
        };
        let (exe, reopened) = fake_exe(&bus.dir);
        let (events, events_rx) = mpsc::channel(32);
        let (view, view_rx) = watch::channel(view);
        let (pins_tx, pins) = mpsc::channel(1);
        let conn = bus.connect().await;
        let (request, requests) = unlock::Request::channel();
        let task = tokio::spawn(unlock::run(
            conn, events_rx, requests, view_rx, pins_tx, exe, prompt,
        ));
        Self {
            bus,
            server,
            calls,
            events: Some(events),
            view,
            pins,
            request,
            reopened,
            task,
        }
    }

    async fn send(&self, event: LockEvent) {
        self.events.as_ref().unwrap().send(event).await.unwrap();
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn notifies(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Notify { .. }))
            .collect()
    }

    /// Waits for the `n`th `Notify` and returns its id.
    async fn notify_id(&self, n: usize) -> u32 {
        wait_for("Notify", Duration::from_secs(5), || {
            match self.notifies().get(n - 1) {
                Some(Call::Notify { id, .. }) => Some(*id),
                _ => None,
            }
        })
        .await
    }

    /// The server emits `ActionInvoked(id, action)`, as Plasma does on a click.
    async fn click(&self, id: u32, action: &str) {
        emit_action(self.server.as_ref().unwrap(), id, action).await;
    }

    /// The next PIN handed on, for the phone `s1` (the view's).
    async fn next_pin(&mut self) -> Pin {
        let (serial, pin) = self.next_tagged_pin().await;
        assert_eq!(serial, "s1");
        pin
    }

    async fn next_tagged_pin(&mut self) -> (String, Pin) {
        tokio::time::timeout(Duration::from_secs(5), self.pins.recv())
            .await
            .expect("no PIN within 5 s")
            .unwrap()
    }

    async fn assert_no_pin(&mut self) {
        self.assert_no_pin_within(Duration::from_millis(500)).await;
    }

    async fn assert_no_pin_within(&mut self, within: Duration) {
        if let Ok(pin) = tokio::time::timeout(within, self.pins.recv()).await {
            panic!("unexpected PIN handed on: {pin:?}");
        }
    }
}

fn pin(digits: &str) -> Pin {
    Pin::parse(digits.to_owned()).unwrap()
}

// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn one_notice_per_locked_stretch() {
    let rig = Rig::start(true, echo_prompt("1234")).await;
    for n in 0..20 {
        let reason = if n % 2 == 0 {
            LockReason::Clipboard
        } else {
            LockReason::Open
        };
        rig.send(LockEvent::Locked {
            serial: "s1".into(),
            reason,
        })
        .await;
    }
    rig.notify_id(1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        rig.notifies(),
        [Call::Notify {
            id: 1,
            replaces: 0,
            icon: "smartphone".into(),
            summary: "Unlock your Test Phone".into(),
            body: "Lantern can't paste or open apps while Test Phone is locked.".into(),
            actions: vec!["unlock".into(), "Enter PIN…".into()],
            urgency: Some(1),
        }]
    );
    // A new stretch after the unlock gets its own notice.
    rig.send(LockEvent::Unlocked {
        serial: "s1".into(),
    })
    .await;
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Clipboard,
    })
    .await;
    rig.notify_id(2).await;
    assert!(rig.calls().contains(&Call::Close(1)));
}

/// The helper's lock read at session-up (`locked{reason:"session"}`) prompts at once, with the
/// same notice as any other stretch.
#[tokio::test]
async fn session_lock_prompts_at_once() {
    let rig = Rig::start(true, echo_prompt("1234")).await;
    rig.view
        .send_modify(|view| view.phones[0].name = Some("Pixel".into()));
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Session,
    })
    .await;
    rig.notify_id(1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        rig.notifies(),
        [Call::Notify {
            id: 1,
            replaces: 0,
            icon: "smartphone".into(),
            summary: "Unlock your Pixel".into(),
            body: "Lantern can't paste or open apps while Pixel is locked.".into(),
            actions: vec!["unlock".into(), "Enter PIN…".into()],
            urgency: Some(1),
        }]
    );
}

#[tokio::test]
async fn action_prompts_and_sends_pin() {
    let mut rig = Rig::start(true, echo_prompt("1234")).await;
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Clipboard,
    })
    .await;
    let id = rig.notify_id(1).await;
    // Anyone else on the bus emitting the signal is ignored.
    let other = rig.bus.connect().await;
    emit_action(&other, id, "unlock").await;
    rig.assert_no_pin().await;
    // Another action or another id is ignored too.
    rig.click(id, "default").await;
    rig.click(id + 7, "unlock").await;
    rig.assert_no_pin().await;
    rig.click(id, "unlock").await;
    assert_eq!(rig.next_pin().await, pin("1234"));
    rig.assert_no_pin().await;
}

#[tokio::test]
async fn invalid_pin_is_refused_and_not_sent() {
    let mut rig = Rig::start(true, echo_prompt("12a4")).await;
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Open,
    })
    .await;
    let id = rig.notify_id(1).await;
    rig.click(id, "unlock").await;
    rig.notify_id(2).await;
    rig.assert_no_pin().await;
    match &rig.notifies()[1] {
        Call::Notify {
            summary,
            body,
            actions,
            ..
        } => {
            assert_eq!(summary, "Couldn't unlock Test Phone");
            assert_eq!(body, "the PIN must be 4 to 16 digits");
            assert_eq!(actions, &["unlock", "Try again"]);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn wrong_pin_notifies_once_no_retry() {
    let mut rig = Rig::start(true, echo_prompt("1234")).await;
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Clipboard,
    })
    .await;
    let id = rig.notify_id(1).await;
    rig.click(id, "unlock").await;
    assert_eq!(rig.next_pin().await, pin("1234"));
    rig.send(LockEvent::Result {
        serial: "s1".into(),
        ok: false,
        reason: Some("wrong PIN".into()),
    })
    .await;
    rig.notify_id(2).await;
    rig.assert_no_pin().await;
    let notifies = rig.notifies();
    assert_eq!(notifies.len(), 2, "{notifies:?}");
    let wrong: Vec<_> = notifies
        .iter()
        .filter(|c| matches!(c, Call::Notify { summary, .. } if summary.starts_with("That PIN didn't unlock")))
        .collect();
    assert_eq!(wrong.len(), 1, "{notifies:?}");
    match wrong[0] {
        Call::Notify {
            summary, actions, ..
        } => {
            assert_eq!(summary, "That PIN didn't unlock Test Phone");
            assert_eq!(actions, &["unlock", "Try again"]);
        }
        _ => unreachable!(),
    }
    // "Try again" prompts once more, on the user's click only.
    let id = rig.notify_id(2).await;
    rig.click(id, "unlock").await;
    assert_eq!(rig.next_pin().await, pin("1234"));
}

#[tokio::test]
async fn unconfirmed_unlock_does_not_blame_the_pin() {
    let rig = Rig::start(true, echo_prompt("1234")).await;
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Clipboard,
    })
    .await;
    rig.notify_id(1).await;
    rig.send(LockEvent::Result {
        serial: "s1".into(),
        ok: false,
        reason: Some("unlock not confirmed".into()),
    })
    .await;
    rig.notify_id(2).await;
    match &rig.notifies()[1] {
        Call::Notify { summary, body, .. } => {
            assert_eq!(summary, "Couldn't unlock Test Phone");
            assert_eq!(
                body,
                "Lantern couldn't confirm Test Phone unlocked. Check the phone before trying again."
            );
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn success_closes_notice_and_reopens_pending() {
    let rig = Rig::start(true, echo_prompt("1234")).await;
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Open,
    })
    .await;
    let id = rig.notify_id(1).await;
    rig.send(LockEvent::Result {
        serial: "s1".into(),
        ok: true,
        reason: None,
    })
    .await;
    rig.send(LockEvent::Reopen {
        serial: "dev".into(),
        apps: vec![("com.foo".into(), 0), ("com.bar".into(), 10)],
    })
    .await;
    let log = rig.reopened.clone();
    let mut lines = wait_for("both reopens", Duration::from_secs(5), || {
        let text = std::fs::read_to_string(&log).ok()?;
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        (lines.len() >= 2).then_some(lines)
    })
    .await;
    lines.sort();
    assert_eq!(
        lines,
        [
            "open --phone dev -- com.foo",
            "open --phone dev --user 10 -- com.bar"
        ]
    );
    assert!(rig.calls().contains(&Call::Close(id)), "{:?}", rig.calls());
}

#[tokio::test]
async fn no_notification_server_is_harmless() {
    let mut rig = Rig::start(false, echo_prompt("1234")).await;
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Clipboard,
    })
    .await;
    rig.send(LockEvent::Result {
        serial: "s1".into(),
        ok: false,
        reason: Some("wrong PIN".into()),
    })
    .await;
    rig.send(LockEvent::Unlocked {
        serial: "s1".into(),
    })
    .await;
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Open,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!rig.task.is_finished(), "run must keep going");
    rig.assert_no_pin().await;
    // It ends cleanly when the daemon drops its sender.
    rig.events.take();
    let task = std::mem::replace(&mut rig.task, tokio::spawn(async { Ok(()) }));
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("run did not end")
        .unwrap()
        .unwrap();
}

/// A PIN dialog that answers `text` after `secs` seconds.
fn slow_prompt(secs: &str, text: &str) -> Prompt {
    Prompt {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), format!("sleep {secs}; echo {text}")],
    }
}

/// R6: the PIN carries the serial of the phone whose notification was clicked, even when the
/// session moved to another phone while the dialog was open (the run loop then wipes it).
#[tokio::test]
async fn pin_is_tagged_with_the_phone_clicked() {
    let mut rig = Rig::start(true, slow_prompt("0.5", "1234")).await;
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Clipboard,
    })
    .await;
    let id = rig.notify_id(1).await;
    rig.click(id, "unlock").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.view.send_modify(|view| {
        view.phones[0].serial = "s2".into();
        view.phones[0].name = Some("Other".into());
    });
    let (serial, got) = rig.next_tagged_pin().await;
    assert_eq!(serial, "s1");
    assert_eq!(got, pin("1234"));
}

/// R6: an answer whose locked stretch ended since the click (unlocked on the phone, or the
/// session ended) is wiped, not handed on.
#[tokio::test]
async fn answer_after_the_stretch_ended_is_dropped() {
    let mut rig = Rig::start(true, slow_prompt("0.5", "1234")).await;
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Clipboard,
    })
    .await;
    let id = rig.notify_id(1).await;
    rig.click(id, "unlock").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.send(LockEvent::Unlocked {
        serial: "s1".into(),
    })
    .await;
    // A new stretch before the answer does not revive it.
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Open,
    })
    .await;
    rig.assert_no_pin_within(Duration::from_millis(1500)).await;
}

/// The tray's request takes the notification's path: one dialog, the PIN tagged with the
/// phone shown at the click. It only works during a locked stretch.
#[tokio::test]
async fn request_prompts_only_while_locked() {
    let mut rig = Rig::start(true, echo_prompt("1234")).await;
    // Not locked yet (or the click raced the unlock): no dialog.
    rig.request.ask("s1".into());
    rig.assert_no_pin().await;
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Clipboard,
    })
    .await;
    rig.notify_id(1).await;
    rig.request.ask("s1".into());
    assert_eq!(rig.next_pin().await, pin("1234"));
    rig.assert_no_pin().await;
    // After the stretch ends, a request does nothing again.
    rig.send(LockEvent::Unlocked {
        serial: "s1".into(),
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.request.ask("s1".into());
    rig.assert_no_pin().await;
}

/// One dialog at a time, whichever source clicked: a tray request or a notification click
/// while a dialog is open does nothing.
#[tokio::test]
async fn second_click_while_dialog_open_does_nothing() {
    let mut rig = Rig::start(true, slow_prompt("0.5", "1234")).await;
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Clipboard,
    })
    .await;
    let id = rig.notify_id(1).await;
    rig.request.ask("s1".into());
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.click(id, "unlock").await;
    rig.request.ask("s1".into());
    assert_eq!(rig.next_pin().await, pin("1234"));
    rig.assert_no_pin_within(Duration::from_millis(1500)).await;
    // Once it answered, the next click opens a new one.
    rig.click(id, "unlock").await;
    assert_eq!(rig.next_pin().await, pin("1234"));
}

/// The tray's request names its phone: a request for a phone that isn't the live, locked one
/// opens no dialog (the answer would go to another phone).
#[tokio::test]
async fn tray_request_for_another_phone_opens_nothing() {
    let mut rig = Rig::start(true, echo_prompt("1234")).await;
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Clipboard,
    })
    .await;
    rig.notify_id(1).await;
    rig.request.ask("s2".into());
    rig.assert_no_pin().await;
    // The live phone's own request still prompts, pinned to it.
    rig.request.ask("s1".into());
    assert_eq!(rig.next_pin().await, pin("1234"));
    rig.assert_no_pin().await;
}

/// Plasma shows a summary as plain text and a body as markup: the phone's name and the
/// failure reason are escaped in bodies only.
#[tokio::test]
async fn names_are_escaped() {
    let rig = Rig::start(true, echo_prompt("12a4")).await;
    rig.view
        .send_modify(|view| view.phones[0].name = Some("A&B <1>".into()));
    rig.send(LockEvent::Locked {
        serial: "s1".into(),
        reason: LockReason::Clipboard,
    })
    .await;
    let id = rig.notify_id(1).await;
    rig.send(LockEvent::Result {
        serial: "s1".into(),
        ok: false,
        reason: Some("<odd> & more".into()),
    })
    .await;
    rig.notify_id(2).await;
    rig.send(LockEvent::Result {
        serial: "s1".into(),
        ok: false,
        reason: Some("unlock not confirmed".into()),
    })
    .await;
    rig.notify_id(3).await;
    // The dialog's answer is not a PIN: its own notice.
    rig.click(id, "unlock").await;
    rig.notify_id(4).await;
    let texts: Vec<(String, String)> = rig
        .notifies()
        .into_iter()
        .map(|call| match call {
            Call::Notify { summary, body, .. } => (summary, body),
            _ => unreachable!(),
        })
        .collect();
    let pair = |s: &str, b: &str| (s.to_owned(), b.to_owned());
    assert_eq!(
        texts,
        [
            pair(
                "Unlock your A&B <1>",
                "Lantern can't paste or open apps while A&amp;B &lt;1&gt; is locked."
            ),
            pair("Couldn't unlock A&B <1>", "&lt;odd&gt; &amp; more"),
            pair(
                "Couldn't unlock A&B <1>",
                "Lantern couldn't confirm A&amp;B &lt;1&gt; unlocked. Check the phone before trying again."
            ),
            pair("Couldn't unlock A&B <1>", "the PIN must be 4 to 16 digits"),
        ]
    );
}

/// Phones `a` ("Pixel") and `b` ("Other"), both live.
fn two_view() -> View {
    let mut a = locked_view().phones.remove(0);
    a.serial = "a".into();
    a.name = Some("Pixel".into());
    let mut b = a.clone();
    b.serial = "b".into();
    b.name = Some("Other".into());
    View { phones: vec![a, b] }
}

/// A PIN dialog that appends the phone's name to `log`, then answers 1234 after `secs`.
fn naming_prompt(log: &Path, secs: &str) -> Prompt {
    Prompt {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            format!(
                "printf '%s\\n' \"$1\" >> '{}'; sleep {secs}; echo 1234",
                log.display()
            ),
            "sh".into(),
            "{name}".into(),
        ],
    }
}

fn locked(serial: &str) -> LockEvent {
    LockEvent::Locked {
        serial: serial.into(),
        reason: LockReason::Clipboard,
    }
}

fn unlocked(serial: &str) -> LockEvent {
    LockEvent::Unlocked {
        serial: serial.into(),
    }
}

/// The names the dialog was run for, in order.
fn asked(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Each phone's stretch has its own notice; neither replaces the other.
#[tokio::test]
async fn two_phones_two_notices() {
    let rig = Rig::start_view(two_view(), echo_prompt("1234")).await;
    rig.send(locked("a")).await;
    rig.notify_id(1).await;
    rig.send(locked("b")).await;
    rig.notify_id(2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let got: Vec<(u32, String)> = rig
        .notifies()
        .into_iter()
        .map(|call| match call {
            Call::Notify {
                replaces, summary, ..
            } => (replaces, summary),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        got,
        [
            (0, "Unlock your Pixel".to_owned()),
            (0, "Unlock your Other".to_owned())
        ]
    );
    assert!(!rig.calls().iter().any(|c| matches!(c, Call::Close(_))));
}

/// Review Focus 3: b locks while a's dialog is open; the answer goes to a, never to b.
#[tokio::test]
async fn answer_goes_to_the_phone_asked_for() {
    let mut rig = Rig::start_view(two_view(), slow_prompt("0.5", "1234")).await;
    rig.send(locked("a")).await;
    let id = rig.notify_id(1).await;
    rig.click(id, "unlock").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.send(locked("b")).await;
    let (serial, got) = rig.next_tagged_pin().await;
    assert_eq!(serial, "a");
    assert_eq!(got, pin("1234"));
    rig.assert_no_pin_within(Duration::from_millis(1000)).await;
}

/// Decision 10: b's click during a's dialog is queued and opens b's dialog after a's answer;
/// if b unlocked meanwhile, it opens nothing.
#[tokio::test]
async fn click_for_b_while_a_asks_is_queued() {
    let log = std::env::temp_dir().join(format!(
        "lantern-unlock-asked-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_file(&log);
    let mut rig = Rig::start_view(two_view(), naming_prompt(&log, "0.5")).await;
    rig.send(locked("a")).await;
    let a_id = rig.notify_id(1).await;
    rig.send(locked("b")).await;
    let b_id = rig.notify_id(2).await;
    rig.click(a_id, "unlock").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.click(b_id, "unlock").await;
    // Twice is still one queued dialog.
    rig.request.ask("b".into());
    let (serial, _) = rig.next_tagged_pin().await;
    assert_eq!(serial, "a");
    let (serial, got) = rig.next_tagged_pin().await;
    assert_eq!(serial, "b");
    assert_eq!(got, pin("1234"));
    rig.assert_no_pin_within(Duration::from_millis(1000)).await;
    assert_eq!(asked(&log), ["Pixel", "Other"]);

    // b unlocks before a's dialog answers: b's queued click opens nothing.
    let _ = std::fs::remove_file(&log);
    rig.send(unlocked("a")).await;
    rig.send(unlocked("b")).await;
    rig.send(locked("a")).await;
    let a_id = rig.notify_id(3).await;
    rig.send(locked("b")).await;
    let b_id = rig.notify_id(4).await;
    rig.click(a_id, "unlock").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.click(b_id, "unlock").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.send(unlocked("b")).await;
    let (serial, _) = rig.next_tagged_pin().await;
    assert_eq!(serial, "a");
    rig.assert_no_pin_within(Duration::from_millis(1000)).await;
    assert_eq!(asked(&log), ["Pixel"]);
    let _ = std::fs::remove_file(&log);
}

#[tokio::test]
async fn unlock_of_one_closes_only_its_notice() {
    let mut rig = Rig::start_view(two_view(), echo_prompt("1234")).await;
    rig.send(locked("a")).await;
    let a_id = rig.notify_id(1).await;
    rig.send(locked("b")).await;
    let b_id = rig.notify_id(2).await;
    rig.send(LockEvent::Result {
        serial: "a".into(),
        ok: true,
        reason: None,
    })
    .await;
    let calls = rig.calls.clone();
    wait_for("a's notice closed", Duration::from_secs(5), || {
        calls
            .lock()
            .unwrap()
            .contains(&Call::Close(a_id))
            .then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !rig.calls().contains(&Call::Close(b_id)),
        "{:?}",
        rig.calls()
    );
    // b's notice still prompts, for b.
    rig.click(b_id, "unlock").await;
    let (serial, _) = rig.next_tagged_pin().await;
    assert_eq!(serial, "b");
    // a's old notice id does nothing now.
    rig.click(a_id, "unlock").await;
    rig.assert_no_pin().await;
}

/// a's stretch ends during a's dialog while b stays locked: the answer is wiped, and goes to
/// nobody.
#[tokio::test]
async fn answer_after_its_stretch_ended_is_wiped() {
    let mut rig = Rig::start_view(two_view(), slow_prompt("0.5", "1234")).await;
    rig.send(locked("a")).await;
    let id = rig.notify_id(1).await;
    rig.send(locked("b")).await;
    rig.notify_id(2).await;
    rig.click(id, "unlock").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.send(unlocked("a")).await;
    rig.assert_no_pin_within(Duration::from_millis(1500)).await;
}

#[tokio::test]
async fn tray_request_needs_that_phones_stretch() {
    let mut rig = Rig::start_view(two_view(), echo_prompt("1234")).await;
    rig.send(locked("a")).await;
    rig.notify_id(1).await;
    rig.request.ask("b".into());
    rig.assert_no_pin().await;
    rig.request.ask("a".into());
    assert_eq!(rig.next_tagged_pin().await.0, "a");
    rig.send(locked("b")).await;
    rig.notify_id(2).await;
    rig.request.ask("b".into());
    assert_eq!(rig.next_tagged_pin().await.0, "b");
    rig.assert_no_pin().await;
}

/// Task 4 note: b locks while a's stretch is open (the hub forwards only that first edge). b
/// gets its notice at once, and it still prompts after a's stretch ended.
#[tokio::test]
async fn second_phone_locked_during_a_stretch_still_prompts() {
    let mut rig = Rig::start_view(two_view(), slow_prompt("0.3", "1234")).await;
    rig.send(locked("a")).await;
    let a_id = rig.notify_id(1).await;
    rig.click(a_id, "unlock").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.send(locked("b")).await;
    let b_id = rig.notify_id(2).await;
    rig.send(unlocked("a")).await;
    // a's answer is wiped (its stretch ended); b's notice is still there and prompts for b.
    rig.assert_no_pin_within(Duration::from_millis(800)).await;
    assert!(
        !rig.calls().contains(&Call::Close(b_id)),
        "{:?}",
        rig.calls()
    );
    rig.click(b_id, "unlock").await;
    let (serial, got) = rig.next_tagged_pin().await;
    assert_eq!(serial, "b");
    assert_eq!(got, pin("1234"));
}

/// Decision 15: when lanternd stops (its events sender dropped), each phone's notice on screen
/// is closed before `run` returns.
#[tokio::test]
async fn notice_closes_when_events_end() {
    let mut rig = Rig::start_view(two_view(), echo_prompt("1234")).await;
    rig.send(locked("a")).await;
    let a = rig.notify_id(1).await;
    rig.send(locked("b")).await;
    let b = rig.notify_id(2).await;
    rig.events.take();
    let task = std::mem::replace(&mut rig.task, tokio::spawn(async { Ok(()) }));
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("run did not end")
        .unwrap()
        .unwrap();
    let mut closes: Vec<u32> = rig
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Close(id) => Some(id),
            _ => None,
        })
        .collect();
    closes.sort();
    let mut want = vec![a, b];
    want.sort();
    assert_eq!(closes, want, "{:?}", rig.calls());
}

/// R9 m3: a failed result arriving after that phone's stretch ended (it was unlocked by hand
/// meanwhile) is only logged: no notice inviting another PIN.
#[tokio::test]
async fn failed_result_after_the_stretch_ended_is_not_shown() {
    let rig = Rig::start(true, echo_prompt("1234")).await;
    rig.send(locked("s1")).await;
    let id = rig.notify_id(1).await;
    rig.send(unlocked("s1")).await;
    rig.send(LockEvent::Result {
        serial: "s1".into(),
        ok: false,
        reason: Some("unlock not confirmed".into()),
    })
    .await;
    // A result for a phone never locked is not shown either.
    rig.send(LockEvent::Result {
        serial: "zz".into(),
        ok: false,
        reason: Some("wrong PIN".into()),
    })
    .await;
    wait_for("the close", Duration::from_secs(5), || {
        rig.calls().contains(&Call::Close(id)).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(rig.notifies().len(), 1, "{:?}", rig.calls());
    // The next stretch prompts as usual, and a failure in it is shown.
    rig.send(locked("s1")).await;
    rig.notify_id(2).await;
    rig.send(LockEvent::Result {
        serial: "s1".into(),
        ok: false,
        reason: Some("wrong PIN".into()),
    })
    .await;
    rig.notify_id(3).await;
}

/// R9 m6: b's click queued behind a's dialog is dropped when b unlocks; b's next stretch opens
/// no dialog unless clicked again.
#[tokio::test]
async fn queued_click_is_dropped_when_that_phone_unlocks() {
    let log = std::env::temp_dir().join(format!(
        "lantern-unlock-asked-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_file(&log);
    let mut rig = Rig::start_view(two_view(), naming_prompt(&log, "0.8")).await;
    rig.send(locked("a")).await;
    let a_id = rig.notify_id(1).await;
    rig.send(locked("b")).await;
    let b_id = rig.notify_id(2).await;
    rig.click(a_id, "unlock").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.click(b_id, "unlock").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    // b unlocks and locks again while a's dialog is still open.
    rig.send(unlocked("b")).await;
    rig.send(locked("b")).await;
    rig.notify_id(3).await;
    let (serial, _) = rig.next_tagged_pin().await;
    assert_eq!(serial, "a");
    rig.assert_no_pin_within(Duration::from_millis(1500)).await;
    assert_eq!(asked(&log), ["Pixel"]);
    let _ = std::fs::remove_file(&log);
}
