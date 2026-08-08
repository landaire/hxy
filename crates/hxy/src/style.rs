// HXY theme: violet glass, pink signal, lime data accents.
// Keeps hex-view colors untouched; applies only to egui UI chrome/components.
// Dark and light variants share spacing and interaction; each theme
// supplies its own Visuals. The active one follows the OS preference.

use egui::Color32;
use egui::CornerRadius;
use egui::Margin;
use egui::Stroke;
use egui::Style;
use egui::Vec2;
use egui::Visuals;
use egui::epaint::Shadow;
use egui::style::Interaction;
use egui::style::ScrollStyle;
use egui::style::Selection;
use egui::style::Spacing;
use egui::style::TextCursorStyle;
use egui::style::WidgetVisuals;
use egui::style::Widgets;

// Core surfaces
const SURFACE: Color32 = Color32::from_rgb(10, 11, 16); // #0A0B10
const PANEL: Color32 = Color32::from_rgb(16, 17, 25); // #101119
const CARD: Color32 = Color32::from_rgb(22, 23, 34); // #161722
const CARD_BRIGHT: Color32 = Color32::from_rgb(31, 31, 46); // #1F1F2E

// Borders
const BORDER: Color32 = Color32::from_rgb(48, 47, 68); // #302F44
const BORDER_BRIGHT: Color32 = Color32::from_rgb(92, 78, 125); // #5C4E7D

// Brand accents
const VIOLET: Color32 = Color32::from_rgb(157, 86, 255); // #9D56FF
/// Muted lavender accent. Used wherever we'd reach for a saturated
/// "pop" color (selection outline, active-widget stroke, text
/// cursor) without the eye fatigue of a saturated pink. Sits in the
/// same hue family as VIOLET so the theme stays cohesive.
const LAVENDER: Color32 = Color32::from_rgb(181, 164, 224); // #B5A4E0
const CYAN: Color32 = Color32::from_rgb(96, 205, 215); // #60CDD7
const GOLD: Color32 = Color32::from_rgb(245, 204, 78); // #F5CC4E

// Text
const TEXT: Color32 = Color32::from_rgb(230, 229, 240); // #E6E5F0
const TEXT_DIM: Color32 = Color32::from_rgb(151, 149, 170); // #9795AA
const TEXT_BRIGHT: Color32 = Color32::from_rgb(250, 248, 255); // #FAF8FF

// Selection
const SELECTION_BG: Color32 = Color32::from_rgb(82, 42, 124); // #522A7C
const SELECTION_STROKE: Color32 = LAVENDER;

// Light-mode counterparts. Same violet family, inverted value scale:
// surfaces go lavender-tinted white, accents deepen so they keep
// contrast against light backgrounds.
const SURFACE_LIGHT: Color32 = Color32::from_rgb(236, 233, 244); // #ECE9F4
const PANEL_LIGHT: Color32 = Color32::from_rgb(247, 245, 252); // #F7F5FC
const CARD_LIGHT: Color32 = Color32::from_rgb(255, 255, 255); // #FFFFFF
const CARD_BRIGHT_LIGHT: Color32 = Color32::from_rgb(236, 231, 247); // #ECE7F7

const BORDER_LIGHT: Color32 = Color32::from_rgb(211, 206, 227); // #D3CEE3
const BORDER_BRIGHT_LIGHT: Color32 = Color32::from_rgb(167, 148, 204); // #A794CC

/// Deepened lavender for light mode; the dark-mode LAVENDER washes
/// out against white surfaces.
const LAVENDER_DEEP: Color32 = Color32::from_rgb(123, 95, 192); // #7B5FC0
const CYAN_DEEP: Color32 = Color32::from_rgb(15, 126, 140); // #0F7E8C
const GOLD_DEEP: Color32 = Color32::from_rgb(158, 120, 6); // #9E7806

const TEXT_LIGHT: Color32 = Color32::from_rgb(35, 33, 46); // #23212E
const TEXT_DIM_LIGHT: Color32 = Color32::from_rgb(106, 102, 128); // #6A6680
const TEXT_BRIGHT_LIGHT: Color32 = Color32::from_rgb(15, 13, 23); // #0F0D17

