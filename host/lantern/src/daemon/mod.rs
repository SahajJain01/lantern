pub mod catalog;
pub mod companion;
pub mod control;
pub mod ipc;
pub mod phones;
pub mod registry;
pub mod runs;
pub mod session;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use futures_core::Stream;
use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use ipc::{HelperWedged, Hub};
use lantern_proto::PhoneState;
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
use crate::daemon::runs::{PhoneRun, RunEvent, Runs};
use crate::daemon::session::{start_with_companion, HandshakeError};
use crate::notices::escape_markup;
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

/// Ends `serial`'s session, if its run has one. Its window clients are closed at once
/// (`hub.session_down`): windows belong to one helper session and cannot survive a restart.
/// The returned future sends `bye` and waits for the helper; the caller runs it (in the
/// background, so another phone's frames keep flowing meanwhile).
fn close_run<F>(
    serial: &str,
    run: &mut PhoneRun<F>,
    hub: &mut Hub,
) -> Option<BoxFuture<'static, ()>> {
    let (session, _) = run.active.take()?;
    hub.session_down(serial);
    let serial = serial.to_owned();
    Some(Box::pin(async move {
        session.close().await;
        tracing::info!(serial, "session closed");
    }))
}

/// Ends whatever runs for `serial` and forgets its run; its flags are the caller's. A phone
/// offline in its grace keeps the rest of it in `leaving`. Every other phone's run is
/// untouched. Returns its session's close, as [`close_run`].
fn release<F>(
    serial: &str,
    why: &str,
    runs: &mut Runs<F>,
    hub: &mut Hub,
    leaving: &mut HashMap<String, Instant>,
) -> Option<BoxFuture<'static, ()>>
where
    F: Future<Output = Result<SessionConn>>,
{
    let mut run = runs.release(serial)?;
    run.cancel_start(serial, why);
    if let Some(until) = run.grace {
        leaving.insert(serial.to_owned(), until);
    }
    close_run(serial, &mut run, hub)
}

/// How long a phone's due start waits for its previous session's close to finish. A close is
/// bounded, but can take ~16 s (`SessionConn::close`: the Bye and both socket halves, then the
/// adb forward removal and the helper's exit, each with its own timeout); one still running
/// after this hold is aborted first. If the abort lands before it removed its adb forward, the
/// next start's stale-forward sweep removes that forward.
const CLOSE_HOLD: Duration = Duration::from_secs(5);

/// How long quitting waits for the closes still running; the rest are aborted.
const CLOSE_CAP: Duration = Duration::from_secs(10);

/// How long quitting waits for the connect and unlock prompts to close their notices (decision
/// 15), once the run loop has dropped their event senders.
const SHUTDOWN_NOTICES: Duration = Duration::from_secs(3);

/// A session close running in the background.
struct Close {
    serial: String,
    since: Instant,
    task: tokio::task::JoinHandle<()>,
}

/// Session closes running in the background, each with its phone; quitting waits for them.
/// A phone's next start waits for its own close (R8): the old close removes the adb forward
/// it made, and the new session's forward can have the same port once adb dropped the old one
/// (Offline), so the close must be done before the start makes its forward.
#[derive(Default)]
struct Closing {
    closes: Vec<Close>,
    /// Phones whose due start waits for their close: a start is due as soon as it ends.
    held: HashSet<String>,
}

impl Closing {
    fn spawn(&mut self, serial: &str, close: Option<BoxFuture<'static, ()>>) {
        self.prune(None);
        if let Some(close) = close {
            self.closes.push(Close {
                serial: serial.to_owned(),
                since: Instant::now(),
                task: tokio::spawn(close),
            });
        }
    }

    /// Forgets ended closes, except a held phone's (other than `asking`'s), which stay until
    /// [`Self::next_closed`] reports them.
    fn prune(&mut self, asking: Option<&str>) {
        let held = &self.held;
        self.closes.retain(|close| {
            !close.task.is_finished()
                || (held.contains(&close.serial) && asking != Some(close.serial.as_str()))
        });
    }

