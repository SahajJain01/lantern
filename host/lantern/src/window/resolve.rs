//! Which phone `lantern open` without `--phone` uses (an old pin, a hand-typed command): pure,
//! from lanternd's `status_report.phones` and the phones whose menu entries list the app.

use lantern_proto::PhoneReport;

/// Where the window goes.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    /// Open on this live phone.
    Phone(String),
    /// No live phone has the app: name this phone (the one that connected last) so lanternd
    /// offers to connect it; `None`: no phone is known for the app and none is live.
    Connect(Option<String>),
}

/// Recency for picking a phone: the largest `connected_at` (unknown = 0), ties to the smallest
/// serial.
fn recency(connected_at: Option<u64>, serial: &str) -> (u64, std::cmp::Reverse<&str>) {
    (connected_at.unwrap_or(0), std::cmp::Reverse(serial))
}

/// `phones`: lanternd's phones; `have`: the serials whose menu entries list the app.
/// 1. A live phone that has the app (the most recently connected one).
/// 2. No phone is known to have the app (a new app): the most recently connected live phone.
/// 3. Phones have the app but none of them is live: `Connect` the one connected last.
/// 4. Otherwise `Connect(None)`.
pub fn resolve(phones: &[PhoneReport], have: &[String]) -> Target {
    let live = || phones.iter().filter(|p| p.state.is_live());
    let has = |serial: &str| have.iter().any(|h| h == serial);
    let newest = |it: &mut dyn Iterator<Item = &PhoneReport>| {
        it.max_by(|a, b| {
            recency(a.connected_at, &a.serial).cmp(&recency(b.connected_at, &b.serial))
        })
        .map(|p| p.serial.clone())
    };
    if let Some(serial) = newest(&mut live().filter(|p| has(&p.serial))) {
        return Target::Phone(serial);
    }
    if have.is_empty() {
        return match newest(&mut live()) {
            Some(serial) => Target::Phone(serial),
            None => Target::Connect(None),
        };
    }
    let connected_at = |serial: &str| {
        phones
            .iter()
            .find(|p| p.serial == serial)
            .and_then(|p| p.connected_at)
    };
    let last = have
        .iter()
        .max_by(|a, b| recency(connected_at(a), a).cmp(&recency(connected_at(b), b)));
    Target::Connect(last.cloned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lantern_proto::PhoneState;

    fn phone(serial: &str, state: PhoneState, connected_at: Option<u64>) -> PhoneReport {
        PhoneReport {
            serial: serial.into(),
            name: None,
            state,
            plugged: state != PhoneState::Unplugged,
            off: state == PhoneState::Off,
            connected_at,
        }
    }

    fn have(serials: &[&str]) -> Vec<String> {
        serials.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn one_live_phone_with_the_app() {
        let phones = [
            phone("a", PhoneState::Waiting, Some(900)),
            phone("b", PhoneState::Ready, Some(100)),
        ];
        assert_eq!(
            resolve(&phones, &have(&["a", "b"])),
            Target::Phone("b".into())
        );
        // Every live state counts.
        for state in [
            PhoneState::Starting,
            PhoneState::Retrying,
            PhoneState::Reconnecting,
        ] {
            let phones = [phone("b", state, None)];
            assert_eq!(resolve(&phones, &have(&["b"])), Target::Phone("b".into()));
        }
    }

    /// Shaped for 4c (several live phones): the most recently connected one wins.
    #[test]
    fn two_live_newest_wins() {
        let phones = [
            phone("a", PhoneState::Ready, Some(200)),
            phone("b", PhoneState::Ready, Some(300)),
        ];
        assert_eq!(
            resolve(&phones, &have(&["a", "b"])),
            Target::Phone("b".into())
        );
        // Only the live phones that have the app count.
        assert_eq!(resolve(&phones, &have(&["a"])), Target::Phone("a".into()));
        // Ties go to the smallest serial.
        let tied = [
            phone("b", PhoneState::Ready, Some(5)),
            phone("a", PhoneState::Ready, Some(5)),
        ];
        assert_eq!(
            resolve(&tied, &have(&["a", "b"])),
            Target::Phone("a".into())
        );
    }

    #[test]
    fn unknown_app_goes_to_live_phone() {
        let phones = [
            phone("a", PhoneState::Off, Some(900)),
            phone("b", PhoneState::Ready, Some(100)),
        ];
        assert_eq!(resolve(&phones, &[]), Target::Phone("b".into()));
    }

    #[test]
    fn none_live_names_last_connected_owner() {
        let phones = [
            phone("a", PhoneState::Unplugged, Some(100)),
            phone("b", PhoneState::Off, Some(300)),
            phone("c", PhoneState::Waiting, Some(900)),
        ];
        assert_eq!(
            resolve(&phones, &have(&["a", "b"])),
            Target::Connect(Some("b".into()))
        );
        // A phone lanternd doesn't list (or never connected) counts as connected at 0; ties go
        // to the smallest serial.
        assert_eq!(
            resolve(&phones, &have(&["x", "w"])),
            Target::Connect(Some("w".into()))
        );
        assert_eq!(
            resolve(&phones, &have(&["x", "a"])),
            Target::Connect(Some("a".into()))
        );
        // Another phone being live doesn't change it: it lacks the app.
        let phones = [
            phone("a", PhoneState::Ready, Some(900)),
            phone("b", PhoneState::Unplugged, Some(300)),
        ];
        assert_eq!(
            resolve(&phones, &have(&["b"])),
            Target::Connect(Some("b".into()))
        );
    }

    #[test]
    fn nothing_known_is_connect_none() {
        assert_eq!(resolve(&[], &[]), Target::Connect(None));
        let phones = [
            phone("a", PhoneState::Waiting, Some(100)),
            phone("b", PhoneState::Off, None),
            phone("c", PhoneState::Unplugged, Some(5)),
            phone("d", PhoneState::Undoing, Some(9)),
        ];
        assert_eq!(resolve(&phones, &[]), Target::Connect(None));
    }
}
