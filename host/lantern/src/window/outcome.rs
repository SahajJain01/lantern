//! The window client's protocol bookkeeping that decides how `lantern open` ends, kept free of
//! winit so it is unit-testable: which window id is ours, the phone's last `error{msg}`, and the
//! exit status and message for a `closed` or a daemon disconnect.

use lantern_proto::CloseReason;

/// How the process ends: exit status plus what to print on stderr (`lantern: <message>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    pub code: i32,
    pub message: Option<String>,
}

/// Why the daemon refused an open in a way that gets a "Connect your …" notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// No phone is connected (or lanternd is not running).
    NoPhone,
    /// The connected phone does not have the app; another phone does.
    NotOnThisPhone,
}

#[derive(Debug, Default)]
pub struct OpenState {
    /// Window id from `opened`.
    win: Option<u32>,
    /// The phone's last `error{msg}` for us, shown when the window or the open fails.
    error_msg: Option<String>,
}

impl OpenState {
    pub fn win(&self) -> Option<u32> {
        self.win
    }

    pub fn opened(&mut self, stream: u32) {
        self.win = Some(stream);
    }

    /// `error{msg}` on `stream`. On stream 0 before any `opened` the daemon refused the open
    /// (and closes the connection): exit now. Otherwise it is remembered for the `closed` or
    /// disconnect that follows.
    pub fn on_error(&mut self, stream: u32, msg: String) -> Option<Exit> {
        if stream == 0 && self.win.is_none() {
            return Some(Exit {
                code: 1,
                message: Some(msg),
            });
        }
        self.error_msg = Some(msg);
        None
    }

    /// `closed{reason}` on `stream`; `None` if it is not for us. A window that failed to open
    /// on the phone gets `error{msg}` then `closed{reason:"error"}` on its stream and never an
    /// `opened`, so a `closed` on a non-zero stream while we have no window is our open failing.
    pub fn on_closed(&self, stream: u32, reason: CloseReason) -> Option<Exit> {
        let ours = match self.win {
            Some(win) => win == stream,
            None => stream != 0,
        };
        if !ours {
            return None;
        }
        Some(match (reason, self.win) {
            (CloseReason::Closed | CloseReason::AppExited, Some(_)) => Exit {
                code: 0,
                message: None,
            },
            (CloseReason::Error, Some(_)) => self.failure("the window failed on the phone"),
            (_, None) => self.failure("the app could not be opened on the phone"),
        })
    }

    /// The daemon closed the connection while we had no window (and were not told to focus an
    /// existing one).
    pub fn on_disconnected(&self) -> Exit {
        const WHY: &str = "the daemon closed the connection";
        let message = match &self.error_msg {
            Some(msg) => format!("{msg} ({WHY})"),
            None => WHY.to_owned(),
        };
        Exit {
            code: 1,
            message: Some(message),
        }
    }

    /// Which "Connect your …" notification `error{msg}` on `stream` calls for: only the daemon
    /// refusing our open (stream 0, before `opened`) because no phone is connected or the
    /// connected one lacks the app.
    pub fn notice(&self, stream: u32, msg: &str) -> Option<Notice> {
        use crate::daemon::ipc::{NOT_CONNECTED, NOT_ON_THIS_PHONE};
        if stream != 0 || self.win.is_some() {
            return None;
        }
        match msg {
            NOT_CONNECTED => Some(Notice::NoPhone),
            NOT_ON_THIS_PHONE => Some(Notice::NotOnThisPhone),
            _ => None,
        }
    }

    fn failure(&self, fallback: &str) -> Exit {
        Exit {
            code: 1,
            message: Some(
                self.error_msg
                    .clone()
                    .unwrap_or_else(|| fallback.to_owned()),
            ),
        }
    }
}

/// Whether failing to connect to lanternd's socket means nobody is serving it (not running or a
/// stale socket), which for the user is the same as no phone being connected.
pub fn daemon_unreachable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(msg: &str) -> Option<Exit> {
        Some(Exit {
            code: 1,
            message: Some(msg.into()),
        })
    }

    /// The helper's reply to an open it cannot do: error then closed{error} on N, no opened.
    #[test]
    fn failed_open_exits_1_with_the_phones_message() {
        let mut s = OpenState::default();
        assert_eq!(s.on_error(1, "no launch intent for com.foo".into()), None);
        assert_eq!(
            s.on_closed(1, CloseReason::Error),
            failed("no launch intent for com.foo")
        );
    }

    #[test]
    fn failed_open_without_a_message_still_fails() {
        let s = OpenState::default();
        assert_eq!(
            s.on_closed(3, CloseReason::Error),
            failed("the app could not be opened on the phone")
        );
    }

    #[test]
    fn disconnect_after_a_phone_error_shows_it() {
        let mut s = OpenState::default();
        assert_eq!(s.on_error(1, "no launch intent for com.foo".into()), None);
        assert_eq!(
            Some(s.on_disconnected()),
            failed("no launch intent for com.foo (the daemon closed the connection)")
        );
        assert_eq!(
            Some(OpenState::default().on_disconnected()),
            failed("the daemon closed the connection")
        );
    }

    #[test]
    fn daemon_refusal_on_stream_0_exits_at_once() {
        let mut s = OpenState::default();
        assert_eq!(
            s.on_error(0, "phone not connected".into()),
            failed("phone not connected")
        );
    }

    #[test]
    fn closed_for_our_window() {
        let mut s = OpenState::default();
        s.opened(2);
        assert_eq!(s.on_closed(5, CloseReason::Closed), None, "not ours");
        assert_eq!(s.on_closed(0, CloseReason::Error), None, "not ours");
        assert_eq!(
            s.on_closed(2, CloseReason::AppExited),
            Some(Exit {
                code: 0,
                message: None
            })
        );
        assert_eq!(
            s.on_closed(2, CloseReason::Error),
            failed("the window failed on the phone")
        );
        assert_eq!(s.on_error(2, "encoder died".into()), None);
        assert_eq!(s.on_closed(2, CloseReason::Error), failed("encoder died"));
    }

    #[test]
    fn notice_only_for_refused_open() {
        let s = OpenState::default();
        assert_eq!(s.notice(0, "phone not connected"), Some(Notice::NoPhone));
        assert_eq!(
            s.notice(0, "app not on this phone"),
            Some(Notice::NotOnThisPhone)
        );
        assert_eq!(s.notice(0, "open out of range: user=..."), None);
        assert_eq!(s.notice(3, "phone not connected"), None);
        assert_eq!(s.notice(3, "app not on this phone"), None);
        let mut opened = OpenState::default();
        opened.opened(1);
        assert_eq!(opened.notice(0, "phone not connected"), None);
        assert_eq!(opened.notice(0, "app not on this phone"), None);
    }

    #[test]
    fn unreachable_daemon_means_no_phone() {
        use std::io::{Error, ErrorKind};
        assert!(daemon_unreachable(&Error::from(ErrorKind::NotFound)));
        assert!(daemon_unreachable(&Error::from(
            ErrorKind::ConnectionRefused
        )));
        assert!(!daemon_unreachable(&Error::from(
            ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn closed_on_stream_0_before_opened_is_not_ours() {
        assert_eq!(OpenState::default().on_closed(0, CloseReason::Error), None);
    }
}
