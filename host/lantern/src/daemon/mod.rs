pub mod catalog;
pub mod companion;
pub mod control;
pub mod ipc;
pub mod registry;
pub mod session;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use futures_core::Stream;
use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use ipc::Hub;
use lantern_proto::SessionState;
use session::{Inbound, SessionConn};
use tokio::net::UnixListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::adb::{track_devices, Adb, DeviceEvent};
use crate::clipboard::{PcClipboard, PcOffer};
use crate::daemon::companion::Names;
use crate::daemon::control::{Command, Notifier, Phone, UndoRunner, View};
use crate::daemon::session::{start_with_companion, HandshakeError};
use crate::settings::{self, SettingsHandle};

/// The phone to serve: `configured` (`$LANTERN_SERIAL`) if set, else `current` (the live
/// session's phone) while it is online, else the first of `online` (kept in the order devices
/// came online).
fn select_serial(
    configured: Option<&str>,
    online: &[String],
    current: Option<&str>,
) -> Option<String> {
    if let Some(serial) = configured {
        return Some(serial.to_owned());
    }
    if let Some(current) = current.filter(|c| online.iter().any(|s| s == c)) {
        return Some(current.to_owned());
    }
    online.first().cloned()
}

/// Backoff for restarting a session after a failed start or helper death, while the target
/// device stays connected.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Delay before the first retry; doubles per consecutive failure.
    pub initial: Duration,
    pub max: Duration,
    /// A session that lived at least this long resets the failure count when it dies.
    pub stable_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            stable_after: Duration::from_secs(60),
        }
    }
}

impl RetryPolicy {
    fn delay(&self, failures: u32) -> Duration {
        let shift = failures.saturating_sub(1).min(16);
        self.initial.saturating_mul(1 << shift).min(self.max)
    }
}

/// Ends the active session, if any. Every window client is closed with it: windows belong to
/// one helper session and cannot survive a restart.
async fn close_session(session: &mut Option<(SessionConn, Instant)>, hub: &mut Hub) {
    hub.reset();
    if let Some((session, _)) = session.take() {
        let serial = session.serial().to_owned();
        session.close().await;
        tracing::info!(serial, "session closed");
    }
}

type Starting<F> = Option<(String, Pin<Box<F>>)>;

fn cancel_start<F>(starting: &mut Starting<F>, why: &str) {
    if let Some((serial, future)) = starting.take() {
        // Dropping the start future drops its PendingSession, which kills the helper and
        // removes the forward.
        drop(future);
        tracing::info!(serial, why, "session start cancelled");
    }
}

/// The daemon's optional parts; each one left `None` is simply off.
#[derive(Default)]
pub struct Parts {
    /// The phone's apps as menu entries, kept in step with the helper's
    /// `app`/`apps_end`/`app_removed` messages.
    pub catalog: Option<catalog::Catalog>,
    /// The PC clipboard, synced with the phone's.
    pub clipboard: Option<PcClipboard>,
    /// The user's settings (clipboard sync on/off).
    pub settings: Option<SettingsHandle>,
    /// Commands from the tray menu.
    pub commands: Option<mpsc::Receiver<Command>>,
    /// Gets the daemon's [`View`] after every change.
    pub view: Option<watch::Sender<View>>,
    /// Phone names for the view and notifications (else the serial).
    pub names: Option<Names>,
    /// Runs "Undo phone setup" (without it that command is ignored).
    pub undo: Option<UndoRunner>,
    /// Desktop notifications (the undo result).
    pub notify: Option<Notifier>,
}

/// The parts of [`Parts`] the run loop itself uses.
#[derive(Default)]
struct Controls {
    commands: Option<mpsc::Receiver<Command>>,
    view: Option<watch::Sender<View>>,
    names: Option<Names>,
    undo: Option<UndoRunner>,
    notify: Option<Notifier>,
}

