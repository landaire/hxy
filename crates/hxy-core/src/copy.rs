//! Clipboard-copy format identity and byte/scalar formatting shared
//! by the frontends. Menu layout stays app-side; the kind enum and
//! the pure formatters live here so framework-neutral code (template
//! panel copy actions) can produce clipboard text without depending
//! on a UI crate.

use std::fmt::Write;

/// Every format the app knows how to render a selection / field as.
/// The Value-prefixed variants only make sense for scalar nodes
/// (known integer width + signedness) -- the byte variants work on
/// any span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyKind {
    BytesLossyUtf8,
    BytesHexSpaced,
    BytesHexCompact,
    BytesDecimalCsv,
    BytesOctalCsv,
    BytesCArray,
    BytesRustArray,
    /// Standard base64 (RFC 4648) over the raw selection bytes.
    BytesBase64,
    /// Lossy UTF-8 decode of the selection, then base64 over that
    /// text. Non-UTF-8 bytes become U+FFFD before encoding, so the
    /// output round-trips cleanly through "base64 -d" as valid UTF-8.
    TextBase64,
    ValueHex,
    ValueDecimal,
    ValueOctal,
    /// Render a parsed struct node (from the template panel) as a
    /// Rust struct literal with inline `field: value` initialisers.
    /// Requires tree context, so it's handled outside the plain
    /// byte formatter (see the app's `format_template_struct`).
    StructRust,
    /// Same idea, but as a C99 designated initialiser block
    /// (`.field = value`).
    StructC,
}

impl CopyKind {
    pub fn is_value(self) -> bool {
        matches!(self, Self::ValueHex | Self::ValueDecimal | Self::ValueOctal)
    }

    /// True for the struct-literal variants -- those need the
    /// template tree and are handled outside the plain byte /
    /// scalar formatters.
    pub fn is_struct(self) -> bool {
        matches!(self, Self::StructRust | Self::StructC)
    }
}

/// Format `bytes` using `kind`. `ident_hint` becomes the variable
/// name for the C / Rust array templates; `type_hint` becomes a
/// trailing comment so the reader knows what the underlying field
/// was. Returns `None` for any Value-kind; use [`format_scalar`].
pub fn format_bytes(kind: CopyKind, bytes: &[u8], ident_hint: &str, type_hint: &str) -> Option<String> {
    match kind {
        CopyKind::BytesLossyUtf8 => Some(String::from_utf8_lossy(bytes).into_owned()),
        CopyKind::BytesHexSpaced => Some(bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ")),
        CopyKind::BytesHexCompact => Some(bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join("")),
        CopyKind::BytesDecimalCsv => Some(bytes.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(", ")),
        CopyKind::BytesOctalCsv => Some(bytes.iter().map(|b| format!("0o{b:o}")).collect::<Vec<_>>().join(", ")),
        CopyKind::BytesCArray => {
            let ident = sanitize_ident(ident_hint);
            let mut out = String::new();
            let _ = write!(out, "uint8_t {ident}[{}] = {{ ", bytes.len());
            for (i, b) in bytes.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "0x{b:02X}");
            }
            out.push_str(" }; /* ");
            out.push_str(type_hint);
            out.push_str(" */");
            Some(out)
        }
        CopyKind::BytesRustArray => {
            let ident = sanitize_ident(ident_hint);
            let mut out = String::new();
            let _ = write!(out, "let {ident}: [u8; {}] = [", bytes.len());
            for (i, b) in bytes.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "0x{b:02X}");
            }
            let _ = write!(out, "]; // {type_hint}");
            Some(out)
        }
        CopyKind::BytesBase64 => {
            use base64::Engine as _;
            Some(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        CopyKind::TextBase64 => {
            use base64::Engine as _;
            let text = String::from_utf8_lossy(bytes);
            Some(base64::engine::general_purpose::STANDARD.encode(text.as_bytes()))
        }
        _ => None,
    }
}

