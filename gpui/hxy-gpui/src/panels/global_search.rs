//! [`GlobalSearchPanel`]: singleton center-dock tab that searches every
//! open file for a pattern and lists every hit.
//!
//! Query encoding and the byte scan itself live in `hxy_panels::search`
//! (shared with the egui front end's `crates/hxy/src/search/global.rs`,
//! generic over the file-id type -- specialized here to `Entity<FilePanel>`
//! since gpui has no `FileId` analog). This file only owns the query
//! controls, runs the scan on the background executor, and renders the
//! aggregated match list.
//!
//! Jumping to a result (tab focus) is NOT done here: `GlobalSearchPanel`
//! has no handle to the dock/`Workspace` that could bring another file's
//! tab to the front (same constraint documented on
//! [`StringsJumped`](super::strings::StringsJumped)). `jump_to_row`
//! applies the selection + scroll on the target file's pane directly (it
//! does own that handle, via the match's `file_id`) and emits
//! [`GlobalSearchJumped`]; `Workspace::on_global_search_jumped` reacts by
//! focusing that file's tab.
//!
//! Search order: every file in [`OpenFilePanels`]' published order, which
//! mirrors `Workspace::open_files` -- insertion order, with a reopened or
//! refocused file moved to the end of that registry. This differs from
//! the egui app's `FileId`-ascending sort (gpui has no orderable file id
//! to sort by), but is equally stable frame-to-frame for an unchanged set
//! of open files. Within one file, matches are ascending-offset (from
//! `find_all`), matching egui exactly.

use std::sync::Arc;

use gpui::App;
use gpui::AppContext;
use gpui::Context;
use gpui::Entity;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::SharedString;
use gpui::Styled;
use gpui::Subscription;
use gpui::Task;
use gpui::WeakEntity;
use gpui::Window;
use gpui::div;
use gpui::px;
use gpui_component::ActiveTheme;
use gpui_component::Disableable;
use gpui_component::Icon;
use gpui_component::IconName;
use gpui_component::Selectable;
use gpui_component::Sizable;
use gpui_component::button::Button;
use gpui_component::checkbox::Checkbox;
use gpui_component::dock::BasePanel;
use gpui_component::dock::Panel;
use gpui_component::dock::PanelEvent;
use gpui_component::h_flex;
use gpui_component::input::Input;
use gpui_component::input::InputEvent;
use gpui_component::input::InputState;
use gpui_component::label::Label;
use gpui_component::table::Column;
use gpui_component::table::DataTable;
use gpui_component::table::TableDelegate;
use gpui_component::table::TableEvent;
use gpui_component::table::TableState;
use gpui_component::v_flex;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_core::Selection;
use hxy_panels::search::EncodeError;
use hxy_panels::search::Endian;
use hxy_panels::search::NumberWidth;
use hxy_panels::search::SearchKind;
use hxy_panels::search::encode_query;
use hxy_panels::search::find_all;

use super::FilePanel;
use super::strings::OpenFilePanels;

/// Stable identifier for layout (de)serialization; must never change.
pub const GLOBAL_SEARCH_PANEL_NAME: &str = "GlobalSearchPanel";

/// `hxy_panels::search::GlobalSearchState` specialized to gpui's file
/// identity: the `FilePanel` entity itself (there is no `FileId`-style
/// orderable id in this port -- see the module doc).
pub type GlobalSearchState = hxy_panels::search::GlobalSearchState<Entity<FilePanel>>;
pub type GlobalMatch = hxy_panels::search::GlobalMatch<Entity<FilePanel>>;

/// Every open [`FilePanel`], mirroring `checksums`/`strings`/`entropy`'s
/// own copy of this accessor (see their module docs for why it's
/// duplicated per panel rather than shared: each is a tiny read of the
/// same global, not worth a cross-module coupling).
fn open_file_panels(cx: &App) -> Vec<Entity<FilePanel>> {
    match cx.try_global::<OpenFilePanels>() {
        Some(g) => g.0.clone(),
        None => Vec::new(),
    }
}

/// Emitted by [`GlobalSearchPanel::jump_to_row`] after applying a jump
/// on the target file's pane. See the module doc for why the tab-focus
/// half of the jump happens in the workspace's subscriber instead.
#[derive(Clone)]
pub struct GlobalSearchJumped {
    pub file: Entity<FilePanel>,
}

