//! The tray menu as data: which rows show for a [`View`], their texts and what a click does.
//! Pure, so every state's menu is unit-tested; `dbusmenu` turns it into D-Bus properties.
//!
//! Every row has a fixed id, so a click on an id stays valid across layout updates.

use lantern_proto::CompanionState;

use lantern_proto::PhoneState;

use crate::daemon::control::{PhoneView, View};
use crate::settings::Settings;

/// What a click on a row does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Connect,
    Disconnect,
    ToggleClipboard,
    ToggleKeyboard,
    ToggleLogin,
    OpenSettings,
    UndoSetup,
    /// Asks the unlock prompt for its PIN dialog (the notification action's path).
    Unlock,
    Quit,
}

/// One menu row. Hidden rows are left out of [`items`].
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// Text only (shown disabled).
    Label {
        id: i32,
        text: String,
    },
    Separator {
        id: i32,
    },
    Button {
        id: i32,
        text: String,
        enabled: bool,
        action: Action,
    },
    /// A checkmark row.
    Check {
        id: i32,
        text: String,
        on: bool,
        action: Action,
    },
}

impl Item {
    pub fn id(&self) -> i32 {
        match self {
            Item::Label { id, .. }
            | Item::Separator { id }
            | Item::Button { id, .. }
            | Item::Check { id, .. } => *id,
        }
    }
}

/// The companion `msg` lanternd sends when the Lantern keyboard is off in settings.
const KEYBOARD_OFF_MSG: &str = "keyboard off in Lantern settings";

/// The settings error row's text (row 12).
pub const SETTINGS_ERROR: &str = "Settings file has an error; using defaults";

/// The phone the menu is about: the live one, else the one being undone, else the last
/// switched-off phone that is plugged in. (Task 4 gives every phone its own rows.)
pub(crate) fn shown(view: &View) -> Option<&PhoneView> {
    view.live()
        .or_else(|| view.phones.iter().find(|p| p.state == PhoneState::Undoing))
        .or_else(|| {
            view.phones
                .iter()
                .rev()
                .find(|p| p.state == PhoneState::Off)
        })
}

/// The shown phone's name for menu texts: its recorded name, else its serial.
pub(crate) fn name(view: &View) -> &str {
    shown(view).map(PhoneView::display_name).unwrap_or_default()
}

/// The shown phone's state.
fn state(view: &View) -> Option<PhoneState> {
    shown(view).map(|p| p.state)
}

/// Whether the shown phone's PIN lock is in the way.
fn locked(view: &View) -> bool {
    shown(view).is_some_and(|p| p.locked)
}

/// The live session's companion `msg`, if the phone is ready and has one.
fn companion_msg(view: &View) -> Option<(CompanionState, &str)> {
    match shown(view) {
        Some(PhoneView {
            state: PhoneState::Ready,
            companion: Some((state, Some(msg))),
            ..
        }) => Some((*state, msg.as_str())),
        _ => None,
    }
}

/// Row 1: what Lantern is doing with which phone.
pub fn status_line(view: &View) -> String {
    let name = name(view);
    match state(view) {
        Some(PhoneState::Starting) => format!("Connecting to {name}…"),
        Some(PhoneState::Ready) if locked(view) => format!("Connected to {name} (locked)"),
        Some(PhoneState::Ready) => format!("Connected to {name}"),
        Some(PhoneState::Retrying | PhoneState::Reconnecting) => {
            format!("Reconnecting to {name}…")
        }
        Some(PhoneState::Off) => format!("Disconnected from {name}"),
        Some(PhoneState::Undoing) => format!("Removing Lantern from {name}…"),
        None | Some(PhoneState::Waiting | PhoneState::Unplugged) => "No phone connected".to_owned(),
    }
}

/// The overlay icon name on the tray icon, or `""` for none.
pub fn overlay(view: &View) -> &'static str {
    match state(view) {
        Some(PhoneState::Ready) if companion_msg(view).is_some() => "emblem-warning",
        Some(
            PhoneState::Starting
            | PhoneState::Retrying
            | PhoneState::Reconnecting
            | PhoneState::Undoing,
        ) => "view-refresh",
        Some(PhoneState::Off) => "media-playback-pause",
        _ => "",
    }
}

/// The StatusNotifierItem `Status`: `Passive` (tucked away) with no phone, else `Active`.
pub fn sni_status(view: &View) -> &'static str {
    match shown(view) {
        None => "Passive",
        Some(_) => "Active",
    }
}

