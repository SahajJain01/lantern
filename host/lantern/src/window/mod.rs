//! `lantern open <pkg>`: one process per app window.
//!
//! Connects to lanternd, sends `open`, and on `opened` shows a Wayland window (app_id
//! `lantern.<pkg>`) that renders the app's video and forwards input. Threads: the winit event
//! loop (main), a socket reader that also decodes video (latest frame wins), and a socket writer.

pub mod activation;
pub mod debounce;
pub mod decode;
pub mod geometry;
pub mod input;
pub mod keymap;
pub mod render;

use std::collections::{BTreeMap, VecDeque};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use lantern_proto::{
    read_frame, write_frame, CloseReason, Control, Frame, InputEvent, Kind, VideoPacket,
};
use serde::{Deserialize, Serialize};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::platform::startup_notify::WindowAttributesExtStartupNotify;
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{ActivationToken, Window, WindowId};

use crate::daemon::ipc;
use debounce::Debouncer;
use decode::{Decoder, Nv12Frame};
use geometry::{display_request, rotation_request, ResizeRequests, ResizedKind};
use input::InputState;
use render::Renderer;

pub const DEFAULT_LOGICAL: (u32, u32) = (1280, 800);
/// After `close`, how long to wait for `closed` before exiting anyway.
const CLOSE_WAIT: Duration = Duration::from_secs(1);
/// How long "Phone disconnected" stays up before the process exits.
const DISCONNECTED_SHOW: Duration = Duration::from_millis(1500);
/// Spacing of `LANTERN_DEBUG_INPUT` steps.
const DEBUG_STEP: Duration = Duration::from_secs(2);

enum UserEvent {
    Control(u32, Control),
    FrameReady,
    Disconnected,
    Fatal(String),
}

type Slot = Arc<Mutex<Option<Nv12Frame>>>;

// ---------------------------------------------------------------------------------------------
// Remembered window sizes: ~/.local/state/lantern/windows.json = {"<pkg>": {"w":..,"h":..}}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Size {
    w: u32,
    h: u32,
}

fn sizes_path() -> Option<PathBuf> {
    Some(dirs::state_dir()?.join("lantern").join("windows.json"))
}

fn load_sizes(path: &Path) -> BTreeMap<String, Size> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn remembered_size(pkg: &str) -> (u32, u32) {
    let sizes = sizes_path().map(|p| load_sizes(&p)).unwrap_or_default();
    match sizes.get(pkg) {
        Some(s) if (200..=16384).contains(&s.w) && (150..=16384).contains(&s.h) => (s.w, s.h),
        _ => DEFAULT_LOGICAL,
    }
}

fn remember_size(pkg: &str, size: (u32, u32)) {
    let Some(path) = sizes_path() else { return };
    if let Err(error) = remember_size_in(&path, pkg, size) {
        tracing::warn!(%error, path = %path.display(), "could not remember window size");
    }
}

fn remember_size_in(path: &Path, pkg: &str, (w, h): (u32, u32)) -> Result<()> {
    std::fs::create_dir_all(path.parent().expect("has parent"))?;
    // Several windows may exit at once: serialize the read-modify-write with an flock on a
    // side file (released when `lock` closes), and give each writer its own tmp name.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("json.lock"))?;
    // SAFETY: flock on a file descriptor we own for the duration of the call.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut sizes = load_sizes(path);
    sizes.insert(pkg.to_owned(), Size { w, h });
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&sizes)?)?;
    std::fs::rename(&tmp, path)?;
    drop(lock);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Socket threads

