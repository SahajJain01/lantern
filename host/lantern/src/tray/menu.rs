//! The tray menu as data: which rows show for a [`View`], their texts and what a click does.
//! Pure, so every state's menu is unit-tested; `dbusmenu` turns it into D-Bus properties.
//!
//! Every row has a fixed id, so a click on an id stays valid across layout updates. A phone's
//! rows take their ids from its [`Slots`] slot `n` (`100n`, `100n+1`, `100n+2`, and `100n+50`
//! in the Remove submenu), which stays bound to that serial for the tray's lifetime: a click
//! on a row never reaches another phone after phones were plugged or unplugged.

use std::collections::HashMap;

use lantern_proto::PhoneState;

use crate::daemon::control::{PhoneView, View};
use crate::settings::Settings;

/// What a click on a row does. Phone actions name the phone's serial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Connect {
        serial: String,
    },
    Disconnect {
        serial: String,
    },
    ToggleClipboard,
    ToggleKeyboard,
    ToggleAutoconnect,
    ToggleScreenOff,
    ToggleLogin,
    OpenSettings,
    /// Asks before removing Lantern from that phone (kdialog), then undoes its setup.
    UndoSetup {
        serial: String,
    },
    /// Asks the unlock prompt for its PIN dialog for that phone (the notification action's
    /// path).
    Unlock {
        serial: String,
    },
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
    /// A row that opens `children`.
    Submenu {
        id: i32,
        text: String,
        children: Vec<Item>,
    },
}

impl Item {
    pub fn id(&self) -> i32 {
        match self {
            Item::Label { id, .. }
            | Item::Separator { id }
            | Item::Button { id, .. }
            | Item::Check { id, .. }
            | Item::Submenu { id, .. } => *id,
        }
    }

    /// A submenu's rows (none for any other row).
    pub fn children(&self) -> &[Item] {
        match self {
            Item::Submenu { children, .. } => children,
            _ => &[],
        }
    }
}

/// Each phone's slot `n` (1, 2, …) for its row ids. A serial keeps its slot for the tray's
/// lifetime and a slot is never handed to another serial.
#[derive(Debug, Default)]
pub struct Slots {
    slots: HashMap<String, i32>,
    last: i32,
}

impl Slots {
    /// `serial`'s slot, giving it the next free one the first time.
    pub fn slot(&mut self, serial: &str) -> i32 {
        if let Some(n) = self.slots.get(serial) {
            return *n;
        }
        self.last += 1;
        self.slots.insert(serial.to_owned(), self.last);
        self.last
    }
}

/// The companion `msg` lanternd sends when the Lantern keyboard is off in settings.
const KEYBOARD_OFF_MSG: &str = "keyboard off in Lantern settings";

/// The settings error row's text (row 12).
pub const SETTINGS_ERROR: &str = "Settings file has an error; using defaults";

/// Whether the phone has its own rows: plugged in, or connected (a phone reconnecting within
/// the unplug grace is not plugged), or being undone.
fn listed(phone: &PhoneView) -> bool {
    phone.plugged || phone.state.is_live() || phone.state == PhoneState::Undoing
}

/// The phones with their own rows, in view order.
fn listed_phones(view: &View) -> impl Iterator<Item = &PhoneView> {
    view.phones.iter().filter(|p| listed(p))
}

/// The ready phone's companion `msg`, if it has one.
fn companion_msg(phone: &PhoneView) -> Option<&str> {
    match phone {
        PhoneView {
            state: PhoneState::Ready,
            companion: Some((_, Some(msg))),
            ..
        } => Some(msg.as_str()),
        _ => None,
    }
}

/// Row `100n+1`: what the phone needs attention for, if anything.
fn phone_note(phone: &PhoneView) -> Option<String> {
    match phone.state {
        PhoneState::Starting => Some("Connecting…".to_owned()),
        PhoneState::Retrying | PhoneState::Reconnecting => Some("Reconnecting…".to_owned()),
        PhoneState::Undoing => Some("Removing Lantern…".to_owned()),
        PhoneState::Ready if phone.locked => Some("Locked".to_owned()),
        PhoneState::Ready => companion_msg(phone).map(|msg| {
            if msg == KEYBOARD_OFF_MSG {
                "Typing through the Lantern keyboard is off".to_owned()
            } else {
                format!("Companion: {msg}")
            }
        }),
        PhoneState::Unplugged | PhoneState::Waiting | PhoneState::Off => None,
    }
}

