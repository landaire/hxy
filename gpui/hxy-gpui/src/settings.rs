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
//! Known live-apply deviation: a global `hex_columns` change applies
//! to every open pane, clobbering a palette-set per-pane column count
//! (egui keeps a per-tab `hex_columns_override` that survives; the
//! gpui pane has no override slot yet).

use std::sync::Arc;

use gpui::App;
use gpui::Global;
use gpui::Hsla;
use hxy_core::format::NumericFormat;
use hxy_core::format::TemplateValueFormats;
pub use hxy_settings::AppSettings;
use hxy_settings::ByteHighlightScheme;
use hxy_settings::persist::SaveSink;

/// The live [`AppSettings`], readable anywhere via
/// [`settings`] and observable via `cx.observe_global::<SettingsGlobal>`.
pub struct SettingsGlobal(pub AppSettings);

impl Global for SettingsGlobal {}

/// The persistence handle. `None` when the settings database failed
/// to open at startup: the app runs on defaults and mutations apply
/// in-memory only (mirrors egui's sink-less fallback).
pub struct SettingsSink(pub Option<SaveSink>);

impl Global for SettingsSink {}

/// How the startup load degraded, if it did. Drives which warning
/// toast the shell shows once the notification layer exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsLoadFailure {
    /// The database (or the runtime it needs) could not be opened:
    /// no sink, nothing saves this session.
    StoreUnavailable,
    /// The database opened but the stored blob would not decode: the
    /// sink is live, so the next save overwrites the stored settings.
    Corrupt,
}

impl SettingsLoadFailure {
    pub fn toast_key(self) -> &'static str {
        match self {
            Self::StoreUnavailable => "gpui-settings-store-unavailable",
            Self::Corrupt => "gpui-settings-load-corrupt",
        }
    }
}

/// Everything the blocking startup load produces.
pub struct SettingsBoot {
    pub settings: AppSettings,
    pub sink: Option<SaveSink>,
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
            (AppSettings::default(), Some(SettingsLoadFailure::Corrupt))
        }
    };
    SettingsBoot { settings, sink: Some(SaveSink::new(pool, runtime)), failure }
}

/// Install the boot result as the app globals. Call once at startup,
/// before the window (and thus any consumer) is created.
pub fn init(cx: &mut App, boot: SettingsBoot) {
    cx.set_global(SettingsGlobal(boot.settings));
    cx.set_global(SettingsSink(boot.sink));
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

/// Saturation/lightness pairs for the value-scheme byte palette,
/// ported from egui hxy-view's `ValueGradient::TEXT_DARK` /
/// `TEXT_LIGHT`. Only the Text-mode constants apply here: the gpui
/// pane paints glyph colors only, so `byte_highlight_mode:
/// Background` has no background fill to drive (deviation from egui).
const VALUE_GRADIENT_TEXT_DARK: (f32, f32) = (0.75, 0.68);
const VALUE_GRADIENT_TEXT_LIGHT: (f32, f32) = (0.7, 0.4);

/// The byte-value glyph palette the current settings ask for, or
/// `None` to leave the pane's built-in byte-class colors in charge.
/// `Some` only for the Value scheme with highlighting on: the Class
/// scheme maps to the pane's built-in class colors, and "highlight
/// off" also falls back to them (the gpui pane has no fully plain
/// glyph mode -- deviation from egui).
pub fn value_palette(settings: &AppSettings, dark: bool) -> Option<Arc<[Hsla; 256]>> {
    if !settings.byte_value_highlight || settings.byte_highlight_scheme != ByteHighlightScheme::Value {
        return None;
    }
    let (s, l) = if dark { VALUE_GRADIENT_TEXT_DARK } else { VALUE_GRADIENT_TEXT_LIGHT };
    let mut table = [Hsla { h: 0.0, s, l, a: 1.0 }; 256];
    for (byte, slot) in table.iter_mut().enumerate() {
        // egui: hue = byte / 256 * 360 degrees; gpui Hsla hue is 0..1.
        slot.h = byte as f32 / 256.0;
    }
    Some(Arc::new(table))
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
                SettingsBoot { settings: AppSettings::default(), sink: Some(SaveSink::new(pool, rt)), failure: None },
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
        cx.update(|cx| init(cx, SettingsBoot { settings: AppSettings::default(), sink: None, failure: None }));
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

    #[test]
    fn value_palette_only_for_value_scheme_with_highlight_on() {
        let mut s = AppSettings {
            byte_value_highlight: true,
            byte_highlight_scheme: ByteHighlightScheme::Value,
            ..AppSettings::default()
        };
        let table = value_palette(&s, true).expect("value scheme installs a palette");
        // Hue wheel: byte 0 at hue 0, byte 128 half way around.
        assert_eq!(table[0].h, 0.0);
        assert!((table[128].h - 0.5).abs() < 1e-6);

        s.byte_highlight_scheme = ByteHighlightScheme::Class;
        assert!(value_palette(&s, true).is_none(), "class scheme keeps the built-in colors");
        s.byte_highlight_scheme = ByteHighlightScheme::Value;
        s.byte_value_highlight = false;
        assert!(value_palette(&s, true).is_none(), "highlight off clears the palette");
    }
}
