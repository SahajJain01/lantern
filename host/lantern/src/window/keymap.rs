//! winit physical keys to Android keycodes and meta state.

use winit::keyboard::{KeyCode, ModifiersState};

pub const KEYCODE_BACK: u32 = 4;
pub const KEYCODE_ESCAPE: u32 = 111;

pub fn android_keycode(k: KeyCode) -> Option<u32> {
    use KeyCode::*;
    Some(match k {
        KeyA => 29,
        KeyB => 30,
        KeyC => 31,
        KeyD => 32,
        KeyE => 33,
        KeyF => 34,
        KeyG => 35,
        KeyH => 36,
        KeyI => 37,
        KeyJ => 38,
        KeyK => 39,
        KeyL => 40,
        KeyM => 41,
        KeyN => 42,
        KeyO => 43,
        KeyP => 44,
        KeyQ => 45,
        KeyR => 46,
        KeyS => 47,
        KeyT => 48,
        KeyU => 49,
        KeyV => 50,
        KeyW => 51,
        KeyX => 52,
        KeyY => 53,
        KeyZ => 54,
        Digit0 => 7,
        Digit1 => 8,
        Digit2 => 9,
        Digit3 => 10,
        Digit4 => 11,
        Digit5 => 12,
        Digit6 => 13,
        Digit7 => 14,
        Digit8 => 15,
        Digit9 => 16,
        F1 => 131,
        F2 => 132,
        F3 => 133,
        F4 => 134,
        F5 => 135,
        F6 => 136,
        F7 => 137,
        F8 => 138,
        F9 => 139,
        F10 => 140,
        F11 => 141,
        F12 => 142,
        ArrowUp => 19,
        ArrowDown => 20,
        ArrowLeft => 21,
        ArrowRight => 22,
        Enter => 66,
        Escape => KEYCODE_ESCAPE,
        Backspace => 67,
        Tab => 61,
        Space => 62,
        Delete => 112,
        Insert => 124,
        Home => 122,
        End => 123,
        PageUp => 92,
        PageDown => 93,
        ShiftLeft => 59,
        ShiftRight => 60,
        ControlLeft => 113,
        ControlRight => 114,
        AltLeft => 57,
        AltRight => 58,
        SuperLeft => 117,
        SuperRight => 118,
        CapsLock => 115,
        NumLock => 143,
        ScrollLock => 116,
        PrintScreen => 120,
        Pause => 121,
        ContextMenu => 82,
        Minus => 69,
        Equal => 70,
        BracketLeft => 71,
        BracketRight => 72,
        Backslash => 73,
        Semicolon => 74,
        Quote => 75,
        Slash => 76,
        Comma => 55,
        Period => 56,
        Backquote => 68,
        Numpad0 => 144,
        Numpad1 => 145,
        Numpad2 => 146,
        Numpad3 => 147,
        Numpad4 => 148,
        Numpad5 => 149,
        Numpad6 => 150,
        Numpad7 => 151,
        Numpad8 => 152,
        Numpad9 => 153,
        NumpadDivide => 154,
        NumpadMultiply => 155,
        NumpadSubtract => 156,
        NumpadAdd => 157,
        NumpadDecimal => 158,
        NumpadComma => 159,
        NumpadEnter => 160,
        NumpadEqual => 161,
        AudioVolumeUp => 24,
        AudioVolumeDown => 25,
        AudioVolumeMute => 164,
        MediaPlayPause => 85,
        MediaStop => 86,
        MediaTrackNext => 87,
        MediaTrackPrevious => 88,
        _ => return None,
    })
}

/// Window-level mapping: Escape is Android BACK unless Super is held (then ESCAPE).
pub fn window_keycode(k: KeyCode, mods: ModifiersState) -> Option<u32> {
    if k == KeyCode::Escape && !mods.super_key() {
        return Some(KEYCODE_BACK);
    }
    android_keycode(k)
}

const LETTERS: [KeyCode; 26] = {
    use KeyCode::*;
    [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO,
        KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ]
};

const DIGITS: [KeyCode; 10] = {
    use KeyCode::*;
    [
        Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
    ]
};