/// The tooltip: what Lantern is doing with which phones. A live phone that isn't ready comes
/// first (`Connecting to {name}…` / `Reconnecting to {name}…`); else the ready phones:
/// `Connected to {a}`, `Connected to {a} and {b}`, or `Connected to {n} phones` for three or
/// more.
pub fn status_line(view: &View) -> String {
    if let Some(busy) = view.live_phones().find(|p| p.state != PhoneState::Ready) {
        let name = busy.display_name();
        return match busy.state {
            PhoneState::Starting => format!("Connecting to {name}…"),
            _ => format!("Reconnecting to {name}…"),
        };
    }
    let live: Vec<&str> = view.live_phones().map(PhoneView::display_name).collect();
    match live.as_slice() {
        [] => match listed_phones(view).count() {
            0 => "No phone plugged in".to_owned(),
            1 => "1 phone plugged in, none connected".to_owned(),
            n => format!("{n} phones plugged in, none connected"),
        },
        [a] => format!("Connected to {a}"),
        [a, b] => format!("Connected to {a} and {b}"),
        many => format!("Connected to {} phones", many.len()),
    }
}

/// The SNI tooltip's text: [`status_line`] with the phone names escaped, since the host reads
/// it as markup (the line's own words hold no `&`, `<` or `>`).
pub fn tooltip_text(view: &View) -> String {
    crate::notices::escape_markup(&status_line(view))
}

/// The overlay icon name on the tray icon, or `""` for none: busy while any live phone isn't
/// ready or any phone is being undone, a warning while any ready phone has a companion `msg`,
/// paused while no phone is live and one is listed.
pub fn overlay(view: &View) -> &'static str {
    let busy = view.live_phones().any(|p| p.state != PhoneState::Ready)
        || view.phones.iter().any(|p| p.state == PhoneState::Undoing);
    if busy {
        "view-refresh"
    } else if view.phones.iter().any(|p| companion_msg(p).is_some()) {
        "emblem-warning"
    } else if view.live_phones().next().is_none() && listed_phones(view).next().is_some() {
        "media-playback-pause"
    } else {
        ""
    }
}

/// The StatusNotifierItem `Status`: `Passive` (tucked away) with no phone plugged in, else
/// `Active`.
pub fn sni_status(view: &View) -> &'static str {
    match listed_phones(view).next() {
        None => "Passive",
        Some(_) => "Active",
    }
}

