pub mod catalog;
pub mod companion;
pub mod control;
pub mod ipc;
pub mod phones;
pub mod registry;
pub mod session;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use futures_core::Stream;
use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use ipc::Hub;
use lantern_proto::{PhoneState, SessionState};
use session::{Inbound, SessionConn};
use tokio::net::UnixListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::adb::{track_devices, Adb, DeviceEvent};
use crate::clipboard::{PcClipboard, PcOffer};
use crate::daemon::companion::Names;
use crate::daemon::control::{
    Command, ConnectEvent, Notifier, Opener, PhoneView, UndoRunner, View,
};
use crate::daemon::phones::{on_plug, OnPlug, Timing};
use crate::daemon::session::{start_with_companion, HandshakeError};
use crate::settings::{self, Settings, SettingsHandle};
use crate::unlock::{self, LockEvent};

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
    /// The user's settings (clipboard sync, autoconnect; without them autoconnect is on).
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
    /// PINs from the unlock prompt, each with the serial of the phone it was asked for: it goes
    /// to the helper as one `unlock` frame only while that phone's session is live (otherwise it
    /// is dropped, i.e. wiped, with a notice).
    pub unlock: Option<mpsc::Receiver<(String, unlock::Pin)>>,
    /// Gets the helper's `locked`/`unlocked`/`unlock_result` and the opens to redo.
    pub lock_events: Option<mpsc::Sender<LockEvent>>,
    /// The device records (`devices/<serial>.json`) holding each phone's `off`/`live` flags.
    /// With `None` the flags live in memory only.
    pub state_dir: Option<PathBuf>,
    /// The unplug grace and how long a connect-and-open request waits.
    pub timing: Timing,
    /// Gets the "plugged in" / "plug it in" / refused-open prompts (`try_send`: a full channel
    /// drops one). Set only when the connect prompt runs: it also makes an open for a phone that
    /// isn't connected an offer to connect it ([`Hub::set_connect_events`]).
    pub connect_events: Option<mpsc::Sender<ConnectEvent>>,
    /// Whether the connect prompt's notifications get through: refused opens are offered to it
    /// only while they do (else the window gets the old error and shows its own notice). `None`
    /// with `connect_events` set: always offered.
    pub connect_health: Option<crate::connect::Health>,
    /// Opens the app of a connect-and-open request once its phone is ready.
    pub opener: Option<Opener>,
}

/// The parts of [`Parts`] the run loop itself uses.
#[derive(Default)]
struct Controls {
    commands: Option<mpsc::Receiver<Command>>,
    view: Option<watch::Sender<View>>,
    names: Option<Names>,
    undo: Option<UndoRunner>,
    notify: Option<Notifier>,
    unlock: Option<mpsc::Receiver<(String, unlock::Pin)>>,
    /// `autoconnect` (without settings it counts as on).
    settings: Option<watch::Receiver<Settings>>,
    state_dir: Option<PathBuf>,
    timing: Timing,
    connect_events: Option<mpsc::Sender<ConnectEvent>>,
    opener: Option<Opener>,
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
        unlock,
        lock_events,
        state_dir,
        timing,
        connect_events,
        connect_health,
        opener,
    } = parts;
    let mut hub = Hub::new(Some(listener));
    hub.set_state_dir(state_dir.clone());
    if connect_events.is_some() {
        hub.set_connect_events(connect_health);
    }
    if let Some(events) = lock_events {
        hub.set_lock_events(events);
    }
    if let Some(catalog) = catalog {
        hub.set_catalog(catalog);
    }
    let settings = settings.map(|settings| {
        hub.set_settings(settings.subscribe());
        settings.subscribe()
    });
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
        unlock,
        settings,
        state_dir,
        timing,
        connect_events,
        opener,
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

/// A phone's `off`/`live` flags (see [`companion::Record`]).
#[derive(Debug, Clone, Copy, Default)]
struct Flags {
    off: bool,
    live: bool,
}

/// A connect request for a phone that wasn't ready: connect it when it is plugged in, and open
/// `open` (`(pkg, user)`) once it is ready, until `until`.
struct Pending {
    open: Option<(String, u32)>,
    until: Instant,
}

/// The phone table: what lanternd knows about each phone besides the session it serves.
struct Table {
    /// Where the device records are; `None`: the flags live in memory only.
    state_dir: Option<PathBuf>,
    /// Online phones, in the order they came online.
    plugged: Vec<String>,
    /// Each phone's flags as last read or written.
    flags: HashMap<String, Flags>,
    /// The phones with a device record, re-read after each record write.
    known: Vec<String>,
    /// The phone switched off most recently in this run (the top-level `paused`).
    last_off: Option<String>,
    /// Connect requests (with or without an app), at most one per phone (a later one replaces
    /// it).
    pending: HashMap<String, Pending>,
}