/// The character a US layout gives for a letter, digit, space or US punctuation key, with or
/// without Shift. These are exactly the characters the Android key events Lantern sends for the
/// key produce, so a key whose text matches is sent as a key, anything else as text.
pub fn us_char(k: KeyCode, shift: bool) -> Option<char> {
    use KeyCode::*;
    if let Some(i) = LETTERS.iter().position(|&l| l == k) {
        let c = (b'a' + i as u8) as char;
        return Some(if shift { c.to_ascii_uppercase() } else { c });
    }
    if let Some(i) = DIGITS.iter().position(|&d| d == k) {
        return Some(if shift {
            b")!@#$%^&*("[i] as char
        } else {
            (b'0' + i as u8) as char
        });
    }
    let (plain, shifted) = match k {
        Space => (' ', ' '),
        Minus => ('-', '_'),
        Equal => ('=', '+'),
        BracketLeft => ('[', '{'),
        BracketRight => (']', '}'),
        Backslash => ('\\', '|'),
        Semicolon => (';', ':'),
        Quote => ('\'', '"'),
        Comma => (',', '<'),
        Period => ('.', '>'),
        Slash => ('/', '?'),
        Backquote => ('`', '~'),
        _ => return None,
    };
    Some(if shift { shifted } else { plain })
}

/// The US-layout letter or digit key for `c` (either case), for debug `key:` steps.
pub fn char_key(c: char) -> Option<KeyCode> {
    let c = c.to_ascii_lowercase();
    match c {
        'a'..='z' => Some(LETTERS[(c as u8 - b'a') as usize]),
        '0'..='9' => Some(DIGITS[(c as u8 - b'0') as usize]),
        _ => None,
    }
}

const META_SHIFT_ON: u32 = 0x1;
const META_ALT_ON: u32 = 0x2;
const META_ALT_LEFT_ON: u32 = 0x10;
const META_SHIFT_LEFT_ON: u32 = 0x40;
const META_CTRL_ON: u32 = 0x1000;
const META_CTRL_LEFT_ON: u32 = 0x2000;
const META_META_ON: u32 = 0x10000;
const META_META_LEFT_ON: u32 = 0x20000;

