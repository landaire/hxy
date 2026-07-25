//! [`InspectorPanel`]: right-dock panel showing every registered
//! decoder's reading of the 16-byte window at the active file's caret.
//!
//! Decoding itself lives entirely in `hxy_panels::inspector` (shared
//! with the egui front end); this module only reads bytes, renders,
//! and tracks which file is active.
//!
//! Side docks expose no way to fetch a live panel entity back out of
//! `DockArea` once `DockArea::load` has rebuilt one from a persisted
//! layout (unlike the center tree, which `DockArea::items()` exposes).
//! So the workspace never holds a direct handle to this panel; instead
//! it publishes the active pane into the [`ActiveHexPane`] global, and
//! every `InspectorPanel` (freshly built or restored) self-subscribes
//! to it. This assumes a single live workspace per process, true of
//! this shell (one window, `main.rs`).

use std::sync::Arc;

use gpui::App;
use gpui::Context;
use gpui::Entity;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::Global;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::Rgba;
use gpui::SharedString;
use gpui::StatefulInteractiveElement;
use gpui::Styled;
use gpui::Subscription;
use gpui::Window;
use gpui::div;
use gpui::px;
use gpui_component::ActiveTheme;
use gpui_component::Selectable;
use gpui_component::button::Button;
use gpui_component::dock::Panel;
use gpui_component::dock::PanelEvent;
use gpui_component::dock::PanelInfo;
use gpui_component::dock::PanelState;
use gpui_component::h_flex;
use gpui_component::label::Label;
use gpui_component::v_flex;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_editor::HexEditor;
use hxy_panels::inspector::Decoded;
use hxy_panels::inspector::Decoder;
use hxy_panels::inspector::Endian;
use hxy_panels::inspector::InspectorState;
use hxy_panels::inspector::IntRadix;
use hxy_panels::inspector::default_decoders;
use hxy_view_gpui::HexPane;

/// Stable identifier for layout (de)serialization; must never change.
pub const INSPECTOR_PANEL_NAME: &str = "InspectorPanel";

/// Bytes read past the caret for the decoder table -- wide enough for
/// every built-in decoder (the widest is `Int128`/`UInt128` at 16
/// bytes). Mirrors the egui app's `inspector_caret_window`
/// (`crates/hxy/src/app/mod.rs`).
const WINDOW_LEN: u64 = 16;

/// The pane the inspector should decode bytes from, published by the
/// workspace whenever the active file tab changes. See the module doc
/// for why this is a global rather than a direct entity handle.
#[derive(Clone)]
pub(crate) struct ActiveHexPane(pub Option<Entity<HexPane>>);

impl Global for ActiveHexPane {}

fn active_hex_pane(cx: &App) -> Option<Entity<HexPane>> {
    cx.try_global::<ActiveHexPane>().and_then(|active| active.0.clone())
}

pub struct InspectorPanel {
    state: InspectorState,
    decoders: Vec<Arc<dyn Decoder>>,
    active_pane: Option<Entity<HexPane>>,
    /// Re-notifies this panel whenever the active pane's editor changes
    /// (selection move, edit), so the decoded window stays current.
    /// Re-established whenever the active pane itself changes.
    active_pane_observe: Option<Subscription>,
    _global_observe: Subscription,
    focus_handle: FocusHandle,
}

