//! Loads the embedded hxy theme (assets/themes/hxy.json) and installs it
//! as the default light/dark theme pair.
//!
//! Assigning `Theme::light_theme`/`dark_theme` and re-running
//! `Theme::change` is the supported gpui-component override path: every
//! later appearance toggle (`sync_system_appearance` from the window
//! observer in `main`) re-applies from these configs, so the brand colors
//! survive mode switches. Direct `ThemeColor` pokes would be wiped.
//!
//! The JSON maps `crates/hxy/src/style.rs` constants onto gpui-component
//! theme keys. Direct mappings (dark / light): background = SURFACE
//! #0A0B10 / #ECE9F4, foreground = TEXT #E6E5F0 / #23212E,
//! muted.foreground = TEXT_DIM #9795AA / #6A6680, accent.foreground =
//! LAVENDER #B5A4E0 / LAVENDER_DEEP #7B5FC0, selection = SELECTION_BG
//! #522A7C / #DCC9F5 (gpui-component clamps selection alpha to 0.3, so
//! the rendered tint is a translucent wash of the egui-opaque value --
//! recorded deviation, not expressible in JSON), border = BORDER #302F44
//! / #D3CEE3, warning = GOLD
//! #F5CC4E / GOLD_DEEP #9E7806, danger = error_fg #FF5C80 / #C42046,
//! link and info = hyperlink CYAN #60CDD7 / CYAN_DEEP #0F7E8C, caret =
//! text_cursor LAVENDER / LAVENDER_DEEP.
//!
//! Judgement mappings (gpui key with no direct egui constant):
//! - primary.background: VIOLET #9D56FF in dark; light uses LAVENDER_DEEP
//!   #7B5FC0 because VIOLET fails text contrast as a button fill on light
//!   surfaces and LAVENDER_DEEP is style.rs's designated light deepening
//!   of the same family.
//! - primary.foreground: TEXT_BRIGHT #FAF8FF / #FFFFFF, the readable text
//!   on a violet fill; egui buttons never place text on VIOLET.
//! - secondary/muted background: egui inactive widget fill #1A1B27 /
//!   #F3F0FA (the neutral control tone); secondary hover/active take the
//!   egui hovered #27233A / #E9E2F8 and active #362450 / #DFD2F6 fills so
//!   button states track the egui interaction ramp.
//! - accent.background: the egui hovered fill (gpui paints accent as the
//!   MenuItem/ListItem hover background; egui has no accent-bg role).
//! - ring: BORDER_BRIGHT #5C4E7D / #A794CC, egui's brightened border for
//!   emphasized outlines (the fallback would be stock blue).
//! - popover.background: CARD #161722 / CARD_LIGHT #FFFFFF (egui menus
//!   and popups draw on the card tone).
//! - danger/warning/info foreground: dark mode sets SURFACE #0A0B10 (the
//!   fills are bright, so dark text reads); light mode keeps the computed
//!   fallback (primary.foreground white), readable on the deepened fills.
//! - tab.active.background, title_bar, sidebar, table.head: PANEL
//!   #101119 / #F7F5FC, matching hxy_dock_style's active-tab-on-panel
//!   treatment and egui's panel_fill chrome; tab_bar is left to its
//!   fallback (background = SURFACE), which is exactly the dock style's
//!   tab-bar band. tab.foreground dims inactive tab text to TEXT_DIM so
//!   the active tab (foreground) stands out; egui separates tabs by
//!   outline color, but gpui-component has no per-tab border key, so the
//!   VIOLET active-tab outline is not expressible (recorded deviation).
//! - scrollbar.background: transparent SURFACE so the track does not
//!   paint an opaque gutter (the fallback is the opaque background);
//!   the thumb falls back to accent, a subtle violet-grey like egui's.
//! - highlight editor colors: egui code_bg #12131C / #EEEAF8 with TEXT,
//!   set per mode so an appearance toggle cannot leave the other mode's
//!   stock highlight theme active.
//!
//! Everything else is intentionally left out. Most fallbacks derive from
//! the keys above (lists/tables from background + primary, drag/drop and
//! progress from primary, switch and skeleton from secondary, input
//! border from border); success/bullish/bearish, chart.* and the base.*
//! colors keep gpui-component's stock values, which egui has no
//! counterpart for. mono_font is not set: the platform default stays,
//! while egui embeds its own font (recorded deviation).

use std::rc::Rc;

use gpui::App;
use gpui_component::Theme;
use gpui_component::ThemeConfig;
use gpui_component::ThemeMode;
use gpui_component::ThemeSet;

