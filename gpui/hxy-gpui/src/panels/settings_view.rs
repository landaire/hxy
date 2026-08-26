//! [`SettingsPanel`]: the dockable settings editor. Mirrors the egui
//! settings tab (`crates/hxy/src/app/mod.rs::settings_ui`) for the
//! fields the gpui shell applies (see `crate::settings`'s deviation
//! ledger). Rows for fields the shell does not honor are not rendered:
//! zoom, check-for-updates, language, the address separator pair, and
//! the whole Memory section (`byte_cache_limit_mib` -- nothing here
//! constructs a byte cache). egui's settings tab does not render
//! `file_watch_prefs` rows either, so none appear here.
//!
//! Every mutation routes through [`crate::settings::update_settings`],
//! so a change persists immediately and live-applies via the
//! workspace's settings observer. Enum rows render as segmented
//! selected-buttons (the shell's established idiom, see
//! `panels/strings.rs`) where egui uses a `ComboBox`; numeric rows
//! commit on each text change, clamped to the matching egui
//! `DragValue` range, and re-sync from the global while unfocused so
//! an external change (e.g. the palette's global column count)
//! refreshes an open panel.

use gpui::AnyElement;
use gpui::App;
use gpui::AppContext;
use gpui::Context;
use gpui::ElementId;
use gpui::Entity;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::SharedString;
use gpui::StatefulInteractiveElement;
use gpui::Styled;
use gpui::Subscription;
use gpui::Window;
use gpui::div;
use gpui::px;
use gpui::component::ActiveTheme;
use gpui::component::Disableable;
use gpui::component::Icon;
use gpui::component::IconName;
use gpui::component::Selectable;
use gpui::component::button::Button;
use gpui::component::button::ButtonVariants;
use gpui::component::dock::BasePanel;
use gpui::component::dock::Panel;
use gpui::component::dock::PanelEvent;
use gpui::component::h_flex;
use gpui::component::input::InputEvent;
use gpui::component::input::InputState;
use gpui::component::input::NumberInput;
use gpui::component::input::NumberInputEvent;
use gpui::component::input::StepAction;
use gpui::component::label::Label;
use gpui::component::switch::Switch;
use gpui::component::tooltip::Tooltip;
use gpui::component::v_flex;
use hxy_core::ColumnCount;
use hxy_settings::AutoReloadMode;
use hxy_settings::ByteHighlightMode;
use hxy_settings::ByteHighlightScheme;
use hxy_settings::IntValueType;
use hxy_settings::NumericBase;
use hxy_settings::NumericFormat;
use hxy_settings::OffsetBase;
use hxy_settings::RecomputeDeadline;

use crate::settings::AppSettings;
use crate::settings::SettingsGlobal;
use crate::settings::update_settings;

/// Stable identifier for layout (de)serialization; must never change.
pub const SETTINGS_PANEL_NAME: &str = "SettingsPanel";

/// Width of the label column, standing in for egui's two-column grid.
const LABEL_WIDTH: f32 = 230.0;

/// Ceiling for the hex-columns input, matching the egui slider cap and
/// the palette's `MAX_COLUMNS`.
const MAX_COLUMNS: u64 = 64;

/// Ceiling for the poll-interval input, matching egui's `DragValue`
/// range (0 disables polling).
const MAX_POLL_INTERVAL_MS: u64 = 600_000;

/// Which numeric-format a row edits: the app-wide length/offset format
/// or one of the eight per-integer-type template value slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FormatSlot {
    Main,
    Ty(IntValueType),
}

fn slot_format(s: &AppSettings, slot: FormatSlot) -> NumericFormat {
    match slot {
        FormatSlot::Main => s.numeric_format,
        FormatSlot::Ty(ty) => s.template_value_formats.slot(ty),
    }
}

fn slot_format_mut(s: &mut AppSettings, slot: FormatSlot) -> &mut NumericFormat {
    match slot {
        FormatSlot::Main => &mut s.numeric_format,
        FormatSlot::Ty(ty) => s.template_value_formats.slot_mut(ty),
    }
}