/// The menu's rows for this state. `login` is whether lanternd starts at login (`None`: the
/// row is hidden); `settings_error` shows that the settings file doesn't parse.
pub fn items(
    view: &View,
    settings: &Settings,
    login: Option<bool>,
    settings_error: bool,
) -> Vec<Item> {
    let mut rows = vec![Item::Label {
        id: 1,
        text: status_line(view),
    }];
    if locked(view) {
        rows.push(Item::Button {
            id: 13,
            text: format!("Unlock {}…", name(view)),
            enabled: true,
            action: Action::Unlock,
        });
    }
    if let Some((_, msg)) = companion_msg(view) {
        let text = if msg == KEYBOARD_OFF_MSG {
            "Typing through the Lantern keyboard is off".to_owned()
        } else {
            format!("Companion: {msg}")
        };
        rows.push(Item::Label { id: 2, text });
    }
    if settings_error {
        rows.push(Item::Label {
            id: 12,
            text: SETTINGS_ERROR.to_owned(),
        });
    }
    let connection = match state(view) {
        Some(
            PhoneState::Starting
            | PhoneState::Ready
            | PhoneState::Retrying
            | PhoneState::Reconnecting,
        ) => Some(("Disconnect", Action::Disconnect)),
        Some(PhoneState::Off) => Some(("Connect", Action::Connect)),
        _ => None,
    };
    if let Some((text, action)) = connection {
        rows.push(Item::Button {
            id: 3,
            text: text.to_owned(),
            enabled: true,
            action,
        });
    }
    rows.push(Item::Separator { id: 4 });
    rows.push(Item::Check {
        id: 5,
        text: "Sync clipboard".to_owned(),
        on: settings.clipboard_sync,
        action: Action::ToggleClipboard,
    });
    rows.push(Item::Check {
        id: 6,
        text: "Type with the Lantern keyboard (from the next connection)".to_owned(),
        on: settings.lantern_keyboard,
        action: Action::ToggleKeyboard,
    });
    if let Some(on) = login {
        rows.push(Item::Check {
            id: 7,
            text: "Start Lantern at login".to_owned(),
            on,
            action: Action::ToggleLogin,
        });
    }
    rows.push(Item::Button {
        id: 8,
        text: "Open settings folder".to_owned(),
        enabled: true,
        action: Action::OpenSettings,
    });
    rows.push(Item::Separator { id: 9 });
    rows.push(Item::Button {
        id: 10,
        text: "Undo phone setup…".to_owned(),
        enabled: state(view).is_some_and(|state| state != PhoneState::Undoing),
        action: Action::UndoSetup,
    });
    rows.push(Item::Button {
        id: 11,
        text: "Quit Lantern".to_owned(),
        enabled: true,
        action: Action::Quit,
    });
    rows
}