/// [`serve`] with the daemon's optional [`Parts`].
pub async fn serve_with<E, S, F, D>(
    listener: UnixListener,
    parts: Parts,
    configured: Option<String>,
    events: E,
    starter: S,
    stop: D,
    policy: RetryPolicy,
) -> Result<()>
where
    E: Stream<Item = DeviceEvent>,
    S: FnMut(String) -> F,
    F: Future<Output = Result<SessionConn>>,
    D: Future<Output = &'static str>,
{
    let Parts {
        catalog,
        clipboard,
        settings,
        commands,
        view,
        names,
        undo,
        notify,
    } = parts;
    let mut hub = Hub::new(Some(listener));
    if let Some(catalog) = catalog {
        hub.set_catalog(catalog);
    }
    if let Some(settings) = settings {
        hub.set_settings(settings.subscribe());
    }
    let offers = clipboard.map(|clipboard| {
        hub.set_clipboard(clipboard.handle);
        clipboard.offers
    });
    let controls = Controls {
        commands,
        view,
        names,
        undo,
        notify,
    };
    let path = hub.socket_path();
    let result = drive(
        hub, offers, controls, configured, events, starter, stop, policy,
    )
    .await;
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// The daemon's state machine without a window socket: helper frames are drained and dropped.
/// See [`serve`].
pub async fn run_loop<E, S, F, D>(
    configured: Option<String>,
    events: E,
    starter: S,
    stop: D,
    policy: RetryPolicy,
) -> Result<()>
where
    E: Stream<Item = DeviceEvent>,
    S: FnMut(String) -> F,
    F: Future<Output = Result<SessionConn>>,
    D: Future<Output = &'static str>,
{
    drive(
        Hub::new(None),
        None,
        Controls::default(),
        configured,
        events,
        starter,
        stop,
        policy,
    )
    .await
}

/// The daemon: phone session management plus window clients on `listener`. Generic over its
/// inputs so tests can drive it with fake devices, fake helpers and a fake stop signal.
/// The socket file is removed when this returns.
pub async fn serve<E, S, F, D>(
    listener: UnixListener,
    configured: Option<String>,
    events: E,
    starter: S,
    stop: D,
    policy: RetryPolicy,
) -> Result<()>
where
    E: Stream<Item = DeviceEvent>,
    S: FnMut(String) -> F,
    F: Future<Output = Result<SessionConn>>,
    D: Future<Output = &'static str>,
{
    serve_with(
        listener,
        Parts::default(),
        configured,
        events,
        starter,
        stop,
        policy,
    )
    .await
}

/// The phone to start: [`select_serial`] over the online phones that are not `paused`, and
/// only if it is online.
fn target(
    configured: Option<&str>,
    connected: &[String],
    paused: Option<&str>,
    current: Option<&str>,
) -> Option<String> {
    let online: Vec<String> = connected
        .iter()
        .filter(|serial| Some(serial.as_str()) != paused)
        .cloned()
        .collect();
    select_serial(configured, &online, current).filter(|serial| online.contains(serial))
}

/// Phone names, read once per serial and again only when [`Self::refresh`]ed (on Online and
/// when a session becomes ready), never once per loop turn.
struct NameCache {
    names: Option<Names>,
    cache: HashMap<String, Option<String>>,
}

impl NameCache {
    fn get(&mut self, serial: &str) -> Option<String> {
        let names = self.names.as_ref()?;
        self.cache
            .entry(serial.to_owned())
            .or_insert_with(|| names(serial))
            .clone()
    }

    /// Reads `serial`'s name again (a session start may have just recorded it).
    fn refresh(&mut self, serial: &str) {
        self.cache.remove(serial);
        self.get(serial);
    }
}

/// The phone being undone and its undo.
type Undoing = Option<(String, BoxFuture<'static, (bool, String)>)>;

/// Every branch is polled concurrently: a session start in flight never blocks the stop signal,
/// device events or clients, and helper death is noticed through `SessionConn::closed`. The live
/// session's inbound frames are always drained (routed to window clients).
///
/// `paused` (Disconnect, Undo) is set before the session is closed. Nothing starts for a paused
/// phone until it goes offline or gets Connect, and nothing starts at all while an undo runs.
#[allow(clippy::too_many_arguments)]
async fn drive<E, S, F, D>(
    mut hub: Hub,
    mut offers: Option<mpsc::Receiver<PcOffer>>,
    controls: Controls,
    configured: Option<String>,
    events: E,
    mut starter: S,
    stop: D,
    policy: RetryPolicy,
) -> Result<()>
where
    E: Stream<Item = DeviceEvent>,
    S: FnMut(String) -> F,
    F: Future<Output = Result<SessionConn>>,
    D: Future<Output = &'static str>,
{
    tokio::pin!(events);
    tokio::pin!(stop);
    let Controls {
        mut commands,
        view,
        names,
        undo,
        notify,
    } = controls;
    let mut names = NameCache {
        names,
        cache: HashMap::new(),
    };
    // Online devices, in the order they came online.
    let mut connected: Vec<String> = Vec::new();
    let mut active: Option<(SessionConn, Instant)> = None;
    let mut starting: Starting<F> = None;
    let mut retry_at: Option<Instant> = None;
    let mut failures: u32 = 0;
    // The phone the user disconnected (or undid); never a start target while set.
    let mut paused: Option<String> = None;
    let mut undoing: Undoing = None;

    loop {
        let idle = active.is_none() && starting.is_none() && undoing.is_none();
        match (&active, &starting, &paused) {
            (Some((session, _)), _, _) => hub.set_session(
                SessionState::Ready,
                Some(session.serial().to_owned()),
                Some(session.helper_sha().to_owned()),
            ),
            (None, Some((serial, _)), _) => {
                hub.set_session(SessionState::Starting, Some(serial.clone()), None)
            }
            (None, None, Some(serial)) => {
                hub.set_session(SessionState::Paused, Some(serial.clone()), None)
            }
            (None, None, None) => hub.set_session(SessionState::Idle, None, None),
        }
        if let Some(view) = &view {
            let phone = if let Some((serial, _)) = &undoing {
                Phone::Undoing {
                    serial: serial.clone(),
                }
            } else if let Some((session, _)) = &active {
                Phone::Ready {
                    serial: session.serial().to_owned(),
                    companion: hub.companion(),
                }
            } else if let Some((serial, _)) = &starting {
                Phone::Starting {
                    serial: serial.clone(),
                }
            } else if let Some(serial) = retry_at
                .and_then(|_| target(configured.as_deref(), &connected, paused.as_deref(), None))
            {
                // A start due now (a phone just came online) is as good as starting.
                if failures > 0 {
                    Phone::Retrying { serial }
                } else {
                    Phone::Starting { serial }
                }
            } else if let Some(serial) = &paused {
                Phone::Paused {
                    serial: serial.clone(),
                }
            } else {
                Phone::None
            };
            let name = phone.serial().and_then(|serial| names.get(serial));
            let next = View { phone, name };
            view.send_if_modified(|current| {
                if *current == next {
                    return false;
                }
                tracing::debug!(?next, "view");
                *current = next;
                true
            });
        }
        let wake = retry_at.unwrap_or_else(Instant::now);
        // Set when the live session must be torn down and restarted.
        let mut ended: Option<&'static str> = None;
        // Set when the daemon must end (signal or Quit).
        let mut quit: Option<&'static str> = None;
        // Commands to apply after this turn's event.
        let mut queued: Vec<Command> = Vec::new();
        tokio::select! {
            why = &mut stop => quit = Some(why),
            cmd = async { commands.as_mut().expect("guarded").recv().await }, if commands.is_some() => {
                match cmd {
                    Some(Command::Quit) => quit = Some("Quit command"),
                    Some(cmd) => queued.push(cmd),
                    None => {
                        tracing::debug!("command channel closed");
                        commands = None;
                    }
                }
            }
            event = events.next() => {
                let Some(event) = event else {
                    cancel_start(&mut starting, "device stream ended");
                    close_session(&mut active, &mut hub).await;
                    if let Some((serial, undo)) = undoing.take() {
                        tracing::info!(serial, "waiting for undo-setup to finish");
                        let (ok, output) = undo.await;
                        undo_done(&serial, ok, &output, &mut names, notify.as_ref());
                    }
                    bail!("adb track-devices ended");
                };
                match event {
                    DeviceEvent::Online(serial) => {
                        tracing::info!(serial, "device online");
                        if !connected.contains(&serial) {
                            connected.push(serial.clone());
                        }
                        names.refresh(&serial);
                        if paused.as_deref() == Some(serial.as_str()) {
                            tracing::info!(serial, "phone is disconnected in Lantern; not starting");
                        }
                        let current = active.as_ref().map(|(s, _)| s.serial().to_owned());
                        let target = target(configured.as_deref(), &connected, paused.as_deref(), current.as_deref());
                        if idle && target.as_deref() == Some(serial.as_str()) {
                            failures = 0;
                            retry_at = Some(Instant::now());
                        }
                    }
                    DeviceEvent::Offline(serial) => {
                        tracing::info!(serial, "device offline");
                        let before = target(configured.as_deref(), &connected, paused.as_deref(), None);
                        connected.retain(|s| *s != serial);
                        if paused.as_deref() == Some(serial.as_str()) {
                            tracing::info!(serial, "unplugged: no longer disconnected in Lantern");
                            paused = None;
                        }
                        if starting.as_ref().map(|(s, _)| s.as_str()) == Some(serial.as_str()) {
                            cancel_start(&mut starting, "device offline");
                        }
                        if active.as_ref().map(|(s, _)| s.serial()) == Some(serial.as_str()) {
                            close_session(&mut active, &mut hub).await;
                        }
                        let current = active.as_ref().map(|(s, _)| s.serial().to_owned());
                        let after = target(configured.as_deref(), &connected, paused.as_deref(), current.as_deref());
                        let idle = active.is_none() && starting.is_none() && undoing.is_none();
                        match after {
                            None => {
                                retry_at = None;
                                failures = 0;
                            }
                            // The live (or backed-off) phone left: serve the next online one now.
                            Some(next) if idle && (retry_at.is_none() || before.as_ref() != Some(&next)) => {
                                tracing::info!(serial = next, "next online phone");
                                failures = 0;
                                retry_at = Some(Instant::now());
                            }
                            Some(_) => {}
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(wake), if idle && retry_at.is_some() => {
                retry_at = None;
                if let Some(serial) = target(configured.as_deref(), &connected, paused.as_deref(), None) {
                    tracing::info!(serial, attempt = failures + 1, "starting session");
                    starting = Some((serial.clone(), Box::pin(starter(serial))));
                }
            }
            result = async { starting.as_mut().expect("guarded").1.as_mut().await }, if starting.is_some() => {
                let (serial, _) = starting.take().expect("guarded");
                match result {
                    Ok(session) => {
                        tracing::info!(serial, helper = session.helper_sha(), "session ready");
                        hub.session_started(&serial);
                        // The start has just recorded the phone's name.
                        names.refresh(&serial);
                        active = Some((session, Instant::now()));
                    }
                    Err(error) if HandshakeError::find(&error).is_some() => {
                        // The embedded helper answers the same way on every attempt; retrying
                        // would only spin. Stop with the explained error (non-zero exit).
                        tracing::error!(serial, error = format!("{error:#}"), "helper protocol mismatch; not retrying");
                        close_session(&mut active, &mut hub).await;
                        return Err(error.context(format!("session start on {serial} failed")));
                    }
                    Err(error) => {
                        failures += 1;
                        let delay = policy.delay(failures);
                        tracing::error!(serial, error = format!("{error:#}"), retry_in = ?delay, "session start failed");
                        retry_at = Some(Instant::now() + delay);
                    }
                }
            }
            (ok, output) = async { undoing.as_mut().expect("guarded").1.as_mut().await }, if undoing.is_some() => {
                let (serial, _) = undoing.take().expect("guarded");
                undo_done(&serial, ok, &output, &mut names, notify.as_ref());
                // Paused until replugged or connected again (unless it was unplugged meanwhile).
                paused = connected.contains(&serial).then_some(serial);
            }
            event = hub.next_event() => {
                let helper = active.as_ref().map(|(session, _)| session.tx.clone());
                if hub.on_client(event, helper.as_ref()).await.is_err() {
                    ended = Some("helper stopped reading");
                }
                queued.extend(hub.take_commands().into_iter().map(Command::from));
            }
            offer = async { offers.as_mut().expect("guarded").recv().await }, if offers.is_some() => {
                match offer {
                    // Only while a session is live: nothing is queued for a later session.
                    Some(offer) => if let Some((session, _)) = active.as_ref() {
                        let helper = session.tx.clone();
                        if hub.on_pc_clip(offer, &helper).await.is_err() {
                            ended = Some("helper stopped reading");
                        }
                    },
                    None => {
                        tracing::warn!("PC clipboard thread ended; clipboard sync off");
                        offers = None;
                    }
                }
            }
            inbound = async { active.as_mut().expect("guarded").0.next().await }, if active.is_some() => {
                match inbound {
                    Inbound::Frame(frame) => {
                        let helper = active.as_ref().expect("guarded").0.tx.clone();
                        if hub.on_helper(frame, &helper).await.is_err() {
                            ended = Some("helper stopped reading");
                        }
                    }
                    Inbound::Ended(why) => ended = Some(why),
                }
            }
        }
        if let Some(why) = quit {
            tracing::info!("{why} received");
            cancel_start(&mut starting, why);
            close_session(&mut active, &mut hub).await;
            if let Some((serial, undo)) = undoing.take() {
                tracing::info!(serial, "waiting for undo-setup to finish before quitting");
                let (ok, output) = undo.await;
                undo_done(&serial, ok, &output, &mut names, notify.as_ref());
            }
            return Ok(());
        }
        if let Some(why) = ended {
            let lived = active
                .as_ref()
                .map(|(_, since)| since.elapsed())
                .unwrap_or_default();
            let serial = active
                .as_ref()
                .map(|(s, _)| s.serial().to_owned())
                .unwrap_or_default();
            tracing::warn!(serial, why, ?lived, "helper session ended unexpectedly");
            close_session(&mut active, &mut hub).await;
            if lived >= policy.stable_after {
                failures = 0;
            }
            failures += 1;
            let delay = policy.delay(failures);
            tracing::info!(serial, retry_in = ?delay, "restarting session");
            retry_at = Some(Instant::now() + delay);
        }
        for cmd in queued {
            if undoing.is_some() {
                tracing::info!(?cmd, "undo-setup running; command ignored");
                continue;
            }
            // The phone being served, started or retried.
            let live = active
                .as_ref()
                .map(|(session, _)| session.serial().to_owned())
                .or_else(|| starting.as_ref().map(|(serial, _)| serial.clone()))
                .or_else(|| {
                    retry_at.and_then(|_| {
                        target(configured.as_deref(), &connected, paused.as_deref(), None)
                    })
                });
            match cmd {
                Command::Disconnect => {
                    let Some(serial) = live else {
                        tracing::info!("disconnect: no phone is being served");
                        continue;
                    };
                    tracing::info!(serial, "disconnected in Lantern");
                    // Paused first: nothing may restart it while the session closes.
                    paused = Some(serial);
                    retry_at = None;
                    failures = 0;
                    cancel_start(&mut starting, "disconnected in Lantern");
                    // `bye`: the helper restores the phone.
                    close_session(&mut active, &mut hub).await;
                }
                Command::Connect => {
                    if let Some(serial) = paused.take() {
                        tracing::info!(serial, "connected in Lantern");
                    }
                    let current = active.as_ref().map(|(s, _)| s.serial().to_owned());
                    let idle = active.is_none() && starting.is_none();
                    if idle
                        && target(configured.as_deref(), &connected, None, current.as_deref())
                            .is_some()
                    {
                        failures = 0;
                        retry_at = Some(Instant::now());
                    }
                }
                Command::UndoSetup => {
                    let Some(run) = &undo else {
                        tracing::warn!("undo-setup is not available in this daemon");
                        continue;
                    };
                    let serial = live
                        .or_else(|| paused.clone())
                        .filter(|serial| connected.contains(serial));
                    let Some(serial) = serial else {
                        tracing::info!("undo-setup: no phone online");
                        continue;
                    };
                    tracing::info!(serial, "undo-setup requested");
                    paused = Some(serial.clone());
                    retry_at = None;
                    failures = 0;
                    cancel_start(&mut starting, "undo-setup");
                    // The helper's connection is closed before the undo runs; `run_steps`
                    // itself waits for helper processes still restoring the phone.
                    close_session(&mut active, &mut hub).await;
                    undoing = Some((serial.clone(), run(serial)));
                }
                Command::Quit => unreachable!("Quit is handled when it arrives"),
            }
        }
    }
}

/// Logs the undo's result and notifies the user (one body line per step).
fn undo_done(
    serial: &str,
    ok: bool,
    output: &str,
    names: &mut NameCache,
    notify: Option<&Notifier>,
) {
    if ok {
        tracing::info!(serial, "undo-setup done");
    } else {
        tracing::warn!(serial, output, "undo-setup failed");
    }
    let name = names.get(serial).unwrap_or_else(|| serial.to_owned());
    if let Some(notify) = notify {
        let (summary, body) = control::undo_notice(ok, &name, output);
        notify(&summary, &body);
    }
}

/// `lantern daemon` exit status when the helper breaks the protocol handshake. The embedded
/// helper answers the same way every time, so `lanternd.service` has
/// `RestartPreventExitStatus=3`: systemd must not respawn it.
pub const EXIT_PROTOCOL_MISMATCH: u8 = 3;

/// The `lantern daemon` exit status for a failed run: [`EXIT_PROTOCOL_MISMATCH`] for a
/// handshake protocol error, 1 for any other runtime failure (2 is reserved for usage errors).
pub fn exit_code(error: &anyhow::Error) -> u8 {
    if HandshakeError::find(error).is_some() {
        EXIT_PROTOCOL_MISMATCH
    } else {
        1
    }
}

/// "Undo phone setup" inside lanternd: [`crate::undo::run_steps`] on the serial, its output
/// lines collected for the notification.
fn undo_runner() -> UndoRunner {
    Arc::new(|serial: String| {
        Box::pin(async move {
            let Some(state_dir) = companion::state_dir() else {
                return (false, "no home directory for the state dir".to_owned());
            };
            // Menu-entry files and the menu cache rebuild block; keep them off the async workers.
            let remove_entries = |serial: &str| {
                tokio::task::block_in_place(|| crate::undo::remove_catalogue_entries(serial))
            };
            let mut out = Vec::new();
            let adb = Adb::new(serial);
            let ok = match crate::undo::run_steps(&adb, &state_dir, &remove_entries, &mut out).await
            {
                Ok(ok) => ok,
                Err(error) => {
                    out.extend(format!("undo: FAILED: {error:#}\n").bytes());
                    false
                }
            };
            (ok, String::from_utf8_lossy(&out).trim_end().to_owned())
        })
    })
}

pub async fn run() -> Result<()> {
    let configured = std::env::var("LANTERN_SERIAL")
        .ok()
        .filter(|s| !s.is_empty());
    let listener = ipc::bind(&ipc::runtime_dir()?)?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let shutdown = async move {
        tokio::select! {
            _ = sigint.recv() => "SIGINT",
            _ = sigterm.recv() => "SIGTERM",
        }
    };
    // Shared by every session start: the companion-failure notice is once per daemon run.
    let setup = std::sync::Arc::new(companion::Setup::desktop());
    let catalog = match catalog::Dirs::from_env() {
        Ok(dirs) => Some(catalog::Catalog::new(dirs)),
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "no menu entries for phone apps"
            );
            None
        }
    };
    // The PC clipboard: its own Wayland thread, never on this runtime.
    let (offer_tx, offers) = tokio::sync::mpsc::channel(16);
    let clipboard = PcClipboard {
        handle: crate::clipboard::wayland::spawn(offer_tx),
        offers,
    };
    let settings = SettingsHandle::new(settings::default_path());
    // Runs for the daemon's lifetime; the runtime ends it.
    settings.spawn_poll();
    let session_settings = settings.clone();
    serve_with(
        listener,
        Parts {
            catalog,
            clipboard: Some(clipboard),
            settings: Some(settings),
            names: Some(companion::names()),
            undo: Some(undo_runner()),
            notify: Some(Arc::new(|summary: &str, body: &str| {
                crate::notify::send(summary, body, None)
            })),
            // The tray (its command channel and view) is wired in by the tray task.
            ..Default::default()
        },
        configured,
        track_devices(),
        move |serial| {
            let setup = setup.clone();
            // Read at each session start: a changed `lantern_keyboard` applies from the next one.
            let keyboard = session_settings.get().lantern_keyboard;
            async move { start_with_companion(&Adb::new(serial), &setup, keyboard).await }
        },
        shutdown,
        RetryPolicy::default(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn online(serials: &[&str]) -> Vec<String> {
        serials.iter().map(|serial| (*serial).to_owned()).collect()
    }

    #[test]
    fn configured_serial_wins_over_single_device_default() {
        assert_eq!(
            select_serial(Some("chosen"), &online(&["only"]), None),
            Some("chosen".into())
        );
    }

    #[test]
    fn unset_serial_uses_only_device() {
        assert_eq!(
            select_serial(None, &online(&["only"]), None),
            Some("only".into())
        );
    }

    #[test]
    fn select_serial_prefers_current_then_first_online() {
        assert_eq!(
            select_serial(None, &online(&["b", "a"]), Some("a")),
            Some("a".into())
        );
        assert_eq!(
            select_serial(None, &online(&["b", "a"]), None),
            Some("b".into())
        );
        assert_eq!(select_serial(None, &online(&[]), None), None);
        assert_eq!(
            select_serial(Some("x"), &online(&["a"]), None),
            Some("x".into())
        );
        // A current phone that went offline gives way to the first online one.
        assert_eq!(
            select_serial(None, &online(&["b"]), Some("a")),
            Some("b".into())
        );
    }

    #[test]
    fn target_skips_the_paused_phone() {
        let both = online(&["a", "b"]);
        assert_eq!(target(None, &both, None, None), Some("a".into()));
        assert_eq!(target(None, &both, Some("a"), None), Some("b".into()));
        assert_eq!(target(None, &both, Some("a"), Some("a")), Some("b".into()));
        assert_eq!(target(None, &online(&["a"]), Some("a"), None), None);
        // A configured phone is the only target: paused or offline means none.
        assert_eq!(target(Some("a"), &both, Some("a"), None), None);
        assert_eq!(target(Some("x"), &both, None, None), None);
        assert_eq!(target(Some("b"), &both, Some("a"), None), Some("b".into()));
    }

    #[test]
    fn exit_code_separates_protocol_mismatch_from_runtime_failures() {
        let mismatch = anyhow::Error::new(HandshakeError::VersionMismatch {
            helper: 2,
            expected: 1,
        })
        .context("session start on 20e9318 failed");
        assert_eq!(exit_code(&mismatch), EXIT_PROTOCOL_MISMATCH);
        assert_eq!(EXIT_PROTOCOL_MISMATCH, 3);
        let echo = anyhow::Error::new(HandshakeError::TokenEcho);
        assert_eq!(exit_code(&echo), 3);
        assert_eq!(exit_code(&anyhow::anyhow!("adb track-devices ended")), 1);
    }

    #[test]
    fn retry_delay_doubles_and_caps() {
        let policy = RetryPolicy {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(5),
            stable_after: Duration::from_secs(60),
        };
        let delays: Vec<u64> = (1..=5).map(|n| policy.delay(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 5, 5]);
    }
}
