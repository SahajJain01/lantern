//! Window input → Android input events.
//!
//! The mouse is pointer id 0 with an Android button mask (`BUTTON_PRIMARY`=1,
//! `BUTTON_SECONDARY`=2, `BUTTON_TERTIARY`=4). Each press is a `down` carrying the new mask and
//! each release an `up` carrying the remaining mask (the helper derives the action button from
//! the change); motion is `move` while a button is held, else `hover-move`. Coordinates are
//! normalized to the video rect; a press outside the video is ignored, a drag that leaves it is
//! clamped to the edge.

use lantern_proto::{InputEvent, KeyAction, PointerAction};
use winit::event::{ElementState, MouseButton, MouseScrollDelta};
use winit::keyboard::{KeyCode, ModifiersState};

use super::geometry::{clamp_normalized, to_normalized, Rect};
use super::keymap::{meta_state, window_keycode};

pub const BUTTON_PRIMARY: u32 = 1;
pub const BUTTON_SECONDARY: u32 = 2;
pub const BUTTON_TERTIARY: u32 = 4;

/// Physical pixels of a touchpad/smooth scroll that count as one wheel notch.
pub const PIXELS_PER_NOTCH: f64 = 50.0;

#[derive(Default)]
pub struct InputState {
    buttons: u32,
    cursor: Option<(f64, f64)>,
    pub mods: ModifiersState,
    /// Android keycodes currently down, with their repeat count.
    keys_down: Vec<(KeyCode, u32, u32)>,
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
        let (px, py) = self.cursor?;
        match state {
            ElementState::Pressed => {
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
                self.buttons &= !bit;
                Some(pointer(
                    PointerAction::Up,
                    clamp_normalized(rect, px, py),
                    self.buttons,
                ))
            }
        }
    }

    /// Focus lost mid-gesture: cancel it so the phone does not keep a stuck button.
    pub fn focus_lost(&mut self, rect: Rect) -> Option<InputEvent> {
        if self.buttons == 0 {
            return None;
        }
        self.buttons = 0;
        let xy = self
            .cursor
            .map_or((0.0, 0.0), |(x, y)| clamp_normalized(rect, x, y));
        Some(pointer(PointerAction::Cancel, xy, 0))
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
