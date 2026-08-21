//! Application settings: startup load, the [`SettingsGlobal`] every
//! consumer reads, and the [`update_settings`] mutation path that
//! persists to the shared SQLite store and notifies observers.
//!
//! The gpui app shares `hxy.db` (the `app_settings` key) with the
//! egui app, so a preference changed in one frontend is what the
//! other loads next boot. Unlike egui's per-frame dirty poll, every
//! mutation goes through [`update_settings`], which saves
//! synchronously (SQLite WAL key/value writes are sub-millisecond)
//! and republishes the global so `observe_global` subscribers fire.
//!
//! Fields persisted but not applied by the gpui shell (deviations
//! from egui, each also noted at its natural wiring site):
//! - `zoom_factor`: no clean global scale knob -- gpui-component's
//!   theme font sizes are reset by system-appearance sync and rem
//!   scaling would miss the mono hex grid.
//! - `check_for_updates`: no update checker exists in either
//!   frontend yet (the egui setting is a placeholder too).
//! - `language`: dead in egui as well (no UI, never applied).
//! - `byte_cache_limit_mib`: gpui file opens read whole files into
//!   `MemorySource`; nothing here constructs an `hxy_core::ByteCache`.
//! - `imhex_patterns`: fetch state is tracked by the palette flow
//!   directly (M4a); the shared blob round-trips untouched.
//!
//! Known live-apply deviations:
//! - a global `hex_columns` change applies to every open pane,
//!   clobbering a palette-set per-pane column count (egui keeps a
//!   per-tab `hex_columns_override` that survives; the gpui pane has
//!   no override slot yet).
//! - workspace-host nested file panes pick settings up at
//!   construction only; live changes reach top-level and compare
//!   panes (matching the nested docks' M3 scope).

use std::sync::Arc;

use gpui::App;
use gpui::Global;
use gpui::Hsla;
use hxy_core::byte_palette::BytePalette;
use hxy_core::byte_palette::ValueGradient;
use hxy_core::byte_palette::ValueHighlight;
use hxy_core::format::NumericFormat;
use hxy_core::format::TemplateValueFormats;
pub use hxy_settings::AppSettings;
use hxy_settings::ByteHighlightMode;
use hxy_settings::ByteHighlightScheme;
use hxy_settings::persist::SaveSink;
use hxy_view_gpui::PaneHighlight;
use sqlx::SqlitePool;
use tokio::runtime::Runtime;

use crate::templates::rgba_to_hsla;

/// The live [`AppSettings`], readable anywhere via
/// [`settings`] and observable via `cx.observe_global::<SettingsGlobal>`.
pub struct SettingsGlobal(pub AppSettings);

impl Global for SettingsGlobal {}

/// The persistence handle. `None` when the settings database failed
/// to open at startup: the app runs on defaults and mutations apply
/// in-memory only (mirrors egui's sink-less fallback).
pub struct SettingsSink(pub Option<SaveSink>);

impl Global for SettingsSink {}

/// The shared SQLite pool and its tokio runtime, cloned wherever a
/// subsystem needs the database directly. The settings [`SaveSink`]
/// holds its own clones; this handle lets the plugin layer (grants +
/// per-plugin state) reuse the SAME pool and runtime rather than
/// opening a second connection to `hxy.db`.
#[derive(Clone)]
pub struct PersistHandle {
    pub pool: SqlitePool,
    pub runtime: Arc<Runtime>,
}

/// Global wrapper for the shared [`PersistHandle`]. `None` when the
/// settings database failed to open at startup: dependent subsystems
/// (plugins) then run without persistence, degrading like the sink.
pub struct PersistHandleGlobal(pub Option<PersistHandle>);

impl Global for PersistHandleGlobal {}

/// How the startup load degraded, if it did. Drives which warning
/// toast the shell shows once the notification layer exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsLoadFailure {
    /// The database (or the runtime it needs) could not be opened:
    /// no sink, nothing saves this session.
    StoreUnavailable,
    /// The database opened but the stored blob would not decode: the
    /// sink is live, so the next save overwrites the stored settings.
    Unreadable,
}

