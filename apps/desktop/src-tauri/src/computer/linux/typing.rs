//! What the virtual keyboard is sent for a `key` combo or a chunk of `type` text (S2): batches
//! of one keymap and the key events to play under it. Pure; `wayland` plays them.

use super::{evdev, keymap};
use crate::computer::keys::{KeyCombo, Modifiers};

/// xkb modifier masks of the keymap `keymap::build` writes.
const SHIFT: u32 = 1;
const CONTROL: u32 = 4;
const ALT: u32 = 8;

/// One step of a key sequence, sent to the virtual keyboard in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStep {
    /// xkb modifier mask held from now on.
    Modifiers(u32),
    Key { code: u16, down: bool },
}

/// A keymap to upload and the steps to play under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub keymap: String,
    pub steps: Vec<KeyStep>,
}

/// The mask for the modifiers of a combo: `cmd` and `ctrl` are Control, `fn` has no key.
pub fn modifier_mask(m: &Modifiers) -> u32 {
    [(m.shift, SHIFT), (m.cmd || m.ctrl, CONTROL), (m.alt, ALT)]
        .into_iter()
        .filter(|(on, _)| *on)
        .fold(0, |mask, (_, bit)| mask | bit)
}

fn tap(code: u16) -> [KeyStep; 2] {
    [KeyStep::Key { code, down: true }, KeyStep::Key { code, down: false }]
}

/// The named keys' keymap (no characters of its own).
fn named_keymap() -> Result<String, String> {
    keymap::build("").map(|map| map.text)
}

/// Presses the combo's key with its modifiers held, then releases everything.
pub fn combo_batch(combo: &KeyCombo) -> Result<Batch, String> {
    let code = evdev::key_code(combo.key).ok_or_else(|| format!("no key code for `{}`", combo.key))?;
    let mask = modifier_mask(&combo.modifiers);
    let mut steps = Vec::new();
    if mask != 0 {
        steps.push(KeyStep::Modifiers(mask));
    }
    steps.extend(tap(code));
    if mask != 0 {
        steps.push(KeyStep::Modifiers(0));
    }
    Ok(Batch { keymap: named_keymap()?, steps })
}

/// The batches that type `text`: one per keymap-sized run of characters, with newlines and tabs
/// sent as Return and Tab. Any other control character is an error, before anything is sent.
pub fn text_batches(text: &str) -> Result<Vec<Batch>, String> {
    let named = |key: &str| -> Result<Batch, String> {
        let code = evdev::key_code(key).ok_or_else(|| format!("no key code for `{key}`"))?;
        Ok(Batch { keymap: named_keymap()?, steps: tap(code).to_vec() })
    };
    let mut batches = Vec::new();
    let mut run_start = 0;
    let flush = |run: &str, batches: &mut Vec<Batch>| -> Result<(), String> {
        for chunk in keymap::split_chunks(run) {
            let map = keymap::build(chunk)?;
            batches.push(Batch { keymap: map.text, steps: map.codes.iter().flat_map(|&code| tap(code)).collect() });
        }
        Ok(())
    };
    for (at, c) in text.char_indices() {
        if c == '\n' || c == '\t' {
            flush(&text[run_start..at], &mut batches)?;
            batches.push(named(if c == '\n' { "return" } else { "tab" })?);
            run_start = at + c.len_utf8();
        } else if c.is_control() {
            return Err(format!("cannot type control character U+{:04X}", c as u32));
        }
    }
    flush(&text[run_start..], &mut batches)?;
    Ok(batches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::keys::parse_combo;

    fn key(code: u16, down: bool) -> KeyStep {
        KeyStep::Key { code, down }
    }

    #[test]
    fn a_combo_holds_its_modifiers_around_one_key_tap() {
        let plain = combo_batch(&parse_combo("return").unwrap()).unwrap();
        assert_eq!(plain.steps, [key(28, true), key(28, false)]);
        let chord = combo_batch(&parse_combo("shift+cmd+t").unwrap()).unwrap();
        assert_eq!(
            chord.steps,
            [KeyStep::Modifiers(SHIFT | CONTROL), key(20, true), key(20, false), KeyStep::Modifiers(0)]
        );
        assert!(chord.keymap.starts_with("xkb_keymap"));
    }

    #[test]
    fn cmd_and_ctrl_are_one_control_and_fn_adds_nothing() {
        let mask = |combo: &str| modifier_mask(&parse_combo(combo).unwrap().modifiers);
        assert_eq!(mask("cmd+ctrl+a"), CONTROL);
        assert_eq!(mask("alt+shift+escape"), ALT | SHIFT);
        assert_eq!(mask("fn+a"), 0);
        assert_eq!(mask("a"), 0);
    }

    #[test]
    fn text_is_tapped_char_by_char_under_its_own_keymap() {
        let batches = text_batches("aba").unwrap();
        assert_eq!(batches.len(), 1);
        // Two distinct characters: codes 128 and 129, 'a' tapped twice.
        let codes: Vec<(u16, bool)> =
            batches[0].steps.iter().map(|s| match s { KeyStep::Key { code, down } => (*code, *down), _ => panic!() }).collect();
        assert_eq!(codes, [(128, true), (128, false), (129, true), (129, false), (128, true), (128, false)]);
        assert!(text_batches("").unwrap().is_empty());
    }

    #[test]
    fn newlines_and_tabs_are_return_and_tab_between_runs() {
        let batches = text_batches("ab\ncd\t").unwrap();
        assert_eq!(batches.len(), 4);
        assert_eq!(batches[1].steps, [key(28, true), key(28, false)]);
        assert_eq!(batches[3].steps, [key(15, true), key(15, false)]);
        assert_eq!(batches[0].steps.len(), 4);
        assert_eq!(text_batches("\n").unwrap().len(), 1);
    }

    #[test]
    fn other_control_characters_and_too_many_distinct_characters_are_handled() {
        assert!(text_batches("a\u{7}b").unwrap_err().contains("U+0007"));
        // More than one keymap's worth of distinct characters is split, not refused.
        let long: String = ('\u{4e00}'..).take(keymap::MAX_DISTINCT_CHARS + 5).collect();
        assert_eq!(text_batches(&long).unwrap().len(), 2);
    }
}