/// What a click on row `id` does: `None` for an unknown id, a label, a separator or a
/// disabled button.
pub fn action(items: &[Item], id: i32) -> Option<Action> {
    items
        .iter()
        .find(|item| item.id() == id)
        .and_then(|item| match item {
            Item::Button {
                enabled: true,
                action,
                ..
            }
            | Item::Check { action, .. } => Some(*action),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One phone, "Test Phone" (`s1`), in `state`.
    fn view(state: PhoneState) -> View {
        with(state, None)
    }

    fn with(state: PhoneState, companion: Option<(CompanionState, Option<String>)>) -> View {
        View {
            phones: vec![PhoneView {
                serial: "s1".into(),
                name: Some("Test Phone".to_owned()),
                state,
                plugged: true,
                off: state == PhoneState::Off,
                locked: false,
                companion,
            }],
        }
    }

    fn set_locked(view: &mut View, locked: bool) {
        view.phones[0].locked = locked;
    }

    fn label(id: i32, text: &str) -> Item {
        Item::Label {
            id,
            text: text.to_owned(),
        }
    }

    fn button(id: i32, text: &str, enabled: bool, action: Action) -> Item {
        Item::Button {
            id,
            text: text.to_owned(),
            enabled,
            action,
        }
    }

    /// Rows 4–11 with every setting on and login enabled.
    fn tail(undo_enabled: bool) -> Vec<Item> {
        vec![
            Item::Separator { id: 4 },
            Item::Check {
                id: 5,
                text: "Sync clipboard".to_owned(),
                on: true,
                action: Action::ToggleClipboard,
            },
            Item::Check {
                id: 6,
                text: "Type with the Lantern keyboard (from the next connection)".to_owned(),
                on: true,
                action: Action::ToggleKeyboard,
            },
            Item::Check {
                id: 7,
                text: "Start Lantern at login".to_owned(),
                on: true,
                action: Action::ToggleLogin,
            },
            button(8, "Open settings folder", true, Action::OpenSettings),
            Item::Separator { id: 9 },
            button(10, "Undo phone setup…", undo_enabled, Action::UndoSetup),
            button(11, "Quit Lantern", true, Action::Quit),
        ]
    }

    fn rows(view: &View) -> Vec<Item> {
        items(view, &Settings::default(), Some(true), false)
    }

    #[test]
    fn none() {
        let v = View::default();
        let mut want = vec![label(1, "No phone connected")];
        want.extend(tail(false));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "No phone connected");
        assert_eq!(overlay(&v), "");
        assert_eq!(sni_status(&v), "Passive");
    }

    #[test]
    fn starting() {
        let v = view(PhoneState::Starting);
        let mut want = vec![
            label(1, "Connecting to Test Phone…"),
            button(3, "Disconnect", true, Action::Disconnect),
        ];
        want.extend(tail(true));
        assert_eq!(rows(&v), want);
        assert_eq!(overlay(&v), "view-refresh");
        assert_eq!(sni_status(&v), "Active");
    }

    #[test]
    fn ready() {
        let v = with(PhoneState::Ready, Some((CompanionState::Ready, None)));
        let mut want = vec![
            label(1, "Connected to Test Phone"),
            button(3, "Disconnect", true, Action::Disconnect),
        ];
        want.extend(tail(true));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "Connected to Test Phone");
        assert_eq!(overlay(&v), "");
        assert_eq!(sni_status(&v), "Active");
    }

    #[test]
    fn ready_with_companion_msg() {
        let v = with(
            PhoneState::Ready,
            Some((
                CompanionState::Unavailable,
                Some("install failed".to_owned()),
            )),
        );
        let mut want = vec![
            label(1, "Connected to Test Phone"),
            label(2, "Companion: install failed"),
            button(3, "Disconnect", true, Action::Disconnect),
        ];
        want.extend(tail(true));
        assert_eq!(rows(&v), want);
        assert_eq!(overlay(&v), "emblem-warning");
        assert_eq!(sni_status(&v), "Active");
        let off = with(
            PhoneState::Ready,
            Some((
                CompanionState::Unavailable,
                Some("keyboard off in Lantern settings".to_owned()),
            )),
        );
        assert_eq!(
            rows(&off)[1],
            label(2, "Typing through the Lantern keyboard is off")
        );
        assert_eq!(overlay(&off), "emblem-warning");
    }

    #[test]
    fn retrying() {
        let v = view(PhoneState::Retrying);
        let mut want = vec![
            label(1, "Reconnecting to Test Phone…"),
            button(3, "Disconnect", true, Action::Disconnect),
        ];
        want.extend(tail(true));
        assert_eq!(rows(&v), want);
        assert_eq!(overlay(&v), "view-refresh");
        assert_eq!(sni_status(&v), "Active");
    }

    #[test]
    fn paused() {
        let v = view(PhoneState::Off);
        let mut want = vec![
            label(1, "Disconnected from Test Phone"),
            button(3, "Connect", true, Action::Connect),
        ];
        want.extend(tail(true));
        assert_eq!(rows(&v), want);
        assert_eq!(overlay(&v), "media-playback-pause");
        assert_eq!(sni_status(&v), "Active");
    }

    #[test]
    fn reconnecting_reads_like_retrying() {
        let v = view(PhoneState::Reconnecting);
        assert_eq!(status_line(&v), "Reconnecting to Test Phone…");
        assert_eq!(action(&rows(&v), 3), Some(Action::Disconnect));
        assert_eq!(overlay(&v), "view-refresh");
    }

    #[test]
    fn shows_the_live_phone_else_an_off_one() {
        let mut v = view(PhoneState::Off);
        let mut other = v.phones[0].clone();
        other.serial = "s2".into();
        other.name = Some("Other".into());
        other.state = PhoneState::Waiting;
        other.off = false;
        v.phones.push(other.clone());
        assert_eq!(shown(&v).map(|p| p.serial.as_str()), Some("s1"));
        // A waiting phone alone shows nothing (Task 4 lists it).
        assert_eq!(
            shown(&View {
                phones: vec![other.clone()]
            }),
            None
        );
        v.phones[1].state = PhoneState::Starting;
        assert_eq!(shown(&v).map(|p| p.serial.as_str()), Some("s2"));
        assert_eq!(status_line(&v), "Connecting to Other…");
    }

    #[test]
    fn undoing() {
        let v = view(PhoneState::Undoing);
        let mut want = vec![label(1, "Removing Lantern from Test Phone…")];
        want.extend(tail(false));
        assert_eq!(rows(&v), want);
        assert_eq!(overlay(&v), "view-refresh");
        assert_eq!(sni_status(&v), "Active");
    }

    #[test]
    fn locked_shows_unlock_row() {
        let mut v = with(PhoneState::Ready, Some((CompanionState::Ready, None)));
        set_locked(&mut v, true);
        let mut want = vec![
            label(1, "Connected to Test Phone (locked)"),
            button(13, "Unlock Test Phone…", true, Action::Unlock),
            button(3, "Disconnect", true, Action::Disconnect),
        ];
        want.extend(tail(true));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "Connected to Test Phone (locked)");
        assert_eq!(action(&rows(&v), 13), Some(Action::Unlock));
        // Row 13 right after row 1, before the companion and settings-error rows.
        let mut warn = v.clone();
        warn.phones[0].companion = Some((CompanionState::Unavailable, Some("x".to_owned())));
        let ids: Vec<i32> = items(&warn, &Settings::default(), None, true)
            .iter()
            .map(Item::id)
            .collect();
        assert_eq!(ids, [1, 13, 2, 12, 3, 4, 5, 6, 8, 9, 10, 11]);
        // Not locked: no row 13, and a click on its id does nothing.
        set_locked(&mut v, false);
        assert!(!rows(&v).iter().any(|i| i.id() == 13));
        assert_eq!(action(&rows(&v), 13), None);
        assert_eq!(status_line(&v), "Connected to Test Phone");
    }

    #[test]
    fn serial_stands_in_for_an_unknown_name() {
        let mut v = view(PhoneState::Off);
        v.phones[0].serial = "20e9318".into();
        v.phones[0].name = None;
        assert_eq!(status_line(&v), "Disconnected from 20e9318");
    }

    #[test]
    fn settings_login_and_error_rows() {
        let v = with(
            PhoneState::Ready,
            Some((CompanionState::Unavailable, Some("x".to_owned()))),
        );
        let settings = Settings {
            clipboard_sync: false,
            lantern_keyboard: false,
            autoconnect: false,
        };
        let got = items(&v, &settings, None, true);
        let ids: Vec<i32> = got.iter().map(Item::id).collect();
        // Row 12 right after row 2; row 7 hidden without a login state.
        assert_eq!(ids, [1, 2, 12, 3, 4, 5, 6, 8, 9, 10, 11]);
        assert_eq!(
            got[2],
            label(12, "Settings file has an error; using defaults")
        );
        assert!(matches!(
            got[5],
            Item::Check {
                id: 5,
                on: false,
                ..
            }
        ));
        assert!(matches!(
            got[6],
            Item::Check {
                id: 6,
                on: false,
                ..
            }
        ));
        let off = items(&v, &Settings::default(), Some(false), false);
        assert!(off.iter().any(|i| matches!(
            i,
            Item::Check {
                id: 7,
                on: false,
                ..
            }
        )));
        assert!(!off.iter().any(|i| i.id() == 12));
    }

    #[test]
    fn action_maps_ids_across_states() {
        let ready = rows(&with(PhoneState::Ready, None));
        let paused = rows(&view(PhoneState::Off));
        assert_eq!(action(&ready, 3), Some(Action::Disconnect));
        assert_eq!(action(&paused, 3), Some(Action::Connect));
        assert_eq!(action(&ready, 5), Some(Action::ToggleClipboard));
        assert_eq!(action(&ready, 6), Some(Action::ToggleKeyboard));
        assert_eq!(action(&ready, 7), Some(Action::ToggleLogin));
        assert_eq!(action(&ready, 8), Some(Action::OpenSettings));
        assert_eq!(action(&ready, 10), Some(Action::UndoSetup));
        assert_eq!(action(&ready, 11), Some(Action::Quit));
        // Labels, separators, hidden rows, unknown ids and disabled buttons do nothing.
        for id in [0, 1, 2, 4, 9, 12, 99] {
            assert_eq!(action(&ready, id), None, "id {id}");
        }
        let none = rows(&View::default());
        assert_eq!(action(&none, 3), None);
        assert_eq!(action(&none, 10), None, "undo is disabled without a phone");
    }
}