/// Stable element-id prefix per slot, so the mode/base buttons and the
/// threshold input of different rows never collide.
fn slot_id(slot: FormatSlot) -> &'static str {
    match slot {
        FormatSlot::Main => "settings-nf-main",
        FormatSlot::Ty(IntValueType::U8) => "settings-nf-u8",
        FormatSlot::Ty(IntValueType::U16) => "settings-nf-u16",
        FormatSlot::Ty(IntValueType::U32) => "settings-nf-u32",
        FormatSlot::Ty(IntValueType::U64) => "settings-nf-u64",
        FormatSlot::Ty(IntValueType::S8) => "settings-nf-s8",
        FormatSlot::Ty(IntValueType::S16) => "settings-nf-s16",
        FormatSlot::Ty(IntValueType::S32) => "settings-nf-s32",
        FormatSlot::Ty(IntValueType::S64) => "settings-nf-s64",
    }
}

/// Mode flip preserving the bases / threshold the user already picked,
/// ported from egui's `numeric_format_row` (a plain enum replace would
/// snap the pickers back to arbitrary defaults).
fn to_always(fmt: NumericFormat) -> NumericFormat {
    match fmt {
        NumericFormat::Always(_) => fmt,
        NumericFormat::Threshold { large, .. } => NumericFormat::Always(large),
    }
}

/// See [`to_always`]. The `small`/`threshold` defaults mirror egui's
/// flip (decimal below 256).
fn to_threshold(fmt: NumericFormat) -> NumericFormat {
    match fmt {
        NumericFormat::Threshold { .. } => fmt,
        NumericFormat::Always(base) => {
            NumericFormat::Threshold { small: NumericBase::Decimal, large: base, threshold: 256 }
        }
    }
}

/// One numeric row: the settings field it commits to and the range the
/// matching egui `DragValue` enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NumericSetting {
    /// `hex_columns`; egui range 1..=64.
    HexColumns,
    /// `compare_recompute_deadline` in ms; egui range 100..=60000.
    CompareDeadlineMs,
    /// `file_poll_interval_ms`; egui range 0..=600000, 0 disables.
    PollIntervalMs,
    /// A `NumericFormat::Threshold` boundary; egui range 1...
    Threshold(FormatSlot),
}

impl NumericSetting {
    /// Increment applied by the stepper buttons; the ms inputs mirror
    /// egui's `DragValue::speed(50)`.
    fn step(self) -> u64 {
        match self {
            Self::HexColumns | Self::Threshold(_) => 1,
            Self::CompareDeadlineMs | Self::PollIntervalMs => 50,
        }
    }

    fn clamp(self, value: u64) -> u64 {
        match self {
            Self::HexColumns => value.clamp(1, MAX_COLUMNS),
            Self::CompareDeadlineMs => {
                value.clamp(u64::from(RecomputeDeadline::MIN_MS), u64::from(RecomputeDeadline::MAX_MS))
            }
            Self::PollIntervalMs => value.min(MAX_POLL_INTERVAL_MS),
            Self::Threshold(_) => value.max(1),
        }
    }

    fn current(self, s: &AppSettings) -> u64 {
        match self {
            Self::HexColumns => u64::from(s.hex_columns.get()),
            Self::CompareDeadlineMs => u64::from(s.compare_recompute_deadline.as_ms()),
            Self::PollIntervalMs => u64::from(s.file_poll_interval_ms),
            Self::Threshold(slot) => match slot_format(s, slot) {
                NumericFormat::Threshold { threshold, .. } => threshold,
                // The input is only rendered in Threshold mode; this
                // arm only feeds the unfocused-input resync.
                NumericFormat::Always(_) => 1,
            },
        }
    }

    /// Clamp and write `value` into its settings field. The row's
    /// change handler wraps this in `update_settings`, so tests can
    /// drive the exact production mutation.
    fn commit(self, s: &mut AppSettings, value: u64) {
        let value = self.clamp(value);
        match self {
            Self::HexColumns => {
                // Clamped to 1..=64 above, so `new` cannot reject it;
                // the `if let` avoids an unwrap all the same.
                if let Ok(cols) = ColumnCount::new(value as u16) {
                    s.hex_columns = cols;
                }
            }
            Self::CompareDeadlineMs => s.compare_recompute_deadline = RecomputeDeadline::from_ms(value as u32),
            Self::PollIntervalMs => s.file_poll_interval_ms = value as u32,
            Self::Threshold(slot) => {
                if let NumericFormat::Threshold { threshold, .. } = slot_format_mut(s, slot) {
                    *threshold = value;
                }
            }
        }
    }
}