impl Table {
    fn new(state_dir: Option<PathBuf>) -> Self {
        let mut table = Self {
            state_dir,
            plugged: Vec::new(),
            flags: HashMap::new(),
            known: Vec::new(),
            last_off: None,
            pending: HashMap::new(),
        };
        table.refresh_known();
        table
    }

    /// Re-reads which phones have a record; a phone seen for the first time gets its flags.
    fn refresh_known(&mut self) {
        let Some(dir) = &self.state_dir else {
            return;
        };
        self.known = companion::known_serials(dir);
        for serial in self.known.clone() {
            if !self.flags.contains_key(&serial) {
                self.read(&serial);
            }
        }
    }

    fn plugged(&self, serial: &str) -> bool {
        self.plugged.iter().any(|s| s == serial)
    }

    /// `serial`'s flags, read from its record (else from this run's memory).
    fn read(&mut self, serial: &str) -> Flags {
        let Some(dir) = &self.state_dir else {
            return self.flags.get(serial).copied().unwrap_or_default();
        };
        let flags = companion::read_record(dir, serial)
            .map(|r| Flags {
                off: r.off,
                live: r.live,
            })
            .unwrap_or_default();
        self.flags.insert(serial.to_owned(), flags);
        flags
    }

    fn off(&self, serial: &str) -> bool {
        self.flags.get(serial).is_some_and(|f| f.off)
    }

    /// Sets `serial`'s `off` flag, in memory and in its record. A failed write is logged; the
    /// flag still applies for this run.
    fn set_off(&mut self, serial: &str, off: bool) {
        self.flags.entry(serial.to_owned()).or_default().off = off;
        if let Some(dir) = &self.state_dir {
            let written = companion::set_off(dir, serial, off);
            self.written(serial, "off", off, written);
        }
    }

    /// Sets `serial`'s `live` flag (`true` also stamps `connected_at`), as [`Self::set_off`].
    fn set_live(&mut self, serial: &str, live: bool) {
        self.flags.entry(serial.to_owned()).or_default().live = live;
        if let Some(dir) = &self.state_dir {
            let written = companion::set_live(dir, serial, live);
            self.written(serial, "live", live, written);
        }
    }

