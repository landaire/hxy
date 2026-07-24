//! Framework-neutral input events. UI adapters translate their
//! native key/text events into these; unmapped keys never reach the
//! editor and stay with the host UI.

/// Modifier state accompanying a key press. `command` is the
/// platform primary modifier (cmd on macOS, ctrl elsewhere), matching
/// the egui convention the editor logic was written against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub command: bool,
    pub alt: bool,
}

/// Keys the editor reacts to. `Letter` carries the lowercase ASCII
/// letter ('a'..='z'); shift state travels in [`Modifiers`]. `Digit`
/// carries 0..=9. Anything not representable here is not translated
/// by adapters and therefore never consumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Escape,
    Tab,
    Backspace,
    Digit(u8),
    Letter(char),
}

impl Key {
    /// Hex-digit value for nibble typing: 0-9 and a-f.
    pub fn hex_nibble(self) -> Option<u8> {
        match self {
            Key::Digit(d) if d <= 9 => Some(d),
            Key::Letter(c @ 'a'..='f') => Some(c as u8 - b'a' + 10),
            _ => None,
        }
    }
}

/// One input event fed to [`crate::InputFilter::feed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Key { key: Key, modifiers: Modifiers },
    Text(String),
}

/// Side effect requested by input application. The UI adapter
/// executes these with its native facilities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Write text to the system clipboard (vim yank/delete).
    CopyText(String),
}

/// Whether the editor consumed an event. `Passed` events stay with
/// the host UI (e.g. remain in egui's queue for other widgets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Consumed,
    Passed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_nibble_from_key() {
        assert_eq!(Key::Digit(0).hex_nibble(), Some(0));
        assert_eq!(Key::Digit(9).hex_nibble(), Some(9));
        assert_eq!(Key::Letter('a').hex_nibble(), Some(0xA));
        assert_eq!(Key::Letter('f').hex_nibble(), Some(0xF));
        assert_eq!(Key::Letter('g').hex_nibble(), None);
        assert_eq!(Key::Escape.hex_nibble(), None);
    }
}
