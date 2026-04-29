//! User-visible application settings.

#[cfg(not(target_arch = "wasm32"))]
pub mod persist;

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

/// Base used to render a single numeric value (offset, length,
/// end position). The wider [`NumericFormat`] picks one of these
/// per call; this enum is just the leaf format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NumericBase {
    #[default]
    Hex,
    Decimal,
}

impl NumericBase {
    pub fn toggle(self) -> Self {
        match self {
            Self::Hex => Self::Decimal,
            Self::Decimal => Self::Hex,
        }
    }
}

/// How to format byte offsets / lengths / end positions across
/// the UI. `Always(b)` always uses `b`; `Threshold { ... }`
/// switches between `small` (when value < threshold) and `large`
/// (when value >= threshold), so the user can keep small numbers
/// readable in decimal while big addresses stay compact in hex.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NumericFormat {
    Always(NumericBase),
    Threshold {
        small: NumericBase,
        large: NumericBase,
        threshold: u64,
    },
}

impl Default for NumericFormat {
    fn default() -> Self {
        Self::Always(NumericBase::Hex)
    }
}

impl NumericFormat {
    /// Pick the base that applies to `value`. For `Threshold`,
    /// `large` kicks in at-or-above the threshold so a setting of
    /// `threshold = 256` reads as "show hex once we're past a byte's
    /// worth".
    pub fn pick(self, value: u64) -> NumericBase {
        match self {
            Self::Always(b) => b,
            Self::Threshold { small, large, threshold } => {
                if value >= threshold { large } else { small }
            }
        }
    }

    /// Quick toggle for the click-to-flip status-bar widget. For
    /// `Always(b)`, swap to the other base; for `Threshold`, swap
    /// the two bases (keeping the threshold intact) so the user
    /// gets the inverted view without losing their threshold pick.
    pub fn toggle(self) -> Self {
        match self {
            Self::Always(b) => Self::Always(b.toggle()),
            Self::Threshold { small, large, threshold } => {
                Self::Threshold { small: large, large: small, threshold }
            }
        }
    }
}

/// Type alias kept for the existing call sites that only care
/// about a single base (the status bar's hover tooltip, the
/// click-to-toggle helper). Lets us drop in [`NumericFormat`]
/// without churning every call to `OffsetBase::toggle()`.
pub type OffsetBase = NumericBase;

/// Per-integer-type formats for template scalar field values.
/// The shape is one [`NumericFormat`] per signed / unsigned
/// width so the user can keep, say, `u8` in hex (single bytes
/// often read as flags / magic) while having `u32` in decimal
/// (counters, lengths). Each slot still defers to a template's
/// explicit `[[hex]]` / `[[decimal]]` hint when set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateValueFormats {
    #[serde(default = "default_u8_format")]
    pub u8: NumericFormat,
    #[serde(default = "default_u16_format")]
    pub u16: NumericFormat,
    #[serde(default = "default_u32_format")]
    pub u32: NumericFormat,
    #[serde(default = "default_u64_format")]
    pub u64: NumericFormat,
    #[serde(default = "default_signed_format")]
    pub s8: NumericFormat,
    #[serde(default = "default_signed_format")]
    pub s16: NumericFormat,
    #[serde(default = "default_signed_format")]
    pub s32: NumericFormat,
    #[serde(default = "default_signed_format")]
    pub s64: NumericFormat,
}

impl Default for TemplateValueFormats {
    fn default() -> Self {
        Self {
            u8: default_u8_format(),
            u16: default_u16_format(),
            u32: default_u32_format(),
            u64: default_u64_format(),
            s8: default_signed_format(),
            s16: default_signed_format(),
            s32: default_signed_format(),
            s64: default_signed_format(),
        }
    }
}

/// Identity for the eight integer slots in
/// [`TemplateValueFormats`]. Used by the settings UI to walk the
/// slots in a single loop, and by the panel formatters to look up
/// the right slot per `Value::*Val` arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntValueType {
    U8,
    U16,
    U32,
    U64,
    S8,
    S16,
    S32,
    S64,
}

