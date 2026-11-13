//! `org.kde.StatusNotifierItem` at `/StatusNotifierItem`: Lantern's tray icon. It is only a
//! menu (`ItemIsMenu`); clicks and scrolls on the icon itself do nothing.

use std::sync::Arc;

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use super::menu;
use super::{Shared, MENU_PATH};

/// An icon pixmap list (`a(iiay)`): width, height, ARGB32 bytes. Lantern uses theme names only.
type Pixmaps = Vec<(i32, i32, Vec<u8>)>;

pub struct Sni {
    pub(crate) shared: Arc<Shared>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Sni {
    #[zbus(property)]
    fn category(&self) -> &str {
        "Hardware"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "lantern"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "Lantern"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        menu::sni_status(&self.shared.state().view)
    }

    #[zbus(property)]
    fn icon_name(&self) -> &str {
        "smartphone"
    }

    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        menu::overlay(&self.shared.state().view)
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }

    #[zbus(property)]
    fn overlay_icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }

    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }

    /// (icon name, icon pixmaps, title, text).
    #[zbus(property)]
    fn tool_tip(&self) -> (String, Pixmaps, String, String) {
        (
            "smartphone".to_owned(),
            Vec::new(),
            "Lantern".to_owned(),
            menu::status_line(&self.shared.state().view),
        )
    }

    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        ObjectPath::from_static_str_unchecked(MENU_PATH).into()
    }

    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }

    fn activate(&self, _x: i32, _y: i32) {}

    fn secondary_activate(&self, _x: i32, _y: i32) {}

    fn context_menu(&self, _x: i32, _y: i32) {}

    fn scroll(&self, _delta: i32, _orientation: String) {}

    #[zbus(signal)]
    pub async fn new_status(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn new_overlay_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