/// Android `KeyEvent.META_*` bits for the held modifiers. winit's state does not say which
/// side is held, so the left-side bit is reported with the generic one.
pub fn meta_state(mods: ModifiersState) -> u32 {
    let mut m = 0;
    if mods.shift_key() {
        m |= META_SHIFT_ON | META_SHIFT_LEFT_ON;
    }
    if mods.alt_key() {
        m |= META_ALT_ON | META_ALT_LEFT_ON;
    }
    if mods.control_key() {
        m |= META_CTRL_ON | META_CTRL_LEFT_ON;
    }
    if mods.super_key() {
        m |= META_META_ON | META_META_LEFT_ON;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_letters_and_specials() {
        assert_eq!(android_keycode(KeyCode::KeyA), Some(29));
        assert_eq!(android_keycode(KeyCode::KeyZ), Some(54));
        assert_eq!(android_keycode(KeyCode::Digit0), Some(7));
        assert_eq!(android_keycode(KeyCode::Digit9), Some(16));
        assert_eq!(android_keycode(KeyCode::F1), Some(131));
        assert_eq!(android_keycode(KeyCode::F12), Some(142));
        assert_eq!(android_keycode(KeyCode::ArrowUp), Some(19));
        assert_eq!(android_keycode(KeyCode::ArrowDown), Some(20));
        assert_eq!(android_keycode(KeyCode::ArrowLeft), Some(21));
        assert_eq!(android_keycode(KeyCode::ArrowRight), Some(22));
        let specials = [
            (KeyCode::Enter, 66),
            (KeyCode::Escape, 111),
            (KeyCode::Backspace, 67),
            (KeyCode::Tab, 61),
            (KeyCode::Space, 62),
            (KeyCode::Delete, 112),
            (KeyCode::Home, 122),
            (KeyCode::End, 123),
            (KeyCode::PageUp, 92),
            (KeyCode::PageDown, 93),
            (KeyCode::ShiftLeft, 59),
            (KeyCode::ShiftRight, 60),
            (KeyCode::ControlLeft, 113),
            (KeyCode::ControlRight, 114),
            (KeyCode::AltLeft, 57),
            (KeyCode::AltRight, 58),
            (KeyCode::SuperLeft, 117),
            (KeyCode::SuperRight, 118),
        ];
        for (k, code) in specials {
            assert_eq!(android_keycode(k), Some(code), "{k:?}");
        }
        assert_eq!(android_keycode(KeyCode::Fn), None);

        // Window level: Escape is BACK, Super+Escape passes through as ESCAPE.
        assert_eq!(
            window_keycode(KeyCode::Escape, ModifiersState::empty()),
            Some(KEYCODE_BACK)
        );
        assert_eq!(
            window_keycode(KeyCode::Escape, ModifiersState::SHIFT),
            Some(KEYCODE_BACK)
        );
        assert_eq!(
            window_keycode(KeyCode::Escape, ModifiersState::SUPER),
            Some(KEYCODE_ESCAPE)
        );
        assert_eq!(
            window_keycode(KeyCode::KeyA, ModifiersState::SUPER),
            Some(29)
        );
    }

    #[test]
    fn us_char_letters_digits_punct() {
        assert_eq!(us_char(KeyCode::KeyA, false), Some('a'));
        assert_eq!(us_char(KeyCode::KeyA, true), Some('A'));
        assert_eq!(us_char(KeyCode::KeyZ, false), Some('z'));
        assert_eq!(us_char(KeyCode::KeyE, true), Some('E'));
        assert_eq!(us_char(KeyCode::Digit0, false), Some('0'));
        assert_eq!(us_char(KeyCode::Digit9, false), Some('9'));
        let shifted_digits = ")!@#$%^&*(";
        let digits = [
            KeyCode::Digit0,
            KeyCode::Digit1,
            KeyCode::Digit2,
            KeyCode::Digit3,
            KeyCode::Digit4,
            KeyCode::Digit5,
            KeyCode::Digit6,
            KeyCode::Digit7,
            KeyCode::Digit8,
            KeyCode::Digit9,
        ];
        for (k, c) in digits.into_iter().zip(shifted_digits.chars()) {
            assert_eq!(us_char(k, true), Some(c), "{k:?}");
        }
        assert_eq!(us_char(KeyCode::Space, false), Some(' '));
        assert_eq!(us_char(KeyCode::Space, true), Some(' '));
        let punct = [
            (KeyCode::Minus, '-', '_'),
            (KeyCode::Equal, '=', '+'),
            (KeyCode::BracketLeft, '[', '{'),
            (KeyCode::BracketRight, ']', '}'),
            (KeyCode::Backslash, '\\', '|'),
            (KeyCode::Semicolon, ';', ':'),
            (KeyCode::Quote, '\'', '"'),
            (KeyCode::Comma, ',', '<'),
            (KeyCode::Period, '.', '>'),
            (KeyCode::Slash, '/', '?'),
            (KeyCode::Backquote, '`', '~'),
        ];
        for (k, plain, shifted) in punct {
            assert_eq!(us_char(k, false), Some(plain), "{k:?}");
            assert_eq!(us_char(k, true), Some(shifted), "{k:?} shifted");
        }
        for k in [
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Backspace,
            KeyCode::Escape,
            KeyCode::ArrowUp,
            KeyCode::Numpad1,
            KeyCode::IntlBackslash,
        ] {
            assert_eq!(us_char(k, false), None, "{k:?}");
        }
        // Every letter and digit maps back to its key (debug `key:` steps).
        for c in ('a'..='z').chain('0'..='9') {
            let k = char_key(c).unwrap();
            assert_eq!(us_char(k, false), Some(c));
            assert_eq!(char_key(c.to_ascii_uppercase()), Some(k));
        }
        assert_eq!(char_key('é'), None);
    }

    #[test]
    fn meta_state_bits() {
        assert_eq!(meta_state(ModifiersState::empty()), 0);
        assert_eq!(meta_state(ModifiersState::SHIFT), 0x1 | 0x40);
        assert_eq!(meta_state(ModifiersState::ALT), 0x2 | 0x10);
        assert_eq!(meta_state(ModifiersState::CONTROL), 0x1000 | 0x2000);
        assert_eq!(meta_state(ModifiersState::SUPER), 0x10000 | 0x20000);
        assert_eq!(
            meta_state(ModifiersState::SHIFT | ModifiersState::CONTROL),
            0x3041
        );
    }
}