impl IntValueType {
    /// All eight variants in display order (unsigned widths
    /// first, then signed widths). The settings panel iterates
    /// this; the panel formatters use [`Self::format_for`].
    pub fn all() -> &'static [Self] {
        &[Self::U8, Self::U16, Self::U32, Self::U64, Self::S8, Self::S16, Self::S32, Self::S64]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::S8 => "s8",
            Self::S16 => "s16",
            Self::S32 => "s32",
            Self::S64 => "s64",
        }
    }
}

impl TemplateValueFormats {
    /// Borrow the [`NumericFormat`] for one int slot.
    pub fn slot(&self, ty: IntValueType) -> NumericFormat {
        match ty {
            IntValueType::U8 => self.u8,
            IntValueType::U16 => self.u16,
            IntValueType::U32 => self.u32,
            IntValueType::U64 => self.u64,
            IntValueType::S8 => self.s8,
            IntValueType::S16 => self.s16,
            IntValueType::S32 => self.s32,
            IntValueType::S64 => self.s64,
        }
    }

    /// Mutable handle to one slot, for the settings UI to bind
    /// directly into.
    pub fn slot_mut(&mut self, ty: IntValueType) -> &mut NumericFormat {
        match ty {
            IntValueType::U8 => &mut self.u8,
            IntValueType::U16 => &mut self.u16,
            IntValueType::U32 => &mut self.u32,
            IntValueType::U64 => &mut self.u64,
            IntValueType::S8 => &mut self.s8,
            IntValueType::S16 => &mut self.s16,
            IntValueType::S32 => &mut self.s32,
            IntValueType::S64 => &mut self.s64,
        }
    }
}

fn default_u8_format() -> NumericFormat {
    // Single bytes are usually flags or magic ints -- hex reads
    // better than decimal at that scale.
    NumericFormat::Always(NumericBase::Hex)
}
fn default_u16_format() -> NumericFormat {
    // 16-bit unsigned often packs flags too.
    NumericFormat::Always(NumericBase::Hex)
}
fn default_u32_format() -> NumericFormat {
    // u32 is typically a length / counter -- decimal stays
    // readable even for medium values.
    NumericFormat::Always(NumericBase::Decimal)
}
fn default_u64_format() -> NumericFormat {
    NumericFormat::Always(NumericBase::Decimal)
}
fn default_signed_format() -> NumericFormat {
    // Signed values almost always read as decimal; hex of a
    // negative bit pattern is a developer-debug view, not a
    // default.
    NumericFormat::Always(NumericBase::Decimal)
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

    /// Base used by the status bar for displaying offsets. Kept
    /// as a single base (without the threshold form) because the
    /// status-bar's click-to-toggle UX wants a binary flip; the
    /// richer template-panel / tooltip / palette path uses
    /// [`Self::numeric_format`] instead.
    pub offset_base: OffsetBase,

    /// Format used everywhere a byte offset / length / end
    /// position is rendered as text outside the status bar
    /// (template panel, hover tooltip, palette previews).
    /// Supports a fixed base or a value-threshold split so small
    /// numbers stay readable in decimal while large addresses
    /// stay compact in hex.
    #[serde(default)]
    pub numeric_format: NumericFormat,

    /// Per-integer-type formats used everywhere a template
    /// scalar field's value is rendered (Value column in the
    /// template panel, the breadcrumb tooltip, the visualizer
    /// table). Templates that explicitly set a `[[hex]]` /
    /// `[[decimal]]` display hint on a field still win over
    /// these per-type defaults; the user setting only governs
    /// fields without an explicit hint.
    #[serde(default)]
    pub template_value_formats: TemplateValueFormats,

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

    /// What to do when the watcher reports an open file changed on
    /// disk. Per-path overrides on `file_watch_prefs` win when set.
    #[serde(default)]
    pub auto_reload: AutoReloadMode,

    /// Per-path overrides for [`Self::auto_reload`]. Stored as a
    /// flat Vec rather than a [`HashMap`] / [`BTreeMap`] so the
    /// JSON form round-trips through serde without a custom map
    /// key serializer. Lookups are linear; the list is bounded by
    /// how many files the user has individually opted in / out of.
    #[serde(default)]
    pub file_watch_prefs: Vec<FileWatchPref>,

    /// Polling cadence used both for paths the kernel watcher
    /// rejected and (when `file_poll_all` is set) every watched
    /// path. `0` disables the polling worker entirely. Stored as
    /// milliseconds for human-readable JSON.
    #[serde(default = "default_poll_interval_ms")]
    pub file_poll_interval_ms: u32,

    /// When true, every watched path is polled even when the
    /// notify watcher accepted it. Off by default because the
    /// kernel events are usually enough; users on flaky
    /// filesystems (network drives, FUSE) flip this on.
    #[serde(default)]
    pub file_poll_all: bool,

    /// Upper bound on the shared hex-view byte cache, expressed in
    /// MiB. Default 500 MiB, minimum 20 MiB; no upper cap. The cache
    /// is consulted by every hex view and template run, so a larger
    /// budget trades RAM for fewer disk reads on big files.
    #[serde(default = "default_byte_cache_limit_mib")]
    pub byte_cache_limit_mib: u32,
}

