//! Lantern's system tray icon and menu: a StatusNotifierItem ([`sni`]) with a dbusmenu
//! ([`dbusmenu`]) on the session bus, built from the daemon's [`View`] and the settings.
//!
//! The tray is optional: without a session bus lanternd runs as before ([`spawn`] logs one
//! warning). It never blocks the daemon: it only reads watches and `try_send`s commands, and
//! slow work (files, `systemctl`, `kdialog`, `xdg-open`) runs in its own tasks.

pub mod dbusmenu;
pub mod menu;
pub mod sni;

use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::JoinHandle;
use zbus::proxy::{CacheProperties, MethodFlags};
use zbus::Connection;

use crate::daemon::control::{Command, View};
use crate::settings::{self, Key, Settings, SettingsHandle};
use menu::{Action, Item};

pub(crate) const MENU_PATH: &str = "/MenuBar";
const SNI_PATH: &str = "/StatusNotifierItem";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
/// The systemd user unit "Start Lantern at login" enables.
const UNIT: &str = "lanternd.service";
/// How long a D-Bus call to the watcher, the session bus connection or `systemctl` may take.
const TIMEOUT: Duration = Duration::from_secs(5);

/// What the menu shows, shared by the D-Bus objects and [`run`].
pub(crate) struct State {
    pub(crate) view: View,
    settings: Settings,
    settings_error: bool,
    /// Whether lanternd starts at login; `None` if systemctl can't tell.
    login: Option<bool>,
    /// The dbusmenu layout revision, bumped whenever `items` changes.
    pub(crate) revision: u32,
    pub(crate) items: Vec<Item>,
}

impl State {
    fn rebuild(&mut self) -> bool {
        let items = menu::items(&self.view, &self.settings, self.login, self.settings_error);
        if items == self.items {
            return false;
        }
        self.items = items;
        self.revision += 1;
        true
    }
}

pub(crate) struct Shared {
    state: Mutex<State>,
    commands: mpsc::Sender<Command>,
    settings: SettingsHandle,
    /// Woken after a login toggle changed `State::login`.
    login_changed: Notify,
}

impl Shared {
    /// Never held across an await.
    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The tray's bus name for this process.
fn bus_name() -> String {
    format!("org.kde.StatusNotifierItem-{}-1", std::process::id())
}

/// Connects to the session bus and runs the tray on it. Without a session bus it logs one
/// warning (`no tray: <error>`) and returns `None`; lanternd runs on without a tray.
pub async fn spawn(
    view: watch::Receiver<View>,
    settings: SettingsHandle,
    commands: mpsc::Sender<Command>,
) -> Option<JoinHandle<()>> {
    let conn = match tokio::time::timeout(TIMEOUT, Connection::session()).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(error)) => {
            tracing::warn!("no tray: {error}");
            return None;
        }
        Err(_) => {
            tracing::warn!("no tray: the session bus did not answer within {TIMEOUT:?}");
            return None;
        }
    };
    Some(tokio::spawn(async move {
        if let Err(error) = run(conn, view, settings, commands).await {
            tracing::warn!(error = format!("{error:#}"), "tray stopped");
        }
    }))
}

