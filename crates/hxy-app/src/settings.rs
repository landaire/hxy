//! User-visible application settings.

use std::path::PathBuf;

use hxy_core::ColumnCount;
use serde::Deserialize;
use serde::Serialize;

/// An entry in the recent-files list shown on the welcome screen.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecentFile {
    pub path: PathBuf,
    #[serde(default = "default_ts")]
    pub last_opened: jiff::Timestamp,
}

fn default_ts() -> jiff::Timestamp {
    jiff::Timestamp::UNIX_EPOCH
}

/// How many recents to retain.
pub const MAX_RECENT_FILES: usize = 20;

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

/// Where to paint byte highlight color: as a background fill or as a
/// tint on the glyphs themselves.
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

/// Colour scheme used when byte highlighting is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ByteHighlightScheme {
    /// Group bytes into coarse classes (null, whitespace, printable, …).
    #[default]
    Class,
    /// Give every byte value 0x00..0xFF its own colour.
    Value,
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

    /// Which colour scheme the highlight uses.
    pub byte_highlight_scheme: ByteHighlightScheme,

    /// Show a minimap strip beside the hex view.
    pub show_minimap: bool,

    /// Files the user has opened recently, newest-first, capped at
    /// [`MAX_RECENT_FILES`]. Surfaced on the welcome screen.
    #[serde(default)]
    pub recent_files: Vec<RecentFile>,
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
            byte_highlight_scheme: ByteHighlightScheme::default(),
            show_minimap: true,
            recent_files: Vec::new(),
        }
    }
}

impl AppSettings {
    /// Push a newly-opened path to the top of the recent-files list,
    /// deduplicating and capping at [`MAX_RECENT_FILES`].
    pub fn record_recent(&mut self, path: PathBuf) {
        self.recent_files.retain(|r| r.path != path);
        self.recent_files.insert(0, RecentFile { path, last_opened: jiff::Timestamp::now() });
        self.recent_files.truncate(MAX_RECENT_FILES);
    }
}