    /// Whether `serial`'s due start must wait for its close: `Some(until)` while one runs (the
    /// start is due again when it ends, see [`Self::next_closed`], or at `until`). A close
    /// older than [`CLOSE_HOLD`] is aborted; the start waits for the abort to land.
    fn hold(&mut self, serial: &str) -> Option<Instant> {
        self.prune(Some(serial));
        let now = Instant::now();
        let mut until: Option<Instant> = None;
        for close in self.closes.iter().filter(|close| close.serial == serial) {
            let end = if close.since + CLOSE_HOLD <= now {
                tracing::warn!(serial, "previous session still closing; aborting its close");
                close.task.abort();
                now + Duration::from_secs(1)
            } else {
                close.since + CLOSE_HOLD
            };
            until = Some(until.map_or(end, |until| until.max(end)));
        }
        if until.is_some() {
            self.held.insert(serial.to_owned());
        } else {
            self.held.remove(serial);
        }
        until
    }

    /// The next phone whose start was held and whose last close just ended. Cancel-safe;
    /// pending while there is none.
    async fn next_closed(&mut self) -> String {
        std::future::poll_fn(|cx| {
            let mut ended = Vec::new();
            self.closes.retain_mut(|close| {
                if std::pin::Pin::new(&mut close.task).poll(cx).is_ready() {
                    ended.push(close.serial.clone());
                    false
                } else {
                    true
                }
            });
            for serial in ended {
                if !self.closes.iter().any(|close| close.serial == serial)
                    && self.held.remove(&serial)
                {
                    return std::task::Poll::Ready(serial);
                }
            }
            std::task::Poll::Pending
        })
        .await
    }

    /// Waits for every close still running (each is bounded by `SessionConn::close`), at most
    /// [`CLOSE_CAP`].
    async fn finish(&mut self) {
        self.finish_within(CLOSE_CAP).await;
    }

    /// [`Self::finish`] with its cap; closes still running after it are aborted.
    async fn finish_within(&mut self, cap: Duration) {
        let mut closes = std::mem::take(&mut self.closes);
        self.held.clear();
        let all = futures_util::future::join_all(closes.iter_mut().map(|close| &mut close.task));
        if tokio::time::timeout(cap, all).await.is_err() {
            tracing::warn!("session closes still running at exit; aborting them");
            for close in &closes {
                close.task.abort();
            }
            // An aborted task ends at its next poll; let each one go (drop) before returning.
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                futures_util::future::join_all(closes.iter_mut().map(|close| &mut close.task)),
            )
            .await;
        }
    }
}

/// Ends every phone's run and waits until each session got `bye` and closed. No flag changes.
async fn close_all<F>(runs: &mut Runs<F>, hub: &mut Hub, closing: &mut Closing, why: &str)
where
    F: Future<Output = Result<SessionConn>>,
{
    let mut closes = Vec::new();
    for serial in runs.serials() {
        if let Some(mut run) = runs.release(&serial) {
            run.cancel_start(&serial, why);
            closes.extend(close_run(&serial, &mut run, hub));
        }
    }
    futures_util::future::join_all(closes).await;
    closing.finish().await;
}