impl SettingsLoadFailure {
    pub fn toast_key(self) -> &'static str {
        match self {
            Self::StoreUnavailable => "gpui-settings-store-unavailable",
            Self::Unreadable => "gpui-settings-load-unreadable",
        }
    }
}

/// Everything the blocking startup load produces.
pub struct SettingsBoot {
    pub settings: AppSettings,
    pub sink: Option<SaveSink>,
    pub persist: Option<PersistHandle>,
    pub failure: Option<SettingsLoadFailure>,
}

/// Open the shared settings database and load the app-settings blob,
/// blocking the calling thread. Runs before the window opens (like
/// egui's `load_persistent_state`); any failure degrades to defaults
/// with a `tracing::warn`, never a crash.
pub fn load_blocking() -> SettingsBoot {
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build() {
        Ok(rt) => Arc::new(rt),
        Err(err) => {
            tracing::warn!(%err, "build settings runtime -- using defaults, not persisting");
            return SettingsBoot {
                settings: AppSettings::default(),
                sink: None,
                persist: None,
                failure: Some(SettingsLoadFailure::StoreUnavailable),
            };
        }
    };
    let pool = match runtime.block_on(hxy_settings::persist::open_db()) {
        Ok(pool) => pool,
        Err(err) => {
            tracing::warn!(%err, "open settings database -- using defaults, not persisting");
            return SettingsBoot {
                settings: AppSettings::default(),
                sink: None,
                persist: None,
                failure: Some(SettingsLoadFailure::StoreUnavailable),
            };
        }
    };
    let (settings, failure) = match runtime.block_on(hxy_settings::persist::load_app_settings(&pool)) {
        // A fresh database has no settings row yet; defaults are the
        // correct first-boot state, not a failure.
        Ok(loaded) => (loaded.unwrap_or_default(), None),
        Err(err) => {
            tracing::warn!(%err, "load app settings -- using defaults");
            (AppSettings::default(), Some(SettingsLoadFailure::Unreadable))
        }
    };
    let persist = PersistHandle { pool: pool.clone(), runtime: runtime.clone() };
    SettingsBoot { settings, sink: Some(SaveSink::new(pool, runtime)), persist: Some(persist), failure }
}

/// Install the boot result as the app globals. Call once at startup,
/// before the window (and thus any consumer) is created.
pub fn init(cx: &mut App, boot: SettingsBoot) {
    cx.set_global(SettingsGlobal(boot.settings));
    cx.set_global(SettingsSink(boot.sink));
    cx.set_global(PersistHandleGlobal(boot.persist));
}

/// A snapshot of the current settings. Defaults when the global was
/// never installed -- only unit-test harnesses skip `init`, and
/// defaults are the correct baseline there. Cloning is deliberate
/// (the struct is small; `recent_files` is capped at 20) so callers
/// in render paths never hold a global borrow across `cx` mutation.
pub fn settings(cx: &App) -> AppSettings {
    cx.try_global::<SettingsGlobal>().map(|g| g.0.clone()).unwrap_or_default()
}

/// The numeric/value formats shared by the template panel, the
/// visualizer table, and offset cells.
pub fn formats(cx: &App) -> (NumericFormat, TemplateValueFormats) {
    let s = settings(cx);
    (s.numeric_format, s.template_value_formats)
}

/// Apply `f` to the settings, persist the result, and republish the
/// global so observers fire. A mutation that leaves the settings
/// unchanged is a no-op (no write, no notify) -- same outcome as
/// egui's `PartialEq` dirty check.
pub fn update_settings(cx: &mut App, f: impl FnOnce(&mut AppSettings)) {
    let before = settings(cx);
    let mut after = before.clone();
    f(&mut after);
    if after == before {
        return;
    }
    if let Some(sink) = cx.try_global::<SettingsSink>().and_then(|s| s.0.as_ref())
        && let Err(err) = sink.save_app_settings(&after)
    {
        // Log-not-crash: the in-memory settings still apply for this
        // session even when the disk write fails.
        tracing::error!(%err, "persist app settings");
    }
    cx.set_global(SettingsGlobal(after));
}