/// The menu's rows for this state. `login` is whether lanternd starts at login (`None`: the
/// row is hidden); `settings_error` shows that the settings file doesn't parse. Every phone in
/// the view gets its slot from `slots`.
pub fn items(
    view: &View,
    settings: &Settings,
    login: Option<bool>,
    settings_error: bool,
    slots: &mut Slots,
) -> Vec<Item> {
    let mut rows = Vec::new();
    for phone in listed_phones(view) {
        let base = 100 * slots.slot(&phone.serial);
        let serial = phone.serial.clone();
        let live = phone.state.is_live();
        rows.push(Item::Check {
            id: base,
            text: phone.display_name().to_owned(),
            on: live,
            action: if live {
                Action::Disconnect { serial }
            } else {
                Action::Connect { serial }
            },
        });
        if let Some(text) = phone_note(phone) {
            rows.push(Item::Label { id: base + 1, text });
        }
        if phone.locked {
            rows.push(Item::Button {
                id: base + 2,
                text: format!("Unlock {}…", phone.display_name()),
                enabled: true,
                action: Action::Unlock {
                    serial: phone.serial.clone(),
                },
            });
        }
    }
    // Row 15 ("Lantern connects one phone at a time") is retired; its id is never reused.
    if listed_phones(view).next().is_none() {
        rows.push(Item::Label {
            id: 1,
            text: "No phone plugged in".to_owned(),
        });
    }
    if settings_error {
        rows.push(Item::Label {
            id: 12,
            text: SETTINGS_ERROR.to_owned(),
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
    rows.push(Item::Check {
        id: 16,
        text: "Turn the phone's screen off (from the next connection)".to_owned(),
        on: settings.screen_off,
        action: Action::ToggleScreenOff,
    });
    rows.push(Item::Check {
        id: 14,
        text: "Connect phones when plugged in".to_owned(),
        on: settings.autoconnect,
        action: Action::ToggleAutoconnect,
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
    let remove: Vec<Item> = view
        .phones
        .iter()
        .map(|phone| {
            let name = phone.display_name();
            Item::Button {
                id: 100 * slots.slot(&phone.serial) + 50,
                text: if phone.plugged {
                    name.to_owned()
                } else {
                    format!("{name} (not plugged in)")
                },
                enabled: phone.plugged && phone.state != PhoneState::Undoing,
                action: Action::UndoSetup {
                    serial: phone.serial.clone(),
                },
            }
        })
        .collect();
    if !remove.is_empty() {
        rows.push(Item::Submenu {
            id: 10,
            text: "Remove Lantern from a phone…".to_owned(),
            children: remove,
        });
    }
    rows.push(Item::Button {
        id: 11,
        text: "Quit Lantern".to_owned(),
        enabled: true,
        action: Action::Quit,
    });
    rows
}

/// Row `id`, at any depth.
pub fn find(items: &[Item], id: i32) -> Option<&Item> {
    items.iter().find_map(|item| {
        if item.id() == id {
            Some(item)
        } else {
            find(item.children(), id)
        }
    })
}

/// Every row, submenu rows after their submenu.
pub fn flatten(items: &[Item]) -> Vec<&Item> {
    let mut out = Vec::new();
    for item in items {
        out.push(item);
        out.extend(flatten(item.children()));
    }
    out
}

/// What a click on row `id` (at any depth) does: `None` for an unknown id, a label, a
/// separator, a submenu or a disabled button.
pub fn action(items: &[Item], id: i32) -> Option<Action> {
    find(items, id).and_then(|item| match item {
        Item::Button {
            enabled: true,
            action,
            ..
        }
        | Item::Check { action, .. } => Some(action.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lantern_proto::CompanionState;

    /// A phone `serial` called `name` in `state`: plugged unless Unplugged or Reconnecting.
    fn phone(serial: &str, name: &str, state: PhoneState) -> PhoneView {
        PhoneView {
            serial: serial.into(),
            name: Some(name.to_owned()),
            state,
            plugged: !matches!(state, PhoneState::Unplugged | PhoneState::Reconnecting),
            off: state == PhoneState::Off,
            locked: false,
            companion: (state == PhoneState::Ready).then_some((CompanionState::Ready, None)),
        }
    }

    /// "Test Phone" (`s1`) in `state`.
    fn one(state: PhoneState) -> View {
        View {
            phones: vec![phone("s1", "Test Phone", state)],
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

    fn check(id: i32, text: &str, on: bool, action: Action) -> Item {
        Item::Check {
            id,
            text: text.to_owned(),
            on,
            action,
        }
    }

    fn connect(serial: &str) -> Action {
        Action::Connect {
            serial: serial.into(),
        }
    }

    fn disconnect(serial: &str) -> Action {
        Action::Disconnect {
            serial: serial.into(),
        }
    }

    fn undo(serial: &str) -> Action {
        Action::UndoSetup {
            serial: serial.into(),
        }
    }

    /// The Remove child for "Test Phone" (`s1`, slot 1).
    fn remove_s1(enabled: bool, plugged: bool) -> Item {
        let text = if plugged {
            "Test Phone"
        } else {
            "Test Phone (not plugged in)"
        };
        button(150, text, enabled, undo("s1"))
    }

    /// Rows 4–11 with the default settings and login enabled; row 10 only with `remove`
    /// children.
    fn tail(remove: Vec<Item>) -> Vec<Item> {
        let mut rows = vec![
            Item::Separator { id: 4 },
            check(5, "Sync clipboard", true, Action::ToggleClipboard),
            check(
                6,
                "Type with the Lantern keyboard (from the next connection)",
                true,
                Action::ToggleKeyboard,
            ),
            check(
                16,
                "Turn the phone's screen off (from the next connection)",
                false,
                Action::ToggleScreenOff,
            ),
            check(
                14,
                "Connect phones when plugged in",
                false,
                Action::ToggleAutoconnect,
            ),
            check(7, "Start Lantern at login", true, Action::ToggleLogin),
            button(8, "Open settings folder", true, Action::OpenSettings),
            Item::Separator { id: 9 },
        ];
        if !remove.is_empty() {
            rows.push(Item::Submenu {
                id: 10,
                text: "Remove Lantern from a phone…".to_owned(),
                children: remove,
            });
        }
        rows.push(button(11, "Quit Lantern", true, Action::Quit));
        rows
    }

    fn rows(view: &View) -> Vec<Item> {
        items(
            view,
            &Settings::default(),
            Some(true),
            false,
            &mut Slots::default(),
        )
    }

    #[test]
    fn slots_count_from_one_and_stay() {
        let mut slots = Slots::default();
        assert_eq!(slots.slot("a"), 1);
        assert_eq!(slots.slot("b"), 2);
        assert_eq!(slots.slot("a"), 1);
        assert_eq!(slots.slot("c"), 3);
    }

    #[test]
    fn no_phone_plugged_row() {
        let v = View::default();
        let mut want = vec![label(1, "No phone plugged in")];
        want.extend(tail(vec![]));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "No phone plugged in");
        assert_eq!(overlay(&v), "");
        assert_eq!(sni_status(&v), "Passive");
        // A known phone that isn't plugged in: still the label, and only in the Remove submenu.
        let v = one(PhoneState::Unplugged);
        let mut want = vec![label(1, "No phone plugged in")];
        want.extend(tail(vec![remove_s1(false, false)]));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "No phone plugged in");
        assert_eq!(overlay(&v), "");
        assert_eq!(sni_status(&v), "Passive");
    }

    #[test]
    fn waiting() {
        let v = one(PhoneState::Waiting);
        let mut want = vec![check(100, "Test Phone", false, connect("s1"))];
        want.extend(tail(vec![remove_s1(true, true)]));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "1 phone plugged in, none connected");
        assert_eq!(overlay(&v), "media-playback-pause");
        assert_eq!(sni_status(&v), "Active");
    }

    #[test]
    fn switched_off() {
        let v = one(PhoneState::Off);
        let mut want = vec![check(100, "Test Phone", false, connect("s1"))];
        want.extend(tail(vec![remove_s1(true, true)]));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "1 phone plugged in, none connected");
        assert_eq!(overlay(&v), "media-playback-pause");
        assert_eq!(sni_status(&v), "Active");
    }

    #[test]
    fn starting() {
        let v = one(PhoneState::Starting);
        let mut want = vec![
            check(100, "Test Phone", true, disconnect("s1")),
            label(101, "Connecting…"),
        ];
        want.extend(tail(vec![remove_s1(true, true)]));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "Connecting to Test Phone…");
        assert_eq!(overlay(&v), "view-refresh");
        assert_eq!(sni_status(&v), "Active");
    }

    #[test]
    fn ready() {
        let v = one(PhoneState::Ready);
        let mut want = vec![check(100, "Test Phone", true, disconnect("s1"))];
        want.extend(tail(vec![remove_s1(true, true)]));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "Connected to Test Phone");
        assert_eq!(overlay(&v), "");
        assert_eq!(sni_status(&v), "Active");
    }

    #[test]
    fn ready_with_companion_msg() {
        let mut v = one(PhoneState::Ready);
        v.phones[0].companion = Some((
            CompanionState::Unavailable,
            Some("install failed".to_owned()),
        ));
        let mut want = vec![
            check(100, "Test Phone", true, disconnect("s1")),
            label(101, "Companion: install failed"),
        ];
        want.extend(tail(vec![remove_s1(true, true)]));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "Connected to Test Phone");
        assert_eq!(overlay(&v), "emblem-warning");
        v.phones[0].companion = Some((
            CompanionState::Unavailable,
            Some("keyboard off in Lantern settings".to_owned()),
        ));
        assert_eq!(
            rows(&v)[1],
            label(101, "Typing through the Lantern keyboard is off")
        );
        assert_eq!(overlay(&v), "emblem-warning");
    }

    #[test]
    fn retrying() {
        let v = one(PhoneState::Retrying);
        let mut want = vec![
            check(100, "Test Phone", true, disconnect("s1")),
            label(101, "Reconnecting…"),
        ];
        want.extend(tail(vec![remove_s1(true, true)]));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "Reconnecting to Test Phone…");
        assert_eq!(overlay(&v), "view-refresh");
        assert_eq!(sni_status(&v), "Active");
    }

    #[test]
    fn reconnecting() {
        // Unplugged within the grace: still its row, but it can't be removed until it is back.
        let v = one(PhoneState::Reconnecting);
        let mut want = vec![
            check(100, "Test Phone", true, disconnect("s1")),
            label(101, "Reconnecting…"),
        ];
        want.extend(tail(vec![remove_s1(false, false)]));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "Reconnecting to Test Phone…");
        assert_eq!(overlay(&v), "view-refresh");
        assert_eq!(sni_status(&v), "Active");
    }

    #[test]
    fn locked() {
        let mut v = one(PhoneState::Ready);
        v.phones[0].locked = true;
        let mut want = vec![
            check(100, "Test Phone", true, disconnect("s1")),
            label(101, "Locked"),
            button(
                102,
                "Unlock Test Phone…",
                true,
                Action::Unlock {
                    serial: "s1".into(),
                },
            ),
        ];
        want.extend(tail(vec![remove_s1(true, true)]));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "Connected to Test Phone");
        assert_eq!(
            action(&rows(&v), 102),
            Some(Action::Unlock {
                serial: "s1".into()
            })
        );
        // Not locked: no row 102, and a click on its id does nothing.
        v.phones[0].locked = false;
        assert_eq!(action(&rows(&v), 102), None);
    }

    #[test]
    fn undoing() {
        let v = one(PhoneState::Undoing);
        let mut want = vec![
            check(100, "Test Phone", false, connect("s1")),
            label(101, "Removing Lantern…"),
        ];
        want.extend(tail(vec![remove_s1(false, true)]));
        assert_eq!(rows(&v), want);
        assert_eq!(overlay(&v), "view-refresh");
        assert_eq!(sni_status(&v), "Active");
    }

    #[test]
    fn two_phones_show_both_rows() {
        let v = View {
            phones: vec![
                phone("s1", "Xiaomi 11T Pro", PhoneState::Ready),
                phone("s2", "edge 50 pro", PhoneState::Waiting),
            ],
        };
        let mut want = vec![
            check(100, "Xiaomi 11T Pro", true, disconnect("s1")),
            check(200, "edge 50 pro", false, connect("s2")),
        ];
        want.extend(tail(vec![
            button(150, "Xiaomi 11T Pro", true, undo("s1")),
            button(250, "edge 50 pro", true, undo("s2")),
        ]));
        assert_eq!(rows(&v), want);
        assert_eq!(status_line(&v), "Connected to Xiaomi 11T Pro");
        assert_eq!(overlay(&v), "");
        let none = View {
            phones: vec![
                phone("s1", "Xiaomi 11T Pro", PhoneState::Off),
                phone("s2", "edge 50 pro", PhoneState::Waiting),
            ],
        };
        assert_eq!(status_line(&none), "2 phones plugged in, none connected");
        assert_eq!(overlay(&none), "media-playback-pause");
    }

    /// Decision 4: row 15 ("Lantern connects one phone at a time") is retired; its id is never
    /// used again.
    #[test]
    fn no_one_at_a_time_row() {
        let views = [
            View {
                phones: vec![
                    phone("a", "Pixel", PhoneState::Ready),
                    phone("b", "b", PhoneState::Waiting),
                ],
            },
            View {
                phones: vec![
                    phone("a", "Pixel", PhoneState::Ready),
                    phone("b", "b", PhoneState::Ready),
                    phone("c", "c", PhoneState::Off),
                ],
            },
            one(PhoneState::Ready),
        ];
        for v in views {
            let got = rows(&v);
            assert!(flatten(&got).iter().all(|i| i.id() != 15), "{got:?}");
        }
    }

    /// Decision 19: two live phones, both ready.
    #[test]
    fn two_live_phones_tooltip() {
        let v = View {
            phones: vec![
                phone("dev", "Pixel", PhoneState::Ready),
                phone("b", "b", PhoneState::Ready),
            ],
        };
        assert_eq!(status_line(&v), "Connected to Pixel and b");
        assert_eq!(overlay(&v), "");
        // A phone that isn't live doesn't count.
        let mut more = v.clone();
        more.phones.push(phone("c", "c", PhoneState::Waiting));
        assert_eq!(status_line(&more), "Connected to Pixel and b");
    }

    /// R9 m1: the tooltip body is markup for the SNI host: names are escaped there, while
    /// `status_line` stays raw.
    #[test]
    fn tooltip_text_escapes_names() {
        let v = View {
            phones: vec![
                phone("dev", "A&B", PhoneState::Ready),
                phone("b", "<P>", PhoneState::Ready),
            ],
        };
        assert_eq!(status_line(&v), "Connected to A&B and <P>");
        assert_eq!(tooltip_text(&v), "Connected to A&amp;B and &lt;P&gt;");
        let busy = View {
            phones: vec![phone("dev", "A&B", PhoneState::Starting)],
        };
        assert_eq!(tooltip_text(&busy), "Connecting to A&amp;B…");
    }

    #[test]
    fn three_live_phones_tooltip() {
        let v = View {
            phones: vec![
                phone("a", "Pixel", PhoneState::Ready),
                phone("b", "b", PhoneState::Ready),
                phone("c", "c", PhoneState::Ready),
            ],
        };
        assert_eq!(status_line(&v), "Connected to 3 phones");
        assert_eq!(overlay(&v), "");
    }

    /// Decision 19: a live phone that isn't ready shows first, whatever its place in the view.
    #[test]
    fn connecting_phone_shows_first() {
        let v = View {
            phones: vec![
                phone("a", "Pixel", PhoneState::Ready),
                phone("b", "edge", PhoneState::Starting),
            ],
        };
        assert_eq!(status_line(&v), "Connecting to edge…");
        assert_eq!(overlay(&v), "view-refresh");
        let v = View {
            phones: vec![
                phone("a", "Pixel", PhoneState::Ready),
                phone("b", "edge", PhoneState::Retrying),
                phone("c", "c", PhoneState::Starting),
            ],
        };
        assert_eq!(status_line(&v), "Reconnecting to edge…");
        assert_eq!(overlay(&v), "view-refresh");
        let v = View {
            phones: vec![
                phone("a", "Pixel", PhoneState::Reconnecting),
                phone("b", "edge", PhoneState::Ready),
            ],
        };
        assert_eq!(status_line(&v), "Reconnecting to Pixel…");
    }

    /// The overlay looks at every phone: a companion msg on any ready phone warns, an undo of
    /// any phone is busy.
    #[test]
    fn overlay_of_several_phones() {
        let mut v = View {
            phones: vec![
                phone("a", "Pixel", PhoneState::Ready),
                phone("b", "b", PhoneState::Ready),
            ],
        };
        v.phones[1].companion = Some((CompanionState::Unavailable, Some("x".to_owned())));
        assert_eq!(overlay(&v), "emblem-warning");
        v.phones.push(phone("c", "c", PhoneState::Undoing));
        assert_eq!(overlay(&v), "view-refresh");
    }

    /// Each phone's Locked/Unlock rows sit in its own slot.
    #[test]
    fn locked_rows_per_phone() {
        let mut v = View {
            phones: vec![
                phone("a", "Pixel", PhoneState::Ready),
                phone("b", "b", PhoneState::Ready),
            ],
        };
        v.phones[1].locked = true;
        let got = rows(&v);
        let ids: Vec<i32> = got.iter().map(Item::id).take(4).collect();
        assert_eq!(ids, [100, 200, 201, 202]);
        assert_eq!(got[2], label(201, "Locked"));
        assert_eq!(
            got[3],
            button(
                202,
                "Unlock b…",
                true,
                Action::Unlock { serial: "b".into() }
            )
        );
        assert_eq!(action(&got, 102), None);
        assert!(find(&got, 101).is_none());
    }

    #[test]
    fn remove_submenu_lists_known_phones() {
        let v = View {
            phones: vec![
                phone("s1", "Test Phone", PhoneState::Ready),
                phone("s2", "Old Phone", PhoneState::Unplugged),
            ],
        };
        let got = rows(&v);
        let mut want = vec![check(100, "Test Phone", true, disconnect("s1"))];
        want.extend(tail(vec![
            remove_s1(true, true),
            button(250, "Old Phone (not plugged in)", false, undo("s2")),
        ]));
        assert_eq!(got, want);
        // The unplugged phone has no top-level row, and its Remove entry does nothing.
        assert_eq!(action(&got, 200), None);
        assert_eq!(action(&got, 250), None);
        assert_eq!(action(&got, 150), Some(undo("s1")));
        // The submenu row itself is no action.
        assert_eq!(action(&got, 10), None);
        assert_eq!(find(&got, 250).map(Item::id), Some(250));
    }

    #[test]
    fn action_follows_check_state() {
        for state in [PhoneState::Waiting, PhoneState::Off] {
            assert_eq!(
                action(&rows(&one(state)), 100),
                Some(connect("s1")),
                "{state:?}"
            );
        }
        for state in [
            PhoneState::Starting,
            PhoneState::Ready,
            PhoneState::Retrying,
            PhoneState::Reconnecting,
        ] {
            assert_eq!(
                action(&rows(&one(state)), 100),
                Some(disconnect("s1")),
                "{state:?}"
            );
        }
        let ready = rows(&one(PhoneState::Ready));
        assert_eq!(action(&ready, 5), Some(Action::ToggleClipboard));
        assert_eq!(action(&ready, 6), Some(Action::ToggleKeyboard));
        assert_eq!(action(&ready, 14), Some(Action::ToggleAutoconnect));
        assert_eq!(action(&ready, 16), Some(Action::ToggleScreenOff));
        assert_eq!(action(&ready, 7), Some(Action::ToggleLogin));
        assert_eq!(action(&ready, 8), Some(Action::OpenSettings));
        assert_eq!(action(&ready, 11), Some(Action::Quit));
        // Labels, separators, the retired ids 2, 3 and 13, unknown ids: nothing.
        for id in [0, 1, 2, 3, 4, 9, 12, 13, 15, 101, 102, 99] {
            assert_eq!(action(&ready, id), None, "id {id}");
        }
    }

    #[test]
    fn ids_stay_with_their_phone() {
        let mut slots = Slots::default();
        let mut at = |phones: Vec<PhoneView>| {
            items(
                &View { phones },
                &Settings::default(),
                None,
                false,
                &mut slots,
            )
        };
        let a = || phone("a", "A", PhoneState::Ready);
        let b = || phone("b", "B", PhoneState::Waiting);
        let c = || phone("c", "C", PhoneState::Waiting);
        let first = at(vec![a(), b()]);
        assert_eq!(action(&first, 100), Some(disconnect("a")));
        assert_eq!(action(&first, 200), Some(connect("b")));
        // a unplugged: b keeps 200, and a click on a's old id does nothing.
        let mut gone = a();
        gone.state = PhoneState::Unplugged;
        gone.plugged = false;
        let second = at(vec![b(), gone]);
        assert_eq!(action(&second, 200), Some(connect("b")));
        assert_eq!(action(&second, 100), None);
        assert_eq!(action(&second, 250), Some(undo("b")));
        // c is new and comes first: slot 3; a is back on 100.
        let third = at(vec![c(), b(), a()]);
        assert_eq!(action(&third, 300), Some(connect("c")));
        assert_eq!(action(&third, 200), Some(connect("b")));
        assert_eq!(action(&third, 100), Some(disconnect("a")));
        assert_eq!(action(&third, 350), Some(undo("c")));
        let ids: Vec<i32> = third.iter().map(Item::id).take(4).collect();
        assert_eq!(ids, [300, 200, 100, 4]);
    }

    #[test]
    fn serial_stands_in_for_an_unknown_name() {
        let mut v = one(PhoneState::Ready);
        v.phones[0].serial = "20e9318".into();
        v.phones[0].name = None;
        assert_eq!(status_line(&v), "Connected to 20e9318");
        assert_eq!(
            rows(&v)[0],
            check(100, "20e9318", true, disconnect("20e9318"))
        );
    }

    #[test]
    fn settings_login_and_error_rows() {
        let mut v = one(PhoneState::Ready);
        v.phones[0].companion = Some((CompanionState::Unavailable, Some("x".to_owned())));
        let settings = Settings {
            clipboard_sync: false,
            lantern_keyboard: false,
            autoconnect: true,
            screen_off: true,
        };
        let got = items(&v, &settings, None, true, &mut Slots::default());
        let ids: Vec<i32> = got.iter().map(Item::id).collect();
        // Row 12 right after the phone rows; row 7 hidden without a login state.
        assert_eq!(ids, [100, 101, 12, 4, 5, 6, 16, 14, 8, 9, 10, 11]);
        assert_eq!(
            got[2],
            label(12, "Settings file has an error; using defaults")
        );
        for (index, id) in [(4, 5), (5, 6)] {
            assert!(
                matches!(&got[index], Item::Check { id: i, on: false, .. } if *i == id),
                "{:?}",
                got[index]
            );
        }
        assert!(matches!(
            &got[6],
            Item::Check {
                id: 16,
                on: true,
                ..
            }
        ));
        assert!(matches!(
            &got[7],
            Item::Check {
                id: 14,
                on: true,
                ..
            }
        ));
        let off = items(
            &v,
            &Settings::default(),
            Some(false),
            false,
            &mut Slots::default(),
        );
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
}