pub struct SettingsPanel {
    focus_handle: FocusHandle,
    /// One text state per numeric row, paired with its commit target.
    numeric_inputs: Vec<(NumericSetting, Entity<InputState>)>,
    /// Whether the per-type template formats section is expanded
    /// (collapsed by default, like egui's `CollapsingHeader`).
    formats_expanded: bool,
    _subs: Vec<Subscription>,
}

impl SettingsPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let s = crate::settings::settings(cx);
        let mut targets = vec![
            NumericSetting::HexColumns,
            NumericSetting::CompareDeadlineMs,
            NumericSetting::PollIntervalMs,
            NumericSetting::Threshold(FormatSlot::Main),
        ];
        targets.extend(IntValueType::all().iter().map(|ty| NumericSetting::Threshold(FormatSlot::Ty(*ty))));

        let mut subs = Vec::new();
        let mut numeric_inputs = Vec::new();
        for setting in targets {
            let input = cx.new(|cx| InputState::new(window, cx).default_value(setting.current(&s).to_string()));
            subs.push(cx.subscribe(&input, move |_this, input, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    let text = input.read(cx).value();
                    // A draft that does not parse (empty, mid-edit) is
                    // left uncommitted; egui's DragValue cannot produce
                    // one, so there is no behavior to mirror.
                    if let Ok(value) = text.trim().parse::<u64>() {
                        update_settings(cx, |s| setting.commit(s, value));
                    }
                }
            }));
            subs.push(cx.subscribe_in(&input, window, move |_this, input, event: &NumberInputEvent, window, cx| {
                let NumberInputEvent::Step(action) = event;
                let base = match input.read(cx).value().trim().parse::<u64>() {
                    Ok(value) => value,
                    // Stepping from an unparseable draft restarts from
                    // the last committed value.
                    Err(_) => setting.current(&crate::settings::settings(cx)),
                };
                let next = match action {
                    StepAction::Increment => setting.clamp(base.saturating_add(setting.step())),
                    StepAction::Decrement => setting.clamp(base.saturating_sub(setting.step())),
                };
                input.update(cx, |state, cx| state.set_value(next.to_string(), window, cx));
                update_settings(cx, |s| setting.commit(s, next));
            }));
            numeric_inputs.push((setting, input));
        }
        // Re-render on any settings change so rows edited elsewhere
        // (palette, another row's side effect) stay current.
        subs.push(cx.observe_global::<SettingsGlobal>(|_, cx| cx.notify()));

        Self { focus_handle: cx.focus_handle(), numeric_inputs, formats_expanded: false, _subs: subs }
    }

    fn numeric_input(&self, setting: NumericSetting) -> &Entity<InputState> {
        self.numeric_inputs
            .iter()
            .find(|(s, _)| *s == setting)
            .map(|(_, input)| input)
            // The constructor builds one input per NumericSetting the
            // renderer can name, so a miss is a construction bug.
            .expect("numeric input exists for every rendered setting")
    }

    /// Push the settings value back into any numeric input that is not
    /// focused and disagrees with it (external change, or a clamp).
    /// Focused inputs are left alone so typing is never clobbered.
    fn sync_numeric_inputs(&self, window: &mut Window, cx: &mut Context<Self>) {
        let s = crate::settings::settings(cx);
        for (setting, input) in &self.numeric_inputs {
            if input.read(cx).focus_handle(cx).is_focused(window) {
                continue;
            }
            let current = setting.current(&s);
            if input.read(cx).value().trim().parse::<u64>().ok() == Some(current) {
                continue;
            }
            input.update(cx, |state, cx| state.set_value(current.to_string(), window, cx));
        }
    }

    fn number_row(&self, setting: NumericSetting, width: f32) -> AnyElement {
        div().w(px(width)).child(NumberInput::new(self.numeric_input(setting))).into_any_element()
    }

    /// The mode picker plus its mode-specific sub-controls, ported from
    /// egui's `numeric_format_row`.
    fn numeric_format_controls(&self, slot: FormatSlot, s: &AppSettings) -> AnyElement {
        let id = slot_id(slot);
        let fmt = slot_format(s, slot);
        let is_always = matches!(fmt, NumericFormat::Always(_));
        let mut row = h_flex()
            .gap_1()
            .items_center()
            .flex_wrap()
            .child(choice_button((id, 0usize), hxy_i18n::t("settings-numeric-format-always"), is_always, move |s| {
                let cur = slot_format(s, slot);
                *slot_format_mut(s, slot) = to_always(cur);
            }))
            .child(choice_button(
                (id, 1usize),
                hxy_i18n::t("settings-numeric-format-threshold"),
                !is_always,
                move |s| {
                    let cur = slot_format(s, slot);
                    *slot_format_mut(s, slot) = to_threshold(cur);
                },
            ));
        match fmt {
            NumericFormat::Always(base) => {
                row = row.child(base_buttons((id, 2usize), base, move |s, base| {
                    if let NumericFormat::Always(cur) = slot_format_mut(s, slot) {
                        *cur = base;
                    }
                }));
            }
            NumericFormat::Threshold { small, large, .. } => {
                row = row
                    .child(Label::new(hxy_i18n::t("settings-numeric-format-small-label")))
                    .child(base_buttons((id, 3usize), small, move |s, base| {
                        if let NumericFormat::Threshold { small, .. } = slot_format_mut(s, slot) {
                            *small = base;
                        }
                    }))
                    .child(Label::new(hxy_i18n::t("settings-numeric-format-large-label")))
                    .child(base_buttons((id, 5usize), large, move |s, base| {
                        if let NumericFormat::Threshold { large, .. } = slot_format_mut(s, slot) {
                            *large = base;
                        }
                    }))
                    .child(Label::new(hxy_i18n::t("settings-numeric-format-threshold-label")))
                    .child(self.number_row(NumericSetting::Threshold(slot), 110.0));
            }
        }
        row.into_any_element()
    }

    /// The collapsed-by-default per-integer-type formats section,
    /// ported from egui's `template_value_formats_row`.
    fn template_formats_section(&self, s: &AppSettings, cx: &mut Context<Self>) -> AnyElement {
        let chevron = if self.formats_expanded { IconName::ChevronDown } else { IconName::ChevronRight };
        let header = Button::new("settings-template-formats-toggle")
            .ghost()
            .compact()
            .icon(Icon::new(chevron))
            .label(hxy_i18n::t("settings-template-value-format-collapsed-label"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.formats_expanded = !this.formats_expanded;
                cx.notify();
            }));
        let mut section = v_flex().gap_1().child(header);
        if self.formats_expanded {
            let mono = cx.theme().mono_font_family.clone();
            for ty in IntValueType::all() {
                section = section.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .pl_6()
                        .child(div().w(px(40.0)).flex_none().font_family(mono.clone()).child(ty.label()))
                        .child(self.numeric_format_controls(FormatSlot::Ty(*ty), s)),
                );
            }
        }
        section.into_any_element()
    }
}

