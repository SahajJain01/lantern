//! The connect rules: what lanternd does when a phone comes online. Pure, so every row of the
//! table is unit-tested; the run loop (`daemon::drive`) applies the answer.

use std::time::Duration;

/// The run loop's timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// How long the wanted phone may be offline before it counts as unplugged (a cable blip or
    /// adb restart within it reconnects without asking).
    pub unplug_grace: Duration,
    /// How long a `Connect` for a phone that isn't ready is remembered: a phone not plugged in
    /// connects if plugged in within it, and an app it carries opens once that phone is ready.
    pub pending_for: Duration,
    /// How long after the unplug grace the start-up stale-`live` check waits for adb's first
    /// device list before it runs anyway (decision 14).
    pub list_wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            unplug_grace: Duration::from_secs(10),
            pending_for: Duration::from_secs(120),
            list_wait: Duration::from_secs(30),
        }
    }
}

/// What to do with a phone that just came online.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnPlug {
    /// Start its session (another live phone keeps its own).
    Connect,
    /// Leave it waiting and ask the user ("<phone> is plugged in").
    Prompt,
    /// Leave it waiting without asking.
    Wait,
    /// Not this lanternd's phone (`LANTERN_SERIAL` names another).
    Ignore,
}

/// The rule for `serial` coming online, evaluated in this order. Other live phones play no
/// part: every phone that qualifies connects (decision 4).
/// 1. `configured` (`$LANTERN_SERIAL`) is set: Connect for that serial, Ignore for any other.
/// 2. `pending` (a connect request for it, with or without an app, that hasn't expired):
///    Connect.
/// 3. `live` (it was connected when lanternd stopped): Connect.
/// 4. `autoconnect && !off`: Connect.
/// 5. `autoconnect && off`: Wait.
/// 6. `!autoconnect`: Prompt.
pub fn on_plug(
    serial: &str,
    configured: Option<&str>,
    autoconnect: bool,
    off: bool,
    live: bool,
    pending: bool,
) -> OnPlug {
    if let Some(configured) = configured {
        return if configured == serial {
            OnPlug::Connect
        } else {
            OnPlug::Ignore
        };
    }
    if pending || live {
        return OnPlug::Connect;
    }
    match (autoconnect, off) {
        (true, false) => OnPlug::Connect,
        (true, true) => OnPlug::Wait,
        (false, _) => OnPlug::Prompt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_defaults() {
        let t = Timing::default();
        assert_eq!(t.unplug_grace, Duration::from_secs(10));
        assert_eq!(t.pending_for, Duration::from_secs(120));
        assert_eq!(t.list_wait, Duration::from_secs(30));
    }

    #[test]
    fn on_plug_table() {
        use OnPlug::*;
        // (configured, autoconnect, off, live, pending) for serial "s".
        let rule = |configured, autoconnect, off, live, pending| {
            on_plug("s", configured, autoconnect, off, live, pending)
        };
        // 1. configured: Connect for that serial whatever else holds, Ignore for any other.
        assert_eq!(rule(Some("s"), false, true, false, false), Connect);
        assert_eq!(rule(Some("x"), true, false, true, true), Ignore);
        // 2. pending: Connect, even off with autoconnect off.
        assert_eq!(rule(None, false, true, false, true), Connect);
        // 3. live: Connect, even with autoconnect off.
        assert_eq!(rule(None, false, false, true, false), Connect);
        // 4. autoconnect and not off: Connect.
        assert_eq!(rule(None, true, false, false, false), Connect);
        // 5. autoconnect and off: Wait.
        assert_eq!(rule(None, true, true, false, false), Wait);
        // 6. autoconnect off: Prompt, off or not.
        assert_eq!(rule(None, false, false, false, false), Prompt);
        assert_eq!(rule(None, false, true, false, false), Prompt);
    }
}
