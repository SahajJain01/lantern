//! Window input → Android input events.
//!
//! The mouse is pointer id 0 with an Android button mask (`BUTTON_PRIMARY`=1,
//! `BUTTON_SECONDARY`=2, `BUTTON_TERTIARY`=4). Each press is a `down` carrying the new mask and
//! each release an `up` carrying the remaining mask (the helper derives the action button from
//! the change); motion is `move` while a button is held, else `hover-move`. Coordinates are
//! normalized to the video rect; a press outside the video is ignored, a drag that leaves it is
//! clamped to the edge.

//!
//! Keyboard: a key whose text is what the US-layout Android key event would type (or that has
//! no text, or is pressed with Ctrl/Alt/Super) is sent as a key event; any other text (a dead
//! key's `é`, a non-US layout's letters, AltGr symbols) is committed through the Lantern
//! keyboard as `ime{commit}`. Composition from a PC input method arrives as winit `Ime` events
//! and becomes `ime{compose|commit|finish}`.

use lantern_proto::{Control, ImeOp, InputEvent, KeyAction, PointerAction, MAX_IME_TEXT};
use winit::event::{ElementState, MouseButton, MouseScrollDelta};
use winit::keyboard::{KeyCode, ModifiersState};

use super::geometry::{clamp_normalized, to_normalized, Rect};
use super::keymap::{meta_state, us_char, window_keycode};

/// What a keyboard event becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Typed {
    /// An Android key event ([`InputState::key`]).
    Key,
    /// Text committed through the Lantern keyboard.
    Text(String),
    /// Nothing (the release of a key sent as text, or a dead key).
    Nothing,
}

/// Classifies one key event by its winit `text`. Ctrl, Alt or Super held: a key (shortcuts).
/// A press whose text is what the US layout gives for the key: a key. A press with any other
/// non-empty text free of control characters: that text. Anything else: a key.
pub fn classify(
    code: KeyCode,
    text: Option<&str>,
    mods: ModifiersState,
    state: ElementState,
) -> Typed {
    if mods.control_key() || mods.alt_key() || mods.super_key() || state != ElementState::Pressed {
        return Typed::Key;
    }
    let Some(text) = text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control)) else {
        return Typed::Key;
    };
    let mut us = [0u8; 4];
    match us_char(code, mods.shift_key()) {
        Some(c) if c.encode_utf8(&mut us) == text => Typed::Key,
        _ => Typed::Text(text.to_owned()),
    }
}

/// The `ime` controls for one input-method event. A commit longer than [`MAX_IME_TEXT`] bytes
/// is split into several, on char boundaries; an empty commit is nothing. A preedit cannot be
/// split, so it is cut to [`MAX_IME_TEXT`] bytes. `finish` carries no text.
pub fn ime_controls(op: ImeOp, text: &str) -> Vec<Control> {
    let ime = |text: &str| Control::Ime {
        op,
        text: text.to_owned(),
    };
    match op {
        ImeOp::Finish => vec![ime("")],
        ImeOp::Compose => vec![ime(&text[..floor_char_boundary(text, MAX_IME_TEXT)])],
        ImeOp::Commit => {
            let mut out = Vec::new();
            let mut rest = text;
            while !rest.is_empty() {
                let n = floor_char_boundary(rest, MAX_IME_TEXT);
                out.push(ime(&rest[..n]));
                rest = &rest[n..];
            }
            out
        }
    }
}

/// The largest char boundary of `s` at or below `max` bytes.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if s.len() <= max {
        return s.len();
    }
    (0..=max)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0)
}

pub const BUTTON_PRIMARY: u32 = 1;
pub const BUTTON_SECONDARY: u32 = 2;
pub const BUTTON_TERTIARY: u32 = 4;

/// Physical pixels of a touchpad/smooth scroll that count as one wheel notch.
pub const PIXELS_PER_NOTCH: f64 = 50.0;