/// A two-state segmented pick applying `apply(settings)` on click.
fn choice_button(
    id: impl Into<ElementId>,
    label: String,
    selected: bool,
    apply: impl Fn(&mut AppSettings) + 'static,
) -> Button {
    Button::new(id).label(label).compact().selected(selected).on_click(move |_, _, cx| {
        update_settings(cx, |s| apply(s));
    })
}

/// Hex / Decimal pair, standing in for egui's `base_combo`.
fn base_buttons(
    id: (&'static str, usize),
    current: NumericBase,
    apply: impl Fn(&mut AppSettings, NumericBase) + Clone + 'static,
) -> AnyElement {
    let hex_apply = apply.clone();
    h_flex()
        .gap_1()
        .child(choice_button(
            (id.0, id.1 * 100),
            hxy_i18n::t("gpui-settings-base-hex"),
            current == NumericBase::Hex,
            move |s| hex_apply(s, NumericBase::Hex),
        ))
        .child(choice_button(
            (id.0, id.1 * 100 + 1),
            hxy_i18n::t("gpui-settings-base-decimal"),
            current == NumericBase::Decimal,
            move |s| apply(s, NumericBase::Decimal),
        ))
        .into_any_element()
}

/// A boolean row's switch, routing through `update_settings`.
fn toggle(id: impl Into<ElementId>, checked: bool, apply: impl Fn(&mut AppSettings, bool) + 'static) -> Switch {
    Switch::new(id).checked(checked).on_click(move |checked: &bool, _, cx| {
        let value = *checked;
        update_settings(cx, |s| apply(s, value));
    })
}

/// One two-column row: fixed-width label, control.
fn setting_row(label: String, control: impl IntoElement) -> impl IntoElement {
    h_flex().gap_2().items_center().child(div().w(px(LABEL_WIDTH)).flex_none().child(Label::new(label))).child(control)
}

/// Like [`setting_row`], with a hover tooltip on the label (egui hangs
/// these off the value widget; the label is the stable hover target
/// here).
fn setting_row_with_tooltip(
    id: &'static str,
    label: String,
    tooltip: String,
    control: impl IntoElement,
) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .id(id)
                .w(px(LABEL_WIDTH))
                .flex_none()
                .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                .child(Label::new(label)),
        )
        .child(control)
}

