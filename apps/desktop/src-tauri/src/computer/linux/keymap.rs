//! The XKB keymap for the Wayland virtual keyboard (S2): `zwp_virtual_keyboard_v1` takes a
//! text keymap, so the one we upload binds the named keys at their evdev codes and gives each
//! distinct character of the text to type its own keycode (the `wtype` approach). One keymap
//! serves both `key` combos and `type` text. The keymap is self-contained: no system includes.
//!
//! Keycodes in the keymap are evdev + 8; `Keymap::codes` and the `evdev` module hold the evdev
//! codes, which are what the virtual keyboard's `key` request takes.

use super::evdev;

/// Most distinct characters one keymap carries; longer text goes out in several keymaps.
pub const MAX_DISTINCT_CHARS: usize = 200;

/// First evdev code given to a character, above every named key and modifier.
const FIRST_CHAR_CODE: u16 = 128;

/// xkb keycode = evdev code + 8.
const XKB_OFFSET: u32 = 8;

/// A keymap for one chunk of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    /// The XKB v1 text keymap to upload.
    pub text: String,
    /// For each char of the chunk, in order, the evdev code to press.
    pub codes: Vec<u16>,
}

/// The keysym number of a character: Latin-1 for U+0020..U+007E and U+00A0..U+00FF, else the
/// Unicode keysym `0x01000000 + codepoint`.
pub fn keysym(c: char) -> u32 {
    match c as u32 {
        cp @ (0x20..=0x7e | 0xa0..=0xff) => cp,
        cp => 0x0100_0000 + cp,
    }
}

/// The xkb keysym name of a named key from `keys` (`delete` is BackSpace, `forwarddelete` Delete).
fn named_keysym(key: &str) -> Option<&str> {
    Some(match key {
        "return" => "Return", "delete" => "BackSpace", "forwarddelete" => "Delete", "tab" => "Tab",
        "escape" => "Escape", "space" => "space", "home" => "Home", "end" => "End",
        "pageup" => "Prior", "pagedown" => "Next", "left" => "Left", "right" => "Right",
        "up" => "Up", "down" => "Down", "-" => "minus", "=" => "equal", "[" => "bracketleft",
        "]" => "bracketright", ";" => "semicolon", "'" => "apostrophe", "`" => "grave",
        "\\" => "backslash", "," => "comma", "." => "period", "/" => "slash",
        "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            // `Fn` is the key name capitalised.
            return F_KEYS.iter().find(|(name, _)| *name == key).map(|(_, sym)| *sym);
        }
        letter_or_digit if letter_or_digit.len() == 1 && letter_or_digit.as_bytes()[0].is_ascii_alphanumeric() => {
            letter_or_digit
        }
        _ => return None,
    })
}

const F_KEYS: &[(&str, &str)] = &[
    ("f1", "F1"), ("f2", "F2"), ("f3", "F3"), ("f4", "F4"), ("f5", "F5"), ("f6", "F6"),
    ("f7", "F7"), ("f8", "F8"), ("f9", "F9"), ("f10", "F10"), ("f11", "F11"), ("f12", "F12"),
];

fn is_letter(key: &str) -> bool {
    key.len() == 1 && key.as_bytes()[0].is_ascii_lowercase()
}

/// The keymap for `text`, one keycode per distinct character in order of first appearance.
/// Control characters (newline included) and more than `MAX_DISTINCT_CHARS` distinct characters
/// are errors; `split_chunks` cuts longer text first.
pub fn build(text: &str) -> Result<Keymap, String> {
    let mut chars: Vec<char> = Vec::new();
    let mut codes = Vec::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            return Err(format!("cannot type control character U+{:04X}", c as u32));
        }
        let index = match chars.iter().position(|known| *known == c) {
            Some(index) => index,
            None => {
                if chars.len() == MAX_DISTINCT_CHARS {
                    return Err(format!("more than {MAX_DISTINCT_CHARS} distinct characters in one keymap"));
                }
                chars.push(c);
                chars.len() - 1
            }
        };
        codes.push(FIRST_CHAR_CODE + index as u16);
    }
    Ok(Keymap { text: write(&chars), codes })
}