fn reader(mut sock: UnixStream, proxy: EventLoopProxy<UserEvent>, slot: Slot) {
    let mut decoder: Option<Decoder> = None;
    let mut decode_errors = 0u32;
    loop {
        let frame = match read_frame(&mut sock) {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "daemon connection failed");
                break;
            }
        };
        match frame.kind {
            Kind::Control => {
                let c = match Control::decode(&frame.payload) {
                    Ok(c) => c,
                    Err(error) => {
                        tracing::warn!(%error, "bad control from daemon; ignoring it");
                        continue;
                    }
                };
                if let Control::Opened { codec, .. } = &c {
                    match Decoder::new(codec) {
                        Ok(d) => decoder = Some(d),
                        Err(error) => {
                            let _ = proxy.send_event(UserEvent::Fatal(format!(
                                "decoder for {codec}: {error:#}"
                            )));
                            return;
                        }
                    }
                }
                if proxy
                    .send_event(UserEvent::Control(frame.stream, c))
                    .is_err()
                {
                    return;
                }
            }
            Kind::Video => {
                let Some(d) = decoder.as_mut() else { continue };
                let pkt = match VideoPacket::decode(&frame.payload) {
                    Ok(p) => p,
                    Err(error) => {
                        tracing::warn!(%error, "bad video packet");
                        continue;
                    }
                };
                match d.push(&pkt) {
                    Ok(frames) => {
                        if let Some(last) = frames.into_iter().last() {
                            let was_empty = slot.lock().unwrap().replace(last).is_none();
                            if was_empty && proxy.send_event(UserEvent::FrameReady).is_err() {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        decode_errors += 1;
                        if decode_errors <= 5 || decode_errors.is_multiple_of(100) {
                            tracing::warn!(%error, decode_errors, "decode error (waiting for the next keyframe)");
                        }
                    }
                }
            }
            Kind::Input => {}
        }
    }
    let _ = proxy.send_event(UserEvent::Disconnected);
}

fn writer(mut sock: UnixStream, rx: mpsc::Receiver<Frame>, done: mpsc::Sender<()>) {
    while let Ok(frame) = rx.recv() {
        if let Err(error) = write_frame(&mut sock, &frame) {
            tracing::debug!(%error, "daemon write failed");
            break;
        }
    }
    let _ = done.send(());
}

// ---------------------------------------------------------------------------------------------
// Debug input (smoke tests drive the real input path without a physical mouse)

#[derive(Debug, Clone)]
enum DebugStep {
    /// Click at a fraction of the window.
    Tap(f64, f64),
    /// Wheel notches at a fraction of the window.
    Scroll(f64, f64, f32),
    Key(KeyCode),
    Close,
}

