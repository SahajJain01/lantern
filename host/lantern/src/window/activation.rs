//! Activating our window with an XDG activation token from another process (the second
//! `lantern open` of an already-open app). winit 0.30 only uses a token at window creation and
//! its `focus_window` is a no-op on Wayland, so this binds `xdg_activation_v1` on winit's own
//! `wl_display` (a guest backend with its own event queue) and calls `activate(token, surface)`.

use anyhow::{bail, Context, Result};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;
use winit::window::Window;

struct State;

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgActivationV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &XdgActivationV1,
        _: <XdgActivationV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub struct Activator {
    conn: Connection,
    activation: XdgActivationV1,
    surface: WlSurface,
    /// Kept alive while `activation` is bound to it (it never receives events).
    _queue: EventQueue<State>,
}

impl Activator {
    /// Binds xdg-activation for `window`. Fails off Wayland or without the global.
    pub fn new(window: &Window) -> Result<Self> {
        let display = match window.display_handle()?.as_raw() {
            RawDisplayHandle::Wayland(h) => h.display.as_ptr(),
            _ => bail!("not a Wayland window"),
        };
        let surface = match window.window_handle()?.as_raw() {
            RawWindowHandle::Wayland(h) => h.surface.as_ptr(),
            _ => bail!("not a Wayland window"),
        };
        // SAFETY: both pointers come from the live winit window, which outlives the Activator
        // (it is dropped with the window state). The guest backend never closes the display.
        let (conn, surface) = unsafe {
            let conn = Connection::from_backend(Backend::from_foreign_display(display.cast()));
            let id = ObjectId::from_ptr(WlSurface::interface(), surface.cast())
                .context("wl_surface id")?;
            (
                conn.clone(),
                WlSurface::from_id(&conn, id).context("wl_surface proxy")?,
            )
        };
        let (globals, queue) = registry_queue_init::<State>(&conn).context("wayland registry")?;
        let activation: XdgActivationV1 = globals
            .bind(&queue.handle(), 1..=1, ())
            .context("xdg_activation_v1 not offered")?;
        Ok(Self {
            conn,
            activation,
            surface,
            _queue: queue,
        })
    }

    /// Asks the compositor to activate (raise and focus) the window with `token`.
    pub fn activate(&self, token: &str) -> Result<()> {
        self.activation.activate(token.to_owned(), &self.surface);
        self.conn.flush().context("flush wayland")?;
        Ok(())
    }
}

impl Drop for Activator {
    fn drop(&mut self) {
        self.activation.destroy();
        let _ = self.conn.flush();
    }
}