#[derive(Default)]
pub struct InputState {
    buttons: u32,
    cursor: Option<(f64, f64)>,
    /// The last position seen, kept after the cursor leaves the window.
    last_cursor: Option<(f64, f64)>,
    pub mods: ModifiersState,
    /// Android keycodes currently down, with their repeat count.
    keys_down: Vec<(KeyCode, u32, u32)>,
    /// Keys whose press was sent as text (or was a dead key): their release sends nothing.
    text_keys: Vec<KeyCode>,
}

fn pointer(action: PointerAction, (x, y): (f32, f32), buttons: u32) -> InputEvent {
    let pressure = if buttons != 0 { 1.0 } else { 0.0 };
    InputEvent::Pointer {
        action,
        id: 0,
        x,
        y,
        buttons,
        pressure,
    }
}

fn button_bit(b: MouseButton) -> Option<u32> {
    match b {
        MouseButton::Left => Some(BUTTON_PRIMARY),
        MouseButton::Right => Some(BUTTON_SECONDARY),
        MouseButton::Middle => Some(BUTTON_TERTIARY),
        _ => None,
    }
}

impl InputState {
    pub fn cursor_moved(&mut self, rect: Rect, px: f64, py: f64) -> Option<InputEvent> {
        self.cursor = Some((px, py));
        self.last_cursor = self.cursor;
        if self.buttons != 0 {
            return Some(pointer(
                PointerAction::Move,
                clamp_normalized(rect, px, py),
                self.buttons,
            ));
        }
        to_normalized(rect, px, py).map(|xy| pointer(PointerAction::HoverMove, xy, 0))
    }

    pub fn cursor_left(&mut self) {
        self.cursor = None;
    }

    pub fn mouse_button(
        &mut self,
        rect: Rect,
        button: MouseButton,
        state: ElementState,
    ) -> Option<InputEvent> {
        let bit = button_bit(button)?;
        match state {
            ElementState::Pressed => {
                let (px, py) = self.cursor?;
                if self.buttons & bit != 0 {
                    return None;
                }
                // A gesture starts only inside the video; extra buttons join it anywhere.
                let xy = if self.buttons == 0 {
                    to_normalized(rect, px, py)?
                } else {
                    clamp_normalized(rect, px, py)
                };
                self.buttons |= bit;
                Some(pointer(PointerAction::Down, xy, self.buttons))
            }
            ElementState::Released => {
                if self.buttons & bit == 0 {
                    return None;
                }
                // The cursor may have left the window mid-drag: release where it was last seen.
                let (px, py) = self.cursor.or(self.last_cursor).unwrap_or_default();
                self.buttons &= !bit;
                Some(pointer(
                    PointerAction::Up,
                    clamp_normalized(rect, px, py),
                    self.buttons,
                ))
            }
        }
    }

    /// Focus lost: cancel a gesture in progress and release every held key, so the phone keeps
    /// no stuck button, key or modifier (their real ups go to whichever window has focus).
    pub fn focus_lost(&mut self, rect: Rect) -> Vec<InputEvent> {
        let mut out = Vec::new();
        if self.buttons != 0 {
            self.buttons = 0;
            let xy = self
                .cursor
                .or(self.last_cursor)
                .map_or((0.0, 0.0), |(x, y)| clamp_normalized(rect, x, y));
            out.push(pointer(PointerAction::Cancel, xy, 0));
        }
        // Keys held at blur (Alt of an Alt+Tab, or anything else) never get their up here:
        // release them now, with no modifiers, so the phone keeps no stuck key or META state.
        out.extend(
            self.keys_down
                .drain(..)
                .map(|(_, keycode, _)| InputEvent::Key {
                    action: KeyAction::Up,
                    keycode,
                    meta: 0,
                    repeat: 0,
                }),
        );
        self.mods = ModifiersState::empty();
        self.text_keys.clear();
        out
    }

