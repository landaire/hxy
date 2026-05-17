//! Clipboard paste helpers for the hex editor.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseHexError {
    #[error("odd hex digit count ({count}); every byte needs two digits")]
    OddDigits { count: usize },
}

/// Parse clipboard text as a hex byte sequence. Permissive: any
/// character that isn't a hex digit is silently skipped, so pasted
/// hexdumps, `xxd -p` output, C-literal arrays, log lines, and other
/// mixed-format snippets all parse without the user scrubbing them
/// first. The `0x` / `0X` prefix is recognised explicitly so a
/// literal like `0xff` stays a single byte instead of decomposing
/// into the digits `0`, `f`, `f`.
pub fn parse_hex_clipboard(input: &str) -> Result<Vec<u8>, ParseHexError> {
    let mut digits: Vec<char> = Vec::with_capacity(input.len());
    let mut iter = input.chars().peekable();
    while let Some(ch) = iter.next() {
        if ch == '0' && matches!(iter.peek(), Some('x' | 'X')) {
            iter.next();
            continue;
        }
        if ch.is_ascii_hexdigit() {
            digits.push(ch);
        }
    }
    if !digits.len().is_multiple_of(2) {
        return Err(ParseHexError::OddDigits { count: digits.len() });
    }
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks(2) {
        let hi = nibble(pair[0]);
        let lo = nibble(pair[1]);
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
    fn skips_non_hex_garbage() {
        // Punctuation, brackets, and stray non-hex letters around
        // hex digits get dropped so users can paste hexdumps,
        // Python repr output, and partial log lines without
        // scrubbing them by hand.
        assert_eq!(parse_hex_clipboard("de ad zz be ef").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(parse_hex_clipboard("[0xde, 0xad, 0xbe, 0xef]").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn full_integer_hex_literals_parse() {
        // `0xffaa` keeps its `0x` prefix stripped intact (so it
        // stays a single multi-byte literal, not the digits 0/f/f
        // followed by `xaa`). Multi-byte literals are the main
        // motivation for the permissive parser.
        assert_eq!(parse_hex_clipboard("0xffaa, 0x1234").unwrap(), vec![0xFF, 0xAA, 0x12, 0x34]);
        assert_eq!(parse_hex_clipboard("0xDEADBEEF").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }
}