impl InspectorPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::with_state(InspectorState::default(), cx)
    }

    /// Rebuild from persisted [`PanelInfo`]; unrecognized/missing
    /// fields fall back to their `InspectorState::default()` value.
    pub fn restore(info: &PanelInfo, cx: &mut Context<Self>) -> Self {
        Self::with_state(state_from_info(info), cx)
    }

    fn with_state(state: InspectorState, cx: &mut Context<Self>) -> Self {
        let global_observe = cx.observe_global::<ActiveHexPane>(|this, cx| {
            this.set_active_pane(active_hex_pane(cx), cx);
        });
        let mut this = Self {
            state,
            decoders: default_decoders(),
            active_pane: None,
            active_pane_observe: None,
            _global_observe: global_observe,
            focus_handle: cx.focus_handle(),
        };
        let initial_pane = active_hex_pane(cx);
        this.set_active_pane(initial_pane, cx);
        this
    }

    fn set_active_pane(&mut self, pane: Option<Entity<HexPane>>, cx: &mut Context<Self>) {
        if self.active_pane.as_ref().map(Entity::entity_id) == pane.as_ref().map(Entity::entity_id) {
            return;
        }
        self.active_pane_observe = pane.as_ref().map(|pane| cx.observe(pane, |_, _, cx| cx.notify()));
        self.active_pane = pane;
        cx.notify();
    }

    /// The active pane's caret offset and up to [`WINDOW_LEN`] bytes
    /// starting there, or `None` when there's no active pane or no
    /// caret.
    fn caret_window(&self, cx: &App) -> Option<(u64, Vec<u8>)> {
        let pane = self.active_pane.as_ref()?;
        caret_window(pane.read(cx).editor())
    }

    fn set_endian(&mut self, endian: Endian, cx: &mut Context<Self>) {
        self.state.endian = endian;
        cx.notify();
    }

    fn set_radix(&mut self, radix: IntRadix, cx: &mut Context<Self>) {
        self.state.radix = radix;
        cx.notify();
    }

    fn endian_button(&self, endian: Endian, label_key: &str, id: &'static str, cx: &Context<Self>) -> Button {
        Button::new(id)
            .label(hxy_i18n::t(label_key))
            .compact()
            .selected(self.state.endian == endian)
            .on_click(cx.listener(move |this, _, _, cx| this.set_endian(endian, cx)))
    }

    fn radix_button(&self, radix: IntRadix, label_key: &str, id: &'static str, cx: &Context<Self>) -> Button {
        Button::new(id)
            .label(hxy_i18n::t(label_key))
            .compact()
            .selected(self.state.radix == radix)
            .on_click(cx.listener(move |this, _, _, cx| this.set_radix(radix, cx)))
    }

    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_2()
            .px_2()
            .py_1()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(Label::new(hxy_i18n::t("gpui-inspector-endian-label")).text_color(cx.theme().muted_foreground))
            .child(self.endian_button(Endian::Little, "gpui-inspector-endian-little", "inspector-endian-little", cx))
            .child(self.endian_button(Endian::Big, "gpui-inspector-endian-big", "inspector-endian-big", cx))
            .child(Label::new(hxy_i18n::t("gpui-inspector-radix-label")).text_color(cx.theme().muted_foreground))
            .child(self.radix_button(IntRadix::Decimal, "gpui-inspector-radix-decimal", "inspector-radix-decimal", cx))
            .child(self.radix_button(IntRadix::Hex, "gpui-inspector-radix-hex", "inspector-radix-hex", cx))
            .child(self.radix_button(IntRadix::Binary, "gpui-inspector-radix-binary", "inspector-radix-binary", cx))
    }

    fn render_rows(&self, bytes: &[u8], cx: &Context<Self>) -> impl IntoElement {
        v_flex().id("inspector-rows").flex_1().overflow_y_scroll().children(self.decoders.iter().map(|decoder| {
            let decoded = decoder.decode(bytes, self.state.endian, self.state.radix);
            h_flex()
                .justify_between()
                .gap_2()
                .px_2()
                .py_0p5()
                .child(Label::new(decoder.name().to_string()).text_color(cx.theme().muted_foreground))
                .child(render_decoded(decoded, cx))
        }))
    }
}

