//! Clipboard-copy format helpers shared between the hex view
//! context menu, the Edit menu, and the template panel's per-row
//! context menu. Keeps formatting logic and menu layout in one
//! place so new kinds (e.g. Zig array, JSON array) only need
//! editing here.

#![cfg(not(target_arch = "wasm32"))]

use std::fmt::Write;

/// Every format the app knows how to render a selection / field as.
/// The Value-prefixed variants only make sense for scalar nodes
/// (known integer width + signedness) — the byte variants work on
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
    ValueHex,
    ValueDecimal,
    ValueOctal,
}

impl CopyKind {
    pub fn is_value(self) -> bool {
        matches!(self, Self::ValueHex | Self::ValueDecimal | Self::ValueOctal)
    }
}

/// Menu items for the bytes submenu (available on any selection).
pub const BYTES_MENU: &[(&str, CopyKind)] = &[
    ("Bytes (UTF-8)", CopyKind::BytesLossyUtf8),
    ("Hex (spaced)", CopyKind::BytesHexSpaced),
    ("Hex (compact)", CopyKind::BytesHexCompact),
    ("Decimal (CSV)", CopyKind::BytesDecimalCsv),
    ("Octal (CSV)", CopyKind::BytesOctalCsv),
    ("C array", CopyKind::BytesCArray),
    ("Rust array", CopyKind::BytesRustArray),
];

/// Menu items for the scalar-value submenu (only visible for
/// nodes whose decoded value is a single integer).
pub const VALUE_MENU: &[(&str, CopyKind)] = &[
    ("Hex", CopyKind::ValueHex),
    ("Decimal", CopyKind::ValueDecimal),
    ("Octal", CopyKind::ValueOctal),
];

/// Format `bytes` using `kind`. `ident_hint` becomes the variable
/// name for the C / Rust array templates; `type_hint` becomes a
/// trailing comment so the reader knows what the underlying field
/// was. Returns `None` for any Value-kind; use [`format_scalar`].
pub fn format_bytes(kind: CopyKind, bytes: &[u8], ident_hint: &str, type_hint: &str) -> Option<String> {
    match kind {
        CopyKind::BytesLossyUtf8 => Some(String::from_utf8_lossy(bytes).into_owned()),
        CopyKind::BytesHexSpaced => {
            Some(bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" "))
        }
        CopyKind::BytesHexCompact => {
            Some(bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(""))
        }
        CopyKind::BytesDecimalCsv => {
            Some(bytes.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(", "))
        }
        CopyKind::BytesOctalCsv => {
            Some(bytes.iter().map(|b| format!("0o{b:o}")).collect::<Vec<_>>().join(", "))
        }
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

/// Render both submenus under a shared "Copy as" section. Returns
/// the kind the user picked this frame, or `None`. When
/// `show_value_submenu` is false (no scalar context), only the
/// bytes submenu appears.
pub fn copy_as_menu(ui: &mut egui::Ui, show_value_submenu: bool) -> Option<CopyKind> {
    let mut picked: Option<CopyKind> = None;
    ui.menu_button("Copy bytes as", |ui| {
        for (label, kind) in BYTES_MENU {
            if ui.button(*label).clicked() {
                picked = Some(*kind);
                ui.close();
            }
        }
    });
    if show_value_submenu {
        ui.menu_button("Copy value as", |ui| {
            for (label, kind) in VALUE_MENU {
                if ui.button(*label).clicked() {
                    picked = Some(*kind);
                    ui.close();
                }
            }
        });
    }
    picked
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
}
