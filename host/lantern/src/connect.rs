//! The connect prompt: "<phone> is plugged in" and "Connect your <phone>" (an open refused for
//! a phone that isn't connected), each with a Connect action, and the action-less "Plug in your
//! <phone>". Each phone has at most one notice on screen; a click on Connect sends
//! [`Command::Connect`] to lanternd's run loop, the only way this prompt connects anything.
//!
//! [`Health`] tells lanternd whether these notices get through: it offers Connect for a refused
//! open (and the window stays silent) only while they do, so a missing or hung notification
//! server never leaves the user with no notice at all.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zbus::Connection;

use crate::daemon::control::{Command, ConnectEvent};
use crate::notices::{escape_markup, server_answers, Notices};

/// The notification action that connects the phone.
const ACTION: &str = "connect";
/// How often an unhealthy prompt asks the server whether it is back.
const PROBE_EVERY: Duration = Duration::from_secs(3);

/// "{name} is plugged in" and its body. Summaries are plain text in Plasma, so only bodies
/// are escaped for markup.
pub fn plugged_notice(name: &str) -> (String, String) {
    (
        format!("{name} is plugged in"),
        "Connect it to use its apps on this PC. Its screen turns off while it is connected."
            .to_owned(),
    )
}

/// "Connect your {name}" for an open of `app` refused because its phone isn't connected
/// (`name` and `app` escaped in the body only).
pub fn offered_notice(name: &str, app: &str) -> (String, String) {
    let summary = format!("Connect your {name}");
    let (name, app) = (escape_markup(name), escape_markup(app));
    (
        summary,
        format!("{app} is on your {name}. Connect it to open {app} here."),
    )
}

/// "Plug in your {name}" after Connect for a phone that isn't plugged in (`app` opens on it;
/// escaped in the body).
pub fn plug_in_notice(name: &str, app: Option<&str>) -> (String, String) {
    let body = match app {
        Some(app) => format!(
            "{} opens if you plug it in within 2 minutes.",
            escape_markup(app)
        ),
        None => "It connects if you plug it in within 2 minutes.".to_owned(),
    };
    (format!("Plug in your {name}"), body)
}

/// Whether the connect prompt's notifications get through: false until the server first
/// answers, false after a failed `Notify`, true again once it answers. Cheap to clone; the
/// daemon's [`crate::daemon::ipc::Hub`] reads it for every refused open.
#[derive(Debug, Clone, Default)]
pub struct Health(Arc<AtomicBool>);

impl Health {
    /// Unhealthy until the prompt's first successful call.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn healthy(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn set(&self, healthy: bool) {
        let was = self.0.swap(healthy, Ordering::SeqCst);
        if was != healthy {
            tracing::info!(healthy, "connect prompt notifications");
        }
    }
}

/// A notice on screen for one phone.
struct Shown {
    id: u32,
    /// `Some` while it has the Connect action: the app (pkg, user) to open after connecting
    /// (`None` inside for a plain plug-in).
    connect: Option<Option<(String, u32)>>,
}

/// Shows the connect notices for `events` and sends a Connect click as
/// [`Command::Connect`] on `commands` (`try_send`), until `events` closes. `health` follows the
/// notification server (see [`Health`]). Without a server it logs one warning and carries on.
pub async fn run(
    conn: Connection,
    mut events: mpsc::Receiver<ConnectEvent>,
    commands: mpsc::Sender<Command>,
    health: Health,
) -> Result<()> {
    let (mut notices, mut actions) = Notices::new(conn).await?;
    // Asks the server whether it answers while the prompt is unhealthy; ends with `run`.
    let probe = {
        let conn = notices.connection().clone();
        let health = health.clone();
        AbortOnDrop(tokio::spawn(async move {
            loop {
                if !health.healthy() && server_answers(&conn).await {
                    health.set(true);
                }
                tokio::time::sleep(PROBE_EVERY).await;
            }
        }))
    };
    // At most one notice per phone.
    let mut shown: HashMap<String, Shown> = HashMap::new();
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                let (serial, (summary, body), connect) = match event {
                    ConnectEvent::PluggedIn { serial, name } => {
                        (serial, plugged_notice(&name), Some(None))
                    }
                    ConnectEvent::OpenRefused { serial, name, app, pkg, user } => {
                        (serial, offered_notice(&name, &app), Some(Some((pkg, user))))
                    }
                    ConnectEvent::PlugIn { serial, name, app } => {
                        (serial, plug_in_notice(&name, app.as_deref()), None)
                    }
                    ConnectEvent::Gone { serial } => {
                        if let Some(old) = shown.remove(&serial) {
                            tracing::debug!(serial, id = old.id, "closing the connect notice");
                            notices.close(old.id).await;
                        }
                        continue;
                    }
                };
                let replaces = shown.get(&serial).map_or(0, |old| old.id);
                let action: &[(&str, &str)] = if connect.is_some() {
                    &[(ACTION, "Connect")]
                } else {
                    &[]
                };
                let id = notices.notify(replaces, &summary, &body, action).await;
                health.set(id != 0);
                if id == 0 {
                    // The offer was already answered: the window that asked shows nothing.
                    if let Some(Some((pkg, user))) = &connect {
                        tracing::warn!(
                            serial,
                            pkg,
                            user,
                            "\"Connect your …\" not shown; later opens get the window's own notice until notifications work"
                        );
                    }
                    shown.remove(&serial);
                    continue;
                }
                shown.insert(serial, Shown { id, connect });
            }
            message = actions.next() => {
                let Some(Ok(message)) = message else { continue };
                let Some((id, action)) = notices.action(&message) else { continue };
                if id == 0 || action != ACTION {
                    continue;
                }
                let Some(serial) = shown
                    .iter()
                    .find(|(_, s)| s.id == id && s.connect.is_some())
                    .map(|(serial, _)| serial.clone())
                else {
                    continue;
                };
                let Some(Shown { connect: Some(open), .. }) = shown.remove(&serial) else {
                    continue;
                };
                tracing::info!(serial, ?open, "Connect clicked");
                if let Err(error) = commands.try_send(Command::Connect { serial, open }) {
                    tracing::warn!(%error, "Connect click dropped");
                }
                notices.close(id).await;
            }
        }
    }
    drop(probe);
    Ok(())
}

/// Aborts the task when dropped.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts [`run`] on its own session-bus connection (5 s to connect). Without a bus there is
/// no connect prompt: one warning, and `None` (plug-ins then wait silently and refused opens
/// get the old errors).
pub async fn spawn(
    events: mpsc::Receiver<ConnectEvent>,
    commands: mpsc::Sender<Command>,
    health: Health,
) -> Option<JoinHandle<()>> {
    const TIMEOUT: Duration = Duration::from_secs(5);
    let conn = match tokio::time::timeout(TIMEOUT, Connection::session()).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(error)) => {
            tracing::warn!("no connect prompt: {error}");
            return None;
        }
        Err(_) => {
            tracing::warn!("no connect prompt: the session bus did not answer within {TIMEOUT:?}");
            return None;
        }
    };
    Some(tokio::spawn(async move {
        if let Err(error) = run(conn, events, commands, health).await {
            tracing::warn!(error = format!("{error:#}"), "connect prompt stopped");
        }
    }))
}