use crate::assets::Assets;

const THEME_ASSET_PATH: &str = "themes/hxy.json";

#[derive(Debug, thiserror::Error)]
pub enum ThemeLoadError {
    #[error("embedded theme asset missing: {path}")]
    MissingAsset { path: &'static str },
    #[error("theme JSON parse failed: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("theme set defines no {mode} theme")]
    MissingMode { mode: &'static str },
}

struct ThemePair {
    light: ThemeConfig,
    dark: ThemeConfig,
}

fn parse_embedded_theme() -> Result<ThemePair, ThemeLoadError> {
    let file = Assets::get(THEME_ASSET_PATH).ok_or(ThemeLoadError::MissingAsset { path: THEME_ASSET_PATH })?;
    let set: ThemeSet = serde_json::from_slice(&file.data)?;
    let mut light = None;
    let mut dark = None;
    for config in set.themes {
        match config.mode {
            ThemeMode::Light => light = Some(config),
            ThemeMode::Dark => dark = Some(config),
        }
    }
    Ok(ThemePair {
        light: light.ok_or(ThemeLoadError::MissingMode { mode: "light" })?,
        dark: dark.ok_or(ThemeLoadError::MissingMode { mode: "dark" })?,
    })
}

/// Installs the hxy theme pair as the app default. Call after
/// `gpui_component::init` (which creates the `Theme` global) and before
/// the window's first `sync_system_appearance`.
///
/// The JSON is embedded, so a load failure is a build defect; it is
/// logged and the stock theme stays rather than panicking.
pub fn init(cx: &mut App) {
    match parse_embedded_theme() {
        Ok(pair) => {
            let theme = Theme::global_mut(cx);
            theme.light_theme = Rc::new(pair.light);
            theme.dark_theme = Rc::new(pair.dark);
            let mode = theme.mode;
            Theme::change(mode, None, cx);
        }
        Err(error) => {
            tracing::error!(%error, "failed to load embedded hxy theme; keeping stock theme");
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::Hsla;
    use gpui::TestAppContext;
    use gpui_component::ActiveTheme;

    use super::*;

    fn hsla(hex: &str) -> Hsla {
        gpui::Rgba::try_from(hex).expect("valid hex").into()
    }

    #[test]
    fn embedded_theme_parses() {
        let pair = parse_embedded_theme().expect("embedded hxy.json parses");
        assert_eq!(pair.light.name.as_ref(), "hxy Light");
        assert_eq!(pair.dark.name.as_ref(), "hxy Dark");
        assert!(pair.light.is_default && pair.dark.is_default);
    }

    #[gpui::test]
    fn init_applies_brand_colors(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            init(cx);
        });
        cx.update(|cx| {
            let theme = cx.theme();
            // Stock backgrounds are #0a0a0a / #ffffff, so equality with
            // the hxy value also proves the stock theme was replaced.
            if theme.is_dark() {
                assert_eq!(theme.background, hsla("#0A0B10"));
            } else {
                assert_eq!(theme.background, hsla("#ECE9F4"));
            }
        });
    }

    /// The map's headline risk: appearance toggles rebuild every color
    /// from `light_theme`/`dark_theme`, so the custom configs must be
    /// what `Theme::change` re-applies.
    #[gpui::test]
    fn brand_colors_survive_appearance_toggle(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            init(cx);
        });

        cx.update(|cx| Theme::change(ThemeMode::Dark, None, cx));
        cx.update(|cx| {
            let theme = cx.theme();
            assert_eq!(theme.background, hsla("#0A0B10"));
            assert_eq!(theme.foreground, hsla("#E6E5F0"));
            assert_eq!(theme.accent_foreground, hsla("#B5A4E0"));
            assert_eq!(theme.muted_foreground, hsla("#9795AA"));
        });

        cx.update(|cx| Theme::change(ThemeMode::Light, None, cx));
        cx.update(|cx| {
            let theme = cx.theme();
            assert_eq!(theme.background, hsla("#ECE9F4"));
            assert_eq!(theme.foreground, hsla("#23212E"));
            assert_eq!(theme.accent_foreground, hsla("#7B5FC0"));
            assert_eq!(theme.muted_foreground, hsla("#6A6680"));
        });

        cx.update(|cx| Theme::change(ThemeMode::Dark, None, cx));
        cx.update(|cx| {
            assert_eq!(cx.theme().background, hsla("#0A0B10"), "custom dark colors survive a round trip");
        });
    }
}
