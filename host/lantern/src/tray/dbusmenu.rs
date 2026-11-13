//! `com.canonical.dbusmenu` at `/MenuBar`: the tray's menu, built from [`menu::items`].
//!
//! The root (id 0) has every top-level row as a child; a submenu row (`children-display =
//! "submenu"`) has its own rows. Each row carries `label`, `enabled`, `visible`, and
//! `type = "separator"` or `toggle-type`/`toggle-state` as needed. Every method finds rows at
//! any depth.

use std::collections::HashMap;
use std::sync::Arc;

use zbus::fdo;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

use super::menu::{self, Item};
use super::Shared;

/// A property value; our values never hold file descriptors, so this can't fail.
fn owned<'a>(value: impl Into<Value<'a>>) -> OwnedValue {
    value
        .into()
        .try_into_owned()
        .expect("a value without file descriptors")
}

/// dbusmenu labels treat `_` as a mnemonic marker; `__` is a literal underscore.
fn escape(text: &str) -> String {
    text.replace('_', "__")
}

/// The row's dbusmenu properties, only those in `names` unless it is empty.
pub(crate) fn properties(item: &Item, names: &[String]) -> HashMap<String, OwnedValue> {
    let mut props: Vec<(&str, OwnedValue)> = vec![("visible", owned(true))];
    match item {
        Item::Label { text, .. } => {
            props.push(("label", owned(escape(text))));
            props.push(("enabled", owned(false)));
        }
        Item::Separator { .. } => props.push(("type", owned("separator"))),
        Item::Button { text, enabled, .. } => {
            props.push(("label", owned(escape(text))));
            props.push(("enabled", owned(*enabled)));
        }
        Item::Check { text, on, .. } => {
            props.push(("label", owned(escape(text))));
            props.push(("enabled", owned(true)));
            props.push(("toggle-type", owned("checkmark")));
            props.push(("toggle-state", owned(i32::from(*on))));
        }
        Item::Submenu { text, .. } => {
            props.push(("label", owned(escape(text))));
            props.push(("enabled", owned(true)));
            props.push(("children-display", owned("submenu")));
        }
    }
    props
        .into_iter()
        .filter(|(name, _)| names.is_empty() || names.iter().any(|n| n == name))
        .map(|(name, value)| (name.to_owned(), value))
        .collect()
}

/// The root item's properties.
fn root_properties(names: &[String]) -> HashMap<String, OwnedValue> {
    let mut props = HashMap::new();
    if names.is_empty() || names.iter().any(|n| n == "children-display") {
        props.insert("children-display".to_owned(), owned("submenu"));
    }
    props
}

type Layout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// The layouts of `items`, each with its own rows while `depth` allows (`-1`: all levels,
/// `0`: none).
fn children(items: &[Item], depth: i32, names: &[String]) -> Vec<OwnedValue> {
    if depth == 0 {
        return Vec::new();
    }
    items
        .iter()
        .map(|item| owned(layout(item, depth - i32::from(depth > 0), names)))
        .collect()
}

/// `item`'s layout with its rows down to `depth` more levels.
fn layout(item: &Item, depth: i32, names: &[String]) -> Layout {
    (
        item.id(),
        properties(item, names),
        children(item.children(), depth, names),
    )
}

pub struct Menu {
    pub(crate) shared: Arc<Shared>,
}

impl Menu {
    fn items(&self) -> (u32, Vec<Item>) {
        let state = self.shared.state();
        (state.revision, state.items.clone())
    }

    fn unknown(id: i32) -> fdo::Error {
        fdo::Error::InvalidArgs(format!("no menu item {id}"))
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    /// The layout under `parent_id` (the root 0 or any row, submenu rows included), its rows
    /// down to `recursion_depth` levels (`-1`: all).
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> fdo::Result<(u32, Layout)> {
        let (revision, items) = self.items();
        if parent_id == 0 {
            let rows = children(&items, recursion_depth, &property_names);
            return Ok((revision, (0, root_properties(&property_names), rows)));
        }
        let item = menu::find(&items, parent_id).ok_or_else(|| Self::unknown(parent_id))?;
        Ok((revision, layout(item, recursion_depth, &property_names)))
    }

    /// Properties of `ids` (every row when empty); unknown ids are skipped.
    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        let (_, items) = self.items();
        let mut out = Vec::new();
        if ids.is_empty() || ids.contains(&0) {
            out.push((0, root_properties(&property_names)));
        }
        out.extend(
            menu::flatten(&items)
                .into_iter()
                .filter(|item| ids.is_empty() || ids.contains(&item.id()))
                .map(|item| (item.id(), properties(item, &property_names))),
        );
        out
    }