/// The shared highlight-mode enum for the persisted setting (mirrors
/// egui's `ByteHighlightModeExt::as_view`).
pub fn view_mode(mode: ByteHighlightMode) -> ValueHighlight {
    match mode {
        ByteHighlightMode::Background => ValueHighlight::Background,
        ByteHighlightMode::Text => ValueHighlight::Text,
    }
}

/// The byte-value highlight the current settings ask for, or `None`
/// when highlighting is off (the pane then paints every glyph in the
/// theme foreground, egui's highlight-off look). The Class scheme
/// takes the shared six-class tables, the Value scheme the shared HSL
/// gradient; both pick the dark/light variant for the current theme
/// and the background/text variant for the user's highlight mode,
/// exactly like egui's `build_palette` (`crates/hxy/src/view/hex_body.rs`).
pub fn highlight_palette(settings: &AppSettings, dark: bool) -> Option<PaneHighlight> {
    if !settings.byte_value_highlight {
        return None;
    }
    let mode = view_mode(settings.byte_highlight_mode);
    let table: [Hsla; 256] = match settings.byte_highlight_scheme {
        ByteHighlightScheme::Class => {
            let palette = BytePalette::for_theme_and_mode(dark, mode);
            std::array::from_fn(|byte| rgba_to_hsla(palette.color_for(byte as u8)))
        }
        ByteHighlightScheme::Value => {
            let gradient = ValueGradient::for_theme_and_mode(dark, mode);
            std::array::from_fn(|byte| rgba_to_hsla(gradient.color_for(byte as u8)))
        }
    };
    Some(PaneHighlight { mode, table: Arc::new(table) })
}