impl EventEmitter<GlobalSearchJumped> for GlobalSearchPanel {}

pub struct GlobalSearchPanel {
    focus_handle: FocusHandle,
    state: GlobalSearchState,
    query_input: Entity<InputState>,
    _query_sub: Subscription,
    running: bool,
    /// Bumped by [`Self::refresh`] and [`Self::run`]'s empty-pattern
    /// branch. A [`Self::run`] scan captures the value at launch and
    /// compares it on completion: a mismatch means the query/settings
    /// changed while the scan was in flight, so its results are for a
    /// pattern the user has already moved past and are discarded
    /// rather than clobbering what's on screen.
    search_serial: u64,
    _search_task: Option<Task<()>>,
    table: Entity<TableState<GlobalSearchTableDelegate>>,
    _table_sub: Subscription,
}

impl GlobalSearchPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query_input = cx.new(|cx| InputState::new(window, cx));
        let query_sub = cx.subscribe_in(&query_input, window, Self::on_query_event);
        let weak = cx.entity().downgrade();
        let table = cx.new(|cx| TableState::new(GlobalSearchTableDelegate::new(weak), window, cx));
        let table_sub = cx.subscribe(&table, Self::on_table_event);
        Self {
            focus_handle: cx.focus_handle(),
            state: GlobalSearchState::default(),
            query_input,
            _query_sub: query_sub,
            running: false,
            search_serial: 0,
            _search_task: None,
            table,
            _table_sub: table_sub,
        }
    }

    fn on_query_event(
        &mut self,
        _input: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                self.state.query_state.query = self.query_input.read(cx).value().to_string();
                self.refresh(cx);
            }
            InputEvent::PressEnter { .. } => self.run(cx),
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    fn on_table_event(
        &mut self,
        _table: Entity<TableState<GlobalSearchTableDelegate>>,
        event: &TableEvent,
        cx: &mut Context<Self>,
    ) {
        if let TableEvent::SelectRow(row_ix) = event {
            self.jump_to_row(*row_ix, cx);
        }
    }

    /// Re-derive the pattern from the current query/settings and drop
    /// the now-stale aggregated match list -- the user must explicitly
    /// re-run to see fresh results (egui parity: `GlobalSearchEvent::Refresh`
    /// never auto-rescans).
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.state.query_state.refresh_pattern();
        self.state.matches.clear();
        self.state.active_idx = None;
        self.search_serial = self.search_serial.wrapping_add(1);
        self.table.update(cx, |t, cx| t.refresh(cx));
        cx.notify();
    }

    fn set_kind(&mut self, kind: SearchKind, cx: &mut Context<Self>) {
        self.state.query_state.kind = kind;
        self.refresh(cx);
    }

    fn set_width(&mut self, width: NumberWidth, cx: &mut Context<Self>) {
        self.state.query_state.width = width;
        self.refresh(cx);
    }

    fn set_signed(&mut self, signed: bool, cx: &mut Context<Self>) {
        self.state.query_state.signed = signed;
        self.refresh(cx);
    }

    fn set_endian(&mut self, endian: Endian, cx: &mut Context<Self>) {
        self.state.query_state.endian = endian;
        self.refresh(cx);
    }

    /// Re-run the scan over every open file's source on the background
    /// executor, replacing the aggregated match list once it lands.
    /// `find_all` itself never runs on the UI thread -- it executes
    /// inside `cx.background_spawn`, mirroring every other analysis
    /// panel in this crate (checksums/entropy/strings).
    pub(crate) fn run(&mut self, cx: &mut Context<Self>) {
        self.state.query_state.refresh_pattern();
        let Some(pattern) = self.state.query_state.pattern.clone() else {
            self.state.matches.clear();
            self.state.active_idx = None;
            self.search_serial = self.search_serial.wrapping_add(1);
            self.table.update(cx, |t, cx| t.refresh(cx));
            cx.notify();
            return;
        };
        if self.running {
            return;
        }
        self.search_serial = self.search_serial.wrapping_add(1);
        let serial = self.search_serial;
        let files = open_file_panels(cx);
        let sources: Vec<Arc<dyn HexSource>> =
            files.iter().map(|f| f.read(cx).pane().read(cx).editor().source().clone()).collect();
        self.running = true;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let per_file: Vec<Vec<u64>> = cx
                .background_spawn(async move {
                    sources
                        .iter()
                        .map(|source| {
                            let bounds = ByteRange::new(ByteOffset::new(0), ByteOffset::new(source.len().get()))
                                .expect("0 <= len");
                            find_all(source.as_ref(), &pattern, bounds)
                        })
                        .collect()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.running = false;
                this._search_task = None;
                // The query/settings changed (via `refresh`) while this
                // scan was in flight -- its results are for a pattern
                // the user has already moved past. Drop them; `refresh`
                // already cleared `matches` for the current query.
                if this.search_serial != serial {
                    return;
                }
                let mut matches = Vec::new();
                for (file, offsets) in files.into_iter().zip(per_file) {
                    for offset in offsets {
                        matches.push(GlobalMatch { file_id: file.clone(), offset });
                    }
                }
                this.state.active_idx = if matches.is_empty() { None } else { Some(0) };
                this.state.matches = matches;
                this.table.update(cx, |t, cx| t.refresh(cx));
                cx.notify();
            });
        });
        self._search_task = Some(task);
    }

    /// Set the target file's selection to the match range and scroll it
    /// into view, then emit [`GlobalSearchJumped`] so the workspace can
    /// bring that file's tab to the front. `pub(crate)` so the
    /// workspace's own tests can drive a jump directly, mirroring
    /// `StringsPanel::jump_to_row`.
    pub(crate) fn jump_to_row(&mut self, row_ix: usize, cx: &mut Context<Self>) {
        let Some(m) = self.state.matches.get(row_ix).cloned() else { return };
        let Some(pattern) = self.state.query_state.pattern.clone() else { return };
        // `matches` holds a strong `Entity<FilePanel>`, so the handle
        // stays valid even after the file's tab closes. Skip rather
        // than resurrect a closed file as a new tab -- mirrors egui's
        // `JumpTo`, which no-ops when `app.files.get_mut(&m.file_id)`
        // comes back empty.
        if !open_file_panels(cx).iter().any(|f| f.entity_id() == m.file_id.entity_id()) {
            return;
        }
        let end_inclusive = m.offset.saturating_add(pattern.len() as u64).saturating_sub(1);
        let pane = m.file_id.read(cx).pane().clone();
        pane.update(cx, |pane, cx| {
            pane.editor_mut().set_selection(Some(Selection {
                anchor: ByteOffset::new(m.offset),
                cursor: ByteOffset::new(end_inclusive),
            }));
            pane.editor_mut().set_scroll_to_byte(ByteOffset::new(m.offset));
            pane.sync_pending_scroll(cx);
        });
        self.state.active_idx = Some(row_ix);
        cx.emit(GlobalSearchJumped { file: m.file_id });
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> &GlobalSearchState {
        &self.state
    }

    /// Test-only shortcut for driving [`Self::run`] without going
    /// through the real `InputState` typing flow (which needs a
    /// focused window). `pub(crate)` so the workspace's own
    /// integration tests can set up a query before calling `run`.
    #[cfg(test)]
    pub(crate) fn set_query_for_test(&mut self, kind: SearchKind, query: String) {
        self.state.query_state.kind = kind;
        self.state.query_state.query = query;
    }

    /// Test-only hook onto the real [`Self::refresh`] path (the one
    /// `search_serial`-bumping code path a query/settings edit runs
    /// through), for tests that need to simulate "the user edited the
    /// query" without a focused `InputState`.
    #[cfg(test)]
    pub(crate) fn refresh_for_test(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
    }

    fn kind_button(&self, kind: SearchKind, label_key: &str, id: &'static str, cx: &Context<Self>) -> Button {
        Button::new(id)
            .label(hxy_i18n::t(label_key))
            .compact()
            .selected(self.state.query_state.kind == kind)
            .on_click(cx.listener(move |this, _, _window, cx| this.set_kind(kind, cx)))
    }

    fn width_button(&self, width: NumberWidth, cx: &Context<Self>) -> Button {
        let id: &'static str = match width {
            NumberWidth::W8 => "global-search-width-8",
            NumberWidth::W16 => "global-search-width-16",
            NumberWidth::W32 => "global-search-width-32",
            NumberWidth::W64 => "global-search-width-64",
        };
        let bits = (width.bytes() * 8).to_string();
        Button::new(id)
            .label(hxy_i18n::t_args("search-number-width", &[("bits", &bits)]))
            .compact()
            .selected(self.state.query_state.width == width)
            .on_click(cx.listener(move |this, _, _window, cx| this.set_width(width, cx)))
    }

    fn endian_button(&self, endian: Endian, label_key: &str, id: &'static str, cx: &Context<Self>) -> Button {
        Button::new(id)
            .label(hxy_i18n::t(label_key))
            .compact()
            .selected(self.state.query_state.endian == endian)
            .on_click(cx.listener(move |this, _, _window, cx| this.set_endian(endian, cx)))
    }

    fn render_status(&self, cx: &Context<Self>) -> gpui::AnyElement {
        // The shared `SearchState` stores the encode error as a Display
        // string; re-derive the typed variant here (matching on it, never
        // parsing the text) so the message can be localized.
        if self.state.query_state.error.is_some() {
            let qs = &self.state.query_state;
            let message = match encode_query(qs.kind, &qs.query, qs.width, qs.signed, qs.endian) {
                Err(err) => hxy_i18n::t(search_error_key(&err)),
                Ok(_) => hxy_i18n::t("gpui-global-search-error-generic"),
            };
            return Label::new(message).text_color(cx.theme().danger).into_any_element();
        }
        if self.running {
            return Label::new(hxy_i18n::t("gpui-global-search-running"))
                .text_color(cx.theme().muted_foreground)
                .into_any_element();
        }
        Label::new(hxy_i18n::t_args(
            "gpui-global-search-match-count",
            &[("count", &self.state.matches.len().to_string())],
        ))
        .text_color(cx.theme().muted_foreground)
        .into_any_element()
    }

    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut row = h_flex()
            .gap_2()
            .items_center()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(Label::new(hxy_i18n::t("search-find-label")))
            .child(self.kind_button(SearchKind::Text, "search-kind-text", "global-search-kind-text-btn", cx))
            .child(self.kind_button(
                SearchKind::HexBytes,
                "search-kind-hex-bytes",
                "global-search-kind-hex-bytes-btn",
                cx,
            ))
            .child(self.kind_button(SearchKind::Number, "search-kind-number", "global-search-kind-number-btn", cx));

        if matches!(self.state.query_state.kind, SearchKind::Number) {
            row = row
                .child(self.width_button(NumberWidth::W8, cx))
                .child(self.width_button(NumberWidth::W16, cx))
                .child(self.width_button(NumberWidth::W32, cx))
                .child(self.width_button(NumberWidth::W64, cx))
                .child(
                    Checkbox::new("global-search-signed")
                        .label(hxy_i18n::t("search-signed"))
                        .checked(self.state.query_state.signed)
                        .on_click(cx.listener(|this, checked: &bool, _window, cx| this.set_signed(*checked, cx))),
                )
                .child(self.endian_button(
                    Endian::Little,
                    "search-endian-little",
                    "global-search-endian-little-btn",
                    cx,
                ))
                .child(self.endian_button(Endian::Big, "search-endian-big", "global-search-endian-big-btn", cx));
        }

        row.child(div().w(px(220.0)).child(Input::new(&self.query_input)))
            .child(
                Button::new("global-search-run")
                    .icon(IconName::Search)
                    .tooltip(hxy_i18n::t("gpui-global-search-run-tooltip"))
                    .compact()
                    .loading(self.running)
                    .disabled(self.running)
                    .on_click(cx.listener(|this, _, _window, cx| this.run(cx))),
            )
            .child(self.render_status(cx))
    }
}