    /// [`classify`] with memory: a key whose press went as text (or a `dead` key, which types
    /// nothing until the next key) sends nothing on release, so the phone never gets an
    /// unmatched key-up; a key already down as a key keeps repeating as a key.
    pub fn typed(
        &mut self,
        code: KeyCode,
        text: Option<&str>,
        dead: bool,
        state: ElementState,
        repeat: bool,
    ) -> Typed {
        let as_text = self.text_keys.contains(&code);
        match state {
            ElementState::Released => {
                if as_text {
                    self.text_keys.retain(|&k| k != code);
                    return Typed::Nothing;
                }
                Typed::Key
            }
            ElementState::Pressed => {
                if repeat && self.keys_down.iter().any(|k| k.0 == code) {
                    return Typed::Key;
                }
                if dead {
                    if !as_text {
                        self.text_keys.push(code);
                    }
                    return Typed::Nothing;
                }
                let typed = classify(code, text, self.mods, state);
                match typed {
                    Typed::Text(_) if !as_text => self.text_keys.push(code),
                    Typed::Key if as_text => return Typed::Nothing,
                    _ => {}
                }
                typed
            }
        }
    }

    pub fn wheel(&mut self, rect: Rect, delta: MouseScrollDelta) -> Option<InputEvent> {
        let (px, py) = self.cursor?;
        let (x, y) = to_normalized(rect, px, py)?;
        let (h, v) = match delta {
            MouseScrollDelta::LineDelta(h, v) => (h, v),
            MouseScrollDelta::PixelDelta(p) => (
                (p.x / PIXELS_PER_NOTCH) as f32,
                (p.y / PIXELS_PER_NOTCH) as f32,
            ),
        };
        // Android AXIS_HSCROLL is positive to the right content-wise inverse of winit's x.
        let h = -h;
        (h != 0.0 || v != 0.0).then_some(InputEvent::Scroll { x, y, h, v })
    }