/// The byte-value highlight for compare panes. The egui compare pane
/// turns highlighting on/off and picks the mode but never installs a
/// palette override, so hxy-view always falls back to the Class
/// tables there and the scheme setting is ignored
/// (`crates/hxy/src/compare/pane.rs` vs hxy-view's
/// `for_theme_and_mode` fallback); this mirrors that.
pub fn compare_highlight_palette(settings: &AppSettings, dark: bool) -> Option<PaneHighlight> {
    if !settings.byte_value_highlight {
        return None;
    }
    let mode = view_mode(settings.byte_highlight_mode);
    let palette = BytePalette::for_theme_and_mode(dark, mode);
    let table: [Hsla; 256] = std::array::from_fn(|byte| rgba_to_hsla(palette.color_for(byte as u8)));
    Some(PaneHighlight { mode, table: Arc::new(table) })
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use hxy_settings::persist::open_db_in;

    use super::*;

    fn runtime() -> Arc<tokio::runtime::Runtime> {
        Arc::new(tokio::runtime::Builder::new_current_thread().enable_all().build().expect("build runtime"))
    }

    /// Install a real sqlite-backed sink rooted at a tempdir, so
    /// `update_settings` exercises the production persist path.
    fn init_with_tempdir(cx: &mut TestAppContext, dir: &std::path::Path) {
        let rt = runtime();
        let pool = rt.block_on(open_db_in(dir)).expect("open db");
        cx.update(|cx| {
            init(
                cx,
                SettingsBoot {
                    settings: AppSettings::default(),
                    sink: Some(SaveSink::new(pool, rt)),
                    persist: None,
                    failure: None,
                },
            );
        });
    }

    /// `update_settings` persists through the sink: a second,
    /// independent connection to the same tempdir database reads the
    /// mutated value back.
    #[gpui::test]
    fn update_settings_persists_to_sqlite(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        init_with_tempdir(cx, dir.path());

        cx.update(|cx| {
            update_settings(cx, |s| {
                s.hex_columns = hxy_core::ColumnCount::new(24).unwrap();
                s.file_poll_interval_ms = 4321;
            })
        });

        let rt = runtime();
        let loaded = rt
            .block_on(async {
                let pool = open_db_in(dir.path()).await?;
                hxy_settings::persist::load_app_settings(&pool).await
            })
            .expect("reload")
            .expect("settings stored");
        assert_eq!(loaded.hex_columns.get(), 24);
        assert_eq!(loaded.file_poll_interval_ms, 4321);
    }

    /// `update_settings` republishes the global so observers fire and
    /// later reads see the new value; a no-op mutation does neither.
    #[gpui::test]
    fn update_settings_notifies_observers(cx: &mut TestAppContext) {
        struct Probe {
            seen: Vec<u32>,
        }
        cx.update(|cx| {
            init(cx, SettingsBoot { settings: AppSettings::default(), sink: None, persist: None, failure: None })
        });
        let probe = cx.update(|cx| {
            gpui::AppContext::new(cx, |cx: &mut gpui::Context<Probe>| {
                cx.observe_global::<SettingsGlobal>(|probe, cx| {
                    probe.seen.push(settings(cx).file_poll_interval_ms);
                })
                .detach();
                Probe { seen: Vec::new() }
            })
        });

        cx.update(|cx| update_settings(cx, |s| s.file_poll_interval_ms = 777));
        cx.update(|cx| update_settings(cx, |s| s.file_poll_interval_ms = 777));
        assert_eq!(probe.read_with(cx, |p, _| p.seen.clone()), vec![777], "one change, one notification");
        cx.update(|cx| assert_eq!(settings(cx).file_poll_interval_ms, 777));
    }

    /// Class scheme, table-driven: every (theme, mode) pair resolves
    /// each byte class to the matching entry of the shared table, and
    /// the pane mode rides along.
    #[test]
    fn class_scheme_resolves_each_class_from_the_shared_tables() {
        let mut s = AppSettings {
            byte_value_highlight: true,
            byte_highlight_scheme: ByteHighlightScheme::Class,
            ..AppSettings::default()
        };
        let cases = [
            (true, ByteHighlightMode::Background, BytePalette::BG_DARK),
            (false, ByteHighlightMode::Background, BytePalette::BG_LIGHT),
            (true, ByteHighlightMode::Text, BytePalette::TEXT_DARK),
            (false, ByteHighlightMode::Text, BytePalette::TEXT_LIGHT),
        ];
        for (dark, mode, expected) in cases {
            s.byte_highlight_mode = mode;
            let hl = highlight_palette(&s, dark).expect("highlight on installs a palette");
            assert_eq!(hl.mode, view_mode(mode));
            let class_samples = [
                (0x00u8, expected.null),
                (0xFF, expected.all_bits),
                (b'\t', expected.whitespace),
                (b'A', expected.printable),
                (0x01, expected.control),
                (0x80, expected.extended),
            ];
            for (byte, want) in class_samples {
                assert_eq!(hl.table[byte as usize], rgba_to_hsla(want), "byte {byte:#04x} dark={dark} mode={mode:?}");
            }
        }
    }

    /// Value scheme: the table walks the shared HSL gradient with the
    /// (theme, mode) parameter pair, and highlight-off installs none.
    #[test]
    fn value_scheme_uses_the_shared_gradient_and_off_installs_none() {
        let mut s = AppSettings {
            byte_value_highlight: true,
            byte_highlight_scheme: ByteHighlightScheme::Value,
            byte_highlight_mode: ByteHighlightMode::Text,
            ..AppSettings::default()
        };
        let hl = highlight_palette(&s, true).expect("value scheme installs a palette");
        assert_eq!(hl.mode, ValueHighlight::Text);
        for byte in [0u8, 64, 128, 255] {
            assert_eq!(hl.table[byte as usize], rgba_to_hsla(ValueGradient::TEXT_DARK.color_for(byte)));
        }
        s.byte_highlight_mode = ByteHighlightMode::Background;
        let hl = highlight_palette(&s, false).expect("background variant");
        assert_eq!(hl.mode, ValueHighlight::Background);
        assert_eq!(hl.table[128], rgba_to_hsla(ValueGradient::BG_LIGHT.color_for(128)));

        s.byte_value_highlight = false;
        assert!(highlight_palette(&s, true).is_none(), "highlight off installs no palette");
    }
}
