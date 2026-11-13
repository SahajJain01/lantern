//! The connect rules: what lanternd does when a phone comes online. Pure, so every row of the
//! table is unit-tested; the run loop (`daemon::drive`) applies the answer.

use std::time::Duration;

/// The run loop's timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// How long the wanted phone may be offline before it counts as unplugged (a cable blip or
    /// adb restart within it reconnects without asking).
    pub unplug_grace: Duration,
    /// How long a `Connect` that carries an app to open waits for that phone's session.
    pub pending_for: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            unplug_grace: Duration::from_secs(10),
            pending_for: Duration::from_secs(120),
        }
    }
}

/// What to do with a phone that just came online.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnPlug {
    /// Make it the wanted phone (closing another live one, whose `off` flag is untouched).
    Connect,
    /// Leave it waiting and ask the user ("<phone> is plugged in").
    Prompt,
    /// Leave it waiting without asking.
    Wait,
    /// Not this lanternd's phone (`LANTERN_SERIAL` names another).
    Ignore,
}

/// The rule for `serial` coming online, evaluated in this order:
/// 1. `configured` (`$LANTERN_SERIAL`) is set: Connect for that serial, Ignore for any other.
/// 2. `pending` (a connect-and-open request for it that hasn't expired): Connect.
/// 3. `live` (it was connected when lanternd stopped): Connect if nothing is `wanted`, else
///    Wait.
/// 4. `autoconnect && !off`: Connect if nothing is wanted, else Prompt.
/// 5. `autoconnect && off`: Wait.
/// 6. `!autoconnect`: Prompt.
pub fn on_plug(
    serial: &str,
    configured: Option<&str>,
    autoconnect: bool,
    off: bool,
    live: bool,
    pending: bool,
    wanted: Option<&str>,
) -> OnPlug {
    if let Some(configured) = configured {
        return if configured == serial {
            OnPlug::Connect
        } else {
            OnPlug::Ignore
        };
    }
    if pending {
        return OnPlug::Connect;
    }
    if live {
        return if wanted.is_none() {
            OnPlug::Connect
        } else {
            OnPlug::Wait
        };
    }
    match (autoconnect, off) {
        (true, false) if wanted.is_none() => OnPlug::Connect,
        (true, false) => OnPlug::Prompt,
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
    }

    #[test]
    fn on_plug_table() {
        use OnPlug::*;
        // (configured, autoconnect, off, live, pending, wanted) for serial "s".
        let rule = |configured, autoconnect, off, live, pending, wanted| {
            on_plug("s", configured, autoconnect, off, live, pending, wanted)
        };
        // 1. configured: Connect for that serial whatever else holds, Ignore for any other.
        assert_eq!(rule(Some("s"), false, true, false, false, None), Connect);
        assert_eq!(
            rule(Some("s"), false, true, false, false, Some("x")),
            Connect
        );
        assert_eq!(rule(Some("x"), true, false, true, true, None), Ignore);
        // 2. pending: Connect, even off, with autoconnect off and another phone wanted.
        assert_eq!(rule(None, false, true, false, true, Some("x")), Connect);
        // 3. live: Connect if nothing is wanted, else Wait.
        assert_eq!(rule(None, false, false, true, false, None), Connect);
        assert_eq!(rule(None, false, false, true, false, Some("x")), Wait);
        // 4. autoconnect and not off: Connect if nothing is wanted, else Prompt.
        assert_eq!(rule(None, true, false, false, false, None), Connect);
        assert_eq!(rule(None, true, false, false, false, Some("x")), Prompt);
        // 5. autoconnect and off: Wait.
        assert_eq!(rule(None, true, true, false, false, None), Wait);
        assert_eq!(rule(None, true, true, false, false, Some("x")), Wait);
        // 6. autoconnect off: Prompt, off or not, wanted or not.
        assert_eq!(rule(None, false, false, false, false, None), Prompt);
        assert_eq!(rule(None, false, true, false, false, None), Prompt);
        assert_eq!(rule(None, false, false, false, false, Some("x")), Prompt);
    }
}