fn render_decoded(decoded: Option<Decoded>, cx: &App) -> gpui::AnyElement {
    match decoded {
        Some(Decoded::Text(text)) => Label::new(text).into_any_element(),
        Some(Decoded::Color { rgba, label }) => h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .size(px(12.0))
                    .rounded(px(3.0))
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(Rgba { r: f32::from(rgba[0]) / 255.0, g: f32::from(rgba[1]) / 255.0, b: f32::from(rgba[2]) / 255.0, a: f32::from(rgba[3]) / 255.0 }),
            )
            .child(Label::new(label))
            .into_any_element(),
        None => Label::new(hxy_i18n::t("gpui-inspector-decode-empty")).into_any_element(),
    }
}

/// Read up to [`WINDOW_LEN`] bytes starting at `editor`'s caret.
/// Mirrors the egui app's `inspector_caret_window`
/// (`crates/hxy/src/app/mod.rs`) so both front ends read the same
/// window from the same caret rule.
fn caret_window(editor: &HexEditor) -> Option<(u64, Vec<u8>)> {
    let caret = editor.selection()?.cursor.get();
    let src_len = editor.source().len().get();
    if src_len == 0 {
        return None;
    }
    if caret >= src_len {
        return Some((caret, Vec::new()));
    }
    let end = caret.saturating_add(WINDOW_LEN).min(src_len);
    let range = ByteRange::new(ByteOffset::new(caret), ByteOffset::new(end)).ok()?;
    let bytes = editor.source().read(range).ok()?;
    Some((caret, bytes))
}

fn endian_key(endian: Endian) -> &'static str {
    match endian {
        Endian::Little => "little",
        Endian::Big => "big",
    }
}

fn radix_key(radix: IntRadix) -> &'static str {
    match radix {
        IntRadix::Decimal => "decimal",
        IntRadix::Hex => "hex",
        IntRadix::Binary => "binary",
    }
}

/// Rebuild `InspectorState` from persisted JSON. A missing field is a
/// normal older-layout/first-open case (silently defaulted, same rule
/// `InspectorState::default()` already applies); a present-but-
/// unrecognized value is corrupt data and gets a warning before
/// falling back, matching `persist::load`'s convention for malformed
/// persisted state.
fn state_from_info(info: &PanelInfo) -> InspectorState {
    let mut state = InspectorState::default();
    let PanelInfo::Panel(value) = info else { return state };
    if let Some(endian) = value.get("endian").and_then(|v| v.as_str()) {
        state.endian = match endian {
            "little" => Endian::Little,
            "big" => Endian::Big,
            other => {
                tracing::warn!(endian = other, "restore: unrecognized inspector endian; using default");
                Endian::default()
            }
        };
    }
    if let Some(radix) = value.get("radix").and_then(|v| v.as_str()) {
        state.radix = match radix {
            "decimal" => IntRadix::Decimal,
            "hex" => IntRadix::Hex,
            "binary" => IntRadix::Binary,
            other => {
                tracing::warn!(radix = other, "restore: unrecognized inspector radix; using default");
                IntRadix::default()
            }
        };
    }
    state
}

impl Panel for InspectorPanel {
    fn panel_name(&self) -> &'static str {
        INSPECTOR_PANEL_NAME
    }

    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(hxy_i18n::t("tab-inspector"))
    }

    /// The inspector is a persistent workspace utility toggled by
    /// `cmd-i`, not user-closable: a tab close removes the panel from
    /// the dock (`Dock::remove_panel`) with no recovery, unlike
    /// closing a dock via `toggle_dock`, which only hides it.
    fn closable(&self, _cx: &App) -> bool {
        false
    }

    /// Persist endian/radix so relaunching with the panel open restores
    /// the same decoding settings. Open/closed state is the dock's own
    /// concern (`DockState::open`), not this panel's.
    fn dump(&self, _cx: &App) -> PanelState {
        let mut state = PanelState::new(self);
        state.info = PanelInfo::panel(serde_json::json!({
            "endian": endian_key(self.state.endian),
            "radix": radix_key(self.state.radix),
        }));
        state
    }
}

