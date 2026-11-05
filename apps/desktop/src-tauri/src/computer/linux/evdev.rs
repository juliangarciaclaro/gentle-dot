//! Linux evdev key codes (`linux/input-event-codes.h`) for the key names `keys` parses (S2),
//! US layout. `cmd` presses Ctrl, the key Linux shortcuts use where macOS uses ⌘ (as `winput`
//! does on Windows); `fn` has no key on Linux and is ignored.

use crate::computer::keys::Modifiers;

pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_LEFTALT: u16 = 56;

/// Every name `keys::parse_combo` accepts (aliases aside) and its evdev code.
pub(super) const KEYS: &[(&str, u16)] = &[
    ("escape", 1), ("1", 2), ("2", 3), ("3", 4), ("4", 5), ("5", 6), ("6", 7), ("7", 8), ("8", 9),
    ("9", 10), ("0", 11), ("-", 12), ("=", 13), ("delete", 14), ("tab", 15), ("q", 16), ("w", 17),
    ("e", 18), ("r", 19), ("t", 20), ("y", 21), ("u", 22), ("i", 23), ("o", 24), ("p", 25), ("[", 26),
    ("]", 27), ("return", 28), ("a", 30), ("s", 31), ("d", 32), ("f", 33), ("g", 34), ("h", 35),
    ("j", 36), ("k", 37), ("l", 38), (";", 39), ("'", 40), ("`", 41), ("\\", 43), ("z", 44), ("x", 45),
    ("c", 46), ("v", 47), ("b", 48), ("n", 49), ("m", 50), (",", 51), (".", 52), ("/", 53),
    ("space", 57), ("f1", 59), ("f2", 60), ("f3", 61), ("f4", 62), ("f5", 63), ("f6", 64),
    ("f7", 65), ("f8", 66), ("f9", 67), ("f10", 68), ("f11", 87), ("f12", 88), ("home", 102),
    ("up", 103), ("pageup", 104), ("left", 105), ("right", 106), ("end", 107), ("down", 108),
    ("pagedown", 109), ("forwarddelete", 111),
];

/// The evdev code of a canonical key name from `keys`.
pub fn key_code(name: &str) -> Option<u16> {
    KEYS.iter().find(|(known, _)| *known == name).map(|(_, code)| *code)
}

/// The modifier keys to hold for a combo, in macOS's order (Cmd, Shift, Alt, Ctrl). `cmd` and
/// `ctrl` are the same key here, pressed once.
pub fn held_modifiers(m: &Modifiers) -> Vec<u16> {
    let mut held = Vec::new();
    for (on, code) in [
        (m.cmd, KEY_LEFTCTRL), (m.shift, KEY_LEFTSHIFT), (m.alt, KEY_LEFTALT), (m.ctrl, KEY_LEFTCTRL),
    ] {
        if on && !held.contains(&code) {
            held.push(code);
        }
    }
    held
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::keys;

    #[test]
    fn every_key_name_has_a_distinct_code() {
        let mut seen = Vec::new();
        for name in keys::names() {
            let code = key_code(name).unwrap_or_else(|| panic!("no evdev code for `{name}`"));
            assert!(!seen.contains(&code), "`{name}` repeats code {code}");
            seen.push(code);
        }
        assert_eq!(seen.len(), KEYS.len(), "KEYS lists a name `keys` does not know");
    }

    #[test]
    fn codes_follow_input_event_codes() {
        for (name, code) in [
            ("a", 30), ("return", 28), ("delete", 14), ("forwarddelete", 111), ("escape", 1),
            ("f1", 59), ("f10", 68), ("f11", 87), ("f12", 88), ("space", 57), ("left", 105),
            ("pagedown", 109), ("/", 53), ("0", 11),
        ] {
            assert_eq!(key_code(name), Some(code), "{name}");
        }
        assert_eq!(key_code("f13"), None);
    }

    #[test]
    fn modifiers_are_held_in_mac_order_and_cmd_ctrl_press_ctrl_once() {
        let m = |cmd, shift, alt, ctrl, fn_key| Modifiers { cmd, shift, alt, ctrl, fn_key };
        assert_eq!(held_modifiers(&Modifiers::default()), Vec::<u16>::new());
        assert_eq!(held_modifiers(&m(true, true, true, false, false)), [29, 42, 56]);
        assert_eq!(held_modifiers(&m(false, true, true, true, false)), [42, 56, 29]);
        assert_eq!(held_modifiers(&m(true, false, false, true, false)), [29]);
        assert_eq!(held_modifiers(&m(false, false, false, false, true)), Vec::<u16>::new());
    }
}
