//! The tray against a private `dbus-daemon` (never the real session bus): a fake
//! StatusNotifierWatcher records registrations, and a client calls the menu like Plasma does.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command as Process, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use lantern::daemon::control::{Command, Phone, View};
use lantern::settings::SettingsHandle;
use lantern_proto::CompanionState;
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
    fn start() -> Self {
        let dir = scratch();
        let mut child = Process::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .arg(format!("--address=unix:dir={}", dir.display()))
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

fn ready() -> View {
    View {
        phone: Phone::Ready {
            serial: "s1".into(),
            companion: Some((CompanionState::Ready, None)),
        },
        name: Some("Test Phone".into()),
        locked: false,
    }
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
        let (view, view_rx) = watch::channel(ready());
        let (tx, commands) = mpsc::channel(8);
        let conn = bus.connect().await;
        let task = tokio::spawn(lantern::tray::run(conn, view_rx, settings, tx));
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

    /// `GetLayout(0, -1, [])`: the revision and `(id, label)` per row (`""` for separators).
    async fn layout(&self) -> (u32, Vec<(i32, String)>) {
        type Layout = (u32, (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>));
        let (revision, (root, props, children)): Layout = self
            .call(
                "/MenuBar",
                "com.canonical.dbusmenu",
                "GetLayout",
                &(0i32, -1i32, Vec::<String>::new()),
            )
            .await;
        assert_eq!(root, 0);
        assert_eq!(
            String::try_from(props["children-display"].try_clone().unwrap()).unwrap(),
            "submenu"
        );
        let rows = children
            .into_iter()
            .map(|child| {
                let (id, props, _): (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>) =
                    Value::from(child).try_into().unwrap();
                let label = props
                    .get("label")
                    .map(|l| String::try_from(l.try_clone().unwrap()).unwrap())
                    .unwrap_or_default();
                (id, label)
            })
            .collect();
        (revision, rows)
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
            (1, "Connected to Test Phone"),
            (3, "Disconnect"),
            (4, ""),
            (5, "Sync clipboard"),
            (
                6,
                "Type with the Lantern keyboard (from the next connection)"
            ),
            (8, "Open settings folder"),
            (9, ""),
            (10, "Undo phone setup…"),
            (11, "Quit Lantern"),
        ])
    );
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
async fn click_sends_command() {
    let mut tray = Tray::start().await;
    tray.click(3).await;
    let cmd = tokio::time::timeout(Duration::from_secs(2), tray.commands.recv())
        .await
        .expect("no command within 2 s");
    assert_eq!(cmd, Some(Command::Disconnect));
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
    tray.view.send_replace(View {
        phone: Phone::Paused {
            serial: "s1".into(),
        },
        name: Some("Test Phone".into()),
        locked: false,
    });
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
    assert_eq!(rows[0], (1, "Disconnected from Test Phone".to_owned()));
    assert_eq!(rows[1], (3, "Connect".to_owned()));
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

const CHILD: &str = "LANTERN_TRAY_TEST_CHILD";

#[test]
fn no_bus_leaves_daemon_running() {
    if std::env::var_os(CHILD).is_some() {
        // In the child: the session bus address is a socket nobody listens on.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (_view, view_rx) = watch::channel(ready());
            let (tx, _rx) = mpsc::channel(8);
            let settings = SettingsHandle::new(None);
            assert!(lantern::tray::spawn(view_rx, settings, tx).await.is_none());
        });
        return;
    }
    let dir = scratch();
    let dead = dir.join("dead");
    drop(std::os::unix::net::UnixListener::bind(&dead).unwrap());
    let out = Process::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "no_bus_leaves_daemon_running",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}", dead.display()),
        )
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "child failed:\n{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "{stdout}");
}