/// Format a scalar integer value (up to 64 bits, treated as u64 bit
/// pattern) using `kind`. Returns `None` for byte-kind entries.
pub fn format_scalar(kind: CopyKind, raw: u64) -> Option<String> {
    Some(match kind {
        CopyKind::ValueHex => format!("0x{raw:X}"),
        CopyKind::ValueDecimal => format!("{raw}"),
        CopyKind::ValueOctal => format!("0o{raw:o}"),
        _ => return None,
    })
}

/// Produce a valid C / Rust identifier from a freeform name. Non-
/// alphanumeric characters become `_`; a leading digit gets
/// `_`-prefixed; the empty string becomes `"data"`.
pub fn sanitize_ident(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for (i, c) in raw.chars().enumerate() {
        if i == 0 && c.is_ascii_digit() {
            out.push('_');
            out.push(c);
        } else if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() { "data".to_owned() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_spaced_roundtrip() {
        assert_eq!(
            format_bytes(CopyKind::BytesHexSpaced, &[0x50, 0x4B, 0x03, 0x04], "sel", "u8[4]").unwrap(),
            "50 4B 03 04"
        );
    }

    #[test]
    fn c_array_uses_sanitised_ident() {
        let out = format_bytes(CopyKind::BytesCArray, &[1, 2], "fr Crc", "uint").unwrap();
        assert_eq!(out, "uint8_t fr_Crc[2] = { 0x01, 0x02 }; /* uint */");
    }

    #[test]
    fn rust_array_leading_digit_guarded() {
        let out = format_bytes(CopyKind::BytesRustArray, &[0xFF], "3dModel", "u8").unwrap();
        assert_eq!(out, "let _3dModel: [u8; 1] = [0xFF]; // u8");
    }

    #[test]
    fn value_hex_decimal_octal_match_bit_pattern() {
        assert_eq!(format_scalar(CopyKind::ValueHex, 255).as_deref(), Some("0xFF"));
        assert_eq!(format_scalar(CopyKind::ValueDecimal, 255).as_deref(), Some("255"));
        assert_eq!(format_scalar(CopyKind::ValueOctal, 8).as_deref(), Some("0o10"));
    }

    #[test]
    fn empty_ident_falls_back_to_data() {
        assert_eq!(sanitize_ident(""), "data");
    }

    #[test]
    fn bytes_base64_encodes_raw() {
        assert_eq!(
            format_bytes(CopyKind::BytesBase64, b"Many hands make light work.", "sel", "u8[..]").unwrap(),
            "TWFueSBoYW5kcyBtYWtlIGxpZ2h0IHdvcmsu"
        );
    }

    #[test]
    fn bytes_base64_handles_padding() {
        assert_eq!(format_bytes(CopyKind::BytesBase64, b"f", "sel", "u8").unwrap(), "Zg==");
        assert_eq!(format_bytes(CopyKind::BytesBase64, b"fo", "sel", "u8[2]").unwrap(), "Zm8=");
        assert_eq!(format_bytes(CopyKind::BytesBase64, b"foo", "sel", "u8[3]").unwrap(), "Zm9v");
    }

    #[test]
    fn text_base64_matches_bytes_for_valid_utf8() {
        let input = b"hello";
        let as_bytes = format_bytes(CopyKind::BytesBase64, input, "sel", "u8[5]").unwrap();
        let as_text = format_bytes(CopyKind::TextBase64, input, "sel", "u8[5]").unwrap();
        assert_eq!(as_bytes, as_text);
    }

    #[test]
    fn text_base64_replaces_invalid_utf8_with_replacement_char() {
        // 0xFF is never a valid UTF-8 byte; the lossy decode substitutes
        // U+FFFD (three bytes 0xEF 0xBF 0xBD in UTF-8), then base64 encodes.
        let out = format_bytes(CopyKind::TextBase64, &[0xFF], "sel", "u8").unwrap();
        assert_eq!(out, "77+9");
    }
}
