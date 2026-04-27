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

/// Color scheme used when byte highlighting is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ByteHighlightScheme {
    /// Group bytes into coarse classes (null, whitespace, printable, ...).
    #[default]
    Class,
    /// Give every byte value 0x00..0xFF its own color.
    Value,
}

/// Wall-clock budget the compare worker is allowed to spend on a
/// single Myers diff. Past this, [`similar`] falls back to an
/// approximation -- still a valid diff, just less granular. Stored
/// as milliseconds so the persisted form is human-readable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecomputeDeadline {
    ms: u32,
}

impl RecomputeDeadline {
    pub const MIN_MS: u32 = 100;
    pub const MAX_MS: u32 = 60_000;
    pub const DEFAULT: Self = Self { ms: 2000 };

    pub fn from_ms(ms: u32) -> Self {
        Self { ms: ms.clamp(Self::MIN_MS, Self::MAX_MS) }
    }

    pub fn as_ms(self) -> u32 {
        self.ms
    }

    pub fn as_duration(self) -> std::time::Duration {
        std::time::Duration::from_millis(self.ms as u64)
    }
}

impl Default for RecomputeDeadline {
    fn default() -> Self {
        Self::DEFAULT
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

    /// Whether the app should check for updates on launch (placeholder --
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

    /// Which color scheme the highlight uses.
    pub byte_highlight_scheme: ByteHighlightScheme,

    /// Show a minimap strip beside the hex view.
    pub show_minimap: bool,

    /// When the minimap is shown, paint it with the highlight palette.
    /// Off falls back to a plain grayscale gradient that's less busy.
    pub minimap_colored: bool,

    /// Files the user has opened recently, newest-first, capped at
    /// [`MAX_RECENT_FILES`]. Surfaced on the welcome screen.
    #[serde(default)]
    pub recent_files: Vec<RecentFile>,

    /// When `true` (default), pressing Escape inside a palette
    /// sub-mode (Templates, GoToOffset, SelectRange, ...) pops back
    /// to `Mode::Main` instead of closing the palette outright.
    /// Escape from `Main` always closes. Backdrop clicks always
    /// close regardless of this setting -- they're an explicit
    /// "dismiss the whole thing" gesture. Set to `false` to restore
    /// the simpler one-press-closes behaviour.
    #[serde(default = "default_palette_escape_pops_to_parent")]
    pub palette_escape_pops_to_parent: bool,

    /// When `true`, the address column inserts
    /// [`Self::address_separator_char`] between every group of 4 hex
    /// digits (counting from the right) so long offsets stay readable
    /// at a glance, e.g. `0000_0080`.
    #[serde(default)]
    pub address_separator_enabled: bool,

    /// Character used between hex-digit groups in the address column
    /// when [`Self::address_separator_enabled`] is on. Defaults to
    /// `_`, the digit separator used by Rust / Python / Java numeric
    /// literals; other common picks are `'` (C++), `:` (010 Editor),
    /// or ` `.
    #[serde(default = "default_address_separator_char")]
    pub address_separator_char: char,

    /// Top-level input style. `Default` is the standard arrow-key /
    /// type-to-edit dispatcher; `Vim` enables modal editing with
    /// `hjkl`, count prefixes, visual / insert modes, etc. New
    /// `OpenFile`s pick this up at construction; existing tabs are
    /// updated when the user toggles via the palette.
    #[serde(default)]
    pub input_mode: hxy_view::InputMode,

    /// Default upper bound on how long a compare-tab Myers diff may
    /// run before [`similar`] falls back to its approximation.
    /// Per-tab overrides on
    /// [`crate::compare::CompareSession::recompute_deadline_override`]
    /// take precedence when set.
    #[serde(default)]
    pub compare_recompute_deadline: RecomputeDeadline,

    /// State of the upstream `WerWolv/ImHex-Patterns` corpus on disk.
    /// Tracks the SHA-256 of the master tarball we last extracted so
    /// the periodic-update check can detect drift, plus a
    /// "user declined the prompt; don't ask again" flag for the
    /// first-launch dialog.
    #[serde(default)]
    pub imhex_patterns: ImhexPatternsState,
}

/// Persisted state for the bundled ImHex-Patterns corpus.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImhexPatternsState {
    /// SHA-256 of the master.zip we last extracted, hex-encoded. None
    /// when the corpus hasn't been downloaded yet.
    pub installed_hash: Option<String>,
    /// When the user has explicitly declined the first-launch prompt,
    /// don't show it again. They can still trigger a download from
    /// settings.
    pub declined_prompt: bool,
    /// When the last update check ran (for periodic update prompts).
    pub last_check: Option<jiff::Timestamp>,
}

fn default_palette_escape_pops_to_parent() -> bool {
    true
}

fn default_address_separator_char() -> char {
    '_'
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
            minimap_colored: true,
            recent_files: Vec::new(),
            palette_escape_pops_to_parent: true,
            address_separator_enabled: false,
            address_separator_char: default_address_separator_char(),
            input_mode: hxy_view::InputMode::default(),
            compare_recompute_deadline: RecomputeDeadline::default(),
            imhex_patterns: ImhexPatternsState::default(),
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
