//! The tray menu as data: which rows show for a [`View`], their texts and what a click does.
//! Pure, so every state's menu is unit-tested; `dbusmenu` turns it into D-Bus properties.
//!
//! Every row has a fixed id, so a click on an id stays valid across layout updates.

use lantern_proto::CompanionState;

use crate::daemon::control::{Phone, View};
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

/// The phone's name for menu texts: its recorded name, else its serial.
pub(crate) fn name(view: &View) -> &str {
    view.name
        .as_deref()
        .or_else(|| view.phone.serial())
        .unwrap_or_default()
}

/// The live session's companion `msg`, if the phone is ready and has one.
fn companion_msg(view: &View) -> Option<(CompanionState, &str)> {
    match &view.phone {
        Phone::Ready {
            companion: Some((state, Some(msg))),
            ..
        } => Some((*state, msg.as_str())),
        _ => None,
    }
}

/// Row 1: what Lantern is doing with which phone.
pub fn status_line(view: &View) -> String {
    let name = name(view);
    match &view.phone {
        Phone::None => "No phone connected".to_owned(),
        Phone::Starting { .. } => format!("Connecting to {name}…"),
        Phone::Ready { .. } => format!("Connected to {name}"),
        Phone::Retrying { .. } => format!("Reconnecting to {name}…"),
        Phone::Paused { .. } => format!("Disconnected from {name}"),
        Phone::Undoing { .. } => format!("Removing Lantern from {name}…"),
    }
}

/// The overlay icon name on the tray icon, or `""` for none.
pub fn overlay(view: &View) -> &'static str {
    match &view.phone {
        Phone::None => "",
        Phone::Ready { .. } if companion_msg(view).is_some() => "emblem-warning",
        Phone::Ready { .. } => "",
        Phone::Starting { .. } | Phone::Retrying { .. } | Phone::Undoing { .. } => "view-refresh",
        Phone::Paused { .. } => "media-playback-pause",
    }
}

/// The StatusNotifierItem `Status`: `Passive` (tucked away) with no phone, else `Active`.
pub fn sni_status(view: &View) -> &'static str {
    match view.phone {
        Phone::None => "Passive",
        _ => "Active",
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
    let connection = match &view.phone {
        Phone::Starting { .. } | Phone::Ready { .. } | Phone::Retrying { .. } => {
            Some(("Disconnect", Action::Disconnect))
        }
        Phone::Paused { .. } => Some(("Connect", Action::Connect)),
        Phone::None | Phone::Undoing { .. } => None,
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
        enabled: view.phone.serial().is_some() && !matches!(view.phone, Phone::Undoing { .. }),
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

    fn view(phone: Phone) -> View {
        View {
            phone,
            name: Some("Test Phone".to_owned()),
        }
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

    fn serial(s: &str) -> String {
        s.to_owned()
    }

    #[test]
    fn none() {
        let v = View {
            phone: Phone::None,
            name: None,
        };
        let mut want = vec![label(1, "No phone connected")];
        want.extend(tail(false));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "No phone connected");
        assert_eq!(overlay(&v), "");
        assert_eq!(sni_status(&v), "Passive");
    }

    #[test]
    fn starting() {
        let v = view(Phone::Starting {
            serial: serial("s1"),
        });
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
        let v = view(Phone::Ready {
            serial: serial("s1"),
            companion: Some((CompanionState::Ready, None)),
        });
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
        let v = view(Phone::Ready {
            serial: serial("s1"),
            companion: Some((
                CompanionState::Unavailable,
                Some("install failed".to_owned()),
            )),
        });
        let mut want = vec![
            label(1, "Connected to Test Phone"),
            label(2, "Companion: install failed"),
            button(3, "Disconnect", true, Action::Disconnect),
        ];
        want.extend(tail(true));
        assert_eq!(rows(&v), want);
        assert_eq!(overlay(&v), "emblem-warning");
        assert_eq!(sni_status(&v), "Active");
        let off = view(Phone::Ready {
            serial: serial("s1"),
            companion: Some((
                CompanionState::Unavailable,
                Some("keyboard off in Lantern settings".to_owned()),
            )),
        });
        assert_eq!(
            rows(&off)[1],
            label(2, "Typing through the Lantern keyboard is off")
        );
        assert_eq!(overlay(&off), "emblem-warning");
    }

    #[test]
    fn retrying() {
        let v = view(Phone::Retrying {
            serial: serial("s1"),
        });
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
        let v = view(Phone::Paused {
            serial: serial("s1"),
        });
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
    fn undoing() {
        let v = view(Phone::Undoing {
            serial: serial("s1"),
        });
        let mut want = vec![label(1, "Removing Lantern from Test Phone…")];
        want.extend(tail(false));
        assert_eq!(rows(&v), want);
        assert_eq!(overlay(&v), "view-refresh");
        assert_eq!(sni_status(&v), "Active");
    }

    #[test]
    fn serial_stands_in_for_an_unknown_name() {
        let v = View {
            phone: Phone::Paused {
                serial: serial("20e9318"),
            },
            name: None,
        };
        assert_eq!(status_line(&v), "Disconnected from 20e9318");
    }

    #[test]
    fn settings_login_and_error_rows() {
        let v = view(Phone::Ready {
            serial: serial("s1"),
            companion: Some((CompanionState::Unavailable, Some("x".to_owned()))),
        });
        let settings = Settings {
            clipboard_sync: false,
            lantern_keyboard: false,
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
        let ready = rows(&view(Phone::Ready {
            serial: serial("s1"),
            companion: None,
        }));
        let paused = rows(&view(Phone::Paused {
            serial: serial("s1"),
        }));
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
        let none = rows(&View {
            phone: Phone::None,
            name: None,
        });
        assert_eq!(action(&none, 3), None);
        assert_eq!(action(&none, 10), None, "undo is disabled without a phone");
    }
}
