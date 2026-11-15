//! The tray against a private `dbus-daemon` (never the real session bus): a fake
//! StatusNotifierWatcher records registrations, and a client calls the menu like Plasma does.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command as Process, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use lantern::daemon::control::{Command, PhoneView, View};
use lantern::settings::SettingsHandle;
use lantern::unlock::{self, LockEvent, Pin, Prompt};
use lantern_proto::{CompanionState, LockReason, PhoneState};
use tokio::sync::{mpsc, watch};
use zbus::zvariant::{OwnedValue, Value};
use zbus::{connection, Connection, MatchRule, MessageStream};

static DIRS: AtomicUsize = AtomicUsize::new(0);

/// A fresh, empty scratch directory for one test.
fn scratch() -> PathBuf {
    let n = DIRS.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("lantern-tray-{}-{n}", std::process::id()));
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
    /// stock session config would start Plasma's notification service for the unlock prompt).
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

/// The fake `org.kde.StatusNotifierWatcher`: records every registration.
struct Watcher {
    registered: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(&self, service: String) {
        self.registered.lock().unwrap().push(service);
    }
}

const WATCHER: &str = "org.kde.StatusNotifierWatcher";

fn tray_name() -> String {
    format!("org.kde.StatusNotifierItem-{}-1", std::process::id())
}

/// "Test Phone" (`s1`) in `state`.
fn one(state: PhoneState) -> View {
    View {
        phones: vec![PhoneView {
            serial: "s1".into(),
            name: Some("Test Phone".into()),
            state,
            plugged: true,
            off: state == PhoneState::Off,
            locked: false,
            companion: (state == PhoneState::Ready).then_some((CompanionState::Ready, None)),
        }],
    }
}

fn ready() -> View {
    one(PhoneState::Ready)
}

/// A running tray on a private bus, with a watcher already present.
struct Tray {
    _bus: Bus,
    watcher: Connection,
    registered: Arc<Mutex<Vec<String>>>,
    client: Connection,
    view: watch::Sender<View>,
    commands: mpsc::Receiver<Command>,
    settings_path: PathBuf,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.task.abort();
    }
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

impl Tray {
    async fn start() -> Self {
        Self::start_with(ready(), None).await
    }

    /// A tray showing `view`, holding the unlock prompt's request handle `unlock`.
    async fn start_with(view: View, unlock: Option<unlock::Request>) -> Self {
        let bus = Bus::start();
        let registered = Arc::new(Mutex::new(Vec::new()));
        let watcher = connection::Builder::address(bus.address.as_str())
            .unwrap()
            .serve_at(
                "/StatusNotifierWatcher",
                Watcher {
                    registered: registered.clone(),
                },
            )
            .unwrap()
            .name(WATCHER)
            .unwrap()
            .build()
            .await
            .unwrap();
        let settings_path = bus.dir.join("config").join("settings.toml");
        let settings = SettingsHandle::new(Some(settings_path.clone()));
        let (view, view_rx) = watch::channel(view);
        let (tx, commands) = mpsc::channel(8);
        let conn = bus.connect().await;
        let task = tokio::spawn(lantern::tray::run(conn, view_rx, settings, tx, unlock));
        let client = bus.connect().await;
        let tray = Self {
            _bus: bus,
            watcher,
            registered,
            client,
            view,
            commands,
            settings_path,
            task,
        };
        let r = tray.registered.clone();
        wait_for("first registration", Duration::from_secs(5), || {
            (!r.lock().unwrap().is_empty()).then_some(())
        })
        .await;
        tray
    }

    async fn call<B, R>(&self, path: &str, iface: &str, method: &str, body: &B) -> R
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        let reply = self
            .client
            .call_method(Some(tray_name().as_str()), path, Some(iface), method, body)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e}"));
        reply.body().deserialize().unwrap()
    }

    /// `GetLayout(parent, -1, [])`: the revision, the parent's id and properties, and its
    /// rows.
    async fn get_layout(&self, parent: i32) -> (u32, Row) {
        let (revision, (id, props, children)): (u32, Layout) = self
            .call(
                "/MenuBar",
                "com.canonical.dbusmenu",
                "GetLayout",
                &(parent, -1i32, Vec::<String>::new()),
            )
            .await;
        assert_eq!(id, parent);
        (revision, Row::parse(id, props, children))
    }

    /// `GetLayout(0, -1, [])`: the revision and `(id, label)` per row (`""` for separators).
    async fn layout(&self) -> (u32, Vec<(i32, String)>) {
        let (revision, root) = self.get_layout(0).await;
        assert_eq!(root.string("children-display"), "submenu");
        (revision, root.labels())
    }

    /// Row `id`'s `toggle-state` (`GetProperty`).
    async fn toggle_state(&self, id: i32) -> i32 {
        let value: OwnedValue = self
            .call(
                "/MenuBar",
                "com.canonical.dbusmenu",
                "GetProperty",
                &(id, "toggle-state"),
            )
            .await;
        i32::try_from(value).unwrap()
    }

    /// The next command the tray sent, within `within`.
    async fn command(&mut self, within: Duration) -> Command {
        tokio::time::timeout(within, self.commands.recv())
            .await
            .expect("no command in time")
            .expect("commands closed")
    }

    async fn click(&self, id: i32) {
        let () = self
            .call(
                "/MenuBar",
                "com.canonical.dbusmenu",
                "Event",
                &(id, "clicked", Value::I32(0), 0u32),
            )
            .await;
    }
}