impl BasePanel for GlobalSearchPanel {
    fn panel_name(&self) -> &'static str {
        GLOBAL_SEARCH_PANEL_NAME
    }
}

impl Panel for GlobalSearchPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Search prefix mirrors egui's MAGNIFYING_GLASS. gpui-component
        // 0.5.1 surfaces this element only in single-panel title-bar
        // mode; beside sibling tabs the TabBar renders the plain
        // `tab_name` text (a SharedString with no icon slot), which is
        // also what the dock pane-picker labels rows with.
        h_flex()
            .gap_1()
            .items_center()
            .child(Icon::new(IconName::Search).small())
            .child(SharedString::from(hxy_i18n::t("tab-search-results")))
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(hxy_i18n::t("tab-search-results")))
    }
}

impl Focusable for GlobalSearchPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for GlobalSearchPanel {}

impl Render for GlobalSearchPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(self.render_toolbar(cx))
            .child(div().flex_1().min_h_0().child(DataTable::new(&self.table)))
    }
}

pub struct GlobalSearchTableDelegate {
    columns: [Column; 2],
    panel: WeakEntity<GlobalSearchPanel>,
}

impl GlobalSearchTableDelegate {
    fn new(panel: WeakEntity<GlobalSearchPanel>) -> Self {
        Self {
            columns: [
                Column::new("file", hxy_i18n::t("gpui-global-search-col-file")).width(px(260.0)),
                Column::new("offset", hxy_i18n::t("gpui-global-search-col-offset")).width(px(120.0)),
            ],
            panel,
        }
    }
}