const SELECTION_BG_LIGHT: Color32 = Color32::from_rgb(220, 201, 245); // #DCC9F5
const SELECTION_STROKE_LIGHT: Color32 = LAVENDER_DEEP;

pub fn hxy_style() -> Style {
    base_style(dark_visuals())
}

pub fn hxy_light_style() -> Style {
    base_style(light_visuals())
}

/// Spacing, interaction, and animation shared by both themes.
fn base_style(visuals: Visuals) -> Style {
    Style {
        spacing: Spacing {
            item_spacing: Vec2 { x: 8.0, y: 5.0 },
            window_margin: Margin::same(8),
            button_padding: Vec2 { x: 8.0, y: 3.0 },
            menu_margin: Margin::same(8),
            indent: 18.0,
            interact_size: Vec2 { x: 42.0, y: 22.0 },
            slider_width: 120.0,
            combo_width: 120.0,
            text_edit_width: 280.0,
            icon_width: 16.0,
            icon_width_inner: 9.0,
            icon_spacing: 5.0,
            tooltip_width: 520.0,
            indent_ends_with_horizontal_line: false,
            combo_height: 220.0,
            scroll: ScrollStyle {
                bar_width: 10.0,
                handle_min_length: 18.0,
                bar_inner_margin: 3.0,
                bar_outer_margin: 1.0,
                ..Default::default()
            },
            ..Default::default()
        },

        interaction: Interaction {
            resize_grab_radius_side: 6.0,
            resize_grab_radius_corner: 12.0,
            show_tooltips_only_when_still: true,
            ..Default::default()
        },

        visuals,

        animation_time: 1.0 / 10.0,
        explanation_tooltips: false,
        ..Default::default()
    }
}

fn dark_visuals() -> Visuals {
    Visuals {
        dark_mode: true,
        override_text_color: None,

        widgets: Widgets {
            noninteractive: WidgetVisuals {
                bg_fill: CARD,
                weak_bg_fill: CARD,
                bg_stroke: Stroke { width: 1.0, color: BORDER },
                corner_radius: CornerRadius::ZERO,
                fg_stroke: Stroke { width: 1.0, color: TEXT_DIM },
                expansion: 0.0,
            },

            inactive: WidgetVisuals {
                bg_fill: Color32::from_rgb(26, 27, 39),
                weak_bg_fill: Color32::from_rgb(24, 25, 36),
                bg_stroke: Stroke { width: 1.0, color: BORDER },
                corner_radius: CornerRadius::ZERO,
                fg_stroke: Stroke { width: 1.0, color: TEXT },
                expansion: 0.0,
            },
            hovered: WidgetVisuals {
                bg_fill: Color32::from_rgb(39, 35, 58),
                weak_bg_fill: Color32::from_rgb(43, 38, 64),
                bg_stroke: Stroke { width: 1.0, color: VIOLET },
                corner_radius: CornerRadius::ZERO,
                fg_stroke: Stroke { width: 1.5, color: TEXT_BRIGHT },
                expansion: 1.0,
            },

            active: WidgetVisuals {
                bg_fill: Color32::from_rgb(54, 36, 80),
                weak_bg_fill: Color32::from_rgb(61, 38, 88),
                bg_stroke: Stroke { width: 1.25, color: LAVENDER },
                corner_radius: CornerRadius::ZERO,
                fg_stroke: Stroke { width: 2.0, color: TEXT_BRIGHT },
                expansion: 1.0,
            },

            open: WidgetVisuals {
                bg_fill: CARD_BRIGHT,
                weak_bg_fill: Color32::from_rgb(40, 35, 56),
                bg_stroke: Stroke { width: 1.0, color: BORDER_BRIGHT },
                corner_radius: CornerRadius::ZERO,
                fg_stroke: Stroke { width: 1.0, color: TEXT },
                expansion: 0.0,
            },
        },

        selection: Selection { bg_fill: SELECTION_BG, stroke: Stroke { width: 1.0, color: SELECTION_STROKE } },

        hyperlink_color: CYAN,
        faint_bg_color: Color32::from_rgba_unmultiplied(157, 86, 255, 14),
        extreme_bg_color: SURFACE,
        code_bg_color: Color32::from_rgb(18, 19, 28),

        warn_fg_color: GOLD,
        error_fg_color: Color32::from_rgb(255, 92, 128),

        window_corner_radius: CornerRadius::same(6),
        window_shadow: Shadow {
            spread: 0,
            color: Color32::from_rgba_premultiplied(0, 0, 0, 130),
            blur: 24,
            offset: [0, 18],
        },
        window_fill: PANEL,
        window_stroke: Stroke { width: 1.0, color: BORDER },

        menu_corner_radius: CornerRadius::same(5),
        panel_fill: PANEL,

        popup_shadow: Shadow {
            spread: 0,
            color: Color32::from_rgba_premultiplied(0, 0, 0, 120),
            blur: 16,
            offset: [0, 10],
        },

        resize_corner_size: 12.0,

        text_cursor: TextCursorStyle {
            stroke: Stroke { width: 2.0, color: LAVENDER },
            preview: false,
            ..Default::default()
        },

        button_frame: true,
        collapsing_header_frame: false,
        indent_has_left_vline: true,
        striped: false,
        slider_trailing_fill: true,

        ..Default::default()
    }
}