    fn written(&mut self, serial: &str, flag: &str, value: bool, written: Result<bool>) {
        match written {
            Ok(true) => {
                tracing::debug!(serial, flag, value, "device record updated");
                self.refresh_known();
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(
                serial,
                flag,
                value,
                error = format!("{error:#}"),
                "device record not written; the flag applies until lanternd stops"
            ),
        }
    }

    /// Whether `serial` has a connect request that hasn't expired.
    fn pending(&self, serial: &str) -> bool {
        self.pending
            .get(serial)
            .is_some_and(|p| p.until > Instant::now())
    }

    /// Drops every connect request except `serial`'s: the user's latest choice of
    /// phone wins, so an older request can't switch to its phone when that one is plugged.
    fn keep_pending_for(&mut self, serial: &str) {
        self.pending.retain(|s, p| {
            let keep = s == serial;
            if !keep {
                tracing::info!(
                    serial = s,
                    open = ?p.open,
                    "connect request dropped: another phone chosen"
                );
            }
            keep
        });
    }

    /// The phones the view lists besides the plugged ones: records, else this run's flags.
    fn others(&self) -> Vec<String> {
        let mut all: Vec<String> = self.known.clone();
        all.extend(self.flags.keys().cloned());
        all.sort();
        all.dedup();
        all
    }
}

/// The connect prompt's side in the run loop: its events channel.
struct Prompts {
    events: Option<mpsc::Sender<ConnectEvent>>,
}

impl Prompts {
    /// Hands `event` to the connect prompt without waiting; a full channel drops it.
    fn send(&mut self, event: ConnectEvent) {
        let Some(events) = &self.events else {
            return;
        };
        if let Err(error) = events.try_send(event) {
            tracing::warn!(%error, "connect prompt not listening; event dropped");
        }
    }
}

/// Every branch is polled concurrently: a session start in flight never blocks the stop signal,
/// device events or clients, and helper death is noticed through `SessionConn::closed`. The live
/// session's inbound frames are always drained (routed to window clients).
///
/// The phone table decides what is served (see [`phones::on_plug`]):
/// - `wanted` is the only start target; retries keep it. At most one phone is wanted.
/// - The wanted phone going offline closes its session and starts the unplug grace; back within
///   it, it restarts at once without asking; past it, its `live` flag is cleared and nothing
///   else starts.
/// - Disconnect and Undo set the phone's `off` (and clear `live`) before its session closes;
///   Connect clears `off`. Switching to another phone closes the old one without touching its
///   `off`. A stop signal leaves every flag as it is; Quit clears the live phone's `live`.
/// - The latest choice of phone wins: a Connect, or a phone connecting on plug-in, drops every
///   other phone's connect-and-open request. A phone left waiting loses its `live` flag.
/// - Nothing starts while an undo runs, and commands other than Quit are ignored meanwhile.
/// - Every phone that isn't live gets the same unplug grace: a plugged phone that isn't wanted
///   going offline, or the wanted phone released while offline in its grace (keeping the rest
///   of it). Back within the grace is a blip: it isn't asked and nothing changes, so a loose
///   cable never re-pops a notice. Past it, `Gone`.
/// - A phone's connect notice closes (`Gone`) when it gets ready or is gone for good (the end
///   of its grace, wanted or not).
// `release!` resets `retry_at` before a switch schedules the new phone's start.
#[allow(clippy::too_many_arguments, unused_assignments)]
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
        mut unlock,
        settings,
        state_dir,
        timing,
        connect_events,
        opener,
    } = controls;
    let mut names = NameCache {
        names,
        cache: HashMap::new(),
    };
    hub.set_configured(configured.clone());
    let configured = configured.as_deref();
    // Records with a stale `live` flag are cleared once the grace after start has passed, but
    // never before adb's first device list has been handled (`listed`): a phone that list
    // reports must not lose its flag to a slow adb.
    let mut listed = false;
    let mut startup = state_dir
        .as_ref()
        .map(|_| Instant::now() + timing.unplug_grace);
    let mut table = Table::new(state_dir);
    let mut prompts = Prompts {
        events: connect_events,
    };
    // The one phone to serve.
    let mut wanted: Option<String> = None;
    // The wanted phone is offline: the end of its unplug grace.
    let mut grace: Option<Instant> = None;
    // Plugged phones that aren't wanted and went offline (or were released in their grace): the
    // end of each one's unplug grace. Back within it is a blip: nothing is asked or changed.
    // Past it, `Gone` (the prompt ignores it for a phone with no notice).
    let mut leaving: HashMap<String, Instant> = HashMap::new();
    let mut active: Option<(SessionConn, Instant)> = None;
    let mut starting: Starting<F> = None;
    let mut retry_at: Option<Instant> = None;
    let mut failures: u32 = 0;
    let mut undoing: Undoing = None;

    // Ends whatever runs for the wanted phone and forgets it; evaluates to its serial. Its
    // flags are the caller's. A phone offline in its grace keeps the rest of it in `leaving`.
    macro_rules! release {
        ($why:expr) => {{
            cancel_start(&mut starting, $why);
            close_session(&mut active, &mut hub).await;
            retry_at = None;
            failures = 0;
            let old = wanted.take();
            if let (Some(until), Some(old)) = (grace.take(), old.as_ref()) {
                leaving.insert(old.clone(), until);
            }
            old
        }};
    }

    loop {
        let idle = active.is_none() && starting.is_none() && undoing.is_none();
        // The top-level status: the live phone, else the phone switched off most recently while
        // it stays plugged (and off), else idle.
        let paused = table
            .last_off
            .clone()
            .filter(|s| wanted.is_none() && table.plugged(s) && table.off(s));
        match (&active, &starting, paused) {
            (Some((session, _)), _, _) => hub.set_session(
                SessionState::Ready,
                Some(session.serial().to_owned()),
                Some(session.helper_sha().to_owned()),
            ),
            (None, Some((serial, _)), _) => {
                hub.set_session(SessionState::Starting, Some(serial.clone()), None)
            }
            (None, None, Some(serial)) => hub.set_session(SessionState::Paused, Some(serial), None),
            (None, None, None) => hub.set_session(SessionState::Idle, None, None),
        }
        // In a block: the closure borrows `undoing`, which must not live across an await.
        let now_view = {
            let state_of = |serial: &str| -> PhoneState {
                if undoing.as_ref().is_some_and(|(s, _)| s == serial) {
                    PhoneState::Undoing
                } else if wanted.as_deref() == Some(serial) {
                    if active.is_some() {
                        PhoneState::Ready
                    } else if starting.is_some() {
                        PhoneState::Starting
                    } else if grace.is_some() {
                        PhoneState::Reconnecting
                    } else if failures > 0 {
                        PhoneState::Retrying
                    } else {
                        // A start due now is as good as starting.
                        PhoneState::Starting
                    }
                } else if table.plugged(serial) {
                    if table.off(serial) {
                        PhoneState::Off
                    } else {
                        PhoneState::Waiting
                    }
                } else {
                    PhoneState::Unplugged
                }
            };
            let mut listed: Vec<String> = table.plugged.clone();
            if let Some(w) = wanted.as_ref().filter(|w| !listed.contains(w)) {
                listed.push(w.clone());
            }
            let mut rest: Vec<(String, String)> = table
                .others()
                .into_iter()
                .filter(|s| !listed.contains(s))
                .map(|s| (names.get(&s).unwrap_or_else(|| s.clone()), s))
                .collect();
            rest.sort();
            listed.extend(rest.into_iter().map(|(_, s)| s));
            if let Some(configured) = configured {
                listed.retain(|s| s == configured);
            }
            let live_serial = active.as_ref().map(|(s, _)| s.serial().to_owned());
            View {
                phones: listed
                    .iter()
                    .map(|serial| {
                        let live = live_serial.as_deref() == Some(serial.as_str());
                        PhoneView {
                            serial: serial.clone(),
                            name: names.get(serial),
                            state: state_of(serial),
                            plugged: table.plugged(serial),
                            off: table.off(serial),
                            locked: live && hub.locked(),
                            companion: if live { hub.companion() } else { None },
                        }
                    })
                    .collect(),
            }
        };
        hub.set_view(&now_view);
        if let Some(view) = &view {
            view.send_if_modified(|current| {
                if *current == now_view {
                    return false;
                }
                tracing::debug!(next = ?now_view, "view");
                *current = now_view;
                true
            });
        }
        let wake = retry_at.unwrap_or_else(Instant::now);
        let grace_end = grace.unwrap_or_else(Instant::now);
        let startup_end = startup.unwrap_or_else(Instant::now);
        let leave_end = leaving.values().min().copied().unwrap_or_else(Instant::now);
        // Set when the live session must be torn down and restarted.
        let mut ended: Option<&'static str> = None;
        // Set when the daemon must end (signal or Quit).
        let mut quit: Option<&'static str> = None;
        // Quit (not a stop signal) clears the live phone's `live` flag.
        let mut clear_live = false;
        // Commands to apply after this turn's event.
        let mut queued: Vec<Command> = Vec::new();
        tokio::select! {
            why = &mut stop => quit = Some(why),
            cmd = async { commands.as_mut().expect("guarded").recv().await }, if commands.is_some() => {
                match cmd {
                    Some(Command::Quit) => {
                        quit = Some("Quit command");
                        clear_live = true;
                    }
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
                        if configured.is_some_and(|c| c != serial) {
                            tracing::debug!(serial, "device online; not LANTERN_SERIAL, ignored");
                            continue;
                        }
                        tracing::info!(serial, "device online");
                        if !table.plugged(&serial) {
                            table.plugged.push(serial.clone());
                        }
                        names.refresh(&serial);
                        // The wanted phone back after its grace ended (its expiry not handled
                        // yet, e.g. a zero grace) is a real plug-in: expire it here first.
                        if wanted.as_deref() == Some(serial.as_str())
                            && grace.is_some_and(|until| until <= Instant::now())
                        {
                            grace = None;
                            wanted = None;
                            retry_at = None;
                            failures = 0;
                            tracing::info!(serial, "back after the unplug grace ended; a new plug-in");
                            table.set_live(&serial, false);
                            prompts.send(ConnectEvent::Gone { serial: serial.clone() });
                        }
                        let flags = table.read(&serial);
                        // Back within the grace: a blip. A grace already over (its expiry not
                        // handled yet, e.g. a zero grace) is a real plug-in.
                        let blip = match leaving.remove(&serial) {
                            Some(until) if until > Instant::now() => true,
                            Some(_) => {
                                prompts.send(ConnectEvent::Gone { serial: serial.clone() });
                                false
                            }
                            None => false,
                        };
                        if wanted.as_deref() == Some(serial.as_str()) {
                            if grace.take().is_some() {
                                tracing::info!(serial, "back within the unplug grace; reconnecting");
                                failures = 0;
                                retry_at = Some(Instant::now());
                            }
                        } else if undoing.as_ref().is_some_and(|(s, _)| *s == serial) {
                            tracing::info!(serial, "phone is being undone; not starting");
                        } else if blip {
                            // A loose cable never asks and changes nothing.
                            tracing::info!(serial, "back within the unplug grace; not asking again");
                        } else {
                            let autoconnect = settings
                                .as_ref()
                                .is_none_or(|settings| settings.borrow().autoconnect);
                            let rule = on_plug(
                                &serial,
                                configured,
                                autoconnect,
                                flags.off,
                                flags.live,
                                table.pending(&serial),
                                wanted.as_deref(),
                            );
                            tracing::info!(serial, ?rule, autoconnect, off = flags.off, live = flags.live, "plugged in");
                            match rule {
                                OnPlug::Connect => {
                                    if let Some(old) = release!("another phone connects") {
                                        table.set_live(&old, false);
                                    }
                                    table.keep_pending_for(&serial);
                                    wanted = Some(serial);
                                    retry_at = Some(Instant::now());
                                }
                                OnPlug::Prompt => {
                                    let name = names.get(&serial).unwrap_or_else(|| serial.clone());
                                    prompts.send(ConnectEvent::PluggedIn { serial, name });
                                }
                                OnPlug::Wait => {
                                    // Left waiting: it is no longer the phone to resume, so a
                                    // later blip doesn't connect it without a click.
                                    if flags.live {
                                        table.set_live(&serial, false);
                                    }
                                }
                                OnPlug::Ignore => {}
                            }
                        }
                    }
                    DeviceEvent::Offline(serial) => {
                        let was_plugged = table.plugged(&serial);
                        table.plugged.retain(|s| *s != serial);
                        if wanted.as_deref() == Some(serial.as_str()) {
                            tracing::info!(serial, grace = ?timing.unplug_grace, "device offline; waiting for it to come back");
                            cancel_start(&mut starting, "device offline");
                            close_session(&mut active, &mut hub).await;
                            retry_at = None;
                            failures = 0;
                            grace = Some(Instant::now() + timing.unplug_grace);
                        } else if was_plugged {
                            tracing::info!(serial, "device offline");
                            // Back within the grace is a blip; its notice closes only past it.
                            leaving.insert(serial, Instant::now() + timing.unplug_grace);
                        }
                    }
                    DeviceEvent::Listed => {
                        if !listed {
                            tracing::debug!("first device list handled");
                            listed = true;
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(wake), if idle && retry_at.is_some() => {
                retry_at = None;
                if let Some(serial) = wanted.clone().filter(|s| table.plugged(s)) {
                    tracing::info!(serial, attempt = failures + 1, "starting session");
                    starting = Some((serial.clone(), Box::pin(starter(serial))));
                }
            }
            _ = tokio::time::sleep_until(grace_end), if grace.is_some() => {
                grace = None;
                if let Some(serial) = wanted.take() {
                    tracing::info!(serial, "unplugged (offline past the grace)");
                    table.set_live(&serial, false);
                    prompts.send(ConnectEvent::Gone { serial });
                }
                retry_at = None;
                failures = 0;
            }
            _ = tokio::time::sleep_until(leave_end), if !leaving.is_empty() => {
                let now = Instant::now();
                let gone: Vec<String> = leaving
                    .iter()
                    .filter(|(_, until)| **until <= now)
                    .map(|(serial, _)| serial.clone())
                    .collect();
                for serial in gone {
                    leaving.remove(&serial);
                    tracing::debug!(serial, "away past the grace; closing any notice");
                    prompts.send(ConnectEvent::Gone { serial });
                }
            }
            _ = tokio::time::sleep_until(startup_end), if listed && startup.is_some() => {
                startup = None;
                for serial in table.known.clone() {
                    if table.plugged(&serial) || wanted.as_deref() == Some(serial.as_str()) {
                        continue;
                    }
                    if table.read(&serial).live {
                        tracing::info!(serial, "not plugged in since lanternd started; live flag cleared");
                        table.set_live(&serial, false);
                    }
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
                        table.set_live(&serial, true);
                        prompts.send(ConnectEvent::Gone { serial: serial.clone() });
                        if let Some(Pending { open: Some((pkg, user)), until }) =
                            table.pending.remove(&serial)
                        {
                            if until > Instant::now() {
                                tracing::info!(serial, pkg, user, "opening the requested app");
                                if let Some(opener) = &opener {
                                    opener(&serial, &pkg, user);
                                }
                            } else {
                                tracing::info!(serial, pkg, "connect-and-open request expired");
                            }
                        }
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
                // The phone stays off (set before the undo); nothing else starts.
                table.refresh_known();
            }
            pin = async { unlock.as_mut().expect("guarded").recv().await }, if unlock.is_some() => {
                match pin {
                    Some((serial, pin)) => match active.as_ref() {
                        // Only the phone the PIN was asked for gets it.
                        Some((session, _)) if session.serial() == serial => {
                            let helper = session.tx.clone();
                            if hub.send_unlock(pin, &helper).await.is_err() {
                                ended = Some("helper stopped reading");
                            }
                        }
                        live => {
                            drop(pin);
                            let live = live.map(|(session, _)| session.serial().to_owned());
                            tracing::info!(serial, ?live, "unlock: that phone is not live; PIN dropped");
                            if let Some(notify) = &notify {
                                let name = names.get(&serial).unwrap_or(serial);
                                notify(
                                    &format!("{name} isn't connected"),
                                    "Lantern didn't send the PIN. Connect the phone and try again.",
                                );
                            }
                        }
                    },
                    None => {
                        tracing::debug!("unlock channel closed");
                        unlock = None;
                    }
                }
            }
            event = hub.next_event() => {
                let helper = active.as_ref().map(|(session, _)| session.tx.clone());
                if hub.on_client(event, helper.as_ref()).await.is_err() {
                    ended = Some("helper stopped reading");
                }
                queued.extend(hub.take_commands());
                for (serial, pkg, user) in hub.take_refused() {
                    let name = names.get(&serial).unwrap_or_else(|| serial.clone());
                    let app = hub.app_name(&pkg, user);
                    // Its new notice replaces the old one; the grace mustn't close it.
                    leaving.remove(&serial);
                    prompts.send(
                        ConnectEvent::OpenRefused { serial, name, app, pkg, user },
                    );
                }
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
            if clear_live {
                if let Some(serial) = wanted.clone() {
                    table.set_live(&serial, false);
                }
            }
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
            let serial = match &cmd {
                Command::Connect { serial, .. }
                | Command::Disconnect { serial }
                | Command::UndoSetup { serial } => serial.clone(),
                Command::Quit => unreachable!("Quit is handled when it arrives"),
            };
            if configured.is_some_and(|c| c != serial) {
                tracing::info!(serial, ?cmd, "not LANTERN_SERIAL; command ignored");
                continue;
            }
            match cmd {
                Command::Connect { serial, open } => {
                    tracing::info!(serial, ?open, "connected in Lantern");
                    // Its "Plug in" notice (or its session's Gone) takes over from the grace.
                    leaving.remove(&serial);
                    table.set_off(&serial, false);
                    // The latest choice wins: a request for another phone must not switch later.
                    table.keep_pending_for(&serial);
                    let app = open.as_ref().map(|(pkg, user)| hub.app_name(pkg, *user));
                    let ready = active.as_ref().is_some_and(|(s, _)| s.serial() == serial);
                    let plugged = table.plugged(&serial);
                    let open = match (open, &opener, ready) {
                        (Some((pkg, user)), Some(opener), true) => {
                            opener(&serial, &pkg, user);
                            None
                        }
                        (open, _, _) => open,
                    };
                    // Remembered for `pending_for`: an app to open once it is ready, and for a
                    // phone that isn't plugged in, the connect itself ("It connects if you plug
                    // it in within 2 minutes"). A plain Connect keeps an app still asked for.
                    if open.is_some() || !plugged {
                        let now = Instant::now();
                        let kept = table
                            .pending
                            .remove(&serial)
                            .filter(|p| p.until > now)
                            .and_then(|p| p.open);
                        let open = open.or(kept);
                        let until = now + timing.pending_for;
                        table
                            .pending
                            .insert(serial.clone(), Pending { open, until });
                    }
                    if !plugged {
                        let name = names.get(&serial).unwrap_or_else(|| serial.clone());
                        prompts.send(ConnectEvent::PlugIn { serial, name, app });
                    } else if wanted.as_deref() == Some(serial.as_str()) {
                        // Already wanted: a start in backoff is retried now.
                        if active.is_none() && starting.is_none() && retry_at.is_some() {
                            failures = 0;
                            retry_at = Some(Instant::now());
                        }
                    } else {
                        if let Some(old) = release!("another phone connects") {
                            tracing::info!(serial = old, "switched to another phone");
                            table.set_live(&old, false);
                        }
                        wanted = Some(serial);
                        retry_at = Some(Instant::now());
                    }
                }
                Command::Disconnect { serial } => {
                    tracing::info!(serial, "disconnected in Lantern");
                    // Flags first: nothing may restart it while the session closes.
                    table.set_off(&serial, true);
                    table.set_live(&serial, false);
                    table.last_off = Some(serial.clone());
                    table.pending.remove(&serial);
                    if wanted.as_deref() == Some(serial.as_str()) {
                        // `bye`: the helper restores the phone.
                        release!("disconnected in Lantern");
                    }
                }
                Command::UndoSetup { serial } => {
                    let Some(run) = &undo else {
                        tracing::warn!("undo-setup is not available in this daemon");
                        continue;
                    };
                    if !table.plugged(&serial) {
                        tracing::info!(serial, "undo-setup: that phone is not plugged in");
                        continue;
                    }
                    tracing::info!(serial, "undo-setup requested");
                    table.set_off(&serial, true);
                    table.set_live(&serial, false);
                    table.last_off = Some(serial.clone());
                    table.pending.remove(&serial);
                    if wanted.as_deref() == Some(serial.as_str()) {
                        // The helper's connection is closed before the undo runs; `run_steps`
                        // itself waits for helper processes still restoring the phone.
                        release!("undo-setup");
                    }
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

/// Opens a connect-and-open request's app: `lantern open --phone S [--user N] -- pkg`, not
/// waiting for the window (as the unlock prompt's reopen does).
fn opener() -> Option<Opener> {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            tracing::warn!(%error, "unknown lantern executable; requested apps won't open");
            return None;
        }
    };
    Some(Arc::new(move |serial: &str, pkg: &str, user: u32| {
        let exe = exe.clone();
        let args = unlock::reopen_args(pkg, user, Some(serial));
        let pkg = pkg.to_owned();
        tokio::spawn(async move {
            // desktop::spawn's systemctl lookup blocks: keep it off the async workers.
            let spawned =
                tokio::task::spawn_blocking(move || crate::desktop::spawn(&exe, &args)).await;
            match spawned {
                Ok(Ok(mut child)) => {
                    let _ = child.wait().await;
                }
                Ok(Err(error)) => tracing::warn!(%error, pkg, "could not open the requested app"),
                Err(error) => tracing::warn!(%error, pkg, "open task failed"),
            }
        });
    }))
}

/// The catalogue lanternd serves from: the one-time menu migration ([`catalog::migrate`]) runs
/// first, so `menu.json` records the primary phone before any session is served or any window
/// reopened (else every `lantern open --phone` window would get a suffixed app_id that matches
/// no legacy entry). `pins` is Plasma's applets config, for picking the primary. `rebuild` runs
/// once if the migration changed files. A failed migration gives no catalogue for this run:
/// writing per-phone entries over unmigrated ones would mark it done; the next start retries.
pub fn open_catalog(
    dirs: catalog::Dirs,
    pins: Option<&str>,
    rebuild: catalog::Rebuild,
) -> Option<catalog::Catalog> {
    let pending = !crate::naming::Naming::migrated(&dirs.state);
    match catalog::migrate(&dirs, pins) {
        Ok(m) if pending => {
            tracing::info!(
                "menu migrated: primary={} written={} removed={}",
                m.primary.as_deref().unwrap_or("none"),
                m.written,
                m.removed
            );
            if m.written + m.removed > 0 {
                rebuild();
            }
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "menu migration failed; no menu entries until lanternd restarts"
            );
            return None;
        }
    }
    Some(catalog::Catalog::with_rebuild(dirs, rebuild))
}

/// Plasma's applets config (`$XDG_CONFIG_HOME/plasma-org.kde.plasma.desktop-appletsrc`), read
/// only for its pinned launchers; `None` if unreadable.
fn plasma_pins() -> Option<String> {
    let config = ::dirs::config_dir()?;
    std::fs::read_to_string(config.join("plasma-org.kde.plasma.desktop-appletsrc")).ok()
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
    // Before anything is served: the migration records the primary phone in `menu.json`.
    let catalog = match catalog::Dirs::from_env() {
        Ok(dirs) => {
            let names = companion::names_in(dirs.state.clone());
            let pins = plasma_pins();
            let rebuild: catalog::Rebuild = Arc::new(catalog::run_kbuildsycoca);
            open_catalog(dirs, pins.as_deref(), rebuild).map(|mut catalog| {
                catalog.set_names(names);
                catalog
            })
        }
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
    // The tray: it reads the view and sends commands, never waiting on the run loop. Without
    // a session bus the sender is dropped here and the loop simply gets no tray commands.
    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (view_tx, view_rx) = watch::channel(View::default());
    // The unlock prompt on its own bus connection: PINs come back on `pin_rx` (one at a time),
    // lock events go to it. Without a session bus there is neither.
    let (pin_tx, pin_rx) = mpsc::channel::<(String, unlock::Pin)>(1);
    let (lock_tx, lock_rx) = mpsc::channel(16);
    let prompt = crate::unlock::spawn(lock_rx, view_rx.clone(), pin_tx).await;
    let (unlock, lock_events, request) = match prompt {
        Some((_, request)) => (Some(pin_rx), Some(lock_tx), Some(request)),
        None => (None, None, None),
    };
    // The connect prompt on its own bus connection: "plugged in" / "Connect your …" / "Plug in
    // your …"; its Connect click comes back as a command. Without a session bus there is none:
    // plug-ins then wait silently and refused opens get the old errors.
    let (connect_tx, connect_rx) = mpsc::channel(16);
    let connect_health = crate::connect::Health::new();
    let (connect_events, connect_health) =
        match crate::connect::spawn(connect_rx, cmd_tx.clone(), connect_health.clone()).await {
            Some(_) => (Some(connect_tx), Some(connect_health)),
            None => (None, None),
        };
    // The tray's Unlock row asks the same prompt (it never runs the dialog or sees a PIN).
    crate::tray::spawn(view_rx, settings.clone(), cmd_tx, request).await;
    let mut timing = Timing::default();
    if let Some(ms) = std::env::var("LANTERN_UNPLUG_GRACE_MS")
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok())
    {
        tracing::info!(ms, "unplug grace from LANTERN_UNPLUG_GRACE_MS");
        timing.unplug_grace = Duration::from_millis(ms);
    }
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
            commands: Some(cmd_rx),
            view: Some(view_tx),
            unlock,
            lock_events,
            state_dir: companion::state_dir(),
            timing,
            // Setting this also turns opens for a phone that isn't connected into offers
            // (`CONNECT_OFFERED`), while the prompt's notifications get through.
            connect_events,
            connect_health,
            opener: opener(),
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

    /// Scratch catalogue dirs (never the real XDG dirs) and a rebuild counter.
    fn scratch_dirs(tag: &str) -> (std::path::PathBuf, catalog::Dirs) {
        let root =
            std::env::temp_dir().join(format!("lantern-open-catalog-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dirs = catalog::Dirs::resolve(
            Some(root.join("data")),
            Some(root.join("config")),
            Some(root.join("state")),
            &root,
            "/usr/bin/lantern".into(),
        );
        (root, dirs)
    }

    /// A Plan 3 entry for `pkg` (user 0) listing `serials`.
    fn plant_legacy(dirs: &catalog::Dirs, pkg: &str, serials: &str) {
        std::fs::create_dir_all(&dirs.applications).unwrap();
        let marker = serials.split(';').next().unwrap();
        std::fs::write(
            dirs.applications.join(format!("lantern.{pkg}.desktop")),
            format!(
                "[Desktop Entry]\nType=Application\nName={pkg}\nExec=lantern open {pkg}\n\
                 Categories=X-Lantern;\nStartupWMClass=lantern.{pkg}\n\
                 X-Lantern-Serial={marker}\nX-Lantern-Serials={serials}\n\
                 X-Lantern-Package={pkg}\nX-Lantern-User=0\n"
            ),
        )
        .unwrap();
    }

    fn counter() -> (Arc<std::sync::atomic::AtomicUsize>, catalog::Rebuild) {
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r = runs.clone();
        let rebuild: catalog::Rebuild = Arc::new(move || {
            r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        (runs, rebuild)
    }

    #[test]
    fn startup_records_the_primary_before_the_catalogue_exists() {
        use crate::naming::Naming;
        let (root, dirs) = scratch_dirs("primary");
        // B has fewer entries, but its entry is pinned.
        plant_legacy(&dirs, "com.a", "A;");
        plant_legacy(&dirs, "com.a2", "A;");
        plant_legacy(&dirs, "com.b", "B;");
        let pins = "launchers=applications:lantern.com.b.desktop\n";
        let (runs, rebuild) = counter();
        let catalog = open_catalog(dirs.clone(), Some(pins), rebuild);
        // The only way to a catalogue is through the migration: menu.json is there first.
        assert!(catalog.is_some());
        let naming = Naming::load(&dirs.state);
        assert!(Naming::migrated(&dirs.state));
        assert_eq!(naming.primary.as_deref(), Some("B"));
        // `lantern open --phone B` (a pinned launcher, or an unlock reopen) gets the legacy
        // stem, which is still the pinned file's.
        assert_eq!(naming.app_id("com.b", 0, "B"), "lantern.com.b");
        let pinned =
            std::fs::read_to_string(dirs.applications.join("lantern.com.b.desktop")).unwrap();
        assert!(
            pinned.contains("StartupWMClass=lantern.com.b\n"),
            "{pinned}"
        );
        assert!(pinned.contains("X-Lantern-Serial=B\n"), "{pinned}");
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(catalog);
        // A later start migrates nothing and rebuilds nothing.
        let (runs, rebuild) = counter();
        assert!(open_catalog(dirs.clone(), None, rebuild).is_some());
        assert_eq!(Naming::load(&dirs.state).primary.as_deref(), Some("B"));
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_migration_gives_no_catalogue() {
        use crate::naming::Naming;
        let (root, mut dirs) = scratch_dirs("failed");
        plant_legacy(&dirs, "com.a", "A;");
        // The state dir cannot be created: menu.json is never written.
        std::fs::write(root.join("blocker"), "").unwrap();
        dirs.state = root.join("blocker").join("lantern");
        let (_, rebuild) = counter();
        assert!(open_catalog(dirs.clone(), None, rebuild).is_none());
        assert!(!Naming::migrated(&dirs.state));
        let _ = std::fs::remove_dir_all(root);
    }
}