/// What to do when a file is being watched for external
/// changes. Defaults to `Ask` so the user notices the
/// divergence and chooses; `Always` auto-reloads silently;
/// `Never` skips watcher enrollment entirely (no notify
/// registration, no polling cost) -- distinct from "watch but
/// always ignore", which would still pay the per-tick hashing
/// work for VFS entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoReloadMode {
    Always,
    #[default]
    Ask,
    Never,
}

impl AutoReloadMode {
    pub const ALL: [Self; 3] = [Self::Always, Self::Ask, Self::Never];

    /// Human-readable label key for the settings dropdown / per-
    /// file override picker. Resolved via `hxy_i18n::t` at the
    /// call site so a locale change picks up fresh translations.
    pub fn label_key(self) -> &'static str {
        match self {
            Self::Always => "auto-reload-always",
            Self::Ask => "auto-reload-ask",
            Self::Never => "auto-reload-never",
        }
    }
}

/// Per-file override for [`AppSettings::auto_reload`]. Saved
/// alongside the global setting so the user's "always reload
/// this file" / "never bother me about this file" decisions
/// survive restarts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileWatchPref {
    pub path: PathBuf,
    pub auto_reload: AutoReloadMode,
}

fn default_poll_interval_ms() -> u32 {
    2000
}

fn default_byte_cache_limit_mib() -> u32 {
    hxy_core::CacheLimit::DEFAULT_MIB
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
            numeric_format: NumericFormat::default(),
            template_value_formats: TemplateValueFormats::default(),
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
            auto_reload: AutoReloadMode::default(),
            file_watch_prefs: Vec::new(),
            file_poll_interval_ms: default_poll_interval_ms(),
            file_poll_all: false,
            byte_cache_limit_mib: default_byte_cache_limit_mib(),
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

    /// Auto-reload mode for a specific path: per-file override
    /// when set, otherwise [`Self::auto_reload`].
    pub fn auto_reload_for(&self, path: &std::path::Path) -> AutoReloadMode {
        self.file_watch_prefs.iter().find(|p| p.path == path).map(|p| p.auto_reload).unwrap_or(self.auto_reload)
    }

    /// Set the per-file auto-reload override for `path`. Passing
    /// `None` removes any override so the global setting takes
    /// over again.
    pub fn set_auto_reload_for(&mut self, path: PathBuf, mode: Option<AutoReloadMode>) {
        self.file_watch_prefs.retain(|p| p.path != path);
        if let Some(mode) = mode {
            self.file_watch_prefs.push(FileWatchPref { path, auto_reload: mode });
        }
    }
}