    pub fn key(&mut self, code: KeyCode, state: ElementState, repeat: bool) -> Option<InputEvent> {
        let meta = meta_state(self.mods);
        match state {
            ElementState::Pressed => {
                let keycode = match self.keys_down.iter_mut().find(|k| k.0 == code) {
                    Some(k) if repeat => {
                        k.2 += 1;
                        return Some(InputEvent::Key {
                            action: KeyAction::Down,
                            keycode: k.1,
                            meta,
                            repeat: k.2,
                        });
                    }
                    Some(k) => k.1,
                    None => {
                        let keycode = window_keycode(code, self.mods)?;
                        self.keys_down.push((code, keycode, 0));
                        keycode
                    }
                };
                Some(InputEvent::Key {
                    action: KeyAction::Down,
                    keycode,
                    meta,
                    repeat: 0,
                })
            }
            ElementState::Released => {
                // The up uses the code chosen at press time (Escape stays BACK even if Super
                // was pressed in between).
                let i = self.keys_down.iter().position(|k| k.0 == code)?;
                let (_, keycode, _) = self.keys_down.remove(i);
                Some(InputEvent::Key {
                    action: KeyAction::Up,
                    keycode,
                    meta,
                    repeat: 0,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: Rect = Rect {
        x: 0.0,
        y: 100.0,
        w: 1000.0,
        h: 500.0,
    };

    #[test]
    fn click_is_down_then_up_with_masks() {
        let mut s = InputState::default();
        assert!(matches!(
            s.cursor_moved(R, 500.0, 350.0),
            Some(InputEvent::Pointer {
                action: PointerAction::HoverMove,
                buttons: 0,
                ..
            })
        ));
        let down = s
            .mouse_button(R, MouseButton::Left, ElementState::Pressed)
            .unwrap();
        assert_eq!(
            down,
            InputEvent::Pointer {
                action: PointerAction::Down,
                id: 0,
                x: 0.5,
                y: 0.5,
                buttons: 1,
                pressure: 1.0
            }
        );
        let mv = s.cursor_moved(R, 2000.0, 350.0).unwrap();
        assert_eq!(
            mv,
            InputEvent::Pointer {
                action: PointerAction::Move,
                id: 0,
                x: 1.0,
                y: 0.5,
                buttons: 1,
                pressure: 1.0
            }
        );
        let up = s
            .mouse_button(R, MouseButton::Left, ElementState::Released)
            .unwrap();
        assert_eq!(
            up,
            InputEvent::Pointer {
                action: PointerAction::Up,
                id: 0,
                x: 1.0,
                y: 0.5,
                buttons: 0,
                pressure: 0.0
            }
        );
        // Outside the video (letterbox bar): no hover, no press.
        assert_eq!(s.cursor_moved(R, 10.0, 10.0), None);
        assert_eq!(
            s.mouse_button(R, MouseButton::Left, ElementState::Pressed),
            None
        );
    }

    #[test]
    fn focus_lost_releases_held_keys_and_buttons() {
        let mut s = InputState::default();
        s.cursor_moved(R, 500.0, 350.0);
        s.mouse_button(R, MouseButton::Left, ElementState::Pressed)
            .unwrap();
        s.mods = ModifiersState::ALT;
        s.key(KeyCode::AltLeft, ElementState::Pressed, false)
            .unwrap();
        s.key(KeyCode::KeyA, ElementState::Pressed, false).unwrap();
        let evs = s.focus_lost(R);
        assert_eq!(
            evs,
            vec![
                InputEvent::Pointer {
                    action: PointerAction::Cancel,
                    id: 0,
                    x: 0.5,
                    y: 0.5,
                    buttons: 0,
                    pressure: 0.0
                },
                InputEvent::Key {
                    action: KeyAction::Up,
                    keycode: 57,
                    meta: 0,
                    repeat: 0
                },
                InputEvent::Key {
                    action: KeyAction::Up,
                    keycode: 29,
                    meta: 0,
                    repeat: 0
                },
            ]
        );
        // Already released: a late real up is not sent twice, and nothing is left to release.
        assert_eq!(s.key(KeyCode::KeyA, ElementState::Released, false), None);
        assert_eq!(s.focus_lost(R), vec![]);
    }

    #[test]
    fn release_after_cursor_left_still_sends_up() {
        let mut s = InputState::default();
        s.cursor_moved(R, 500.0, 350.0);
        s.mouse_button(R, MouseButton::Left, ElementState::Pressed)
            .unwrap();
        s.cursor_moved(R, 900.0, 350.0);
        s.cursor_left();
        assert_eq!(
            s.mouse_button(R, MouseButton::Left, ElementState::Released),
            Some(InputEvent::Pointer {
                action: PointerAction::Up,
                id: 0,
                x: 0.9,
                y: 0.5,
                buttons: 0,
                pressure: 0.0
            })
        );
        // With the cursor outside the window, a new press does nothing.
        assert_eq!(
            s.mouse_button(R, MouseButton::Left, ElementState::Pressed),
            None
        );
    }

    #[test]
    fn wheel_lines_and_pixels() {
        let mut s = InputState::default();
        s.cursor_moved(R, 500.0, 350.0);
        assert_eq!(
            s.wheel(R, MouseScrollDelta::LineDelta(0.0, -1.0)),
            Some(InputEvent::Scroll {
                x: 0.5,
                y: 0.5,
                h: -0.0,
                v: -1.0
            })
        );
        let px = MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, 100.0));
        assert_eq!(
            s.wheel(R, px),
            Some(InputEvent::Scroll {
                x: 0.5,
                y: 0.5,
                h: -0.0,
                v: 2.0
            })
        );
    }

    const P: ElementState = ElementState::Pressed;
    const U: ElementState = ElementState::Released;
    const NONE: ModifiersState = ModifiersState::empty();

    #[test]
    fn classify_ascii_is_key() {
        assert_eq!(classify(KeyCode::KeyA, Some("a"), NONE, P), Typed::Key);
        let shift = ModifiersState::SHIFT;
        assert_eq!(classify(KeyCode::KeyA, Some("A"), shift, P), Typed::Key);
        assert_eq!(classify(KeyCode::Digit1, Some("!"), shift, P), Typed::Key);
        assert_eq!(classify(KeyCode::Space, Some(" "), NONE, P), Typed::Key);
        assert_eq!(classify(KeyCode::Slash, Some("/"), NONE, P), Typed::Key);
        // Control characters and keys with no text stay keys.
        assert_eq!(classify(KeyCode::Enter, Some("\r"), NONE, P), Typed::Key);
        assert_eq!(classify(KeyCode::Tab, Some("\t"), NONE, P), Typed::Key);
        assert_eq!(
            classify(KeyCode::Backspace, Some("\u{8}"), NONE, P),
            Typed::Key
        );
        assert_eq!(classify(KeyCode::ArrowLeft, None, NONE, P), Typed::Key);
        assert_eq!(classify(KeyCode::ShiftLeft, None, NONE, P), Typed::Key);
        assert_eq!(classify(KeyCode::KeyA, Some(""), NONE, P), Typed::Key);
        // A release is always a key here (InputState decides what a text key's release is).
        assert_eq!(classify(KeyCode::KeyE, Some("é"), NONE, U), Typed::Key);
    }

    #[test]
    fn classify_dead_key_e_acute_is_text() {
        assert_eq!(
            classify(KeyCode::KeyE, Some("é"), NONE, P),
            Typed::Text("é".into())
        );
        // A non-US layout: German Y/Z swap, French A/Q.
        assert_eq!(
            classify(KeyCode::KeyY, Some("z"), NONE, P),
            Typed::Text("z".into())
        );
        // AltGr is not Alt in winit's state.
        assert_eq!(
            classify(KeyCode::KeyE, Some("€"), NONE, P),
            Typed::Text("€".into())
        );
    }

    #[test]
    fn classify_ctrl_c_is_key() {
        let ctrl = ModifiersState::CONTROL;
        assert_eq!(classify(KeyCode::KeyC, Some("\u{3}"), ctrl, P), Typed::Key);
        assert_eq!(classify(KeyCode::KeyC, Some("c"), ctrl, P), Typed::Key);
        assert_eq!(
            classify(KeyCode::KeyE, Some("é"), ModifiersState::ALT, P),
            Typed::Key
        );
        assert_eq!(
            classify(KeyCode::KeyE, Some("é"), ModifiersState::SUPER, P),
            Typed::Key
        );
    }

    #[test]
    fn text_press_has_no_release() {
        let mut s = InputState::default();
        assert_eq!(
            s.typed(KeyCode::KeyE, Some("é"), false, P, false),
            Typed::Text("é".into())
        );
        // A repeat of a text key repeats the text.
        assert_eq!(
            s.typed(KeyCode::KeyE, Some("é"), false, P, true),
            Typed::Text("é".into())
        );
        assert_eq!(
            s.typed(KeyCode::KeyE, None, false, U, false),
            Typed::Nothing
        );
        // Its release was consumed: a second one is a plain key release (unmatched, so `key`
        // sends nothing).
        assert_eq!(s.typed(KeyCode::KeyE, None, false, U, false), Typed::Key);
        assert_eq!(s.key(KeyCode::KeyE, U, false), None);

        // A dead key sends nothing at all, press or release.
        assert_eq!(
            s.typed(KeyCode::Quote, None, true, P, false),
            Typed::Nothing
        );
        assert_eq!(
            s.typed(KeyCode::Quote, None, false, U, false),
            Typed::Nothing
        );

        // A plain key is unaffected and its up matches its down.
        assert_eq!(
            s.typed(KeyCode::KeyA, Some("a"), false, P, false),
            Typed::Key
        );
        assert!(s.key(KeyCode::KeyA, P, false).is_some());
        assert_eq!(s.typed(KeyCode::KeyA, None, false, U, false), Typed::Key);
        assert!(s.key(KeyCode::KeyA, U, false).is_some());

        // A text key held at blur is forgotten (no up is owed to the phone).
        assert_eq!(
            s.typed(KeyCode::KeyY, Some("z"), false, P, false),
            Typed::Text("z".into())
        );
        assert_eq!(s.focus_lost(R), vec![]);
        assert_eq!(s.typed(KeyCode::KeyY, None, false, U, false), Typed::Key);
    }

    #[test]
    fn long_commit_splits_on_char_boundary() {
        let max = lantern_proto::MAX_IME_TEXT;
        assert_eq!(ime_controls(ImeOp::Commit, ""), vec![]);
        assert_eq!(
            ime_controls(ImeOp::Commit, "Tiếng"),
            vec![Control::Ime {
                op: ImeOp::Commit,
                text: "Tiếng".into()
            }]
        );
        // 'ế' is 3 bytes: MAX-1 ASCII bytes then 'ế' does not fit in the first commit.
        let text = format!("{}ế{}", "a".repeat(max - 1), "b".repeat(max));
        let parts = ime_controls(ImeOp::Commit, &text);
        let texts: Vec<&str> = parts
            .iter()
            .map(|c| match c {
                Control::Ime {
                    op: ImeOp::Commit,
                    text,
                } => text.as_str(),
                other => panic!("not a commit: {other:?}"),
            })
            .collect();
        assert_eq!(texts.len(), 3);
        assert!(texts.iter().all(|t| !t.is_empty() && t.len() <= max));
        assert_eq!(texts[0].len(), max - 1);
        assert_eq!(texts.concat(), text);
        // Exactly MAX bytes is one commit.
        assert_eq!(ime_controls(ImeOp::Commit, &"x".repeat(max)).len(), 1);
        // A preedit cannot be split: it is cut to MAX bytes on a char boundary.
        let long = "ệ".repeat(max);
        match &ime_controls(ImeOp::Compose, &long)[..] {
            [Control::Ime {
                op: ImeOp::Compose,
                text,
            }] => {
                assert!(text.len() <= max && text.len() > max - 3);
                assert!(long.starts_with(text.as_str()));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            ime_controls(ImeOp::Compose, ""),
            vec![Control::Ime {
                op: ImeOp::Compose,
                text: String::new()
            }]
        );
        assert_eq!(
            ime_controls(ImeOp::Finish, "ignored"),
            vec![Control::Ime {
                op: ImeOp::Finish,
                text: String::new()
            }]
        );
    }

    #[test]
    fn escape_is_back_and_up_matches_down() {
        let mut s = InputState::default();
        let d = s
            .key(KeyCode::Escape, ElementState::Pressed, false)
            .unwrap();
        assert!(matches!(
            d,
            InputEvent::Key {
                action: KeyAction::Down,
                keycode: 4,
                ..
            }
        ));
        s.mods = ModifiersState::SUPER;
        let u = s
            .key(KeyCode::Escape, ElementState::Released, false)
            .unwrap();
        assert!(matches!(
            u,
            InputEvent::Key {
                action: KeyAction::Up,
                keycode: 4,
                ..
            }
        ));
        let d = s
            .key(KeyCode::Escape, ElementState::Pressed, false)
            .unwrap();
        assert!(matches!(
            d,
            InputEvent::Key {
                action: KeyAction::Down,
                keycode: 111,
                meta: 0x30000,
                ..
            }
        ));
        let r = s.key(KeyCode::Escape, ElementState::Pressed, true).unwrap();
        assert!(matches!(
            r,
            InputEvent::Key {
                action: KeyAction::Down,
                keycode: 111,
                repeat: 1,
                ..
            }
        ));
    }
}
