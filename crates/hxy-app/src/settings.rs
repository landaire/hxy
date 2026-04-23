//! User-visible application settings.

use hxy_core::ColumnCount;
use serde::Deserialize;
use serde::Serialize;

/// Base used by the status bar to render offsets. User can flip this by
/// clicking on the status bar values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OffsetBase {
    #[default]
    Hex,
    Decimal,
}

impl OffsetBase {
    pub fn toggle(self) -> Self {
        match self {
            Self::Hex => Self::Decimal,
            Self::Decimal => Self::Hex,
        }
    }
}

/// Where to paint byte-class color: as a background fill or as a tint on
/// the glyphs themselves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ByteHighlightMode {
    #[default]
    Background,
    Text,
}

impl ByteHighlightMode {
    pub fn as_view(self) -> hxy_view::ValueHighlight {
        match self {
            Self::Background => hxy_view::ValueHighlight::Background,
            Self::Text => hxy_view::ValueHighlight::Text,
        }
    }
}

/// General application preferences that are safe to persist across sessions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// User's preferred language. `None` means follow the system locale at
    /// startup and don't override it here.
    pub language: Option<String>,

    /// egui zoom factor. 1.0 = native, 1.2 matches `lantia-locator`'s default.
    pub zoom_factor: f32,

    /// Number of hex columns per row in the hex view.
    pub hex_columns: ColumnCount,

    /// Whether the app should check for updates on launch (placeholder —
    /// wired up when we actually implement update checks).
    pub check_for_updates: bool,

    /// Base used by the status bar for displaying offsets.
    pub offset_base: OffsetBase,

    /// When true, render a tint on each byte based on its value class
    /// (null, all-bits, printable ASCII, whitespace, control, extended)
    /// so common patterns are visible at a glance.
    pub byte_value_highlight: bool,

    /// Whether value-class highlighting is painted as a background fill
    /// or as a tint on the glyphs.
    pub byte_highlight_mode: ByteHighlightMode,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            language: None,
            zoom_factor: 1.2,
            hex_columns: ColumnCount::DEFAULT,
            check_for_updates: true,
            offset_base: OffsetBase::default(),
            byte_value_highlight: true,
            byte_highlight_mode: ByteHighlightMode::default(),
        }
    }
}