    fn get_property(&self, id: i32, name: String) -> fdo::Result<OwnedValue> {
        let (_, items) = self.items();
        let mut props = if id == 0 {
            root_properties(&[])
        } else {
            let item = menu::find(&items, id).ok_or_else(|| Self::unknown(id))?;
            properties(item, &[])
        };
        props
            .remove(&name)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("menu item {id} has no {name}")))
    }

    /// Only `clicked` does anything.
    fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        self.on_event(id, &event_id);
    }

    /// [`Self::event`] for several rows; returns the ids that don't exist.
    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        let (_, items) = self.items();
        let mut unknown = Vec::new();
        for (id, event_id, _, _) in events {
            if menu::find(&items, id).is_some() {
                self.on_event(id, &event_id);
            } else {
                unknown.push(id);
            }
        }
        unknown
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    #[zbus(signal)]
    pub async fn layout_updated(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn items_properties_updated(
        emitter: &SignalEmitter<'_>,
        updated_props: Vec<(i32, HashMap<String, OwnedValue>)>,
        removed_props: Vec<(i32, Vec<String>)>,
    ) -> zbus::Result<()>;
}

impl Menu {
    fn on_event(&self, id: i32, event_id: &str) {
        if event_id != "clicked" {
            return;
        }
        let action = menu::action(&self.shared.state().items, id);
        match action {
            Some(action) => {
                tracing::info!(id, ?action, "tray menu click");
                super::perform(&self.shared, action);
            }
            None => tracing::debug!(id, "tray click on a row without an action"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tray::menu::Action;

    fn get(props: &HashMap<String, OwnedValue>, name: &str) -> OwnedValue {
        props[name].try_clone().unwrap()
    }

    #[test]
    fn row_properties() {
        let label = properties(
            &Item::Label {
                id: 1,
                text: "Connected to my_phone".into(),
            },
            &[],
        );
        assert_eq!(
            String::try_from(get(&label, "label")).unwrap(),
            "Connected to my__phone"
        );
        assert!(!bool::try_from(get(&label, "enabled")).unwrap());
        assert!(bool::try_from(get(&label, "visible")).unwrap());
        let check = properties(
            &Item::Check {
                id: 5,
                text: "Sync clipboard".into(),
                on: true,
                action: Action::ToggleClipboard,
            },
            &[],
        );
        assert_eq!(
            String::try_from(get(&check, "toggle-type")).unwrap(),
            "checkmark"
        );
        assert_eq!(i32::try_from(get(&check, "toggle-state")).unwrap(), 1);
        let sep = properties(&Item::Separator { id: 4 }, &["type".to_owned()]);
        assert_eq!(sep.len(), 1);
        assert_eq!(String::try_from(get(&sep, "type")).unwrap(), "separator");
    }

    fn submenu() -> Item {
        Item::Submenu {
            id: 10,
            text: "Remove Lantern from a phone…".into(),
            children: vec![Item::Button {
                id: 150,
                text: "my_phone".into(),
                enabled: true,
                action: Action::UndoSetup {
                    serial: "s1".into(),
                },
            }],
        }
    }

    #[test]
    fn submenu_properties_and_depth() {
        let props = properties(&submenu(), &[]);
        assert_eq!(
            String::try_from(get(&props, "children-display")).unwrap(),
            "submenu"
        );
        assert!(bool::try_from(get(&props, "enabled")).unwrap());
        let items = [submenu()];
        // Depth 1 from the root: the submenu without its rows; -1 and 2: with them.
        let one = children(&items, 1, &[]);
        let (_, _, rows): Layout = Value::from(one[0].try_clone().unwrap()).try_into().unwrap();
        assert!(rows.is_empty());
        for depth in [-1, 2] {
            let all = children(&items, depth, &[]);
            let (id, _, rows): Layout =
                Value::from(all[0].try_clone().unwrap()).try_into().unwrap();
            assert_eq!(id, 10);
            assert_eq!(rows.len(), 1, "depth {depth}");
            let (child, props, _): Layout = Value::from(rows[0].try_clone().unwrap())
                .try_into()
                .unwrap();
            assert_eq!(child, 150);
            assert_eq!(String::try_from(get(&props, "label")).unwrap(), "my__phone");
        }
        assert!(children(&items, 0, &[]).is_empty());
    }
}
