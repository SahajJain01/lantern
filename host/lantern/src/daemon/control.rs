//! Controlling lanternd from the tray menu and `lantern connect|disconnect`: the commands it
//! takes and the phone state ([`View`]) it publishes after every change.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use lantern_proto::{CompanionState, DaemonCmd};

/// A command for the daemon's run loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Serve the paused phone again; a start in backoff is retried now.
    Connect,
    /// End the session and leave the phone alone until it is replugged or connected again.
    Disconnect,
    /// Close the session, then remove Lantern from the phone (`undo::run_steps`).
    UndoSetup,
    /// End the daemon as on SIGTERM (after a running undo finishes).
    Quit,
}

impl From<DaemonCmd> for Command {
    fn from(cmd: DaemonCmd) -> Self {
        match cmd {
            DaemonCmd::Connect => Self::Connect,
            DaemonCmd::Disconnect => Self::Disconnect,
        }
    }
}

/// What the daemon is doing with which phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phone {
    /// No target phone online.
    None,
    Starting {
        serial: String,
    },
    Ready {
        serial: String,
        /// The live session's last `companion` state and its `msg`.
        companion: Option<(CompanionState, Option<String>)>,
    },
    /// Retry backoff after a failed start or helper death; the phone is still online.
    Retrying {
        serial: String,
    },
    /// Disconnect, or after Undo, until the phone is replugged or connected again.
    Paused {
        serial: String,
    },
    Undoing {
        serial: String,
    },
}

impl Phone {
    /// The phone this state is about.
    pub fn serial(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Starting { serial }
            | Self::Ready { serial, .. }
            | Self::Retrying { serial }
            | Self::Paused { serial }
            | Self::Undoing { serial } => Some(serial),
        }
    }
}

/// The daemon's state as the tray shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub phone: Phone,
    /// The phone's recorded name (`companion::Names`), if it has one.
    pub name: Option<String>,
}

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
    fn socket_commands_map_to_commands() {
        assert_eq!(Command::from(DaemonCmd::Connect), Command::Connect);
        assert_eq!(Command::from(DaemonCmd::Disconnect), Command::Disconnect);
    }

    #[test]
    fn phone_serial() {
        assert_eq!(Phone::None.serial(), None);
        let ready = Phone::Ready {
            serial: "a".into(),
            companion: None,
        };
        assert_eq!(ready.serial(), Some("a"));
        assert_eq!(Phone::Undoing { serial: "b".into() }.serial(), Some("b"));
    }
}