/// Serves the tray on `conn` until the daemon's `view` sender is dropped:
/// 1. serves [`sni`] and [`dbusmenu`] and requests `org.kde.StatusNotifierItem-<pid>-1`;
/// 2. registers that name with the StatusNotifierWatcher;
/// 3. registers again whenever the watcher gets a new owner (e.g. plasmashell restarted).
///
/// Every view, settings or login change that alters the menu emits `LayoutUpdated`.
pub async fn run(
    conn: Connection,
    mut view: watch::Receiver<View>,
    settings: SettingsHandle,
    commands: mpsc::Sender<Command>,
) -> Result<()> {
    let mut values = settings.subscribe();
    let mut errors = settings.subscribe_error();
    let login = login_state().await;
    let mut state = State {
        view: view.borrow_and_update().clone(),
        settings: values.borrow_and_update().clone(),
        settings_error: errors.borrow_and_update().is_some(),
        login,
        revision: 0,
        items: Vec::new(),
    };
    state.rebuild();
    let shared = Arc::new(Shared {
        state: Mutex::new(state),
        commands,
        settings,
        login_changed: Notify::new(),
    });
    let server = conn.object_server();
    server
        .at(
            MENU_PATH,
            dbusmenu::Menu {
                shared: shared.clone(),
            },
        )
        .await?;
    server
        .at(
            SNI_PATH,
            sni::Sni {
                shared: shared.clone(),
            },
        )
        .await?;
    let name = bus_name();
    conn.request_name(name.as_str())
        .await
        .with_context(|| format!("request the bus name {name}"))?;
    // Subscribed before the first registration, so a watcher that appears in between is seen.
    let bus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut owners = bus
        .receive_name_owner_changed_with_args(&[(0, WATCHER)])
        .await?;
    register(&conn, &name, true).await;
    loop {
        tokio::select! {
            changed = view.changed() => if changed.is_err() {
                tracing::debug!("daemon view closed; tray ends");
                return Ok(());
            },
            _ = values.changed() => {}
            _ = errors.changed() => {}
            _ = shared.login_changed.notified() => {}
            signal = owners.next() => {
                let Some(signal) = signal else {
                    bail!("NameOwnerChanged stream ended");
                };
                match signal.args() {
                    Ok(args) if args.new_owner().is_some() => {
                        tracing::info!("StatusNotifierWatcher appeared; registering the tray");
                        register(&conn, &name, false).await;
                    }
                    Ok(_) => tracing::info!("StatusNotifierWatcher went away"),
                    Err(error) => tracing::warn!(%error, "bad NameOwnerChanged signal"),
                }
                continue;
            }
        }
        let now = view.borrow_and_update().clone();
        let current = values.borrow_and_update().clone();
        let error = errors.borrow_and_update().is_some();
        refresh(&conn, &shared, now, current, error).await;
    }
}

/// Applies a new view and settings: `LayoutUpdated` if the menu changed, and the
/// StatusNotifierItem signals for a changed status, overlay or tooltip.
async fn refresh(conn: &Connection, shared: &Shared, view: View, values: Settings, error: bool) {
    let (layout, status, overlay, tooltip, revision) = {
        let mut state = shared.state();
        let before = (
            menu::sni_status(&state.view),
            menu::overlay(&state.view),
            menu::status_line(&state.view),
        );
        state.view = view;
        state.settings = values;
        state.settings_error = error;
        let layout = state.rebuild();
        let status = menu::sni_status(&state.view);
        (
            layout,
            (before.0 != status).then_some(status),
            before.1 != menu::overlay(&state.view),
            before.2 != menu::status_line(&state.view),
            state.revision,
        )
    };
    let result: zbus::Result<()> = async {
        let server = conn.object_server();
        if layout {
            let menu = server.interface::<_, dbusmenu::Menu>(MENU_PATH).await?;
            dbusmenu::Menu::layout_updated(menu.signal_emitter(), revision, 0).await?;
        }
        let sni = server.interface::<_, sni::Sni>(SNI_PATH).await?;
        let emitter = sni.signal_emitter();
        if let Some(status) = status {
            sni::Sni::new_status(emitter, status).await?;
        }
        if overlay {
            sni::Sni::new_overlay_icon(emitter).await?;
        }
        if tooltip {
            sni::Sni::new_tool_tip(emitter).await?;
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, "tray: could not signal a menu change");
    }
}