impl Focusable for InspectorPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for InspectorPanel {}

impl Render for InspectorPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let window_bytes = self.caret_window(cx);

        let body = match &window_bytes {
            Some((_, bytes)) => self.render_rows(bytes, cx).into_any_element(),
            None => div()
                .px_2()
                .py_1()
                .text_color(cx.theme().muted_foreground)
                .child(hxy_i18n::t("gpui-inspector-no-caret"))
                .into_any_element(),
        };

        v_flex().size_full().bg(cx.theme().background).child(self.render_toolbar(cx)).child(body)
    }
}

#[cfg(test)]
mod tests {
    use gpui::AppContext;
    use gpui::TestAppContext;
    use hxy_core::HexSource;
    use hxy_core::MemorySource;
    use hxy_core::Selection;

    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
    }

    fn source(bytes: Vec<u8>) -> Arc<dyn HexSource> {
        Arc::new(MemorySource::new(bytes))
    }

    /// Moving the caret (via a real keystroke on the file's pane, not a
    /// direct field poke) changes the inspector's decoded window --
    /// proves the `ActiveHexPane` global wiring keeps the panel fresh
    /// without the workspace holding a direct handle to it.
    #[gpui::test]
    fn caret_move_updates_decoded_window(cx: &mut TestAppContext) {
        setup(cx);
        let bytes: Vec<u8> = (0u8..64).collect();
        let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(bytes), cx));
        let inspector = cx.update(|_window, cx| {
            cx.set_global(ActiveHexPane(Some(pane.clone())));
            cx.new(InspectorPanel::new)
        });
        cx.run_until_parked();

        cx.update(|window, cx| {
            let handle = pane.read(cx).focus_handle(cx);
            window.focus(&handle);
        });
        cx.run_until_parked();

        assert_eq!(inspector.read_with(cx, |insp, cx| insp.caret_window(cx)), None, "no caret yet");

        // Arrow-down establishes a caret at the start of row 1.
        cx.simulate_keystrokes("down");
        let (offset, first_bytes) = inspector.read_with(cx, |insp, cx| insp.caret_window(cx)).expect("caret window");
        assert_eq!(offset, 16);
        assert_eq!(first_bytes, (16u8..32).collect::<Vec<u8>>());

        // Another arrow-down moves the caret and the decoded window
        // with it.
        cx.simulate_keystrokes("down");
        let (offset, next_bytes) = inspector.read_with(cx, |insp, cx| insp.caret_window(cx)).expect("caret window");
        assert_eq!(offset, 32);
        assert_eq!(next_bytes, (32u8..48).collect::<Vec<u8>>());
    }

    /// Real startup constructs the inspector (via `ensure_inspector_dock`)
    /// before the workspace has ever published an active pane (`reconcile`
    /// -> `set_active_file` runs after). Build the panel with the global
    /// still unset, then set it afterward, mirroring that order -- proves
    /// the panel doesn't miss the very first active-pane notification.
    #[gpui::test]
    fn inspector_picks_up_active_pane_set_after_construction(cx: &mut TestAppContext) {
        setup(cx);
        let bytes: Vec<u8> = (0u8..32).collect();
        let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(bytes), cx));
        pane.update(cx, |pane, _| pane.editor_mut().set_selection(Some(Selection::caret(ByteOffset::new(4)))));

        let inspector = cx.update(|_window, cx| {
            assert!(!cx.has_global::<ActiveHexPane>(), "global must be unset at construction, like at boot");
            cx.new(InspectorPanel::new)
        });
        cx.run_until_parked();
        assert_eq!(inspector.read_with(cx, |insp, cx| insp.caret_window(cx)), None, "no active pane yet");

        cx.update(|_window, cx| cx.set_global(ActiveHexPane(Some(pane.clone()))));
        cx.run_until_parked();

        let (offset, window_bytes) = inspector.read_with(cx, |insp, cx| insp.caret_window(cx)).expect("caret window");
        assert_eq!(offset, 4);
        assert_eq!(window_bytes, (4u8..20).collect::<Vec<u8>>());
    }

    /// Flipping endian changes the decoded `UInt16` value for a fixed
    /// caret window -- the panel routes through `hxy_panels`' decoder
    /// unchanged, so a byte-order-dependent decoder proves the toolbar
    /// setting actually reaches `decode()`.
    #[gpui::test]
    fn endian_flip_changes_u16_decode(cx: &mut TestAppContext) {
        setup(cx);
        let (pane, cx) = cx.add_window_view(|_, cx| HexPane::new(source(vec![0x01, 0x80]), cx));
        let inspector = cx.update(|_window, cx| {
            cx.set_global(ActiveHexPane(Some(pane.clone())));
            cx.new(InspectorPanel::new)
        });
        cx.run_until_parked();
        pane.update(cx, |pane, _| pane.editor_mut().set_selection(Some(Selection::caret(ByteOffset::new(0)))));
        cx.run_until_parked();

        let uint16 = default_decoders().into_iter().find(|d| d.name() == "UInt16").unwrap();

        let little = inspector
            .read_with(cx, |insp, cx| {
                let (_, bytes) = insp.caret_window(cx).unwrap();
                uint16.decode(&bytes, insp.state.endian, insp.state.radix)
            })
            .unwrap();
        assert_eq!(little.label(), "32769", "little-endian: 0x8001 read LE");

        inspector.update(cx, |insp, cx| insp.set_endian(Endian::Big, cx));
        cx.run_until_parked();

        let big = inspector
            .read_with(cx, |insp, cx| {
                let (_, bytes) = insp.caret_window(cx).unwrap();
                uint16.decode(&bytes, insp.state.endian, insp.state.radix)
            })
            .unwrap();
        assert_eq!(big.label(), "384", "big-endian: 0x0180 read BE");
    }

    /// Restoring from persisted `PanelInfo` round-trips endian/radix;
    /// unrecognized/absent fields fall back to defaults.
    #[test]
    fn state_from_info_round_trips_endian_and_radix() {
        let info = PanelInfo::panel(serde_json::json!({ "endian": "big", "radix": "hex" }));
        let state = state_from_info(&info);
        assert_eq!(state.endian, Endian::Big);
        assert_eq!(state.radix, IntRadix::Hex);

        let default_info = PanelInfo::panel(serde_json::json!({}));
        let default_state = state_from_info(&default_info);
        assert_eq!(default_state.endian, Endian::Little);
        assert_eq!(default_state.radix, IntRadix::Decimal);

        // Corrupt/unrecognized values fall back to the default rather
        // than propagating garbage or panicking.
        let corrupt_info = PanelInfo::panel(serde_json::json!({ "endian": "sideways", "radix": "roman" }));
        let corrupt_state = state_from_info(&corrupt_info);
        assert_eq!(corrupt_state.endian, Endian::Little);
        assert_eq!(corrupt_state.radix, IntRadix::Decimal);
    }

    #[test]
    fn caret_window_reads_up_to_16_bytes_past_the_caret() {
        let mut editor = HexEditor::new(source((0u8..64).collect()));
        assert_eq!(caret_window(&editor), None, "no selection yet");

        editor.set_selection(Some(Selection::caret(ByteOffset::new(10))));
        let (offset, bytes) = caret_window(&editor).unwrap();
        assert_eq!(offset, 10);
        assert_eq!(bytes, (10u8..26).collect::<Vec<u8>>());

        // Near EOF the window clips to the source's remaining length.
        editor.set_selection(Some(Selection::caret(ByteOffset::new(60))));
        let (offset, bytes) = caret_window(&editor).unwrap();
        assert_eq!(offset, 60);
        assert_eq!(bytes, (60u8..64).collect::<Vec<u8>>());
    }
}