fn light_visuals() -> Visuals {
    Visuals {
        dark_mode: false,
        override_text_color: None,

        widgets: Widgets {
            noninteractive: WidgetVisuals {
                bg_fill: CARD_LIGHT,
                weak_bg_fill: CARD_LIGHT,
                bg_stroke: Stroke { width: 1.0, color: BORDER_LIGHT },
                corner_radius: CornerRadius::ZERO,
                fg_stroke: Stroke { width: 1.0, color: TEXT_DIM_LIGHT },
                expansion: 0.0,
            },

            inactive: WidgetVisuals {
                bg_fill: Color32::from_rgb(243, 240, 250),
                weak_bg_fill: Color32::from_rgb(246, 243, 251),
                bg_stroke: Stroke { width: 1.0, color: BORDER_LIGHT },
                corner_radius: CornerRadius::ZERO,
                fg_stroke: Stroke { width: 1.0, color: TEXT_LIGHT },
                expansion: 0.0,
            },
            hovered: WidgetVisuals {
                bg_fill: Color32::from_rgb(233, 226, 248),
                weak_bg_fill: Color32::from_rgb(229, 221, 247),
                bg_stroke: Stroke { width: 1.0, color: VIOLET },
                corner_radius: CornerRadius::ZERO,
                fg_stroke: Stroke { width: 1.5, color: TEXT_BRIGHT_LIGHT },
                expansion: 1.0,
            },

            active: WidgetVisuals {
                bg_fill: Color32::from_rgb(223, 210, 246),
                weak_bg_fill: Color32::from_rgb(216, 201, 243),
                bg_stroke: Stroke { width: 1.25, color: LAVENDER_DEEP },
                corner_radius: CornerRadius::ZERO,
                fg_stroke: Stroke { width: 2.0, color: TEXT_BRIGHT_LIGHT },
                expansion: 1.0,
            },

            open: WidgetVisuals {
                bg_fill: CARD_BRIGHT_LIGHT,
                weak_bg_fill: Color32::from_rgb(238, 233, 248),
                bg_stroke: Stroke { width: 1.0, color: BORDER_BRIGHT_LIGHT },
                corner_radius: CornerRadius::ZERO,
                fg_stroke: Stroke { width: 1.0, color: TEXT_LIGHT },
                expansion: 0.0,
            },
        },

        selection: Selection {
            bg_fill: SELECTION_BG_LIGHT,
            stroke: Stroke { width: 1.0, color: SELECTION_STROKE_LIGHT },
        },

        hyperlink_color: CYAN_DEEP,
        faint_bg_color: Color32::from_rgba_unmultiplied(157, 86, 255, 14),
        extreme_bg_color: SURFACE_LIGHT,
        code_bg_color: Color32::from_rgb(238, 234, 248),

        warn_fg_color: GOLD_DEEP,
        error_fg_color: Color32::from_rgb(196, 32, 70),

        window_corner_radius: CornerRadius::same(6),
        window_shadow: Shadow {
            spread: 0,
            color: Color32::from_rgba_premultiplied(0, 0, 0, 60),
            blur: 24,
            offset: [0, 18],
        },
        window_fill: PANEL_LIGHT,
        window_stroke: Stroke { width: 1.0, color: BORDER_LIGHT },

        menu_corner_radius: CornerRadius::same(5),
        panel_fill: PANEL_LIGHT,

        popup_shadow: Shadow {
            spread: 0,
            color: Color32::from_rgba_premultiplied(0, 0, 0, 50),
            blur: 16,
            offset: [0, 10],
        },

        resize_corner_size: 12.0,

        text_cursor: TextCursorStyle {
            stroke: Stroke { width: 2.0, color: LAVENDER_DEEP },
            preview: false,
            ..Default::default()
        },

        button_frame: true,
        collapsing_header_frame: false,
        indent_has_left_vline: true,
        striped: false,
        slider_trailing_fill: true,

        ..Visuals::light()
    }
}