impl TableDelegate for GlobalSearchTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, cx: &App) -> usize {
        self.panel.upgrade().map(|p| p.read(cx).state.matches.len()).unwrap_or(0)
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(panel) = self.panel.upgrade() else {
            return div().into_any_element();
        };
        let panel_ref = panel.read(cx);
        let Some(m) = panel_ref.state.matches.get(row_ix).cloned() else {
            return div().into_any_element();
        };
        match col_ix {
            0 => {
                let name = m.file_id.read(cx).tab_name(cx).map(|s| s.to_string()).unwrap_or_default();
                div().child(name).into_any_element()
            }
            // Row click (not this cell specifically) does the jump --
            // see `on_table_event` -- so this only needs link styling.
            1 => div().text_color(cx.theme().primary).child(format!("0x{:X}", m.offset)).into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn render_empty(&mut self, _window: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let text = match self.panel.upgrade() {
            Some(panel) => {
                let panel = panel.read(cx);
                if panel.running {
                    hxy_i18n::t("gpui-global-search-running")
                } else {
                    hxy_i18n::t("gpui-global-search-empty")
                }
            }
            None => String::new(),
        };
        h_flex().size_full().justify_center().text_color(cx.theme().muted_foreground).child(text)
    }
}

/// The i18n key for a query-encode error, matched on the typed variant.
fn search_error_key(err: &EncodeError) -> &'static str {
    match err {
        EncodeError::Empty => "gpui-global-search-error-empty",
        EncodeError::BadHex(_) => "gpui-global-search-error-bad-hex",
        EncodeError::BadNumber { .. } => "gpui-global-search-error-bad-number",
        EncodeError::NumberOverflow { .. } => "gpui-global-search-error-overflow",
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use hxy_core::MemorySource;

    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
    }