/// `RegisterStatusNotifierItem(name)` on the watcher (never auto-started). A watcher missing
/// at start is logged once at info level; the tray registers when one appears.
async fn register(conn: &Connection, name: &str, first: bool) {
    let result = async {
        let proxy: zbus::Proxy<'_> = zbus::proxy::Builder::new(conn)
            .destination(WATCHER)?
            .path(WATCHER_PATH)?
            .interface(WATCHER)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        proxy
            .call_with_flags::<_, _, ()>(
                "RegisterStatusNotifierItem",
                MethodFlags::NoAutoStart.into(),
                &(name,),
            )
            .await
            .map(|_| ())
    };
    match tokio::time::timeout(TIMEOUT, result).await {
        Ok(Ok(())) => tracing::info!(name, "tray registered"),
        Ok(Err(zbus::Error::MethodError(error, _, _)))
            if first && error.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown" =>
        {
            tracing::info!("no StatusNotifierWatcher yet; the tray icon appears when one starts")
        }
        Ok(Err(error)) => tracing::warn!(%error, "tray: StatusNotifierWatcher registration failed"),
        Err(_) => tracing::warn!("tray: StatusNotifierWatcher did not answer within {TIMEOUT:?}"),
    }
}

/// `systemctl --user is-enabled lanternd.service`: `Some(stdout == "enabled")`, or `None` if
/// systemctl fails or says nothing.
async fn login_state() -> Option<bool> {
    let out = systemctl(&["is-enabled", UNIT]).await?;
    let state = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!state.is_empty()).then(|| state == "enabled")
}

/// `systemctl --user <args>` with [`TIMEOUT`]; `None` if it can't run or times out.
async fn systemctl(args: &[&str]) -> Option<std::process::Output> {
    let run = tokio::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(TIMEOUT, run).await {
        Ok(Ok(out)) => Some(out),
        Ok(Err(error)) => {
            tracing::warn!(%error, ?args, "could not run systemctl");
            None
        }
        Err(_) => {
            tracing::warn!(?args, "systemctl timed out");
            None
        }
    }
}

/// Sends `cmd` to the daemon without waiting: a full channel drops the click.
fn send(shared: &Shared, cmd: Command) {
    match shared.commands.try_send(cmd) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(?cmd, "tray: the daemon is busy; click dropped")
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::warn!(?cmd, "tray: the daemon no longer takes commands")
        }
    }
}

/// Does what a menu click asks. Returns at once; slow work runs in its own task.
pub(crate) fn perform(shared: &Arc<Shared>, action: Action) {
    match action {
        Action::Connect => send(shared, Command::Connect),
        Action::Disconnect => send(shared, Command::Disconnect),
        Action::Quit => send(shared, Command::Quit),
        Action::ToggleClipboard => toggle(shared, Key::ClipboardSync),
        Action::ToggleKeyboard => toggle(shared, Key::LanternKeyboard),
        Action::ToggleLogin => {
            let shared = shared.clone();
            tokio::spawn(async move { toggle_login(&shared).await });
        }
        Action::OpenSettings => {
            tokio::spawn(open_settings(shared.settings.clone()));
        }
        Action::UndoSetup => {
            let shared = shared.clone();
            tokio::spawn(async move { confirm_undo(&shared).await });
        }
    }
}

/// Flips a setting in the file (off the async workers). An error is shown as a notification.
fn toggle(shared: &Shared, key: Key) {
    let settings = shared.settings.clone();
    tokio::task::spawn_blocking(move || {
        let current = settings.get();
        let value = match key {
            Key::ClipboardSync => !current.clipboard_sync,
            Key::LanternKeyboard => !current.lantern_keyboard,
        };
        if let Err(error) = settings.set(key, value) {
            let error = format!("{error:#}");
            tracing::warn!(key = key.name(), error, "tray: could not save the setting");
            crate::notify::send("Lantern couldn't save its settings", &error, None);
        }
    });
}

/// `systemctl --user enable|disable lanternd.service`, then reads `is-enabled` again.
async fn toggle_login(shared: &Shared) {
    let enable = shared.state().login != Some(true);
    let verb = if enable { "enable" } else { "disable" };
    match systemctl(&[verb, UNIT]).await {
        Some(out) if out.status.success() => tracing::info!("tray: {verb}d {UNIT}"),
        Some(out) => tracing::warn!(
            status = %out.status,
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "tray: systemctl {verb} {UNIT} failed"
        ),
        None => {}
    }
    let login = login_state().await;
    shared.state().login = login;
    shared.login_changed.notify_one();
}

