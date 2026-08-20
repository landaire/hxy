//! Clipboard-copy menu layout shared between the hex view context
//! menu, the Edit menu, and the template panel's per-row context
//! menu. The pure formatters live in `hxy_core::copy` (shared with
//! the gpui frontend); this module keeps the egui menu rendering.

pub use hxy_core::copy::CopyKind;
pub use hxy_core::copy::format_bytes;
pub use hxy_core::copy::format_scalar;
pub use hxy_core::copy::sanitize_ident;

/// Menu items for the bytes submenu (available on any selection).
pub const BYTES_MENU: &[(&str, CopyKind)] = &[
    ("String (UTF-8)", CopyKind::BytesLossyUtf8),
    ("Hex (spaced)", CopyKind::BytesHexSpaced),
    ("Hex (compact)", CopyKind::BytesHexCompact),
    ("Decimal (CSV)", CopyKind::BytesDecimalCsv),
    ("Octal (CSV)", CopyKind::BytesOctalCsv),
    ("C array", CopyKind::BytesCArray),
    ("Rust array", CopyKind::BytesRustArray),
    ("Base64 (bytes)", CopyKind::BytesBase64),
    ("Base64 (UTF-8 text)", CopyKind::TextBase64),
];

/// Menu items for the scalar-value submenu (only visible for
/// nodes whose decoded value is a single integer).
pub const VALUE_MENU: &[(&str, CopyKind)] =
    &[("Hex", CopyKind::ValueHex), ("Decimal", CopyKind::ValueDecimal), ("Octal", CopyKind::ValueOctal)];

/// Render both submenus under a shared "Copy as" section. Returns
/// the kind the user picked this frame, or `None`. When
/// `show_value_submenu` is false (no scalar context), only the
/// bytes submenu appears.
pub fn copy_as_menu(ui: &mut egui::Ui, show_value_submenu: bool) -> Option<CopyKind> {
    copy_as_menu_full(ui, show_value_submenu, false)
}

/// Same as [`copy_as_menu`] with an additional flag that enables
/// the `Copy as struct` submenu. Only meaningful for nodes whose
/// children describe the struct -- the template panel's row
/// context menu is the one place that knows.
pub fn copy_as_menu_full(ui: &mut egui::Ui, show_value_submenu: bool, show_struct_submenu: bool) -> Option<CopyKind> {
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
    if show_struct_submenu {
        ui.menu_button("Copy struct as", |ui| {
            if ui.button("Rust struct literal").clicked() {
                picked = Some(CopyKind::StructRust);
                ui.close();
            }
            if ui.button("C designated initialiser").clicked() {
                picked = Some(CopyKind::StructC);
                ui.close();
            }
        });
    }
    picked
}
