//! Shared by the notification tests (`tests/unlock.rs`, `tests/connect.rs`): a private
//! `dbus-daemon` with no service directories (never the real session bus) and a fake
//! `org.freedesktop.Notifications` that records `Notify`/`CloseNotification` and emits
//! `ActionInvoked`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command as Process, Stdio};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zbus::zvariant::OwnedValue;
use zbus::{connection, Connection};

static DIRS: AtomicUsize = AtomicUsize::new(0);

/// A fresh scratch directory under `$TMPDIR`, named after the test binary's `prefix`.
pub fn scratch(prefix: &str) -> PathBuf {
    let n = DIRS.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("lantern-{prefix}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A private session bus in a scratch directory, killed (and the directory removed) on drop.
pub struct Bus {
    child: Child,
    pub dir: PathBuf,
    pub address: String,
}

impl Bus {
    /// A session bus with no service directories: nothing is ever bus-activated on it (the
    /// stock session config would start Plasma's notification service for a missing server).
    pub fn start() -> Self {
        let dir = scratch("bus");
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

    pub async fn connect(&self) -> Connection {
        connection::Builder::address(self.address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    }

    /// The fake notification server on its own connection, recording into `calls`.
    pub async fn serve_notes(&self, calls: Arc<Mutex<Vec<Call>>>) -> Connection {
        connection::Builder::address(self.address.as_str())
            .unwrap()
            .serve_at(
                NOTES_PATH,
                Notes {
                    calls,
                    next: AtomicU32::new(0),
                },
            )
            .unwrap()
            .name(NOTES)
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
pub enum Call {
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
pub struct Notes {
    pub calls: Arc<Mutex<Vec<Call>>>,
    pub next: AtomicU32,
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

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "fake".into(),
            "lantern-tests".into(),
            "1".into(),
            "1.2".into(),
        )
    }
}

pub const NOTES: &str = "org.freedesktop.Notifications";
pub const NOTES_PATH: &str = "/org/freedesktop/Notifications";

/// Emits `ActionInvoked(id, action)` from `conn`, as Plasma does on a click.
pub async fn emit_action(conn: &Connection, id: u32, action: &str) {
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

pub async fn wait_for<T>(what: &str, within: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(Instant::now() < deadline, "{what}: not within {within:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