type Layout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// A dbusmenu row and its rows, as `GetLayout` gives them.
struct Row {
    id: i32,
    props: HashMap<String, OwnedValue>,
    children: Vec<Row>,
}

impl Row {
    fn parse(id: i32, props: HashMap<String, OwnedValue>, children: Vec<OwnedValue>) -> Self {
        let children = children
            .into_iter()
            .map(|child| {
                let (id, props, children): Layout = Value::from(child).try_into().unwrap();
                Row::parse(id, props, children)
            })
            .collect();
        Self {
            id,
            props,
            children,
        }
    }

    /// A string property, `""` when absent.
    fn string(&self, name: &str) -> String {
        self.props
            .get(name)
            .map(|v| String::try_from(v.try_clone().unwrap()).unwrap())
            .unwrap_or_default()
    }

    /// `(id, label)` per child row.
    fn labels(&self) -> Vec<(i32, String)> {
        self.children
            .iter()
            .map(|row| (row.id, row.string("label")))
            .collect()
    }
}

/// The rows without row 7, whose presence depends on the host's systemd.
fn without_login(rows: Vec<(i32, String)>) -> Vec<(i32, String)> {
    rows.into_iter().filter(|(id, _)| *id != 7).collect()
}

fn owned(rows: &[(i32, &str)]) -> Vec<(i32, String)> {
    rows.iter().map(|(id, l)| (*id, (*l).to_owned())).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn registers_and_serves_layout() {
    let tray = Tray::start().await;
    assert_eq!(*tray.registered.lock().unwrap(), vec![tray_name()]);
    let (_, rows) = tray.layout().await;
    assert_eq!(
        without_login(rows),
        owned(&[
            (100, "Test Phone"),
            (4, ""),
            (5, "Sync clipboard"),
            (
                6,
                "Type with the Lantern keyboard (from the next connection)"
            ),
            (16, "Turn the phone's screen off (from the next connection)"),
            (14, "Connect phones when plugged in"),
            (8, "Open settings folder"),
            (9, ""),
            (10, "Remove Lantern from a phone…"),
            (11, "Quit Lantern"),
        ])
    );
    // The phone row is a checkmark, on while the phone is live.
    assert_eq!(tray.toggle_state(100).await, 1);
    let status: OwnedValue = tray
        .call(
            "/StatusNotifierItem",
            "org.freedesktop.DBus.Properties",
            "Get",
            &("org.kde.StatusNotifierItem", "Status"),
        )
        .await;
    assert_eq!(String::try_from(status).unwrap(), "Active");
    let menu: OwnedValue = tray
        .call(
            "/StatusNotifierItem",
            "org.freedesktop.DBus.Properties",
            "Get",
            &("org.kde.StatusNotifierItem", "Menu"),
        )
        .await;
    assert_eq!(
        zbus::zvariant::OwnedObjectPath::try_from(menu)
            .unwrap()
            .as_str(),
        "/MenuBar"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn click_phone_row_sends_connect_or_disconnect() {
    let mut tray = Tray::start().await;
    tray.click(100).await;
    assert_eq!(
        tray.command(Duration::from_secs(2)).await,
        Command::Disconnect {
            serial: "s1".into()
        }
    );
    // Switched off: the same row is unchecked and connects it.
    tray.view.send_replace(one(PhoneState::Off));
    wait_for_toggle(&tray, 100, 0).await;
    tray.click(100).await;
    assert_eq!(
        tray.command(Duration::from_secs(2)).await,
        Command::Connect {
            serial: "s1".into(),
            open: None
        }
    );
}

/// Waits until row `id`'s `toggle-state` is `want`.
async fn wait_for_toggle(tray: &Tray, id: i32, want: i32) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while tray.toggle_state(id).await != want {
        assert!(
            Instant::now() < deadline,
            "row {id} never had toggle-state {want}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn autoconnect_toggle_writes_settings() {
    let tray = Tray::start().await;
    let path = tray.settings_path.clone();
    let read = move || -> Option<bool> {
        let text = std::fs::read_to_string(&path).ok()?;
        lantern::settings::parse(&text)
            .ok()
            .map(|(s, _)| s.autoconnect)
    };
    assert_eq!(tray.toggle_state(14).await, 0, "autoconnect defaults off");
    tray.click(14).await;
    let r = read.clone();
    wait_for("autoconnect = true", Duration::from_secs(5), || {
        (r() == Some(true)).then_some(())
    })
    .await;
    wait_for_toggle(&tray, 14, 1).await;
    tray.click(14).await;
    wait_for("autoconnect = false", Duration::from_secs(5), || {
        (read() == Some(false)).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn toggle_writes_settings() {
    let tray = Tray::start().await;
    let path = tray.settings_path.clone();
    let read = move || -> Option<bool> {
        let text = std::fs::read_to_string(&path).ok()?;
        lantern::settings::parse(&text)
            .ok()
            .map(|(s, _)| s.clipboard_sync)
    };
    tray.click(5).await;
    let r = read.clone();
    wait_for("clipboard_sync = false", Duration::from_secs(5), || {
        (r() == Some(false)).then_some(())
    })
    .await;
    // The tray's menu shows it before the second click.
    tray.click(5).await;
    let r = read;
    wait_for("clipboard_sync = true", Duration::from_secs(5), || {
        (r() == Some(true)).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn view_change_emits_layout_updated() {
    let tray = Tray::start().await;
    let (before, _) = tray.layout().await;
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("com.canonical.dbusmenu")
        .unwrap()
        .member("LayoutUpdated")
        .unwrap()
        .build();
    let mut signals = MessageStream::for_match_rule(rule, &tray.client, None)
        .await
        .unwrap();
    tray.view.send_replace(one(PhoneState::Off));
    let signal = tokio::time::timeout(Duration::from_secs(2), signals.next())
        .await
        .expect("no LayoutUpdated within 2 s")
        .unwrap()
        .unwrap();
    let (revision, parent): (u32, i32) = signal.body().deserialize().unwrap();
    assert_eq!(parent, 0);
    assert!(revision > before, "{revision} after {before}");
    let (now, rows) = tray.layout().await;
    assert!(now >= revision);
    assert_eq!(rows[0], (100, "Test Phone".to_owned()));
    assert_eq!(tray.toggle_state(100).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn reregisters_when_watcher_returns() {
    let tray = Tray::start().await;
    assert!(tray.watcher.release_name(WATCHER).await.unwrap());
    tray.watcher.request_name(WATCHER).await.unwrap();
    let r = tray.registered.clone();
    let names = wait_for("second registration", Duration::from_secs(2), || {
        let names = r.lock().unwrap().clone();
        (names.len() >= 2).then_some(names)
    })
    .await;
    assert_eq!(names, vec![tray_name(), tray_name()]);
}

/// The Unlock row takes the notification action's path: the unlock prompt opens its PIN dialog
/// and hands the PIN on tagged with the row's phone; the tray never sees the PIN.
/// A second click while the dialog is open does nothing.
#[tokio::test(flavor = "multi_thread")]
async fn unlock_row_click_prompts() {
    let (request, requests) = unlock::Request::channel();
    let mut locked = ready();
    locked.phones[0].locked = true;
    let tray = Tray::start_with(locked, Some(request)).await;
    // The unlock prompt on the same private bus (no notification server there).
    let (events, events_rx) = mpsc::channel(8);
    let (pins_tx, mut pins) = mpsc::channel::<(String, Pin)>(1);
    let prompt = Prompt {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "sleep 0.5; echo 1234".into()],
    };
    let conn = tray._bus.connect().await;
    let task = tokio::spawn(unlock::run(
        conn,
        events_rx,
        requests,
        tray.view.subscribe(),
        pins_tx,
        PathBuf::from("/bin/false"),
        prompt,
    ));
    events
        .send(LockEvent::Locked {
            serial: "s1".into(),
            reason: LockReason::Clipboard,
        })
        .await
        .unwrap();
    let (_, rows) = tray.layout().await;
    assert_eq!(rows[0], (100, "Test Phone".to_owned()));
    assert_eq!(rows[1], (101, "Locked".to_owned()));
    assert_eq!(rows[2], (102, "Unlock Test Phone…".to_owned()));
    tray.click(102).await;
    tray.click(102).await;
    let (serial, pin) = tokio::time::timeout(Duration::from_secs(5), pins.recv())
        .await
        .expect("no PIN within 5 s")
        .unwrap();
    assert_eq!(serial, "s1");
    assert_eq!(pin, Pin::parse("1234".into()).unwrap());
    // The second click came while the dialog was open: no second dialog, no second PIN.
    if let Ok(more) = tokio::time::timeout(Duration::from_millis(1500), pins.recv()).await {
        panic!("a second PIN: {more:?}");
    }
    task.abort();
}

/// Two phones, only `b` locked: the Unlock row in b's slot (202) asks for b's dialog, and the
/// PIN comes back tagged `b`.
#[tokio::test(flavor = "multi_thread")]
async fn unlock_row_click_names_its_phone() {
    let (request, requests) = unlock::Request::channel();
    let mut view = ready();
    let mut b = view.phones[0].clone();
    b.serial = "b".into();
    b.name = Some("Other".into());
    b.locked = true;
    view.phones.push(b);
    let tray = Tray::start_with(view, Some(request)).await;
    let (events, events_rx) = mpsc::channel(8);
    let (pins_tx, mut pins) = mpsc::channel::<(String, Pin)>(1);
    let prompt = Prompt {
        program: "/bin/echo".into(),
        args: vec!["1234".into()],
    };
    let conn = tray._bus.connect().await;
    let task = tokio::spawn(unlock::run(
        conn,
        events_rx,
        requests,
        tray.view.subscribe(),
        pins_tx,
        PathBuf::from("/bin/false"),
        prompt,
    ));
    events
        .send(LockEvent::Locked {
            serial: "b".into(),
            reason: LockReason::Session,
        })
        .await
        .unwrap();
    let (_, rows) = tray.layout().await;
    assert_eq!(
        &rows[..4],
        owned(&[
            (100, "Test Phone"),
            (200, "Other"),
            (201, "Locked"),
            (202, "Unlock Other…"),
        ])
    );
    tray.click(202).await;
    let (serial, pin) = tokio::time::timeout(Duration::from_secs(5), pins.recv())
        .await
        .expect("no PIN within 5 s")
        .unwrap();
    assert_eq!(serial, "b");
    assert_eq!(pin, Pin::parse("1234".into()).unwrap());
    task.abort();
}

const CHILD: &str = "LANTERN_TRAY_TEST_CHILD";

/// Whether this process is a child [`run_child`] started.
fn is_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

/// Runs test `name` again in a child process with `envs` set (and a fresh scratch dir for
/// them, removed after), and asserts it passed; the child then runs the test's body. A child
/// keeps the process environment (PATH, the session bus) away from the other tests running in
/// this one.
fn run_child(name: &str, envs: impl FnOnce(&std::path::Path) -> Vec<(&'static str, String)>) {
    let dir = scratch();
    let envs = envs(&dir);
    let out = Process::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .envs(envs.iter().map(|(k, v)| (*k, v.as_str())))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "child failed:\n{stdout}\n{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(stdout.contains("1 passed"), "{stdout}");
}

/// A dead session bus address (a socket nobody listens on) in `dir`.
fn dead_bus(dir: &std::path::Path) -> String {
    let dead = dir.join("dead");
    drop(std::os::unix::net::UnixListener::bind(&dead).unwrap());
    format!("unix:path={}", dead.display())
}

/// The environment for a Remove-submenu child: a PATH whose `kdialog` appends its arguments to
/// `<dir>/kdialog.ran` (`$LANTERN_FAKE_KDIALOG_RAN`, which the tests assert, so a bypass that
/// reaches the real kdialog fails) and answers "Remove" (exit 0) after
/// `$LANTERN_FAKE_KDIALOG_SLEEP` seconds, whose `notify-send` appends its arguments to
/// `<dir>/notes`, and a dead session bus (nothing reaches the real desktop).
fn fake_desktop(dir: &std::path::Path, kdialog_sleep: &str) -> Vec<(&'static str, String)> {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let scripts = [
        (
            "kdialog",
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nsleep \"${{LANTERN_FAKE_KDIALOG_SLEEP:-0}}\"\nexit 0\n",
                dir.join("kdialog.ran").display()
            ),
        ),
        (
            "notify-send",
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\n",
                dir.join("notes").display()
            ),
        ),
    ];
    for (name, text) in scripts {
        let path = bin.join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    vec![
        ("PATH", path),
        ("DBUS_SESSION_BUS_ADDRESS", dead_bus(dir)),
        ("LANTERN_FAKE_KDIALOG_SLEEP", kdialog_sleep.to_owned()),
        (
            "LANTERN_FAKE_KDIALOG_RAN",
            dir.join("kdialog.ran").display().to_string(),
        ),
        (
            "LANTERN_FAKE_NOTES",
            dir.join("notes").display().to_string(),
        ),
    ]
}

/// The Remove submenu: `GetLayout(10)` gives its rows, nested ids answer `GetProperty` and
/// `GetGroupProperties`, and a click on a phone's entry goes through the kdialog confirm (faked:
/// "Remove") to `UndoSetup{serial}`.
#[test]
fn submenu_layout_and_click() {
    if !is_child() {
        return run_child("submenu_layout_and_click", |dir| fake_desktop(dir, "0"));
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let mut tray = Tray::start().await;
        let (_, submenu) = tray.get_layout(10).await;
        assert_eq!(submenu.string("children-display"), "submenu");
        assert_eq!(submenu.string("label"), "Remove Lantern from a phone…");
        assert_eq!(submenu.labels(), owned(&[(150, "Test Phone")]));
        // The root's layout carries the submenu's rows too.
        let (_, root) = tray.get_layout(0).await;
        let row = root.children.iter().find(|r| r.id == 10).unwrap();
        assert_eq!(row.labels(), owned(&[(150, "Test Phone")]));
        let label: OwnedValue = tray
            .call(
                "/MenuBar",
                "com.canonical.dbusmenu",
                "GetProperty",
                &(150i32, "label"),
            )
            .await;
        assert_eq!(String::try_from(label).unwrap(), "Test Phone");
        let group: Vec<(i32, HashMap<String, OwnedValue>)> = tray
            .call(
                "/MenuBar",
                "com.canonical.dbusmenu",
                "GetGroupProperties",
                &(vec![150i32], vec!["enabled".to_owned()]),
            )
            .await;
        assert_eq!(group.len(), 1);
        assert_eq!(group[0].0, 150);
        assert!(bool::try_from(group[0].1["enabled"].try_clone().unwrap()).unwrap());
        tray.click(150).await;
        assert_eq!(
            tray.command(Duration::from_secs(5)).await,
            Command::UndoSetup {
                serial: "s1".into()
            }
        );
        assert_fake_kdialog_ran();
    });
}

/// The fake `kdialog` ran (not the real one, which would pop a dialog and hang the test), and
/// it was asked about the phone that was clicked.
fn assert_fake_kdialog_ran() {
    let marker = PathBuf::from(std::env::var("LANTERN_FAKE_KDIALOG_RAN").unwrap());
    let args = std::fs::read_to_string(&marker).expect("the fake kdialog never ran");
    assert!(args.contains("--warningcontinuecancel"), "{args}");
    assert!(args.contains("Remove Lantern from Test Phone?"), "{args}");
}

/// R9: the confirm is pinned to the phone it named. Unplugged while kdialog was open, the
/// "Remove" answer sends nothing and says so.
#[test]
fn remove_cancelled_when_the_phone_was_unplugged() {
    if !is_child() {
        let name = "remove_cancelled_when_the_phone_was_unplugged";
        return run_child(name, |dir| fake_desktop(dir, "1"));
    }
    let notes = PathBuf::from(std::env::var("LANTERN_FAKE_NOTES").unwrap());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let mut tray = Tray::start().await;
        tray.click(150).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        tray.view.send_modify(|view| {
            view.phones[0].state = PhoneState::Unplugged;
            view.phones[0].plugged = false;
            view.phones[0].companion = None;
        });
        if let Ok(cmd) = tokio::time::timeout(Duration::from_secs(3), tray.commands.recv()).await {
            panic!("a command after the phone was unplugged: {cmd:?}");
        }
        let text = wait_for("the cancelled notice", Duration::from_secs(2), || {
            std::fs::read_to_string(&notes).ok()
        })
        .await;
        assert!(text.contains("Undo phone setup cancelled"), "{text}");
        assert!(
            text.contains("Test Phone was unplugged while Lantern was asking"),
            "{text}"
        );
        assert_fake_kdialog_ran();
    });
}

#[test]
fn no_bus_leaves_daemon_running() {
    if !is_child() {
        return run_child("no_bus_leaves_daemon_running", |dir| {
            vec![("DBUS_SESSION_BUS_ADDRESS", dead_bus(dir))]
        });
    }
    // In the child: the session bus address is a socket nobody listens on.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (_view, view_rx) = watch::channel(ready());
        let (tx, _rx) = mpsc::channel(8);
        let settings = SettingsHandle::new(None);
        assert!(lantern::tray::spawn(view_rx, settings, tx, None)
            .await
            .is_none());
    });
}
