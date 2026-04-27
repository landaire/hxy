//! Clipboard paste helpers for the hex editor.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseHexError {
    #[error("invalid hex digit {ch:?} at position {pos}")]
    InvalidDigit { ch: char, pos: usize },
    #[error("odd hex digit count ({count}); every byte needs two digits")]
    OddDigits { count: usize },
}

/// Parse clipboard text as a hex byte sequence. Tolerates whitespace,
/// commas, colons, and `0x` / `0X` prefixes so users can paste output
/// from hexdumps, `xxd -p`, C-literal arrays, and the like.
pub fn parse_hex_clipboard(input: &str) -> Result<Vec<u8>, ParseHexError> {
    let mut digits: Vec<(char, usize)> = Vec::with_capacity(input.len());
    let mut iter = input.char_indices().peekable();
    while let Some((pos, ch)) = iter.next() {
        if ch.is_ascii_whitespace() || matches!(ch, ',' | ':' | ';' | '\\') {
            continue;
        }
        if (ch == '0') && matches!(iter.peek(), Some((_, 'x' | 'X'))) {
            iter.next();
            continue;
        }
        if ch.is_ascii_hexdigit() {
            digits.push((ch, pos));
            continue;
        }
        return Err(ParseHexError::InvalidDigit { ch, pos });
    }
    if !digits.len().is_multiple_of(2) {
        return Err(ParseHexError::OddDigits { count: digits.len() });
    }
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks(2) {
        let hi = nibble(pair[0].0);
        let lo = nibble(pair[1].0);
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn nibble(ch: char) -> u8 {
    match ch {
        '0'..='9' => ch as u8 - b'0',
        'a'..='f' => ch as u8 - b'a' + 10,
        'A'..='F' => ch as u8 - b'A' + 10,
        _ => unreachable!("already validated as ASCII hex"),
    }
}

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("clipboard access: {0}")]
    Arboard(String),
    #[error("clipboard is empty")]
    Empty,
}

#[cfg(not(target_arch = "wasm32"))]
pub fn read_text() -> Result<String, ClipboardError> {
    let mut cb = arboard::Clipboard::new().map_err(|e| ClipboardError::Arboard(e.to_string()))?;
    let text = cb.get_text().map_err(|e| ClipboardError::Arboard(e.to_string()))?;
    if text.is_empty() { Err(ClipboardError::Empty) } else { Ok(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spaced_hex() {
        assert_eq!(parse_hex_clipboard("DE AD BE EF").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn parses_tight_hex() {
        assert_eq!(parse_hex_clipboard("deadbeef").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn tolerates_mixed_delimiters() {
        assert_eq!(parse_hex_clipboard("de,ad:be;ef").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn strips_c_array_prefixes() {
        assert_eq!(parse_hex_clipboard("0xDE, 0xAD, 0xBE, 0xEF").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert!(parse_hex_clipboard("").unwrap().is_empty());
        assert!(parse_hex_clipboard("   \n\t").unwrap().is_empty());
    }

    #[test]
    fn odd_digit_count_errors() {
        assert!(matches!(parse_hex_clipboard("abc"), Err(ParseHexError::OddDigits { count: 3 })));
    }

    #[test]
    fn non_hex_character_errors() {
        assert!(matches!(parse_hex_clipboard("de ad zz"), Err(ParseHexError::InvalidDigit { ch: 'z', .. })));
    }
}
