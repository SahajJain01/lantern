pub mod ipc;
pub mod registry;
pub mod session;

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{bail, Result};
use futures_core::Stream;
use futures_util::StreamExt;
use ipc::Hub;
use session::{Inbound, SessionConn};
use tokio::net::UnixListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::Instant;

use crate::adb::{track_devices, Adb, DeviceEvent};
use crate::daemon::session::{start, HandshakeError};

fn select_serial(configured: Option<&str>, connected: &HashSet<String>) -> Option<String> {
    if let Some(serial) = configured {
        return Some(serial.to_owned());
    }
    if connected.len() == 1 {
        return connected.iter().next().cloned();
    }
    Some("20e9318".to_owned())
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

/// The daemon's state machine without a window socket: helper frames are drained and dropped.
/// See [`serve`].
pub async fn run_loop<E, S, F, D>(
    configured: Option<String>,
    events: E,
    starter: S,
    shutdown: D,
    policy: RetryPolicy,
) -> Result<()>
where
    E: Stream<Item = DeviceEvent>,
    S: FnMut(String) -> F,
    F: Future<Output = Result<SessionConn>>,
    D: Future<Output = &'static str>,
{
    drive(Hub::new(None), configured, events, starter, shutdown, policy).await
}

/// The daemon: phone session management plus window clients on `listener`. Generic over its
/// inputs so tests can drive it with fake devices, fake helpers and a fake shutdown signal.
/// The socket file is removed when this returns.
pub async fn serve<E, S, F, D>(
    listener: UnixListener,
    configured: Option<String>,
    events: E,
    starter: S,
    shutdown: D,
    policy: RetryPolicy,
) -> Result<()>
where
    E: Stream<Item = DeviceEvent>,
    S: FnMut(String) -> F,
    F: Future<Output = Result<SessionConn>>,
    D: Future<Output = &'static str>,
{
    let hub = Hub::new(Some(listener));
    let path = hub.socket_path();
    let result = drive(hub, configured, events, starter, shutdown, policy).await;
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Every branch is polled concurrently: a session start in flight never blocks shutdown, device
/// events or clients, and helper death is noticed through `SessionConn::closed`. The live
/// session's inbound frames are always drained (routed to window clients).
async fn drive<E, S, F, D>(
    mut hub: Hub,
    configured: Option<String>,
    events: E,
    mut starter: S,
    shutdown: D,
    policy: RetryPolicy,
) -> Result<()>
where
    E: Stream<Item = DeviceEvent>,
    S: FnMut(String) -> F,
    F: Future<Output = Result<SessionConn>>,
    D: Future<Output = &'static str>,
{
    tokio::pin!(events);
    tokio::pin!(shutdown);
    let mut connected = HashSet::new();
    let mut active: Option<(SessionConn, Instant)> = None;
    let mut starting: Starting<F> = None;
    let mut retry_at: Option<Instant> = None;
    let mut failures: u32 = 0;

    loop {
        let idle = active.is_none() && starting.is_none();
        let wake = retry_at.unwrap_or_else(Instant::now);
        // Set when the live session must be torn down and restarted.
        let mut ended: Option<&'static str> = None;
        tokio::select! {
            why = &mut shutdown => {
                tracing::info!("{why} received");
                cancel_start(&mut starting, why);
                close_session(&mut active, &mut hub).await;
                return Ok(());
            }
            event = events.next() => {
                let Some(event) = event else {
                    cancel_start(&mut starting, "device stream ended");
                    close_session(&mut active, &mut hub).await;
                    bail!("adb track-devices ended");
                };
                match event {
                    DeviceEvent::Online(serial) => {
                        tracing::info!(serial, "device online");
                        connected.insert(serial.clone());
                        let target = select_serial(configured.as_deref(), &connected);
                        if idle && target.as_deref() == Some(serial.as_str()) {
                            failures = 0;
                            retry_at = Some(Instant::now());
                        }
                    }
                    DeviceEvent::Offline(serial) => {
                        tracing::info!(serial, "device offline");
                        connected.remove(&serial);
                        if starting.as_ref().map(|(s, _)| s.as_str()) == Some(serial.as_str()) {
                            cancel_start(&mut starting, "device offline");
                        }
                        if active.as_ref().map(|(s, _)| s.serial()) == Some(serial.as_str()) {
                            close_session(&mut active, &mut hub).await;
                        }
                        let target = select_serial(configured.as_deref(), &connected);
                        if target.as_deref().is_none_or(|t| !connected.contains(t)) {
                            retry_at = None;
                            failures = 0;
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(wake), if idle && retry_at.is_some() => {
                retry_at = None;
                let target = select_serial(configured.as_deref(), &connected)
                    .filter(|target| connected.contains(target));
                if let Some(serial) = target {
                    tracing::info!(serial, attempt = failures + 1, "starting session");
                    starting = Some((serial.clone(), Box::pin(starter(serial))));
                }
            }
            result = async { starting.as_mut().expect("guarded").1.as_mut().await }, if starting.is_some() => {
                let (serial, _) = starting.take().expect("guarded");
                match result {
                    Ok(session) => {
                        tracing::info!(serial, helper = session.helper_sha(), "session ready");
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
            event = hub.next_event() => {
                let helper = active.as_ref().map(|(session, _)| session.tx.clone());
                if hub.on_client(event, helper.as_ref()).await.is_err() {
                    ended = Some("helper stopped reading");
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
        if let Some(why) = ended {
            let lived = active.as_ref().map(|(_, since)| since.elapsed()).unwrap_or_default();
            let serial = active.as_ref().map(|(s, _)| s.serial().to_owned()).unwrap_or_default();
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
    }
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
    serve(
        listener,
        configured,
        track_devices(),
        |serial| async move { start(&Adb::new(serial)).await },
        shutdown,
        RetryPolicy::default(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devices(serials: &[&str]) -> HashSet<String> {
        serials.iter().map(|serial| (*serial).to_owned()).collect()
    }

    #[test]
    fn configured_serial_wins_over_single_device_default() {
        assert_eq!(
            select_serial(Some("chosen"), &devices(&["only"])),
            Some("chosen".into())
        );
    }

    #[test]
    fn unset_serial_uses_only_device() {
        assert_eq!(
            select_serial(None, &devices(&["only"])),
            Some("only".into())
        );
    }

    #[test]
    fn unset_serial_defaults_when_multiple_devices_are_connected() {
        assert_eq!(
            select_serial(None, &devices(&["one", "two"])),
            Some("20e9318".into())
        );
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