/// Splits `text` into pieces that each fit one keymap, cutting only when the next character
/// would be the 201st distinct one. Concatenating the pieces gives `text` back.
pub fn split_chunks(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut distinct: Vec<char> = Vec::new();
    let mut start = 0;
    for (at, c) in text.char_indices() {
        if !distinct.contains(&c) {
            if distinct.len() == MAX_DISTINCT_CHARS {
                pieces.push(&text[start..at]);
                start = at;
                distinct.clear();
            }
            distinct.push(c);
        }
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

fn write(chars: &[char]) -> String {
    let key = |evdev: u16| format!("<K{}>", evdev as u32 + XKB_OFFSET);
    let modifiers = [
        (evdev::KEY_LEFTCTRL, "Control_L"), (evdev::KEY_LEFTSHIFT, "Shift_L"), (evdev::KEY_LEFTALT, "Alt_L"),
    ];
    let last = FIRST_CHAR_CODE as usize + chars.len().max(1) - 1;
    let max = (last as u32).max(evdev::KEYS.iter().map(|(_, code)| *code as u32).max().unwrap_or(0)) + XKB_OFFSET;

    let mut out = String::from("xkb_keymap {\nxkb_keycodes \"gentle\" {\n");
    out += &format!("  minimum = {XKB_OFFSET};\n  maximum = {max};\n");
    let named = evdev::KEYS.iter().map(|(_, code)| *code);
    let mod_codes = modifiers.iter().map(|(code, _)| *code);
    let char_codes = (0..chars.len()).map(|i| FIRST_CHAR_CODE + i as u16);
    for code in named.chain(mod_codes).chain(char_codes) {
        out += &format!("  {} = {};\n", key(code), code as u32 + XKB_OFFSET);
    }
    out += "};\n";

    out += "xkb_types \"gentle\" {\n  virtual_modifiers NumLock;\n";
    out += "  type \"ONE_LEVEL\" {\n    modifiers = None;\n    map[None] = Level1;\n    level_name[Level1] = \"Any\";\n  };\n";
    out += "  type \"TWO_LEVEL\" {\n    modifiers = Shift;\n    map[Shift] = Level2;\n    level_name[Level1] = \"Base\";\n    level_name[Level2] = \"Shift\";\n  };\n";
    out += "  type \"ALPHABETIC\" {\n    modifiers = Shift+Lock;\n    map[Shift] = Level2;\n    map[Lock] = Level2;\n    level_name[Level1] = \"Base\";\n    level_name[Level2] = \"Caps\";\n  };\n};\n";

    out += "xkb_compatibility \"gentle\" {\n";
    out += "  interpret Shift_L { action = SetMods(modifiers = Shift); };\n";
    out += "  interpret Control_L { action = SetMods(modifiers = Control); };\n";
    out += "  interpret Alt_L { action = SetMods(modifiers = Mod1); };\n};\n";

    out += "xkb_symbols \"gentle\" {\n  name[Group1] = \"Gentle\";\n";
    for (name, code) in evdev::KEYS {
        let sym = named_keysym(name).expect("every named key has a keysym");
        if is_letter(name) {
            out += &format!(
                "  key {} {{ type[Group1] = \"ALPHABETIC\", symbols[Group1] = [ {sym}, {} ] }};\n",
                key(*code),
                sym.to_ascii_uppercase()
            );
        } else {
            out += &format!("  key {} {{ type[Group1] = \"ONE_LEVEL\", symbols[Group1] = [ {sym} ] }};\n", key(*code));
        }
    }
    for (code, sym) in modifiers {
        out += &format!("  key {} {{ type[Group1] = \"ONE_LEVEL\", symbols[Group1] = [ {sym} ] }};\n", key(code));
    }
    for (i, c) in chars.iter().enumerate() {
        let code = FIRST_CHAR_CODE + i as u16;
        out += &format!(
            "  key {} {{ type[Group1] = \"ONE_LEVEL\", symbols[Group1] = [ 0x{:04x} ] }};\n",
            key(code),
            keysym(*c)
        );
    }
    out += &format!("  modifier_map Shift {{ {} }};\n", key(evdev::KEY_LEFTSHIFT));
    out += &format!("  modifier_map Control {{ {} }};\n", key(evdev::KEY_LEFTCTRL));
    out += &format!("  modifier_map Mod1 {{ {} }};\n}};\n}};\n", key(evdev::KEY_LEFTALT));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_for<'a>(map: &'a Keymap, xkb_key: &str) -> Vec<&'a str> {
        map.text.lines().filter(|l| l.contains(&format!("key {xkb_key} "))).collect()
    }

    #[test]
    fn each_distinct_char_gets_one_keycode_in_order_of_first_appearance() {
        let map = build("abca").unwrap();
        assert_eq!(map.codes, [128, 129, 130, 128]);
        // xkb keycode = evdev + 8.
        assert!(line_for(&map, "<K136>")[0].contains("[ 0x0061 ]"));
        assert!(line_for(&map, "<K137>")[0].contains("[ 0x0062 ]"));
        assert!(line_for(&map, "<K138>")[0].contains("[ 0x0063 ]"));
        assert!(map.text.contains("maximum = 138;"));
    }

    #[test]
    fn keysyms_are_latin1_or_unicode_offset() {
        assert_eq!(keysym('a'), 0x61);
        assert_eq!(keysym(' '), 0x20);
        assert_eq!(keysym('é'), 0xe9);
        assert_eq!(keysym('\u{a0}'), 0xa0);
        assert_eq!(keysym('☺'), 0x0100_263a);
        assert_eq!(keysym('€'), 0x0100_20ac);
        let map = build("é☺").unwrap();
        assert!(map.text.contains("[ 0x00e9 ]"));
        assert!(map.text.contains("[ 0x100263a ]"));
    }

    #[test]
    fn named_keys_and_modifiers_are_bound_at_their_evdev_codes() {
        let map = build("").unwrap();
        assert!(map.codes.is_empty());
        for (xkb_key, sym) in [
            ("<K36>", "Return"), ("<K22>", "BackSpace"), ("<K119>", "Delete"), ("<K23>", "Tab"),
            ("<K9>", "Escape"), ("<K38>", "a, A"), ("<K10>", "1"), ("<K65>", "space"),
            ("<K111>", "Up"), ("<K112>", "Prior"), ("<K117>", "Next"), ("<K67>", "F1"),
            ("<K37>", "Control_L"), ("<K50>", "Shift_L"), ("<K64>", "Alt_L"),
        ] {
            let lines = line_for(&map, xkb_key);
            assert_eq!(lines.len(), 1, "{xkb_key}");
            assert!(lines[0].contains(&format!("[ {sym} ]")), "{xkb_key}: {}", lines[0]);
        }
        for modifier in ["Shift { <K50> }", "Control { <K37> }", "Mod1 { <K64> }"] {
            assert!(map.text.contains(&format!("modifier_map {modifier};")), "{modifier}");
        }
    }

    #[test]
    fn the_keymap_is_self_contained() {
        let text = build("hi").unwrap().text;
        assert!(!text.contains("include"));
        for section in ["xkb_keycodes", "xkb_types", "xkb_compatibility", "xkb_symbols"] {
            assert!(text.contains(section), "{section}");
        }
        for ty in ["ONE_LEVEL", "TWO_LEVEL", "ALPHABETIC"] {
            assert!(text.contains(&format!("type \"{ty}\"")), "{ty}");
        }
        assert!(text.contains("minimum = 8;"));
    }

    #[test]
    fn character_keycodes_never_collide_with_named_keys() {
        let text: String = (0x20u8..0x7f).map(char::from).collect();
        let map = build(&text).unwrap();
        let mut seen = std::collections::HashSet::new();
        let defined = map.text.lines().filter(|l| l.starts_with("  <K")).count();
        for line in map.text.lines().filter(|l| l.starts_with("  <K")) {
            assert!(seen.insert(line.split(" = ").next().unwrap().to_owned()), "{line}");
        }
        assert_eq!(seen.len(), defined);
        assert!(map.codes.iter().all(|code| *code >= FIRST_CHAR_CODE));
        assert!(evdev::KEYS.iter().all(|(_, code)| *code < FIRST_CHAR_CODE));
    }

    #[test]
    fn every_named_key_has_a_keysym() {
        for (name, _) in evdev::KEYS {
            assert!(named_keysym(name).is_some(), "{name}");
        }
    }

    #[test]
    fn control_characters_are_rejected() {
        for text in ["a\nb", "tab\there", "\u{7f}", "\u{85}", "\r"] {
            assert!(build(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn too_many_distinct_chars_are_rejected_and_split_into_keymap_sized_pieces() {
        let many: String = (0x4e00..0x4e00 + 450).filter_map(char::from_u32).collect();
        assert!(build(&many).is_err());
        let pieces = split_chunks(&many);
        assert_eq!(pieces.iter().map(|p| p.chars().count()).collect::<Vec<_>>(), [200, 200, 50]);
        assert_eq!(pieces.concat(), many);
        assert!(pieces.iter().all(|p| build(p).is_ok()));
        // Repeats do not count toward the bound; short text stays in one piece.
        let repeated = "ab".repeat(500);
        assert_eq!(split_chunks(&repeated), [repeated.as_str()]);
        assert!(split_chunks("").is_empty());
    }
}