fn section_heading(text: String, cx: &App) -> impl IntoElement {
    div().mt_2().pb_1().border_b_1().border_color(cx.theme().border).font_weight(gpui::FontWeight::SEMIBOLD).child(text)
}

impl BasePanel for SettingsPanel {
    fn panel_name(&self) -> &'static str {
        SETTINGS_PANEL_NAME
    }
}

impl Panel for SettingsPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(hxy_i18n::t("tab-settings"))
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(hxy_i18n::t("tab-settings")))
    }
}

impl Focusable for SettingsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for SettingsPanel {}

impl Render for SettingsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_numeric_inputs(window, cx);
        let s = crate::settings::settings(cx);
        let muted = cx.theme().muted_foreground;

        v_flex()
            .id("settings-panel")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .gap_2()
            .child(section_heading(hxy_i18n::t("settings-general-header"), cx))
            .child(setting_row(hxy_i18n::t("settings-input-mode"), {
                let current = s.input_mode;
                h_flex()
                    .gap_1()
                    .child(choice_button(
                        "settings-input-default",
                        hxy_i18n::t("settings-input-mode-default"),
                        current == hxy_editor::InputMode::Default,
                        |s| s.input_mode = hxy_editor::InputMode::Default,
                    ))
                    .child(choice_button(
                        "settings-input-vim",
                        hxy_i18n::t("settings-input-mode-vim"),
                        current == hxy_editor::InputMode::Vim,
                        |s| s.input_mode = hxy_editor::InputMode::Vim,
                    ))
            }))
            .child(setting_row(hxy_i18n::t("settings-columns"), self.number_row(NumericSetting::HexColumns, 130.0)))
            .child(setting_row(
                hxy_i18n::t("settings-byte-highlight"),
                toggle("settings-byte-highlight", s.byte_value_highlight, |s, v| s.byte_value_highlight = v),
            ))
            .child(setting_row(hxy_i18n::t("settings-byte-highlight-mode"), {
                let current = s.byte_highlight_mode;
                h_flex()
                    .gap_1()
                    .child(choice_button(
                        "settings-mode-background",
                        hxy_i18n::t("settings-byte-highlight-background"),
                        current == ByteHighlightMode::Background,
                        |s| s.byte_highlight_mode = ByteHighlightMode::Background,
                    ))
                    .child(choice_button(
                        "settings-mode-text",
                        hxy_i18n::t("settings-byte-highlight-text"),
                        current == ByteHighlightMode::Text,
                        |s| s.byte_highlight_mode = ByteHighlightMode::Text,
                    ))
            }))
            .child(setting_row(hxy_i18n::t("settings-byte-highlight-scheme"), {
                let current = s.byte_highlight_scheme;
                h_flex()
                    .gap_1()
                    .child(choice_button(
                        "settings-scheme-class",
                        hxy_i18n::t("settings-byte-highlight-scheme-class"),
                        current == ByteHighlightScheme::Class,
                        |s| s.byte_highlight_scheme = ByteHighlightScheme::Class,
                    ))
                    .child(choice_button(
                        "settings-scheme-value",
                        hxy_i18n::t("settings-byte-highlight-scheme-value"),
                        current == ByteHighlightScheme::Value,
                        |s| s.byte_highlight_scheme = ByteHighlightScheme::Value,
                    ))
            }))
            .child(setting_row(
                hxy_i18n::t("settings-minimap"),
                toggle("settings-minimap", s.show_minimap, |s, v| s.show_minimap = v),
            ))
            .child(setting_row(
                hxy_i18n::t("settings-minimap-colored"),
                // Mirrors egui's add_enabled_ui: inert until the
                // minimap itself is on.
                toggle("settings-minimap-colored", s.minimap_colored, |s, v| s.minimap_colored = v)
                    .disabled(!s.show_minimap),
            ))
            .child(setting_row(hxy_i18n::t("settings-offset-base"), {
                let current = s.offset_base;
                h_flex()
                    .gap_1()
                    .child(choice_button(
                        "settings-offset-hex",
                        hxy_i18n::t("gpui-settings-base-hex"),
                        current == OffsetBase::Hex,
                        |s| s.offset_base = OffsetBase::Hex,
                    ))
                    .child(choice_button(
                        "settings-offset-decimal",
                        hxy_i18n::t("gpui-settings-base-decimal"),
                        current == OffsetBase::Decimal,
                        |s| s.offset_base = OffsetBase::Decimal,
                    ))
            }))
            .child(setting_row(
                hxy_i18n::t("settings-numeric-format"),
                self.numeric_format_controls(FormatSlot::Main, &s),
            ))
            .child(setting_row(hxy_i18n::t("settings-template-value-format"), self.template_formats_section(&s, cx)))
            .child(setting_row(
                hxy_i18n::t("gpui-settings-palette-escape"),
                toggle("settings-palette-escape", s.palette_escape_pops_to_parent, |s, v| {
                    s.palette_escape_pops_to_parent = v;
                }),
            ))
            .child(setting_row_with_tooltip(
                "settings-compare-deadline-label",
                hxy_i18n::t("settings-compare-deadline"),
                hxy_i18n::t("settings-compare-deadline-tooltip"),
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(self.number_row(NumericSetting::CompareDeadlineMs, 140.0))
                    .child(div().text_color(muted).child(hxy_i18n::t("gpui-settings-unit-ms"))),
            ))
            .child(section_heading(hxy_i18n::t("settings-watch-header"), cx))
            .child(setting_row(hxy_i18n::t("settings-auto-reload"), {
                let current = s.auto_reload;
                let mut row = h_flex().gap_1();
                for (index, mode) in AutoReloadMode::ALL.into_iter().enumerate() {
                    row = row.child(choice_button(
                        ("settings-auto-reload", index),
                        hxy_i18n::t(mode.label_key()),
                        current == mode,
                        move |s| s.auto_reload = mode,
                    ));
                }
                row
            }))
            .child(setting_row_with_tooltip(
                "settings-poll-interval-label",
                hxy_i18n::t("settings-poll-interval"),
                hxy_i18n::t("settings-poll-interval-tooltip"),
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(self.number_row(NumericSetting::PollIntervalMs, 140.0))
                    .child(div().text_color(muted).child(hxy_i18n::t("gpui-settings-unit-ms"))),
            ))
            .child(setting_row_with_tooltip(
                "settings-poll-all-label",
                hxy_i18n::t("settings-poll-all"),
                hxy_i18n::t("settings-poll-all-tooltip"),
                toggle("settings-poll-all", s.file_poll_all, |s, v| s.file_poll_all = v),
            ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use hxy_settings::persist::SaveSink;
    use hxy_settings::persist::open_db_in;

    use super::*;
    use crate::settings::SettingsBoot;

    #[test]
    fn mode_flip_preserves_bases_and_threshold() {
        let threshold =
            NumericFormat::Threshold { small: NumericBase::Decimal, large: NumericBase::Hex, threshold: 512 };
        assert_eq!(to_always(threshold), NumericFormat::Always(NumericBase::Hex), "large base survives the flip");
        assert_eq!(to_always(NumericFormat::Always(NumericBase::Decimal)), NumericFormat::Always(NumericBase::Decimal));

        assert_eq!(
            to_threshold(NumericFormat::Always(NumericBase::Hex)),
            NumericFormat::Threshold { small: NumericBase::Decimal, large: NumericBase::Hex, threshold: 256 },
            "flip to threshold derives egui's defaults from the picked base",
        );
        assert_eq!(to_threshold(threshold), threshold, "already-threshold is untouched");
    }

    #[test]
    fn numeric_settings_clamp_to_their_egui_ranges() {
        assert_eq!(NumericSetting::HexColumns.clamp(0), 1);
        assert_eq!(NumericSetting::HexColumns.clamp(999), MAX_COLUMNS);
        assert_eq!(NumericSetting::CompareDeadlineMs.clamp(1), u64::from(RecomputeDeadline::MIN_MS));
        assert_eq!(NumericSetting::CompareDeadlineMs.clamp(u64::MAX), u64::from(RecomputeDeadline::MAX_MS));
        assert_eq!(NumericSetting::PollIntervalMs.clamp(0), 0, "0 stays 0: it means polling disabled");
        assert_eq!(NumericSetting::PollIntervalMs.clamp(u64::MAX), MAX_POLL_INTERVAL_MS);
        assert_eq!(NumericSetting::Threshold(FormatSlot::Main).clamp(0), 1);
    }

    #[test]
    fn commit_writes_each_target_field() {
        let mut s = AppSettings::default();
        NumericSetting::HexColumns.commit(&mut s, 24);
        assert_eq!(s.hex_columns.get(), 24);
        NumericSetting::CompareDeadlineMs.commit(&mut s, 5000);
        assert_eq!(s.compare_recompute_deadline.as_ms(), 5000);
        NumericSetting::PollIntervalMs.commit(&mut s, 1234);
        assert_eq!(s.file_poll_interval_ms, 1234);

        let slot = FormatSlot::Ty(IntValueType::U32);
        *slot_format_mut(&mut s, slot) = to_threshold(slot_format(&s, slot));
        NumericSetting::Threshold(slot).commit(&mut s, 4096);
        assert_eq!(
            slot_format(&s, slot),
            NumericFormat::Threshold { small: NumericBase::Decimal, large: NumericBase::Hex, threshold: 4096 },
        );
        // Committing a threshold to an Always-mode slot is a no-op (the
        // input is not rendered in that mode).
        let always_slot = FormatSlot::Ty(IntValueType::U8);
        NumericSetting::Threshold(always_slot).commit(&mut s, 4096);
        assert_eq!(slot_format(&s, always_slot), NumericFormat::Always(NumericBase::Hex));
    }

    /// The highlight-mode row's choice buttons run this exact mutation
    /// under `update_settings`; both directions flip the global.
    #[gpui::test]
    fn byte_highlight_mode_row_flips_the_global(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::settings::init(
                cx,
                SettingsBoot { settings: AppSettings::default(), sink: None, persist: None, failure: None },
            );
        });
        cx.update(|cx| {
            assert_eq!(crate::settings::settings(cx).byte_highlight_mode, ByteHighlightMode::Background);
        });
        cx.update(|cx| update_settings(cx, |s| s.byte_highlight_mode = ByteHighlightMode::Text));
        cx.update(|cx| {
            assert_eq!(crate::settings::settings(cx).byte_highlight_mode, ByteHighlightMode::Text);
        });
        cx.update(|cx| update_settings(cx, |s| s.byte_highlight_mode = ByteHighlightMode::Background));
        cx.update(|cx| {
            assert_eq!(crate::settings::settings(cx).byte_highlight_mode, ByteHighlightMode::Background);
        });
    }

    /// Driving a row's update fn (`commit` under `update_settings`, the
    /// exact production path of the change handler) mutates the global
    /// and persists through a real sqlite sink; an independent
    /// connection reads the new value back.
    #[gpui::test]
    fn row_commit_round_trips_through_global_and_sqlite(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let rt = Arc::new(tokio::runtime::Builder::new_current_thread().enable_all().build().expect("build runtime"));
        let pool = rt.block_on(open_db_in(dir.path())).expect("open db");
        cx.update(|cx| {
            crate::settings::init(
                cx,
                SettingsBoot {
                    settings: AppSettings::default(),
                    sink: Some(SaveSink::new(pool, rt.clone())),
                    persist: None,
                    failure: None,
                },
            );
        });

        cx.update(|cx| update_settings(cx, |s| NumericSetting::HexColumns.commit(s, 999)));

        cx.update(|cx| {
            assert_eq!(crate::settings::settings(cx).hex_columns.get(), 64, "committed value is clamped to 64");
        });
        let loaded = rt
            .block_on(async {
                let pool = open_db_in(dir.path()).await?;
                hxy_settings::persist::load_app_settings(&pool).await
            })
            .expect("reload")
            .expect("settings stored");
        assert_eq!(loaded.hex_columns.get(), 64, "the clamped value reached sqlite");
    }
}