/// `LANTERN_DEBUG_INPUT="tap:0.5,0.3;scroll:0.5,0.5,-3;key:Escape;close"`.
fn parse_debug_steps(s: &str) -> Vec<DebugStep> {
    let mut out = Vec::new();
    for step in s.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (cmd, args) = step.split_once(':').unwrap_or((step, ""));
        let nums: Vec<f64> = args
            .split(',')
            .filter_map(|n| n.trim().parse().ok())
            .collect();
        let parsed = match (cmd, nums.as_slice()) {
            ("tap", [x, y]) => Some(DebugStep::Tap(*x, *y)),
            ("scroll", [x, y, v]) => Some(DebugStep::Scroll(*x, *y, *v as f32)),
            ("key", _) => match args {
                "Escape" => Some(DebugStep::Key(KeyCode::Escape)),
                "Enter" => Some(DebugStep::Key(KeyCode::Enter)),
                "Down" => Some(DebugStep::Key(KeyCode::ArrowDown)),
                "Up" => Some(DebugStep::Key(KeyCode::ArrowUp)),
                "Tab" => Some(DebugStep::Key(KeyCode::Tab)),
                _ => None,
            },
            ("close", _) => Some(DebugStep::Close),
            _ => None,
        };
        match parsed {
            Some(p) => out.push(p),
            None => tracing::warn!(step, "ignoring unknown LANTERN_DEBUG_INPUT step"),
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The window

struct App {
    pkg: String,
    tx: mpsc::Sender<Frame>,
    slot: Slot,
    /// Our own launch token: sent in `open`, then used to activate our new window.
    launch_token: Option<String>,
    logical: (u32, u32),
    open_sent: bool,
    /// Window id from `opened`.
    win: Option<u32>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    activator: Option<activation::Activator>,
    /// `focus` that arrived before our `opened` (stream, token).
    pending_focus: Option<(u32, Option<String>)>,
    scale: f64,
    /// The phone display's current size (from `opened`/`resized`).
    display: (u32, u32),
    /// The sizes recently asked for (`open`/`resize`), to tell their echoes from a rotation.
    requests: ResizeRequests,
    debounce: Debouncer,
    input: InputState,
    last_frame: Option<Nv12Frame>,
    closing: Option<Instant>,
    disconnected: Option<Instant>,
    error_msg: Option<String>,
    exit_code: i32,
    /// Logical window size to remember, captured at exit.
    final_size: Option<(u32, u32)>,
    debug_steps: VecDeque<DebugStep>,
    next_debug: Option<Instant>,
}

impl App {
    fn send(&self, frame: Frame) {
        let _ = self.tx.send(frame);
    }

    fn send_control(&self, stream: u32, c: Control) {
        self.send(Frame {
            kind: Kind::Control,
            stream,
            payload: c.encode(),
        });
    }

    fn send_input(&self, ev: Option<InputEvent>) {
        if let (Some(ev), Some(win)) = (ev, self.win) {
            if self.closing.is_none() && self.disconnected.is_none() {
                self.send(Frame {
                    kind: Kind::Input,
                    stream: win,
                    payload: ev.encode(),
                });
            }
        }
    }

    fn exit(&mut self, el: &ActiveEventLoop, code: i32) {
        self.exit_code = code;
        el.exit();
    }

    /// Drops everything holding Wayland objects (activation, wgpu surface, window) while
    /// winit's display is still connected: after `run_app` returns the display is gone, and
    /// destroying a proxy on it segfaults.
    fn teardown(&mut self) {
        if let Some(window) = &self.window {
            if !window.is_maximized() && window.fullscreen().is_none() {
                let l = window.inner_size().to_logical::<f64>(window.scale_factor());
                self.final_size = Some((l.width.round() as u32, l.height.round() as u32));
            }
        }
        self.activator = None;
        self.renderer = None;
        self.window = None;
    }

    fn rect(&self) -> geometry::Rect {
        let size = self
            .window
            .as_ref()
            .map_or(PhysicalSize::new(1, 1), |w| w.inner_size());
        let video = self
            .renderer
            .as_ref()
            .and_then(Renderer::video_size)
            .unwrap_or(self.display);
        geometry::video_rect((size.width, size.height), video)
    }

    fn create_window(&mut self, el: &ActiveEventLoop) -> Result<()> {
        let (lw, lh) = self.logical;
        let mut attrs = Window::default_attributes()
            .with_title(self.pkg.clone())
            .with_name(format!("lantern.{}", self.pkg), "")
            .with_inner_size(LogicalSize::new(lw, lh))
            .with_min_inner_size(LogicalSize::new(200, 150));
        if let Some(token) = self.launch_token.take() {
            attrs = attrs.with_activation_token(ActivationToken::from_raw(token));
        }
        let window = Arc::new(el.create_window(attrs).context("create window")?);
        self.scale = window.scale_factor();
        let renderer = Renderer::new(window.clone(), el.owned_display_handle())?;
        window.request_redraw();
        self.window = Some(window);
        self.renderer = Some(renderer);
        Ok(())
    }

    fn activate(&mut self, token: Option<String>) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(token) = token else {
            // No token: the compositor will not let us take focus; ask for attention instead.
            window.request_user_attention(Some(winit::window::UserAttentionType::Informational));
            return;
        };
        if self.activator.is_none() {
            match activation::Activator::new(&window) {
                Ok(a) => self.activator = Some(a),
                Err(error) => {
                    tracing::warn!(%error, "xdg-activation unavailable");
                    window.request_user_attention(Some(
                        winit::window::UserAttentionType::Informational,
                    ));
                    return;
                }
            }
        }
        let a = self.activator.as_ref().expect("set above");
        match a.activate(&token) {
            Ok(()) => tracing::info!("activated by a second open"),
            Err(error) => tracing::warn!(%error, "activation failed"),
        }
    }

    /// Asks the phone for a display matching the window's physical size.
    fn maybe_resize(&mut self, (pw, ph): (u32, u32)) {
        let Some(win) = self.win else { return };
        if self.closing.is_some() || self.disconnected.is_some() {
            return;
        }
        let logical = (
            (pw as f64 / self.scale).round() as u32,
            (ph as f64 / self.scale).round() as u32,
        );
        let (w, h, dpi) = display_request(logical, self.scale);
        if self.requests.latest() == Some((w, h)) {
            return;
        }
        tracing::info!(w, h, dpi, "resize");
        self.requests.push((w, h));
        self.send_control(win, Control::Resize { w, h, dpi });
    }

    fn on_resized(&mut self, w: u32, h: u32) {
        self.display = (w, h);
        match self.requests.classify((w, h)) {
            // Our own resize (possibly late during a drag, possibly encoder-aligned).
            ResizedKind::Requested => return,
            ResizedKind::Unrequested => {
                tracing::debug!(w, h, "unrequested resize; letterboxing");
                return;
            }
            ResizedKind::Rotation => {}
        }
        // The app rotated its display: the phone now stands at (w, h). Turn the window,
        // keeping its height.
        self.requests.rotated((w, h));
        let Some(window) = self.window.clone() else {
            return;
        };
        let cur = window.inner_size().to_logical::<f64>(self.scale);
        let cur = (cur.width.round() as u32, cur.height.round() as u32);
        if let Some((lw, lh)) = rotation_request(cur, (w, h)) {
            tracing::info!(
                phone_w = w,
                phone_h = h,
                lw,
                lh,
                "app rotated; turning the window"
            );
            // The resize this causes is debounced and sent as usual. If KWin refuses
            // (maximised/tiled), the video stays letterboxed.
            let _ = window.request_inner_size(LogicalSize::new(lw, lh));
        }
    }

    fn on_control(&mut self, el: &ActiveEventLoop, stream: u32, c: Control) {
        match c {
            Control::Error { msg, .. } if stream == 0 && self.win.is_none() => {
                eprintln!("lantern: {msg}");
                self.exit(el, 1);
            }
            Control::Error { msg, .. } => {
                tracing::error!(stream, msg, "error from phone");
                self.error_msg = Some(msg);
            }
            Control::Focus { activation_token } => {
                if self.win == Some(stream) {
                    self.activate(activation_token);
                } else if self.win.is_none() {
                    self.pending_focus = Some((stream, activation_token));
                }
            }
            Control::Opened {
                display: phone_display,
                task,
                codec,
                w,
                h,
            } if self.win.is_none() => {
                tracing::info!(win = stream, phone_display, task, %codec, w, h, "opened");
                self.win = Some(stream);
                self.display = (w, h);
                if let Err(error) = self.create_window(el) {
                    eprintln!("lantern: {error:#}");
                    self.exit(el, 1);
                    return;
                }
                if let Some((s, token)) = self.pending_focus.take() {
                    if s == stream {
                        self.activate(token);
                    }
                }
            }
            Control::Resized { w, h } if self.win == Some(stream) => {
                tracing::info!(w, h, "resized");
                self.on_resized(w, h);
            }
            Control::Closed { reason } if self.win == Some(stream) => {
                tracing::info!(?reason, "closed");
                match reason {
                    CloseReason::Error => {
                        eprintln!(
                            "lantern: {}",
                            self.error_msg
                                .as_deref()
                                .unwrap_or("the window failed on the phone")
                        );
                        self.exit(el, 1);
                    }
                    CloseReason::Closed | CloseReason::AppExited => self.exit(el, 0),
                }
            }
            other => tracing::debug!(stream, ?other, "ignored control"),
        }
    }

    fn on_disconnected(&mut self, el: &ActiveEventLoop) {
        if self.closing.is_some() {
            self.exit(el, 0);
        } else if self.window.is_some() {
            tracing::warn!("daemon connection closed: phone disconnected");
            let frame = render::message_frame(self.last_frame.as_ref(), "Phone disconnected");
            if let (Some(r), Some(w)) = (self.renderer.as_mut(), self.window.as_ref()) {
                r.upload(&frame);
                w.request_redraw();
            }
            self.slot.lock().unwrap().take();
            self.disconnected = Some(Instant::now() + DISCONNECTED_SHOW);
        } else if let Some((win, _)) = self.pending_focus {
            tracing::info!(win, "already open; focused the existing window");
            self.exit(el, 0);
        } else {
            eprintln!("lantern: the daemon closed the connection");
            self.exit(el, 1);
        }
    }

    fn request_close(&mut self) {
        if self.closing.is_some() {
            return;
        }
        if let Some(win) = self.win {
            tracing::info!(win, "closing");
            self.send_control(win, Control::Close);
        }
        self.closing = Some(Instant::now() + CLOSE_WAIT);
    }

    fn run_debug_step(&mut self, step: DebugStep) {
        tracing::info!(?step, "debug input");
        let Some(window) = self.window.clone() else {
            return;
        };
        let size = window.inner_size();
        let at = |fx: f64, fy: f64| (fx * size.width as f64, fy * size.height as f64);
        let rect = self.rect();
        match step {
            DebugStep::Tap(fx, fy) => {
                let (x, y) = at(fx, fy);
                let ev = self.input.cursor_moved(rect, x, y);
                self.send_input(ev);
                let ev = self
                    .input
                    .mouse_button(rect, MouseButton::Left, ElementState::Pressed);
                self.send_input(ev);
                let ev = self
                    .input
                    .mouse_button(rect, MouseButton::Left, ElementState::Released);
                self.send_input(ev);
            }
            DebugStep::Scroll(fx, fy, v) => {
                let (x, y) = at(fx, fy);
                let ev = self.input.cursor_moved(rect, x, y);
                self.send_input(ev);
                for _ in 0..(v.abs().round() as u32).max(1) {
                    let ev = self
                        .input
                        .wheel(rect, MouseScrollDelta::LineDelta(0.0, v.signum()));
                    self.send_input(ev);
                }
            }
            DebugStep::Key(code) => {
                let ev = self.input.key(code, ElementState::Pressed, false);
                self.send_input(ev);
                let ev = self.input.key(code, ElementState::Released, false);
                self.send_input(ev);
            }
            DebugStep::Close => self.request_close(),
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.open_sent {
            return;
        }
        self.open_sent = true;
        // No window yet (it appears on `opened`, so a refused or already-open app never
        // flashes one): use the outputs' scale. The window corrects it with a resize.
        self.scale = el
            .available_monitors()
            .map(|m| m.scale_factor())
            .fold(0.0, f64::max);
        if self.scale <= 0.0 {
            self.scale = 1.0;
        }
        let (w, h, dpi) = display_request(self.logical, self.scale);
        self.requests.push((w, h));
        tracing::info!(pkg = self.pkg, w, h, dpi, scale = self.scale, "open");
        let open = Control::Open {
            pkg: self.pkg.clone(),
            user: 0,
            w,
            h,
            dpi,
            activation_token: self.launch_token.clone(),
        };
        self.send_control(0, open);
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Control(stream, c) => self.on_control(el, stream, c),
            UserEvent::FrameReady => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::Disconnected => self.on_disconnected(el),
            UserEvent::Fatal(msg) => {
                eprintln!("lantern: {msg}");
                self.exit(el, 1);
            }
        }
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let rect = self.rect();
        match event {
            WindowEvent::CloseRequested => self.request_close(),
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                }
                self.debounce
                    .push(Instant::now(), (size.width, size.height));
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => self.scale = scale_factor,
            WindowEvent::Focused(true) => {
                if let (Some(win), None) = (self.win, self.closing) {
                    self.send_control(
                        win,
                        Control::Focus {
                            activation_token: None,
                        },
                    );
                }
            }
            WindowEvent::Focused(false) => {
                for ev in self.input.focus_lost(rect) {
                    self.send_input(Some(ev));
                }
            }
            WindowEvent::RedrawRequested => {
                let Some(r) = self.renderer.as_mut() else {
                    return;
                };
                if self.disconnected.is_none() {
                    if let Some(f) = self.slot.lock().unwrap().take() {
                        r.upload(&f);
                        self.last_frame = Some(f);
                        if self.next_debug.is_none() && !self.debug_steps.is_empty() {
                            self.next_debug = Some(Instant::now() + DEBUG_STEP);
                        }
                    }
                }
                r.draw();
            }
            WindowEvent::CursorMoved {
                position: PhysicalPosition { x, y },
                ..
            } => {
                let ev = self.input.cursor_moved(rect, x, y);
                self.send_input(ev);
            }
            WindowEvent::CursorLeft { .. } => self.input.cursor_left(),
            WindowEvent::MouseInput { state, button, .. } => {
                let ev = self.input.mouse_button(rect, button, state);
                self.send_input(ev);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let ev = self.input.wheel(rect, delta);
                self.send_input(ev);
            }
            WindowEvent::ModifiersChanged(m) => self.input.mods = m.state(),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                if is_synthetic {
                    return;
                }
                if let PhysicalKey::Code(code) = event.physical_key {
                    let ev = self.input.key(code, event.state, event.repeat);
                    self.send_input(ev);
                }
            }
            _ => {}
        }
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        self.teardown();
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        if self.closing.is_some_and(|t| now >= t) {
            tracing::warn!("no closed from the phone in time; exiting");
            return self.exit(el, 0);
        }
        if self.disconnected.is_some_and(|t| now >= t) {
            return self.exit(el, 1);
        }
        if let Some(size) = self.debounce.due(now) {
            self.maybe_resize(size);
        }
        if self.next_debug.is_some_and(|t| now >= t) {
            if let Some(step) = self.debug_steps.pop_front() {
                self.run_debug_step(step);
            }
            self.next_debug = (!self.debug_steps.is_empty()).then(|| now + DEBUG_STEP);
        }
        let wake = [
            self.closing,
            self.disconnected,
            self.debounce.deadline(),
            self.next_debug,
        ]
        .into_iter()
        .flatten()
        .min();
        el.set_control_flow(match wake {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }
}

/// Runs `lantern open <pkg>`; returns the process exit code.
pub fn run(pkg: &str) -> Result<i32> {
    // Our launch token goes to the daemon (for an already-open window) and to our own window;
    // children must not inherit it.
    let launch_token = std::env::var("XDG_ACTIVATION_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    std::env::remove_var("XDG_ACTIVATION_TOKEN");
    std::env::remove_var("DESKTOP_STARTUP_ID");

    let sock_path = ipc::socket_path_in(&ipc::runtime_dir()?);
    let sock = UnixStream::connect(&sock_path).with_context(|| {
        format!(
            "lanternd is not running (cannot connect to {})",
            sock_path.display()
        )
    })?;

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("start the event loop")?;
    let slot: Slot = Arc::default();
    let (tx, rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    {
        let (rsock, proxy, slot) = (sock.try_clone()?, event_loop.create_proxy(), slot.clone());
        std::thread::Builder::new()
            .name("ipc-read".into())
            .spawn(move || reader(rsock, proxy, slot))?;
        std::thread::Builder::new()
            .name("ipc-write".into())
            .spawn(move || writer(sock, rx, done_tx))?;
    }

    let debug_steps = std::env::var("LANTERN_DEBUG_INPUT")
        .map(|s| parse_debug_steps(&s))
        .unwrap_or_default();
    let mut app = App {
        pkg: pkg.to_owned(),
        tx,
        slot,
        launch_token,
        logical: remembered_size(pkg),
        open_sent: false,
        win: None,
        window: None,
        renderer: None,
        activator: None,
        pending_focus: None,
        scale: 1.0,
        display: (0, 0),
        requests: ResizeRequests::default(),
        debounce: Debouncer::new(),
        input: InputState::default(),
        last_frame: None,
        closing: None,
        disconnected: None,
        error_msg: None,
        exit_code: 0,
        final_size: None,
        debug_steps: debug_steps.into(),
        next_debug: None,
    };
    event_loop.run_app(&mut app).context("event loop")?;

    if let Some(size) = app.final_size {
        remember_size(pkg, size);
    }
    // Let the writer flush (e.g. our `close`), but never hang on a stuck daemon.
    let code = app.exit_code;
    drop(app);
    let _ = done_rx.recv_timeout(Duration::from_millis(500));
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_remember_size_keeps_every_window() {
        let dir = std::env::temp_dir().join(format!("lantern-sizes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("windows.json");
        for round in 0..20 {
            let threads: Vec<_> = (0..8)
                .map(|i| {
                    let path = path.clone();
                    std::thread::spawn(move || {
                        remember_size_in(&path, &format!("pkg{round}.{i}"), (300 + i, 200))
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap().unwrap();
            }
        }
        let sizes = load_sizes(&path);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(sizes.len(), 160, "lost updates: {} of 160", sizes.len());
    }
}