/// Writes the settings file if it is missing, then opens its folder.
async fn open_settings(settings: SettingsHandle) {
    let Some(path) = settings.path().map(ToOwned::to_owned) else {
        tracing::warn!("tray: no settings file (no config directory)");
        return;
    };
    // File IO and desktop::spawn's systemctl lookup block: keep them off the async workers.
    let spawned = tokio::task::spawn_blocking(move || {
        if let Err(error) = settings::ensure_file(&path) {
            let error = format!("{error:#}");
            tracing::warn!(error, "tray: could not write the settings file");
            crate::notify::send("Lantern couldn't save its settings", &error, None);
        }
        let dir = path
            .parent()
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        crate::desktop::spawn("xdg-open", &[&dir])
    })
    .await;
    match spawned {
        Ok(Ok(mut child)) => {
            let _ = child.wait().await;
        }
        Ok(Err(error)) => tracing::warn!(%error, "tray: could not run xdg-open"),
        Err(error) => tracing::warn!(%error, "tray: open settings task failed"),
    }
}

/// The Undo confirmation text for the phone called `name`.
fn undo_question(name: &str) -> String {
    format!(
        "Remove Lantern from {name}? This puts back the phone's keyboard and settings, uninstalls \
         the Lantern companion app and removes {name}'s apps from the Phone menu. Plug the phone \
         in again to set it up again."
    )
}

/// Asks with kdialog; on "Remove" (exit 0) sends `Command::UndoSetup`. Without kdialog it
/// only shows a notification.
async fn confirm_undo(shared: &Shared) {
    let name = menu::name(&shared.state().view).to_owned();
    let question = undo_question(&name);
    let spawned = tokio::task::spawn_blocking(move || {
        crate::desktop::spawn(
            "kdialog",
            &[
                "--title",
                "Lantern",
                "--continue-label",
                "Remove",
                "--warningcontinuecancel",
                &question,
            ],
        )
    })
    .await;
    let mut child = match spawned {
        Ok(Ok(child)) => child,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!("tray: kdialog is missing; undo-setup not asked");
            crate::notify::send(
                "Lantern can't ask before undoing phone setup",
                "Install kdialog to use Undo phone setup from the tray.",
                None,
            );
            return;
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "tray: could not run kdialog");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "tray: kdialog task failed");
            return;
        }
    };
    match child.wait().await {
        Ok(status) if status.code() == Some(0) => send(shared, Command::UndoSetup),
        Ok(_) => tracing::info!("tray: undo-setup cancelled"),
        Err(error) => tracing::warn!(%error, "tray: kdialog wait failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_question_text() {
        assert_eq!(
            undo_question("Pixel"),
            "Remove Lantern from Pixel? This puts back the phone's keyboard and settings, \
             uninstalls the Lantern companion app and removes Pixel's apps from the Phone menu. \
             Plug the phone in again to set it up again."
        );
    }

    #[tokio::test]
    async fn full_channel_drops_the_click() {
        let (tx, mut rx) = mpsc::channel(8);
        let shared = Shared {
            state: Mutex::new(State {
                view: View {
                    phone: crate::daemon::control::Phone::None,
                    name: None,
                },
                settings: Settings::default(),
                settings_error: false,
                login: None,
                revision: 0,
                items: Vec::new(),
            }),
            commands: tx,
            settings: SettingsHandle::new(None),
            login_changed: Notify::new(),
        };
        for _ in 0..10 {
            send(&shared, Command::Connect);
        }
        let mut got = 0;
        while rx.try_recv().is_ok() {
            got += 1;
        }
        assert_eq!(got, 8);
    }
}