    /// Every `EncodeError` variant maps to a distinct, resolvable i18n key
    /// -- matched on the typed variant, never parsed from Display text.
    #[test]
    fn search_error_key_maps_each_variant() {
        let cases = [
            (EncodeError::Empty, "gpui-global-search-error-empty"),
            (EncodeError::BadHex("zz".into()), "gpui-global-search-error-bad-hex"),
            (EncodeError::BadNumber { input: "zz".into(), radix: 16 }, "gpui-global-search-error-bad-number"),
            (
                EncodeError::NumberOverflow { value: "999".into(), bytes: 1, sign: "unsigned integer" },
                "gpui-global-search-error-overflow",
            ),
        ];
        for (err, key) in cases {
            assert_eq!(search_error_key(&err), key);
            assert!(!hxy_i18n::t(key).is_empty(), "{key} resolves to a localized string");
        }
    }

    fn source(bytes: Vec<u8>) -> Arc<dyn HexSource> {
        Arc::new(MemorySource::new(bytes))
    }

    fn build(cx: &mut TestAppContext) -> (Entity<GlobalSearchPanel>, &mut gpui::VisualTestContext) {
        let window = cx.add_window(|window, cx| {
            let panel = cx.new(|cx| GlobalSearchPanel::new(window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<GlobalSearchPanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        (panel, vcx)
    }

    fn file_panel(cx: &mut gpui::VisualTestContext, path: &str, bytes: Vec<u8>) -> Entity<FilePanel> {
        cx.update(|window, cx| {
            cx.new(|cx| FilePanel::new(source(bytes), Some(std::path::PathBuf::from(path)), window, cx))
        })
    }

    /// Two fixture files sharing a pattern aggregate into one ordered
    /// match list: file A's hits first (ascending offset), then file
    /// B's -- the open-file registry order the panel searched in.
    #[gpui::test]
    fn run_aggregates_matches_across_open_files_in_open_order(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);

        let a = file_panel(cx, "/tmp/hxy-global-search-a.bin", b"\x00DEAD\x00DEAD\x00".to_vec());
        let b = file_panel(cx, "/tmp/hxy-global-search-b.bin", b"\x00\x00DEAD\x00".to_vec());
        cx.update(|_window, cx| cx.set_global(OpenFilePanels(vec![a.clone(), b.clone()])));

        panel.update(cx, |p, cx| {
            p.state.query_state.kind = SearchKind::Text;
            p.state.query_state.query = "DEAD".to_string();
            p.run(cx);
        });
        cx.run_until_parked();

        let matches = panel.read_with(cx, |p, _| p.state().matches.clone());
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].file_id.entity_id(), a.entity_id());
        assert_eq!(matches[0].offset, 1);
        assert_eq!(matches[1].file_id.entity_id(), a.entity_id());
        assert_eq!(matches[1].offset, 6);
        assert_eq!(matches[2].file_id.entity_id(), b.entity_id());
        assert_eq!(matches[2].offset, 2);
        assert!(!panel.read_with(cx, |p, _| p.running));
    }

    /// A query with no matches anywhere yields an empty aggregated list,
    /// not an error.
    #[gpui::test]
    fn run_with_no_hits_yields_empty_matches(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        let a = file_panel(cx, "/tmp/hxy-global-search-empty.bin", vec![0u8; 16]);
        cx.update(|_window, cx| cx.set_global(OpenFilePanels(vec![a])));

        panel.update(cx, |p, cx| {
            p.state.query_state.kind = SearchKind::Text;
            p.state.query_state.query = "DEAD".to_string();
            p.run(cx);
        });
        cx.run_until_parked();

        assert!(panel.read_with(cx, |p, _| p.state().matches.is_empty()));
    }

    /// Clicking a result row sets the OWNING file's selection (not the
    /// panel's own, which doesn't have one) to the match range.
    #[gpui::test]
    fn jump_to_row_sets_target_files_selection(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        let a = file_panel(cx, "/tmp/hxy-global-search-jump-a.bin", b"\x00hello\x00".to_vec());
        let b = file_panel(cx, "/tmp/hxy-global-search-jump-b.bin", b"\x00hello\x00".to_vec());
        cx.update(|_window, cx| cx.set_global(OpenFilePanels(vec![a.clone(), b.clone()])));

        panel.update(cx, |p, cx| {
            p.state.query_state.kind = SearchKind::Text;
            p.state.query_state.query = "hello".to_string();
            p.run(cx);
        });
        cx.run_until_parked();

        // Jump to B's match (index 1: A's hit comes first).
        panel.update(cx, |p, cx| p.jump_to_row(1, cx));

        let selection = b.read_with(cx, |f, cx| f.pane().read(cx).editor().selection());
        assert_eq!(selection, Some(Selection { anchor: ByteOffset::new(1), cursor: ByteOffset::new(5) }));
        // A's pane must be untouched.
        let a_selection = a.read_with(cx, |f, cx| f.pane().read(cx).editor().selection());
        assert_eq!(a_selection, None);
    }

    /// A match whose file closed after the scan ran (dropped out of the
    /// open-file registry, but still alive as a strong handle inside
    /// `matches`) must be skipped, not resurrected as a new tab.
    #[gpui::test]
    fn jump_to_row_skips_a_match_whose_file_has_closed(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        let a = file_panel(cx, "/tmp/hxy-global-search-closed-a.bin", b"\x00hello\x00".to_vec());
        let b = file_panel(cx, "/tmp/hxy-global-search-closed-b.bin", b"\x00hello\x00".to_vec());
        cx.update(|_window, cx| cx.set_global(OpenFilePanels(vec![a.clone(), b.clone()])));

        panel.update(cx, |p, cx| {
            p.state.query_state.kind = SearchKind::Text;
            p.state.query_state.query = "hello".to_string();
            p.run(cx);
        });
        cx.run_until_parked();

        // Establish a known active row (B, index 1) before A closes.
        panel.update(cx, |p, cx| p.jump_to_row(1, cx));
        assert_eq!(panel.read_with(cx, |p, _| p.state().active_idx), Some(1));

        // A closes: it drops out of the published open-file registry.
        cx.update(|_window, cx| cx.set_global(OpenFilePanels(vec![b.clone()])));

        // Row 0 is A's (now-closed) match.
        panel.update(cx, |p, cx| p.jump_to_row(0, cx));

        let a_selection = a.read_with(cx, |f, cx| f.pane().read(cx).editor().selection());
        assert_eq!(a_selection, None, "closed file's pane must not be touched");
        assert_eq!(
            panel.read_with(cx, |p, _| p.state().active_idx),
            Some(1),
            "the skipped jump must not change active_idx"
        );
    }

    /// A query/settings change while a scan is in flight must invalidate
    /// that scan's eventual results, not clobber the (already-cleared-by-
    /// `refresh`) state with a stale answer. Interleaves deterministically
    /// via `cx.dispatcher.tick`, gpui's single-step test executor
    /// primitive (`run_until_parked` would drain the scan to completion
    /// before the edit could land in between) -- no sleeps, no real
    /// concurrency (gpui's `TestDispatcher` runs both the foreground
    /// `cx.spawn` task and the `cx.background_spawn`'d task on the same
    /// cooperatively-scheduled queue).
    ///
    /// The two `run()` tasks and why exactly two ticks land "scan done,
    /// completion not yet applied": tick 1 runs the outer `cx.spawn`
    /// task up to its `.await` on `background_spawn` (which queues the
    /// inner task and returns Pending); tick 2 runs that inner task --
    /// `find_all` has no internal await point, so one poll finishes it
    /// and wakes the outer task, which is *not* polled again until the
    /// next tick. At that point `running` is still `true` and `matches`
    /// is still empty, which is the deterministic "in flight" window
    /// this test edits inside of.
    #[gpui::test]
    fn stale_scan_completing_after_a_mid_flight_query_change_is_discarded(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        let a = file_panel(cx, "/tmp/hxy-global-search-race-a.bin", b"\x00hello\x00".to_vec());
        cx.update(|_window, cx| cx.set_global(OpenFilePanels(vec![a.clone()])));

        panel.update(cx, |p, cx| {
            p.state.query_state.kind = SearchKind::Text;
            p.state.query_state.query = "hello".to_string();
            p.run(cx);
        });
        assert!(panel.read_with(cx, |p, _| p.running), "sanity: scan is in flight");

        assert!(cx.dispatcher.tick(false), "tick 1: outer task reaches its await point");
        assert!(cx.dispatcher.tick(false), "tick 2: background scan itself completes");
        assert!(panel.read_with(cx, |p, _| p.running), "sanity: completion not yet applied");
        assert!(panel.read_with(cx, |p, _| p.state().matches.is_empty()), "sanity: no results applied yet");

        // The user edits the query while the panel still considers a
        // scan "in flight" (`running == true`): bumps `search_serial`
        // via the real `refresh()` path (not a hand-rolled test poke).
        panel.update(cx, |p, cx| {
            p.state.query_state.query = "world".to_string();
            p.refresh_for_test(cx);
        });

        // Let the original scan's completion land.
        cx.run_until_parked();

        assert!(!panel.read_with(cx, |p, _| p.running), "running must still clear");
        assert!(
            panel.read_with(cx, |p, _| p.state().matches.is_empty()),
            "the stale scan's results (for \"hello\") must be discarded, not applied over the edited query"
        );
    }
}
