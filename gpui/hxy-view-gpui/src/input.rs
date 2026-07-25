//! GPUI adapter: translates a gpui [`Keystroke`] into hxy-editor's
//! neutral [`InputEvent`]s. The semantic reference is the egui adapter
//! (`hxy-view/src/input.rs`); this produces the same [`Key`] / [`Text`]
//! events for the equivalent physical keys so both frontends drive the
//! shared dispatch identically.

use gpui::Keystroke;
use hxy_editor::InputEvent;
use hxy_editor::Key;
use hxy_editor::Modifiers;

/// Map a gpui `Keystroke` `key` string to the editor's [`Key`]. Named
/// keys arrive lowercased ("left", "escape", "tab", ...); printable
/// keys are the single layout-lowercase char ("a", "4"). `$` is handled
/// by the caller so it can force the shift flag.
fn translate_key(key: &str) -> Option<Key> {
    Some(match key {
        "left" => Key::ArrowLeft,
        "right" => Key::ArrowRight,
        "up" => Key::ArrowUp,
        "down" => Key::ArrowDown,
        "escape" => Key::Escape,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        _ => {
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return None;
            };
            match c {
                '0'..='9' => Key::Digit(c as u8 - b'0'),
                'a'..='z' => Key::Letter(c),
                _ => return None,
            }
        }
    })
}

/// gpui's `platform` modifier is the OS primary (cmd on macOS, super on
/// Linux). `Modifiers::secondary` folds that with control the way egui's
/// `command` does (cmd on macOS, ctrl elsewhere), giving the editor the
/// same "primary modifier held" signal on every platform.
fn translate_modifiers(m: &gpui::Modifiers) -> Modifiers {
    Modifiers { shift: m.shift, command: m.secondary(), alt: m.alt }
}

/// Translate one gpui keystroke into the editor's neutral events.
/// Returns (key event, optional text event) -- the text event
/// reproduces egui's separate Event::Text so ascii typing and vim
/// char-resolution behave identically.
pub(crate) fn translate(keystroke: &Keystroke) -> (Option<InputEvent>, Option<InputEvent>) {
    let modifiers = translate_modifiers(&keystroke.modifiers);

    let key_event = match keystroke.key.as_str() {
        // `$` is vim's LineEnd motion, keyed off `Digit(4) + shift` in
        // the dispatch table. gpui 0.2.2's simulated IME reports Shift+4
        // as key "4" with shift set (Keystroke::parse + with_simulated_ime),
        // which the digit arm below already handles; a physical macOS
        // layout can instead deliver the folded key "$" with no shift.
        // Force shift here so LineEnd fires in that case too.
        "$" => Some(InputEvent::Key { key: Key::Digit(4), modifiers: Modifiers { shift: true, ..modifiers } }),
        key => translate_key(key).map(|key| InputEvent::Key { key, modifiers }),
    };

    // gpui folds the typed character into `key_char`; egui delivers it as
    // a separate Event::Text. Re-synthesize that Text event, gated to
    // ascii-graphic-or-space just as egui only emits Text for printable
    // input (so "\t" / "\n" from tab / enter never reach the editor as
    // text and those keys stay with the host UI).
    let text_event = keystroke.key_char.as_deref().and_then(|s| {
        let first = s.chars().next()?;
        (first.is_ascii_graphic() || first == ' ').then(|| InputEvent::Text(s.to_owned()))
    });

    (key_event, text_event)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ks(key: &str, key_char: Option<&str>, modifiers: gpui::Modifiers) -> Keystroke {
        Keystroke { modifiers, key: key.to_owned(), key_char: key_char.map(str::to_owned) }
    }

    #[test]
    fn letter_yields_key_and_text() {
        let (key, text) = translate(&ks("a", Some("a"), gpui::Modifiers::default()));
        assert_eq!(key, Some(InputEvent::Key { key: Key::Letter('a'), modifiers: Modifiers::default() }));
        assert_eq!(text, Some(InputEvent::Text("a".to_owned())));
    }

    #[test]
    fn named_keys_map_to_arrows_and_escape() {
        assert_eq!(
            translate(&ks("left", None, gpui::Modifiers::default())).0,
            Some(InputEvent::Key { key: Key::ArrowLeft, modifiers: Modifiers::default() })
        );
        assert_eq!(
            translate(&ks("escape", None, gpui::Modifiers::default())).0,
            Some(InputEvent::Key { key: Key::Escape, modifiers: Modifiers::default() })
        );
    }

    #[test]
    fn shift_four_and_dollar_both_reach_line_end() {
        let shifted = translate(&ks("4", Some("4"), gpui::Modifiers::shift())).0;
        assert_eq!(
            shifted,
            Some(InputEvent::Key { key: Key::Digit(4), modifiers: Modifiers { shift: true, ..Default::default() } })
        );

        let dollar = translate(&ks("$", None, gpui::Modifiers::default())).0;
        assert_eq!(
            dollar,
            Some(InputEvent::Key { key: Key::Digit(4), modifiers: Modifiers { shift: true, ..Default::default() } })
        );
    }

    #[test]
    fn control_characters_produce_no_text_event() {
        assert_eq!(translate(&ks("enter", Some("\n"), gpui::Modifiers::default())).1, None);
        assert_eq!(translate(&ks("tab", Some("\t"), gpui::Modifiers::default())).1, None);
    }

    #[test]
    fn unmapped_key_is_not_translated() {
        assert_eq!(translate(&ks("f1", None, gpui::Modifiers::default())).0, None);
    }
}