/// Tweaks on top of egui_dock's `Style::from_egui` for the hxy theme.
///
/// The default has the active tab's fill match `window_fill`, so an
/// active tab visually fuses with the panel below it (the "tab is
/// part of its content" classic look). We instead:
///
/// * Push the tab-bar background to the surface tone (darker than
///   the panel in dark mode, dimmer in light mode) so the strip is
///   clearly its own band.
/// * Keep the active tab on the panel tone so it still "rises" out
///   of the strip into the content -- the seam between active tab
///   and panel disappears, which is the intentional bridge.
/// * Drop a violet accent line under the active tab name so the
///   active tab is identifiable without relying on the bg-merge
///   trick.
/// * Hide the strip's full-width hline (the border under the tab
///   strip) since the tone difference between surface and panel
///   already separates the bands.
pub fn hxy_dock_style(egui_style: &egui::Style) -> egui_dock::Style {
    let (surface, panel, card_bright, border, border_bright, focus_accent) = if egui_style.visuals.dark_mode {
        (SURFACE, PANEL, CARD_BRIGHT, BORDER, BORDER_BRIGHT, LAVENDER)
    } else {
        (SURFACE_LIGHT, PANEL_LIGHT, CARD_BRIGHT_LIGHT, BORDER_LIGHT, BORDER_BRIGHT_LIGHT, LAVENDER_DEEP)
    };
    let mut style = egui_dock::Style::from_egui(egui_style);
    style.tab_bar.bg_fill = surface;
    style.tab_bar.hline_color = Color32::TRANSPARENT;
    style.tab.active.bg_fill = panel;
    style.tab.focused.bg_fill = panel;
    style.tab.inactive.bg_fill = surface;
    style.tab.hovered.bg_fill = card_bright;
    style.tab.hline_below_active_tab_name = true;
    // egui_dock's `from_egui` mapping wires the *hovered* tab to
    // the bright violet `widgets.hovered.bg_stroke.color` while
    // the active tab inherits the dim `widgets.noninteractive`
    // border -- so a hovered-but-inactive tab visually dominates
    // the actually-active tab. Flip the prominence: active tabs
    // get the brand violet outline, focused (kb-focus) tabs get
    // the brighter lavender, and hover/inactive get muted
    // borders that read as a passive shape change.
    style.tab.active.outline_color = VIOLET;
    style.tab.focused.outline_color = focus_accent;
    style.tab.hovered.outline_color = border_bright;
    style.tab.inactive.outline_color = border;
    // The body stroke would draw a box around the tab content area;
    // we want the tab and its panel to read as one shape, so kill
    // it. The panel's own fill provides the boundary.
    style.tab.tab_body.stroke = Stroke::NONE;
    style.tab.tab_body.bg_fill = panel;
    // Drop the 4px bottom padding egui_dock adds around every tab
    // body. Our file tab anchors a `Panel::bottom` (status bar) here,
    // so the default margin shows up as a dead strip below the
    // status text. Top/left/right stay at 4 because the inner panels
    // (VFS tree, console) don't add their own gutters.
    style.tab.tab_body.inner_margin.bottom = 0;
    style
}
