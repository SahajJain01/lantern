pub mod registry;
pub mod session;

use std::collections::HashSet;

use anyhow::{bail, Result};
use futures_util::StreamExt;
use session::SessionConn;
use tokio::signal::unix::{signal, SignalKind};

use crate::adb::{track_devices, Adb, DeviceEvent};
use crate::daemon::session::start;

fn select_serial(configured: Option<&str>, connected: &HashSet<String>) -> Option<String> {
    if let Some(serial) = configured {
        return Some(serial.to_owned());
    }
    if connected.len() == 1 {
        return connected.iter().next().cloned();
    }
    Some("20e9318".to_owned())
}

async fn close_session(session: &mut Option<SessionConn>) {
    if let Some(session) = session.take() {
        let serial = session.serial().to_owned();
        session.close().await;
        tracing::info!(serial, "session closed");
    }
}

pub async fn run() -> Result<()> {
    let configured = std::env::var("LANTERN_SERIAL")
        .ok()
        .filter(|s| !s.is_empty());
    let mut events = Box::pin(track_devices());
    let mut connected = HashSet::new();
    let mut active: Option<SessionConn> = None;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    loop {
        tokio::select! {
            _ = sigint.recv() => {
                tracing::info!("SIGINT received");
                close_session(&mut active).await;
                return Ok(());
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received");
                close_session(&mut active).await;
                return Ok(());
            }
            event = events.next() => {
                let Some(event) = event else {
                    close_session(&mut active).await;
                    bail!("adb track-devices ended");
                };
                match event {
                    DeviceEvent::Online(serial) => {
                        tracing::info!(serial, "device online");
                        connected.insert(serial.clone());
                        let target = select_serial(configured.as_deref(), &connected);
                        if active.is_none() && target.as_deref() == Some(serial.as_str()) {
                            let adb = Adb::new(serial.clone());
                            match start(&adb).await {
                                Ok(session) => {
                                    tracing::info!(serial, helper = session.helper_sha(), "session ready");
                                    active = Some(session);
                                }
                                Err(error) => tracing::error!(serial, %error, "session start failed"),
                            }
                        }
                    }
                    DeviceEvent::Offline(serial) => {
                        tracing::info!(serial, "device offline");
                        connected.remove(&serial);
                        if active.as_ref().map(SessionConn::serial) == Some(serial.as_str()) {
                            close_session(&mut active).await;
                        }
                    }
                }
            }
        }
    }
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
}