/// A wedged helper ends its own session only, and only while it is live.
fn wedged_ends<F>(runs: &Runs<F>, wedged: HelperWedged, ended: &mut Vec<(String, &'static str)>)
where
    F: Future<Output = Result<SessionConn>>,
{
    if runs
        .get(&wedged.serial)
        .is_some_and(|run| run.active.is_some())
    {
        ended.push((wedged.serial, "helper stopped reading"));
    } else {
        tracing::warn!(
            serial = wedged.serial,
            "a helper that isn't live stopped reading"
        );
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
    /// The unplug grace, how long a connect-and-open request waits and how long the start-up
    /// live check waits for adb's first device list.
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
/// device events or clients, and helper death is noticed through each session's inbound queue.
/// Every live session's inbound frames are always drained (routed to window clients), starting
/// from a rotating phone so none starves.
///
/// Every checked phone has its own run ([`Runs`]): session, start, backoff and unplug grace.
/// One phone's failure, retry, offline, Disconnect or Undo never closes, restarts or sends
/// frames to another phone's session or windows. The phone table decides what is served (see
/// [`phones::on_plug`]):
/// - Any phone that qualifies connects, whatever else is live; nothing is switched away.
/// - A wanted phone going offline closes its own session and starts its unplug grace; back
///   within it, it restarts at once without asking; past it, its `live` flag is cleared and
///   its run is forgotten.
/// - A failed start or a helper death backs off on that phone's run only (a wedged helper
///   ends its own session alone). A handshake protocol error still ends lanternd.
/// - Disconnect and Undo set the phone's `off` (and clear `live`) before its session closes;
///   Connect clears `off`. A stop signal leaves every flag as it is; Quit clears the `live`
///   flag of every wanted phone.
/// - Connect-and-open requests are per phone: a choice for one phone never drops another's.
///   Disconnect and Undo drop that phone's own.
/// - One undo at a time: a Connect for the phone being undone, or an Undo of a second phone,
///   is refused with a notice saying why. Commands for other phones run normally meanwhile.
/// - Every phone that isn't wanted gets the same unplug grace: a plugged phone that isn't
///   wanted going offline, or a wanted phone released while offline in its grace (keeping the
///   rest of it). Back within the grace is a blip: it isn't asked and nothing changes, so a
///   loose cable never re-pops a notice. Past it, `Gone`.
/// - A phone's connect notice closes (`Gone`) when it gets ready or is gone for good (the end
///   of its grace, wanted or not).
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
    // Records with a stale `live` flag are cleared once the grace after start has passed and
    // adb's first device list has been handled (`listed`: a phone that list reports must not
    // lose its flag to a slow adb), or at the latest `list_wait` later (decision 14).
    let mut listed = false;
    let started_at = Instant::now();
    let mut startup = state_dir.is_some();
    let mut table = Table::new(state_dir);
    let mut prompts = Prompts {
        events: connect_events,
    };
    let mut runs: Runs<F> = Runs::new();
    // Plugged phones that aren't wanted and went offline (or were released in their grace): the
    // end of each one's unplug grace. Back within it is a blip: nothing is asked or changed.
    // Past it, `Gone` (the prompt ignores it for a phone with no notice).
    let mut leaving: HashMap<String, Instant> = HashMap::new();
    let mut undoing: Undoing = None;
    let mut closing = Closing::default();

    loop {
        // The top-level status when no phone is live: the phone switched off most recently
        // while it stays plugged (and off), else idle.
        let paused = table
            .last_off
            .clone()
            .filter(|s| runs.is_empty() && table.plugged(s) && table.off(s));
        hub.set_paused(paused);
        let now_view = {
            let state_of = |serial: &str| -> PhoneState {
                if undoing.as_ref().is_some_and(|(s, _)| s == serial) {
                    PhoneState::Undoing
                } else if let Some(state) = runs.state(serial) {
                    state
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
            for wanted in runs.serials() {
                if !listed.contains(&wanted) {
                    listed.push(wanted);
                }
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
            View {
                phones: listed
                    .iter()
                    .map(|serial| PhoneView {
                        serial: serial.clone(),
                        name: names.get(serial),
                        state: state_of(serial),
                        plugged: table.plugged(serial),
                        off: table.off(serial),
                        locked: hub.locked(serial),
                        companion: hub.companion(serial),
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
        let wake = runs.next_wake();
        let wake_at = wake.unwrap_or_else(Instant::now);
        let startup_end = started_at
            + timing.unplug_grace
            + if listed {
                Duration::ZERO
            } else {
                timing.list_wait
            };
        let leave_end = leaving.values().min().copied().unwrap_or_else(Instant::now);
        // Sessions that must be torn down and restarted (helper death, wedged helper), with why.
        let mut ended: Vec<(String, &'static str)> = Vec::new();
        // Set when the daemon must end (signal or Quit).
        let mut quit: Option<&'static str> = None;
        // Quit (not a stop signal) clears the `live` flag of every wanted phone.
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
                    close_all(&mut runs, &mut hub, &mut closing, "device stream ended").await;
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
                        // A wanted phone back after its grace ended (its expiry not handled
                        // yet, e.g. a zero grace) is a real plug-in: expire it here first.
                        if runs
                            .get(&serial)
                            .and_then(|run| run.grace)
                            .is_some_and(|until| until <= Instant::now())
                        {
                            runs.release(&serial);
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
                        if let Some(run) = runs.get_mut(&serial) {
                            if run.grace.take().is_some() {
                                tracing::info!(serial, "back within the unplug grace; reconnecting");
                                run.failures = 0;
                                run.retry_at = Some(Instant::now());
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
                            );
                            tracing::info!(serial, ?rule, autoconnect, off = flags.off, live = flags.live, "plugged in");
                            match rule {
                                // Other live phones keep their sessions.
                                OnPlug::Connect => {
                                    runs.want(&serial);
                                }
                                OnPlug::Prompt => {
                                    let name = names.get(&serial).unwrap_or_else(|| serial.clone());
                                    prompts.send(ConnectEvent::PluggedIn { serial, name });
                                }
                                OnPlug::Wait | OnPlug::Ignore => {}
                            }
                        }
                    }
                    DeviceEvent::Offline(serial) => {
                        let was_plugged = table.plugged(&serial);
                        table.plugged.retain(|s| *s != serial);
                        if let Some(run) = runs.get_mut(&serial) {
                            tracing::info!(serial, grace = ?timing.unplug_grace, "device offline; waiting for it to come back");
                            run.cancel_start(&serial, "device offline");
                            closing.spawn(&serial, close_run(&serial, run, &mut hub));
                            run.retry_at = None;
                            run.failures = 0;
                            run.grace = Some(Instant::now() + timing.unplug_grace);
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
            _ = tokio::time::sleep_until(wake_at), if wake.is_some() => {
                let now = Instant::now();
                for serial in runs.serials() {
                    let Some(run) = runs.get_mut(&serial) else { continue };
                    if run.grace.is_some_and(|until| until <= now) {
                        runs.release(&serial);
                        tracing::info!(serial, "unplugged (offline past the grace)");
                        table.set_live(&serial, false);
                        prompts.send(ConnectEvent::Gone { serial });
                        continue;
                    }
                    let due = run.active.is_none()
                        && run.starting.is_none()
                        && run.retry_at.is_some_and(|at| at <= now);
                    if !due {
                        continue;
                    }
                    // R8: not before this phone's previous session finished closing.
                    if let Some(until) = closing.hold(&serial) {
                        tracing::debug!(serial, "start waits for its previous session to close");
                        run.retry_at = Some(until);
                        continue;
                    }
                    run.retry_at = None;
                    let undone = undoing.as_ref().is_some_and(|(s, _)| *s == serial);
                    if table.plugged(&serial) && !undone {
                        tracing::info!(serial, attempt = run.failures + 1, "starting session");
                        run.starting = Some(Box::pin(starter(serial.clone())));
                    }
                }
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
            serial = closing.next_closed() => {
                // Its held start is due now.
                if let Some(run) = runs.get_mut(&serial) {
                    if run.active.is_none() && run.starting.is_none() && run.retry_at.is_some() {
                        run.retry_at = Some(Instant::now());
                    }
                }
            }
            _ = tokio::time::sleep_until(startup_end), if startup => {
                startup = false;
                if !listed {
                    tracing::info!(wait = ?timing.list_wait, "no device list from adb yet; checking live flags anyway");
                }
                for serial in table.known.clone() {
                    if table.plugged(&serial) || runs.is_wanted(&serial) {
                        continue;
                    }
                    if table.read(&serial).live {
                        tracing::info!(serial, "not plugged in since lanternd started; live flag cleared");
                        table.set_live(&serial, false);
                    }
                }
            }
            event = runs.next() => match event {
                RunEvent::Started(serial, Ok(session)) => {
                    tracing::info!(serial, helper = session.helper_sha(), "session ready");
                    hub.session_up(&serial, session.tx.clone(), session.helper_sha());
                    // The start has just recorded the phone's name.
                    names.refresh(&serial);
                    if let Some(run) = runs.get_mut(&serial) {
                        run.active = Some((session, Instant::now()));
                    }
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
                RunEvent::Started(serial, Err(error)) if HandshakeError::find(&error).is_some() => {
                    // The embedded helper answers the same way on every attempt; retrying
                    // would only spin. Stop with the explained error (non-zero exit).
                    tracing::error!(serial, error = format!("{error:#}"), "helper protocol mismatch; not retrying");
                    close_all(&mut runs, &mut hub, &mut closing, "helper protocol mismatch").await;
                    return Err(error.context(format!("session start on {serial} failed")));
                }
                RunEvent::Started(serial, Err(error)) => {
                    if let Some(run) = runs.get_mut(&serial) {
                        run.failures += 1;
                        let delay = policy.delay(run.failures);
                        tracing::error!(serial, error = format!("{error:#}"), retry_in = ?delay, "session start failed");
                        run.retry_at = Some(Instant::now() + delay);
                    }
                }
                RunEvent::Inbound(serial, Inbound::Frame(frame)) => {
                    if let Err(wedged) = hub.on_helper(&serial, frame).await {
                        wedged_ends(&runs, wedged, &mut ended);
                    }
                }
                RunEvent::Inbound(serial, Inbound::Ended(why)) => ended.push((serial, why)),
            },
            (ok, output) = async { undoing.as_mut().expect("guarded").1.as_mut().await }, if undoing.is_some() => {
                let (serial, _) = undoing.take().expect("guarded");
                undo_done(&serial, ok, &output, &mut names, notify.as_ref());
                // The phone stays off (set before the undo); nothing starts for it.
                table.refresh_known();
            }
            pin = async { unlock.as_mut().expect("guarded").recv().await }, if unlock.is_some() => {
                match pin {
                    // Only the phone the PIN was asked for gets it, and only in its stretch.
                    Some((serial, pin)) => match hub.send_unlock(&serial, pin).await {
                        Ok(true) => {}
                        Ok(false) if hub.live_serials().contains(&serial) => {
                            tracing::info!(serial, "unlock: {serial}'s lock stretch is over; PIN dropped");
                        }
                        Ok(false) => {
                            let live = hub.live_serials();
                            tracing::info!(serial, ?live, "unlock: that phone is not live; PIN dropped");
                            if let Some(notify) = &notify {
                                let name = names.get(&serial).unwrap_or(serial);
                                notify(
                                    &format!("{name} isn't connected"),
                                    "Lantern didn't send the PIN. Connect the phone and try again.",
                                );
                            }
                        }
                        Err(wedged) => wedged_ends(&runs, wedged, &mut ended),
                    },
                    None => {
                        tracing::debug!("unlock channel closed");
                        unlock = None;
                    }
                }
            }
            event = hub.next_event() => {
                if let Err(wedged) = hub.on_client(event).await {
                    wedged_ends(&runs, wedged, &mut ended);
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
                    // To the live phones only: nothing is queued for a later session.
                    Some(offer) => {
                        for wedged in hub.on_pc_clip(offer).await {
                            wedged_ends(&runs, wedged, &mut ended);
                        }
                    }
                    None => {
                        tracing::warn!("PC clipboard thread ended; clipboard sync off");
                        offers = None;
                    }
                }
            }
        }
        if let Some(why) = quit {
            tracing::info!("{why} received");
            if clear_live {
                for serial in runs.serials() {
                    table.set_live(&serial, false);
                }
            }
            close_all(&mut runs, &mut hub, &mut closing, why).await;
            if let Some((serial, undo)) = undoing.take() {
                tracing::info!(serial, "waiting for undo-setup to finish before quitting");
                let (ok, output) = undo.await;
                undo_done(&serial, ok, &output, &mut names, notify.as_ref());
            }
            return Ok(());
        }
        for (serial, why) in ended {
            // Only that phone's session; a second report for it in the same turn finds it gone.
            let Some(run) = runs.get_mut(&serial) else {
                continue;
            };
            let Some(lived) = run.active.as_ref().map(|(_, since)| since.elapsed()) else {
                continue;
            };
            tracing::warn!(serial, why, ?lived, "helper session ended unexpectedly");
            closing.spawn(&serial, close_run(&serial, run, &mut hub));
            if lived >= policy.stable_after {
                run.failures = 0;
            }
            run.failures += 1;
            let delay = policy.delay(run.failures);
            tracing::info!(serial, retry_in = ?delay, "restarting session");
            run.retry_at = Some(Instant::now() + delay);
        }
        for cmd in queued {
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
            let being_undone = undoing.as_ref().map(|(s, _)| s.clone());
            match cmd {
                Command::Connect { serial, .. } if being_undone.as_ref() == Some(&serial) => {
                    tracing::info!(serial, "connect refused: undo-setup is running on it");
                    let name = names.get(&serial).unwrap_or_else(|| serial.clone());
                    if let Some(notify) = &notify {
                        notify(
                            &format!("Lantern is still being removed from {name}"),
                            "Connect it again when that's done.",
                        );
                    }
                }
                Command::Connect { serial, open } => {
                    tracing::info!(serial, ?open, "connected in Lantern");
                    // Its "Plug in" notice (or its session's Gone) takes over from the grace.
                    leaving.remove(&serial);
                    table.set_off(&serial, false);
                    let app = open.as_ref().map(|(pkg, user)| hub.app_name(pkg, *user));
                    let ready = runs.get(&serial).is_some_and(|run| run.active.is_some());
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
                    // Another phone's request is left alone (decision 5).
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
                    } else if let Some(run) = runs.get_mut(&serial) {
                        // Already wanted: a start in backoff is retried now.
                        if run.active.is_none() && run.starting.is_none() && run.retry_at.is_some()
                        {
                            run.failures = 0;
                            run.retry_at = Some(Instant::now());
                        }
                    } else {
                        // Other live phones keep their sessions.
                        runs.want(&serial);
                    }
                }
                Command::Disconnect { serial } if being_undone.as_ref() == Some(&serial) => {
                    tracing::info!(serial, "undo-setup running on it; it is off already");
                }
                Command::Disconnect { serial } => {
                    tracing::info!(serial, "disconnected in Lantern");
                    // Flags first: nothing may restart it while the session closes.
                    table.set_off(&serial, true);
                    table.set_live(&serial, false);
                    table.last_off = Some(serial.clone());
                    table.pending.remove(&serial);
                    // R10: its notice goes, and with it any offered app ("Connect your …").
                    prompts.send(ConnectEvent::Gone {
                        serial: serial.clone(),
                    });
                    // `bye`: the helper restores the phone. Only this phone's run ends.
                    let close = release(
                        &serial,
                        "disconnected in Lantern",
                        &mut runs,
                        &mut hub,
                        &mut leaving,
                    );
                    closing.spawn(&serial, close);
                }
                Command::UndoSetup { serial } if being_undone.as_ref() == Some(&serial) => {
                    tracing::info!(serial, "undo-setup is already running on it");
                }
                Command::UndoSetup { serial } => {
                    if let Some(other) = &being_undone {
                        tracing::info!(
                            serial,
                            other,
                            "undo-setup refused: another undo is running"
                        );
                        let other = names.get(other).unwrap_or_else(|| other.clone());
                        let name = names.get(&serial).unwrap_or_else(|| serial.clone());
                        if let Some(notify) = &notify {
                            notify(
                                &format!("Lantern is busy removing itself from {other}"),
                                &format!("Try {} again when that's done.", escape_markup(&name)),
                            );
                        }
                        continue;
                    }
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
                    // R10: its notice goes, and with it any offered app.
                    prompts.send(ConnectEvent::Gone {
                        serial: serial.clone(),
                    });
                    // The helper's connection is closed before the undo runs; `run_steps`
                    // itself waits for helper processes still restoring the phone. Only this
                    // phone's run ends.
                    let close = release(&serial, "undo-setup", &mut runs, &mut hub, &mut leaving);
                    let steps = run(serial.clone());
                    undoing = Some((
                        serial,
                        Box::pin(async move {
                            if let Some(close) = close {
                                close.await;
                            }
                            steps.await
                        }),
                    ));
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
/// no legacy entry). Then the menu files are brought up to date ([`catalog::refresh_menu`]), so
/// a layout change (each phone a top-level category) reaches the menu at the next start even
/// with no phone live. `pins` is Plasma's applets config, for picking the primary. `rebuild`
/// runs once if the migration or the menu changed files. A failed migration gives no catalogue
/// for this run: writing per-phone entries over unmigrated ones would mark it done; the next
/// start retries.
pub fn open_catalog(
    dirs: catalog::Dirs,
    pins: Option<&str>,
    rebuild: catalog::Rebuild,
) -> Option<catalog::Catalog> {
    let pending = !crate::naming::Naming::migrated(&dirs.state);
    let mut changed = false;
    match catalog::migrate(&dirs, pins) {
        Ok(m) if pending => {
            tracing::info!(
                "menu migrated: primary={} written={} removed={}",
                m.primary.as_deref().unwrap_or("none"),
                m.written,
                m.removed
            );
            changed = m.written + m.removed > 0;
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
    match catalog::refresh_menu(&dirs) {
        Ok(menu) => changed |= menu,
        Err(error) => tracing::warn!(
            error = format!("{error:#}"),
            "could not bring the phones' menu up to date"
        ),
    }
    if changed {
        rebuild();
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
    // The prompts' tasks: quitting waits for them to close their notices.
    let mut prompts = Vec::new();
    let (unlock, lock_events, request) = match prompt {
        Some((task, request)) => {
            prompts.push(task);
            (Some(pin_rx), Some(lock_tx), Some(request))
        }
        None => (None, None, None),
    };
    // The connect prompt on its own bus connection: "plugged in" / "Connect your …" / "Plug in
    // your …"; its Connect click comes back as a command. Without a session bus there is none:
    // plug-ins then wait silently and refused opens get the old errors.
    let (connect_tx, connect_rx) = mpsc::channel(16);
    let connect_health = crate::connect::Health::new();
    let (connect_events, connect_health) =
        match crate::connect::spawn(connect_rx, cmd_tx.clone(), connect_health.clone()).await {
            Some(task) => {
                prompts.push(task);
                (Some(connect_tx), Some(connect_health))
            }
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
    let parts = Parts {
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
    };
    // Read at each session start: a changed `lantern_keyboard` or `screen_off` applies from the
    // next one.
    let starter = move |serial| {
        let setup = setup.clone();
        let settings = session_settings.get();
        let (keyboard, screen_off) = (settings.lantern_keyboard, settings.screen_off);
        async move { start_with_companion(&Adb::new(serial), &setup, keyboard, screen_off).await }
    };
    let result = serve_with(
        listener,
        parts,
        configured,
        track_devices(),
        starter,
        shutdown,
        RetryPolicy::default(),
    )
    .await;
    // Quit or a signal: the run loop dropped every event sender with the hub and the controls,
    // so the prompts close their notices and end (decision 15).
    close_prompts(prompts, SHUTDOWN_NOTICES).await;
    result
}

/// Waits for the prompts' tasks to end (they close their notices first), at most `cap`; one
/// still running then is left to the runtime's end.
async fn close_prompts(prompts: Vec<tokio::task::JoinHandle<()>>, cap: Duration) {
    if prompts.is_empty() {
        return;
    }
    let all = futures_util::future::join_all(prompts);
    if tokio::time::timeout(cap, all).await.is_err() {
        tracing::warn!(?cap, "the prompts did not close their notices in time");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decision 15: quitting waits for the prompts to end, but never longer than its cap.
    #[tokio::test]
    async fn close_prompts_waits_within_its_cap() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let quick = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = tx.send(());
        });
        let t0 = std::time::Instant::now();
        close_prompts(vec![quick], Duration::from_secs(3)).await;
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        rx.await.expect("the prompt ran to its end");
        let hung = tokio::spawn(std::future::pending::<()>());
        let t0 = std::time::Instant::now();
        close_prompts(vec![hung], Duration::from_millis(300)).await;
        let took = t0.elapsed();
        assert!(
            took >= Duration::from_millis(300) && took < Duration::from_secs(2),
            "{took:?}"
        );
        close_prompts(Vec::new(), Duration::from_secs(3)).await;
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

    /// R8 (1): a phone's due start waits for its own close only; it is due again when that
    /// close ends.
    #[tokio::test]
    async fn closing_holds_only_its_own_phone() {
        let mut closing = Closing::default();
        let (done, wait) = tokio::sync::oneshot::channel::<()>();
        closing.spawn(
            "b",
            Some(Box::pin(async move {
                let _ = wait.await;
            })),
        );
        closing.spawn("dev", None);
        assert!(closing.hold("dev").is_none());
        let until = closing.hold("b").expect("b's close runs");
        assert!(until > Instant::now() + Duration::from_secs(4), "{until:?}");
        // Nothing has closed yet.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), closing.next_closed())
                .await
                .is_err()
        );
        done.send(()).unwrap();
        // Another phone's close or start meanwhile doesn't lose b's wake-up.
        tokio::time::sleep(Duration::from_millis(50)).await;
        closing.spawn("dev", None);
        assert!(closing.hold("dev").is_none());
        let serial = tokio::time::timeout(Duration::from_secs(2), closing.next_closed())
            .await
            .expect("b's close ended");
        assert_eq!(serial, "b");
        assert!(closing.hold("b").is_none());
        // A close nobody waited for ends quietly.
        closing.spawn("dev", Some(Box::pin(async {})));
        assert!(
            tokio::time::timeout(Duration::from_millis(200), closing.next_closed())
                .await
                .is_err()
        );
    }

    /// R8 (4): quitting waits for the closes at most its cap, then aborts the rest.
    #[tokio::test]
    async fn finish_aborts_closes_after_its_cap() {
        struct Dropped(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let mut closing = Closing::default();
        closing.spawn(
            "b",
            Some(Box::pin(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            })),
        );
        let started = Instant::now();
        closing.finish_within(Duration::from_millis(200)).await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "not aborted"
        );
        assert!(closing.closes.is_empty());
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

    /// An earlier build's "Phone" parent menu: the next start makes each phone a top-level
    /// category and drops Lantern's own `lantern-phone.directory`, once.
    #[test]
    fn startup_moves_a_nested_menu_to_the_top_level_once() {
        let (root, dirs) = scratch_dirs("toplevel");
        plant_legacy(&dirs, "com.a", "A;");
        let (_, rebuild) = counter();
        drop(open_catalog(dirs.clone(), None, rebuild));
        let menu = dirs.menus.join("lantern.menu");
        let parent = dirs.directories.join("lantern-phone.directory");
        let sub = "<Menu><Name>lantern-A</Name><Directory>lantern-phone-A.directory</Directory>\
                   <Include><Category>X-Lantern-A</Category></Include></Menu>";
        let head = "<!DOCTYPE Menu PUBLIC \"-//freedesktop//DTD Menu 1.0//EN\" \
                    \"http://www.freedesktop.org/standards/menu-spec/menu-1.0.dtd\">\n\
                    <Menu><Name>Applications</Name>";
        std::fs::write(
            &menu,
            format!(
                "{head}<Menu><Name>Phone</Name>\
                 <Directory>lantern-phone.directory</Directory>\n{sub}\n</Menu></Menu>\n"
            ),
        )
        .unwrap();
        std::fs::write(
            &parent,
            "[Desktop Entry]\nType=Directory\nName=Phone\nIcon=smartphone\n",
        )
        .unwrap();
        let (runs, rebuild) = counter();
        assert!(open_catalog(dirs.clone(), None, rebuild).is_some());
        assert_eq!(
            std::fs::read_to_string(&menu).unwrap(),
            format!("{head}\n{sub}\n</Menu>\n")
        );
        assert!(!parent.exists());
        assert!(dirs.directories.join("lantern-phone-A.directory").exists());
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
        // A later start changes nothing.
        let (runs, rebuild) = counter();
        assert!(open_catalog(dirs.clone(), None, rebuild).is_some());
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
