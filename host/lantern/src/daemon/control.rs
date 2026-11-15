//! Controlling lanternd from the tray menu and `lantern connect|disconnect`: the commands it
//! takes, the per-phone state ([`View`]) it publishes after every change, and the connect
//! events the prompts show.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use lantern_proto::{CompanionState, PhoneReport, PhoneState};

/// A command for the daemon's run loop. Each names its phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Clear the phone's `off` flag and start its session if it is plugged in (other live
    /// phones keep theirs); otherwise ask the user to plug it in.
    /// `open` (pkg, user) is opened on it once its session is ready (within
    /// [`super::phones::Timing::pending_for`]).
    Connect {
        serial: String,
        open: Option<(String, u32)>,
    },
    /// Set the phone's `off` flag (it stays off across unplugging and restarts) and end its
    /// session if it has one.
    Disconnect { serial: String },
    /// Switch the phone off, close its session if it has one, then remove Lantern from it
    /// (`undo::run_steps`). Only for a plugged-in phone.
    UndoSetup { serial: String },
    /// End the daemon (after a running undo finishes); every live phone's `live` flag is
    /// cleared.
    Quit,
}

/// One phone as the tray and `lantern status` show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneView {
    pub serial: String,
    /// Its recorded name (`companion::Names`), if it has one.
    pub name: Option<String>,
    pub state: PhoneState,
    /// adb reports it online.
    pub plugged: bool,
    /// The user switched it off in Lantern.
    pub off: bool,
    /// Its PIN lock is in the way: from the helper's `locked` until `unlocked`, a successful
    /// `unlock_result`, or the end of the session. Only ever true for a live phone.
    pub locked: bool,
    /// Its live session's last `companion` state and its `msg` (live phones only).
    pub companion: Option<(CompanionState, Option<String>)>,
}

impl PhoneView {
    /// Its name, else its serial.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.serial)
    }

    /// The `status_report.phones` entry, with `connected_at` from its record.
    pub fn report(&self, connected_at: Option<u64>) -> PhoneReport {
        PhoneReport {
            serial: self.serial.clone(),
            name: self.name.clone(),
            state: self.state,
            plugged: self.plugged,
            off: self.off,
            connected_at,
        }
    }
}

/// The daemon's phones: plugged ones in the order they came online, then the other known
/// phones by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    pub phones: Vec<PhoneView>,
}

impl View {
    /// Every phone being served, started, retried or reconnected, in view order.
    pub fn live_phones(&self) -> impl Iterator<Item = &PhoneView> {
        self.phones.iter().filter(|p| p.state.is_live())
    }

    pub fn phone(&self, serial: &str) -> Option<&PhoneView> {
        self.phones.iter().find(|p| p.serial == serial)
    }
}

/// What the connect prompt shows (Task 6). Sent with `try_send`: a full channel drops it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectEvent {
    /// A phone was plugged in and Lantern asks before connecting it (one per plug-in).
    PluggedIn { serial: String, name: String },
    /// An open was refused because its phone isn't connected.
    OpenRefused {
        serial: String,
        name: String,
        app: String,
        pkg: String,
        user: u32,
    },
    /// Connect for a phone that isn't plugged in: plug it in (`app` will open on it).
    PlugIn {
        serial: String,
        name: String,
        app: Option<String>,
    },
    /// The phone connected or was unplugged: close its notices.
    Gone { serial: String },
}

/// Opens an app on a phone once its session is ready: `(serial, pkg, user)`.
pub type Opener = Arc<dyn Fn(&str, &str, u32) + Send + Sync>;

/// Removes Lantern from the phone with this serial: `(all steps ok, output lines joined with
/// newlines)`.
pub type UndoRunner = Arc<dyn Fn(String) -> BoxFuture<'static, (bool, String)> + Send + Sync>;

/// Shows a desktop notification (summary, body).
pub type Notifier = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// The notification after an undo on the phone called `name`: one body line per step.
pub fn undo_notice(ok: bool, name: &str, output: &str) -> (String, String) {
    let summary = if ok {
        format!("Lantern removed itself from {name}")
    } else {
        format!("Undo phone setup failed on {name}")
    };
    (summary, output.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone(serial: &str, state: PhoneState) -> PhoneView {
        PhoneView {
            serial: serial.into(),
            name: None,
            state,
            plugged: true,
            off: false,
            locked: false,
            companion: None,
        }
    }

    #[test]
    fn undo_notice_texts() {
        assert_eq!(
            undo_notice(true, "Pixel", "a: ok\nb: ok"),
            (
                "Lantern removed itself from Pixel".to_owned(),
                "a: ok\nb: ok".to_owned()
            )
        );
        assert_eq!(
            undo_notice(false, "20e9318", "a: FAILED: x").0,
            "Undo phone setup failed on 20e9318"
        );
    }

    #[test]
    fn view_live_phones_and_phone() {
        let mut a = phone("a", PhoneState::Waiting);
        a.name = Some("Pixel".into());
        let b = phone("b", PhoneState::Reconnecting);
        let view = View {
            phones: vec![a.clone(), b.clone()],
        };
        assert_eq!(view.live_phones().collect::<Vec<_>>(), [&b]);
        let c = phone("c", PhoneState::Ready);
        let two = View {
            phones: vec![a.clone(), b.clone(), c.clone()],
        };
        assert_eq!(two.live_phones().collect::<Vec<_>>(), [&b, &c]);
        assert_eq!(View::default().live_phones().count(), 0);
        assert_eq!(view.phone("a"), Some(&a));
        assert_eq!(view.phone("c"), None);
        assert_eq!(a.display_name(), "Pixel");
        assert_eq!(b.display_name(), "b");
        let mut off = phone("c", PhoneState::Off);
        off.off = true;
        assert_eq!(
            off.report(Some(7)),
            PhoneReport {
                serial: "c".into(),
                name: None,
                state: PhoneState::Off,
                plugged: true,
                off: true,
                connected_at: Some(7),
            }
        );
    }
}
