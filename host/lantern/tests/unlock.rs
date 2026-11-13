//! The unlock prompt against a private `dbus-daemon` (never the real session bus): a fake
//! `org.freedesktop.Notifications` records `Notify`/`CloseNotification` and emits
//! `ActionInvoked`; the PIN dialog is `/bin/echo 1234`; the reopen `exe` is a script that
//! records its arguments.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lantern::daemon::control::{Phone, View};
use lantern::unlock::{self, LockEvent, Pin, Prompt};
use lantern_proto::{CompanionState, LockReason};
use tokio::sync::{mpsc, watch};
use zbus::zvariant::OwnedValue;
use zbus::{connection, Connection};

static DIRS: AtomicUsize = AtomicUsize::new(0);

fn scratch() -> PathBuf {
    let n = DIRS.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("lantern-unlock-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A private session bus in a scratch directory, killed (and the directory removed) on drop.
struct Bus {
    child: Child,
    dir: PathBuf,
    address: String,
}

impl Bus {
    /// A session bus with no service directories: nothing is ever bus-activated on it (the
    /// stock session config would start Plasma's notification service for a missing server).
    fn start() -> Self {
        let dir = scratch();
        let config = dir.join("bus.conf");
        std::fs::write(
            &config,
            format!(
                "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN\"\n                  \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
                 <busconfig><type>session</type><listen>unix:dir={}</listen>\
                 <policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>\
                 <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>\n",
                dir.display()
            ),
        )
        .unwrap();
        let mut child = Process::new("dbus-daemon")
            .args(["--nofork", "--print-address=1"])
            .arg(format!("--config-file={}", config.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("run dbus-daemon");
        let mut line = String::new();
        let stdout = child.stdout.take().unwrap();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(stdout), &mut line).unwrap();
        let address = line.trim().to_owned();
        assert!(address.starts_with("unix:"), "dbus-daemon printed {line:?}");
        Self {
            child,
            dir,
            address,
        }
    }

    async fn connect(&self) -> Connection {
        connection::Builder::address(self.address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Call {
    Notify {
        id: u32,
        replaces: u32,
        icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        urgency: Option<u8>,
    },
    Close(u32),
}

/// The fake notification server.
struct Notes {
    calls: Arc<Mutex<Vec<Call>>>,
    next: AtomicU32,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notes {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        assert_eq!(app_name, "Lantern");
        assert_eq!(expire_timeout, -1);
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            self.next.fetch_add(1, Ordering::SeqCst) + 1
        };
        let urgency = hints.get("urgency").and_then(|v| u8::try_from(v).ok());
        self.calls.lock().unwrap().push(Call::Notify {
            id,
            replaces: replaces_id,
            icon: app_icon,
            summary,
            body,
            actions,
            urgency,
        });
        id
    }

    fn close_notification(&self, id: u32) {
        self.calls.lock().unwrap().push(Call::Close(id));
    }
}

const NOTES: &str = "org.freedesktop.Notifications";
const NOTES_PATH: &str = "/org/freedesktop/Notifications";

fn locked_view() -> View {
    View {
        phone: Phone::Ready {
            serial: "s1".into(),
            companion: Some((CompanionState::Ready, None)),
        },
        name: Some("Test Phone".into()),
        locked: true,
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
        let bus = Bus::start();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let server = if with_server {
            Some(
                connection::Builder::address(bus.address.as_str())
                    .unwrap()
                    .serve_at(
                        NOTES_PATH,
                        Notes {
                            calls: calls.clone(),
                            next: AtomicU32::new(0),
                        },
                    )
                    .unwrap()
                    .name(NOTES)
                    .unwrap()
                    .build()
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let (exe, reopened) = fake_exe(&bus.dir);
        let (events, events_rx) = mpsc::channel(32);
        let (view, view_rx) = watch::channel(locked_view());
        let (pins_tx, pins) = mpsc::channel(1);
        let conn = bus.connect().await;
        let task = tokio::spawn(unlock::run(conn, events_rx, view_rx, pins_tx, exe, prompt));
        Self {
            bus,
            server,
            calls,
            events: Some(events),
            view,
            pins,
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

async fn emit_action(conn: &Connection, id: u32, action: &str) {
    conn.emit_signal(
        None::<zbus::names::BusName<'_>>,
        NOTES_PATH,
        NOTES,
        "ActionInvoked",
        &(id, action),
    )
    .await
    .unwrap();
}

async fn wait_for<T>(what: &str, within: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(Instant::now() < deadline, "{what}: not within {within:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
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
        rig.send(LockEvent::Locked(reason)).await;
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
    rig.send(LockEvent::Unlocked).await;
    rig.send(LockEvent::Locked(LockReason::Clipboard)).await;
    rig.notify_id(2).await;
    assert!(rig.calls().contains(&Call::Close(1)));
}

#[tokio::test]
async fn action_prompts_and_sends_pin() {
    let mut rig = Rig::start(true, echo_prompt("1234")).await;
    rig.send(LockEvent::Locked(LockReason::Clipboard)).await;
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
    rig.send(LockEvent::Locked(LockReason::Open)).await;
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
    rig.send(LockEvent::Locked(LockReason::Clipboard)).await;
    let id = rig.notify_id(1).await;
    rig.click(id, "unlock").await;
    assert_eq!(rig.next_pin().await, pin("1234"));
    rig.send(LockEvent::Result {
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
    rig.send(LockEvent::Locked(LockReason::Clipboard)).await;
    rig.notify_id(1).await;
    rig.send(LockEvent::Result {
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
    rig.send(LockEvent::Locked(LockReason::Open)).await;
    let id = rig.notify_id(1).await;
    rig.send(LockEvent::Result {
        ok: true,
        reason: None,
    })
    .await;
    rig.send(LockEvent::Reopen(vec![
        ("com.foo".into(), 0),
        ("com.bar".into(), 10),
    ]))
    .await;
    let log = rig.reopened.clone();
    let mut lines = wait_for("both reopens", Duration::from_secs(5), || {
        let text = std::fs::read_to_string(&log).ok()?;
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        (lines.len() >= 2).then_some(lines)
    })
    .await;
    lines.sort();
    assert_eq!(lines, ["open -- com.foo", "open --user 10 -- com.bar"]);
    assert!(rig.calls().contains(&Call::Close(id)), "{:?}", rig.calls());
}

#[tokio::test]
async fn no_notification_server_is_harmless() {
    let mut rig = Rig::start(false, echo_prompt("1234")).await;
    rig.send(LockEvent::Locked(LockReason::Clipboard)).await;
    rig.send(LockEvent::Result {
        ok: false,
        reason: Some("wrong PIN".into()),
    })
    .await;
    rig.send(LockEvent::Unlocked).await;
    rig.send(LockEvent::Locked(LockReason::Open)).await;
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
    rig.send(LockEvent::Locked(LockReason::Clipboard)).await;
    let id = rig.notify_id(1).await;
    rig.click(id, "unlock").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.view.send_modify(|view| {
        view.phone = Phone::Ready {
            serial: "s2".into(),
            companion: None,
        };
        view.name = Some("Other".into());
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
    rig.send(LockEvent::Locked(LockReason::Clipboard)).await;
    let id = rig.notify_id(1).await;
    rig.click(id, "unlock").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    rig.send(LockEvent::Unlocked).await;
    // A new stretch before the answer does not revive it.
    rig.send(LockEvent::Locked(LockReason::Open)).await;
    rig.assert_no_pin_within(Duration::from_millis(1500)).await;
}
