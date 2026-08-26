//! [`StringsPanel`]: per-file "strings(1)"-style extraction tab.
//!
//! Extraction itself lives entirely in `hxy_panels::strings` (shared
//! with the egui front end); this module only owns the config
//! widgets, the results [`DataTable`], background dispatch, and the
//! jump/hover wiring onto the owning file's [`HexPane`].
//!
//! The panel is a per-file center-dock tab, like the egui app's
//! `Tab::Strings(FileId)`, but keyed by the owning file's path rather
//! than an in-memory id (gpui persists layouts across restarts).
//! Dock-layout restore rebuilds panels one at a time via
//! [`gpui::component::dock::PanelRegistry`], with no way for a
//! restored panel to reach a sibling panel directly. So restore
//! defers binding: it records `owning_path` from the persisted JSON
//! and observes the [`OpenFilePanels`] global (published by
//! `Workspace::reconcile`) until a live `FilePanel` for that path
//! shows up, then binds to it. `Workspace::persist`'s
//! `prune_for_restore` guarantees that path will in fact become a
//! live `FilePanel` shortly after boot (or drops the tab before this
//! module ever sees it), so no separate "give up and warn" timeout is
//! needed here.

use std::path::Path;
use std::path::PathBuf;

use gpui::App;
use gpui::AppContext;
use gpui::Context;
use gpui::Div;
use gpui::Entity;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::Global;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::SharedString;
use gpui::Stateful;
use gpui::StatefulInteractiveElement;
use gpui::Styled;
use gpui::Subscription;
use gpui::Task;
use gpui::WeakEntity;
use gpui::Window;
use gpui::div;
use gpui::px;
use gpui::component::ActiveTheme;
use gpui::component::Disableable;
use gpui::component::Selectable;
use gpui::component::WindowExt;
use gpui::component::button::Button;
use gpui::component::dock::BasePanel;
use gpui::component::dock::Panel;
use gpui::component::dock::PanelEvent;
use gpui::component::dock::PanelInfo;
use gpui::component::dock::PanelState;
use gpui::component::h_flex;
use gpui::component::input::Input;
use gpui::component::input::InputEvent;
use gpui::component::input::InputState;
use gpui::component::label::Label;
use gpui::component::notification::Notification;
use gpui::component::table::Column;
use gpui::component::table::ColumnSort;
use gpui::component::table::DataTable;
use gpui::component::table::TableDelegate;
use gpui::component::table::TableEvent;
use gpui::component::table::TableState;
use gpui::component::v_flex;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_core::Selection;
use hxy_panels::goto::parse_range_expr;
use hxy_panels::strings::DEFAULT_MIN_LENGTH;
use hxy_panels::strings::Encoding;
use hxy_panels::strings::MAX_RESULTS;
use hxy_panels::strings::SortColumn;
use hxy_panels::strings::SortOrder;
use hxy_panels::strings::StringEntry;
use hxy_panels::strings::StringsConfig;
use hxy_panels::strings::StringsResult;
use hxy_panels::strings::extract;
use hxy_panels::strings::sort_entries;
use hxy_view_gpui::HexPane;

use super::FilePanel;

/// Stable identifier for layout (de)serialization; must never change.
pub const STRINGS_PANEL_NAME: &str = "StringsPanel";

/// Whole-file scans under this size auto-run when the panel opens, so
/// the user sees results without an extra click. Mirrors
/// `AUTO_RUN_MAX_BYTES` in `crates/hxy/src/app/mod.rs` (256 MiB); kept
/// as a local copy since the egui crate isn't a dependency here.
const AUTO_RUN_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Every open [`FilePanel`], republished by `Workspace::reconcile` on
/// each layout pass. [`StringsPanel`] observes this to (re)bind to
/// its owning file by path -- see the module doc for why a restored
/// panel can't reach a sibling panel any other way.
pub(crate) struct OpenFilePanels(pub Vec<Entity<FilePanel>>);

impl Global for OpenFilePanels {}

/// The currently published set of open file panels, or an empty list
/// before the workspace has published one yet (a real, valid state --
/// not a fallback for missing data).
fn open_file_panels(cx: &App) -> Vec<Entity<FilePanel>> {
    match cx.try_global::<OpenFilePanels>() {
        Some(g) => g.0.clone(),
        None => Vec::new(),
    }
}

pub struct StringsPanel {
    focus_handle: FocusHandle,
    owning_path: Option<PathBuf>,
    owning_pane: Option<Entity<HexPane>>,
    _rebind_observe: Subscription,
    config: StringsConfig,
    last_result: Option<StringsResult>,
    running: bool,
    /// A run requested while another was in flight. Set by `run` when it
    /// finds itself already `running`; on completion the panel re-runs
    /// once against the current bytes so a reload-triggered recompute is
    /// never swallowed by an in-flight scan.
    pending_rerun: bool,
    _compute: Option<Task<()>>,
    sort: SortOrder,
    /// Indices into `last_result.entries` after the active sort and
    /// filter are applied; recomputed only when one of those changes
    /// (not on every table render, since results can run to
    /// `MAX_RESULTS` rows).
    visible: Vec<usize>,
    filter: String,
    filter_input: Entity<InputState>,
    _filter_sub: Subscription,
    min_length_input: Entity<InputState>,
    range_input: Entity<InputState>,
    table: Entity<TableState<StringsTableDelegate>>,
    _table_sub: Subscription,
}

impl StringsPanel {
    /// Build a fresh panel bound immediately to `pane` (the palette /
    /// View-menu open path, where the owning file is already live).
    pub fn new(pane: Entity<HexPane>, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self::with_state(path, Encoding::default(), DEFAULT_MIN_LENGTH, window, cx);
        this.bind_pane(pane, cx);
        this
    }

    /// Rebuild from persisted [`PanelInfo`]. No live file to bind to
    /// yet (see module doc); binds lazily via [`OpenFilePanels`].
    pub fn restore(info: &PanelInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = path_from_info(info);
        let (encoding, min_length) = config_from_info(info);
        let mut this = Self::with_state(path, encoding, min_length, window, cx);
        this.try_bind_from_global(cx);
        this
    }

    fn with_state(
        path: Option<PathBuf>,
        encoding: Encoding,
        min_length: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let weak = cx.entity().downgrade();
        let table = cx.new(|cx| TableState::new(StringsTableDelegate::new(weak), window, cx));
        let table_sub = cx.subscribe(&table, Self::on_table_event);

        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder(hxy_i18n::t("strings-filter")));
        let filter_sub = cx.subscribe(&filter_input, Self::on_filter_event);

        let min_length_input = cx.new(|cx| InputState::new(window, cx).default_value(min_length.to_string()));
        let range_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(hxy_i18n::t("gpui-strings-range-placeholder")));

        let rebind_observe = cx.observe_global::<OpenFilePanels>(|this, cx| this.try_bind_from_global(cx));

        let config = StringsConfig { encoding, min_length, ..StringsConfig::default() };

        Self {
            focus_handle: cx.focus_handle(),
            owning_path: path,
            owning_pane: None,
            _rebind_observe: rebind_observe,
            config,
            last_result: None,
            running: false,
            pending_rerun: false,
            _compute: None,
            sort: SortOrder::default(),
            visible: Vec::new(),
            filter: String::new(),
            filter_input,
            _filter_sub: filter_sub,
            min_length_input,
            range_input,
            table,
            _table_sub: table_sub,
        }
    }

    /// The file path this panel scans, for the workspace's open-tab
    /// dedup and dock persistence.
    pub(crate) fn owning_path(&self) -> Option<&Path> {
        self.owning_path.as_deref()
    }

    /// Re-anchor this panel onto a new owning path (Save As renamed its
    /// file). The pane binding is unchanged -- it is the same live entity
    /// under a new path -- so only the persisted/lookup key moves.
    pub(crate) fn set_owning_path(&mut self, path: PathBuf) {
        self.owning_path = Some(path);
    }

    /// Look up `owning_path` in the currently published
    /// [`OpenFilePanels`] and bind to it if found. No-op once already
    /// bound, and a no-op that leaves the panel showing its "no
    /// active file" state when the path never resolves (restore
    /// pruning is what keeps that case rare -- see module doc).
    fn try_bind_from_global(&mut self, cx: &mut Context<Self>) {
        if self.owning_pane.is_some() {
            return;
        }
        let Some(path) = self.owning_path.clone() else { return };
        let Some(file) = open_file_panels(cx).into_iter().find(|f| f.read(cx).path() == Some(path.as_path())) else {
            return;
        };
        let pane = file.read(cx).pane().clone();
        self.bind_pane(pane, cx);
    }

    /// Bind to the owning file's pane, backfilling the scan range to
    /// the whole file the first time and auto-running when the file
    /// fits under [`AUTO_RUN_MAX_BYTES`]. Mirrors the egui app's
    /// `render_strings_tab` backfill (`crates/hxy/src/app/mod.rs`).
    fn bind_pane(&mut self, pane: Entity<HexPane>, cx: &mut Context<Self>) {
        if self.owning_pane.is_some() {
            return;
        }
        self.owning_pane = Some(pane.clone());
        let source_len = pane.read(cx).editor().source().len().get();
        if self.config.range.is_empty()
            && source_len > 0
            && let Ok(range) = ByteRange::new(ByteOffset::new(0), ByteOffset::new(source_len))
        {
            self.config.range = range;
            if source_len <= AUTO_RUN_MAX_BYTES {
                self.run(cx);
                return;
            }
        }
        cx.notify();
    }

    /// Indices into `visible` are positions in the results table;
    /// resolve one back to its `StringEntry`.
    fn visible_entry(&self, row_ix: usize) -> Option<&StringEntry> {
        let idx = *self.visible.get(row_ix)?;
        self.last_result.as_ref()?.entries.get(idx)
    }

    fn on_table_event(
        &mut self,
        _table: Entity<TableState<StringsTableDelegate>>,
        event: &TableEvent,
        cx: &mut Context<Self>,
    ) {
        if let TableEvent::SelectRow(row_ix) = event {
            self.jump_to_row(*row_ix, cx);
        }
    }

    fn on_filter_event(&mut self, _input: Entity<InputState>, event: &InputEvent, cx: &mut Context<Self>) {
        if let InputEvent::Change = event {
            self.filter = self.filter_input.read(cx).value().to_string();
            self.recompute_visible();
            self.table.update(cx, |t, cx| t.refresh(cx));
            cx.notify();
        }
    }

    fn set_encoding(&mut self, encoding: Encoding, cx: &mut Context<Self>) {
        self.config.encoding = encoding;
        cx.notify();
    }

    fn set_sort(&mut self, order: SortOrder, cx: &mut Context<Self>) {
        self.sort = order;
        self.recompute_visible();
        cx.notify();
    }

    /// Re-sort `last_result.entries` in place if the panel's sort has
    /// drifted, then rebuild the filtered `visible` index list.
    fn recompute_visible(&mut self) {
        let Some(result) = self.last_result.as_mut() else {
            self.visible.clear();
            return;
        };
        if result.sorted_by != self.sort {
            sort_entries(&mut result.entries, self.sort);
            result.sorted_by = self.sort;
        }
        let filter = self.filter.trim().to_lowercase();
        self.visible = if filter.is_empty() {
            (0..result.entries.len()).collect()
        } else {
            result
                .entries
                .iter()
                .enumerate()
                .filter_map(|(i, e)| e.text.to_lowercase().contains(&filter).then_some(i))
                .collect()
        };
    }

    /// Set the editor selection to the entry's byte range and scroll
    /// it into view, then emit [`StringsJumped`] so the workspace can
    /// bring the owning file's tab to the front. `pub(crate)` so the
    /// workspace's own tests can drive a jump directly (there is no
    /// window in a `cx.subscribe` handler to build a synthetic table
    /// click through).
    ///
    /// Mirrors egui's `jump_to_strings_match`
    /// (`crates/hxy/src/app/mod.rs`), which calls `focus_file_tab`
    /// before applying the selection -- `StringsPanel` has no handle
    /// to the dock to do that itself (see [`StringsJumped`]'s doc), so
    /// the tab-focus half of that happens in the workspace's
    /// subscriber instead. Applying the selection before emitting
    /// (rather than after, as egui does) makes no behavioral
    /// difference here: gpui's effect queue flushes both the pane
    /// update and the emitted event's subscriber before this
    /// function's caller resumes.
    pub(crate) fn jump_to_row(&mut self, row_ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.visible_entry(row_ix) else { return };
        let offset = entry.offset;
        let end_inclusive = entry.end.saturating_sub(1).max(entry.offset);
        let Some(pane) = self.owning_pane.clone() else { return };
        pane.update(cx, |pane, cx| {
            pane.editor_mut().set_selection(Some(Selection {
                anchor: ByteOffset::new(offset),
                cursor: ByteOffset::new(end_inclusive),
            }));
            pane.editor_mut().set_scroll_to_byte(ByteOffset::new(offset));
            pane.sync_pending_scroll(cx);
        });
        cx.emit(StringsJumped);
    }

    /// Install (or clear) the owning pane's hover-highlight band.
    /// `pub(crate)` for the same reason as `jump_to_row`: tests drive
    /// this directly rather than through a synthetic table hover.
    pub(crate) fn set_hover(&mut self, span: Option<ByteRange>, cx: &mut Context<Self>) {
        let Some(pane) = self.owning_pane.clone() else { return };
        pane.update(cx, |pane, cx| pane.set_hover_span(span, cx));
    }

    /// Kick off a background extraction over the current config and
    /// apply the result once it lands. Never runs on the UI thread:
    /// `extract` itself executes inside `cx.background_spawn`, on the
    /// gpui background executor pool.
    fn run(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.owning_pane.clone() else { return };
        if self.running {
            // Queue a re-run rather than dropping it; the completion
            // handler picks it up against the then-current bytes.
            self.pending_rerun = true;
            cx.notify();
            return;
        }
        let source = pane.read(cx).editor().source().clone();
        let config = self.config.clone();
        self.running = true;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let outcome = cx.background_spawn(async move { extract(source.as_ref(), &config) }).await;
            let _ = this.update(cx, |this, cx| {
                this.running = false;
                this._compute = None;
                match outcome {
                    Ok(result) => this.apply_result(result, cx),
                    Err(err) => {
                        tracing::warn!(%err, "strings extraction failed");
                        cx.notify();
                    }
                }
                if std::mem::take(&mut this.pending_rerun) {
                    this.run(cx);
                }
            });
        });
        self._compute = Some(task);
    }

    fn apply_result(&mut self, result: StringsResult, cx: &mut Context<Self>) {
        self.last_result = Some(result);
        self.recompute_visible();
        self.table.update(cx, |t, cx| t.refresh(cx));
        cx.notify();
    }

    /// Re-run against the owning file's current bytes after an external
    /// reload swapped its pane's source. No-op for a panel nobody has used
    /// yet, and -- mirroring egui's `cascade_byte_change` -- skipped for an
    /// empty or over-[`AUTO_RUN_MAX_BYTES`] file so a reload of a giant
    /// dump doesn't pin a background worker.
    pub(crate) fn recompute_after_reload(&mut self, cx: &mut Context<Self>) {
        if !(self.last_result.is_some() || self.running) {
            return;
        }
        let Some(pane) = self.owning_pane.clone() else { return };
        let len = pane.read(cx).editor().source().len().get();
        if len == 0 || len > AUTO_RUN_MAX_BYTES {
            return;
        }
        self.run(cx);
    }

    /// Parse the min-length / range text inputs (blank = keep the
    /// current config value) and kick off a run. Invalid input toasts
    /// an error and leaves the config untouched.
    fn on_run_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.owning_pane.clone() else { return };
        let source_len = pane.read(cx).editor().source().len().get();

        let min_text = self.min_length_input.read(cx).value().to_string();
        if !min_text.trim().is_empty() {
            match min_text.trim().parse::<usize>() {
                Ok(n) if n >= 1 => self.config.min_length = n,
                _ => {
                    window.push_notification(Notification::error(hxy_i18n::t("gpui-strings-invalid-min-length")), cx);
                    return;
                }
            }
        }

        let range_text = self.range_input.read(cx).value().to_string();
        if !range_text.trim().is_empty() {
            match parse_range_expr(range_text.trim(), source_len, &hxy_calculator::NullResolver) {
                Ok(resolved) => {
                    match ByteRange::new(ByteOffset::new(resolved.start), ByteOffset::new(resolved.end_exclusive)) {
                        Ok(range) => self.config.range = range,
                        Err(err) => {
                            window.push_notification(
                                Notification::error(hxy_i18n::t_args(
                                    "gpui-strings-invalid-range",
                                    &[("reason", &err.to_string())],
                                )),
                                cx,
                            );
                            return;
                        }
                    }
                }
                Err(err) => {
                    window.push_notification(
                        Notification::error(hxy_i18n::t_args(
                            "gpui-strings-invalid-range",
                            &[("reason", &err.to_string())],
                        )),
                        cx,
                    );
                    return;
                }
            }
        }

        self.run(cx);
    }

    fn encoding_button(&self, encoding: Encoding, id: &'static str, cx: &Context<Self>) -> Button {
        Button::new(id)
            .label(encoding.label())
            .compact()
            .selected(self.config.encoding == encoding)
            .on_click(cx.listener(move |this, _, _, cx| this.set_encoding(encoding, cx)))
    }

    fn run_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let label = if self.running { hxy_i18n::t("strings-running") } else { hxy_i18n::t("strings-run") };
        Button::new("strings-run-btn")
            .label(label)
            .compact()
            .loading(self.running)
            .disabled(self.running)
            .on_click(cx.listener(|this, _, window, cx| this.on_run_clicked(window, cx)))
    }

    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(self.encoding_button(Encoding::Ascii, "strings-enc-ascii", cx))
                    .child(self.encoding_button(Encoding::Utf8, "strings-enc-utf8", cx))
                    .child(self.encoding_button(Encoding::Utf16Le, "strings-enc-utf16le", cx))
                    .child(self.encoding_button(Encoding::Utf16Be, "strings-enc-utf16be", cx))
                    .child(Label::new(hxy_i18n::t("strings-min-length")))
                    .child(div().w(px(70.0)).child(Input::new(&self.min_length_input)))
                    .child(self.run_button(cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Label::new(hxy_i18n::t("gpui-strings-range-label")))
                    .child(div().w(px(220.0)).child(Input::new(&self.range_input)))
                    .child(Label::new(hxy_i18n::t("strings-filter")))
                    .child(div().w(px(180.0)).child(Input::new(&self.filter_input))),
            )
    }

    fn render_summary(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut col = v_flex().gap_1().px_2().py_1();
        if !self.config.range.is_empty() {
            col = col.child(
                Label::new(hxy_i18n::t_args(
                    "strings-range",
                    &[
                        ("start", &format!("0x{:X}", self.config.range.start().get())),
                        ("end", &format!("0x{:X}", self.config.range.end().get())),
                        ("length", &format_bytes(self.config.range.len().get())),
                    ],
                ))
                .text_color(cx.theme().muted_foreground),
            );
        }
        match &self.last_result {
            Some(result) => {
                let total = result.entries.len();
                let summary = if self.filter.trim().is_empty() {
                    hxy_i18n::t_args("strings-summary", &[("count", &total.to_string())])
                } else {
                    hxy_i18n::t_args(
                        "strings-summary-filtered",
                        &[("count", &total.to_string()), ("filter", &self.filter)],
                    )
                };
                col = col.child(Label::new(summary).text_color(cx.theme().muted_foreground));
                if result.truncated {
                    col = col.child(
                        Label::new(hxy_i18n::t_args("strings-truncated", &[("max", &MAX_RESULTS.to_string())]))
                            .text_color(gpui::rgb(0xF5CC4E)),
                    );
                }
            }
            None if self.running => {
                col = col.child(Label::new(hxy_i18n::t("strings-running")).text_color(cx.theme().muted_foreground));
            }
            None => {}
        }
        col
    }
}

/// Extract the stored owning path from a `StringsPanel` payload.
fn path_from_info(info: &PanelInfo) -> Option<PathBuf> {
    let PanelInfo::Panel(value) = info else { return None };
    value.get("path").and_then(|p| p.as_str()).map(PathBuf::from)
}

/// Rebuild encoding/min_length from persisted JSON. A missing field
/// is a normal older-layout/first-open case (silently defaulted, same
/// rule `StringsConfig::default()` uses); an unrecognized value is
/// corrupt data and gets a warning before falling back. Mirrors
/// `InspectorPanel`'s `state_from_info`.
fn config_from_info(info: &PanelInfo) -> (Encoding, usize) {
    let mut encoding = Encoding::default();
    let mut min_length = DEFAULT_MIN_LENGTH;
    let PanelInfo::Panel(value) = info else { return (encoding, min_length) };
    if let Some(key) = value.get("encoding").and_then(|v| v.as_str()) {
        encoding = encoding_from_key(key);
    }
    if let Some(n) = value.get("min_length").and_then(|v| v.as_u64()) {
        min_length = n as usize;
    }
    (encoding, min_length)
}

fn encoding_key(encoding: Encoding) -> &'static str {
    match encoding {
        Encoding::Ascii => "ascii",
        Encoding::Utf8 => "utf8",
        Encoding::Utf16Le => "utf16le",
        Encoding::Utf16Be => "utf16be",
    }
}

fn encoding_from_key(key: &str) -> Encoding {
    match key {
        "ascii" => Encoding::Ascii,
        "utf8" => Encoding::Utf8,
        "utf16le" => Encoding::Utf16Le,
        "utf16be" => Encoding::Utf16Be,
        other => {
            tracing::warn!(encoding = other, "restore: unrecognized strings encoding; using default");
            Encoding::default()
        }
    }
}

/// The panel base name shown on its tab: the owning file's leaf name,
/// falling back to the untitled placeholder when it has no path.
/// Mirrors `FilePanel`'s `tab_title`.
fn tab_label(path: Option<&Path>) -> String {
    match path {
        Some(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        None => hxy_i18n::t("gpui-file-untitled"),
    }
}

fn format_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GiB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

impl BasePanel for StringsPanel {
    fn panel_name(&self) -> &'static str {
        STRINGS_PANEL_NAME
    }

    /// Persist the owning path plus encoding/min_length; the scan
    /// range is data-dependent (tied to file length) and re-backfills
    /// to "whole file" on rebind instead of round-tripping verbatim.
    fn dump(&self, _cx: &App) -> PanelState {
        let mut state = PanelState::new(self.panel_name());
        state.info = PanelInfo::panel(serde_json::json!({
            "path": self.owning_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "encoding": encoding_key(self.config.encoding),
            "min_length": self.config.min_length,
        }));
        state
    }

    /// Clear the owning pane's hover band on removal, however the tab
    /// closed (the workspace's own close paths, or the tab bar's own
    /// close button, which bypasses the workspace entirely) --
    /// `gpui::component::dock::BasePanel::on_removed` fires unconditionally
    /// from the tab group's `detach_panel`, so this is the one place
    /// that reliably catches all of them. Without it, a pointer left
    /// resting on a row when the tab closes leaves a stale hover band
    /// on a hex view with nothing left to clear it (only
    /// `HexPane::set_source` resets `hover_span`).
    fn on_removed(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_hover(None, cx);
    }
}

impl Panel for StringsPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(hxy_i18n::t_args("tab-strings", &[("name", &tab_label(self.owning_path.as_deref()))]))
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(hxy_i18n::t_args("tab-strings", &[("name", &tab_label(self.owning_path.as_deref()))])))
    }
}

impl Focusable for StringsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for StringsPanel {}

/// Emitted by [`StringsPanel::jump_to_row`] after applying a jump.
/// `StringsPanel` owns the target `HexPane` directly but has no handle
/// to the dock/`Workspace` that could bring its tab to the front (the
/// same "no way to reach a sibling panel" constraint documented at the
/// top of this module for restore-time rebinding). The workspace
/// subscribes to every `StringsPanel` it knows about (see
/// `Workspace::track_strings_panel`) and reacts by focusing the
/// owning file's tab.
#[derive(Clone, Copy, Debug)]
pub struct StringsJumped;

impl EventEmitter<StringsJumped> for StringsPanel {}

impl Render for StringsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = v_flex().size_full().bg(cx.theme().background);
        if self.owning_pane.is_none() {
            return root.child(
                div().p_2().text_color(cx.theme().muted_foreground).child(hxy_i18n::t("strings-no-active-file")),
            );
        }
        root.child(self.render_toolbar(cx))
            .child(self.render_summary(cx))
            .child(div().flex_1().min_h_0().child(DataTable::new(&self.table)))
    }
}

struct StringsTableDelegate {
    columns: [Column; 3],
    panel: WeakEntity<StringsPanel>,
}

impl StringsTableDelegate {
    fn new(panel: WeakEntity<StringsPanel>) -> Self {
        Self {
            columns: [
                Column::new("offset", hxy_i18n::t("strings-col-offset")).width(px(120.0)).sortable(),
                Column::new("text", hxy_i18n::t("strings-col-text")).width(px(420.0)).sortable(),
                Column::new("length", hxy_i18n::t("strings-col-length")).width(px(90.0)).sortable(),
            ],
            panel,
        }
    }

    fn entry_range(&self, row_ix: usize, cx: &App) -> Option<ByteRange> {
        let panel = self.panel.upgrade()?;
        let panel = panel.read(cx);
        let entry = panel.visible_entry(row_ix)?;
        ByteRange::new(ByteOffset::new(entry.offset), ByteOffset::new(entry.end)).ok()
    }
}

impl TableDelegate for StringsTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, cx: &App) -> usize {
        self.panel.upgrade().map(|p| p.read(cx).visible.len()).unwrap_or(0)
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        self.columns[col_ix].clone()
    }

    /// Translate the table's built-in header-click cycle into a
    /// [`SortOrder`] and apply it on the panel. `Default` (the
    /// table's "no sort" state) maps back to the extractor's natural
    /// offset-ascending order, since `hxy_panels::strings` has no
    /// concept of "unsorted".
    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let Some(panel) = self.panel.upgrade() else { return };
        let column = match col_ix {
            0 => SortColumn::Offset,
            1 => SortColumn::Text,
            2 => SortColumn::Length,
            _ => return,
        };
        let order = match sort {
            ColumnSort::Ascending => SortOrder::Asc(column),
            ColumnSort::Descending => SortOrder::Desc(column),
            ColumnSort::Default => SortOrder::Asc(SortColumn::Offset),
        };
        panel.update(cx, |panel, cx| panel.set_sort(order, cx));
    }

    /// Paints the row hover band on the owning file's pane. Attached
    /// to the whole row (rather than per-cell, as the egui table
    /// needs) since gpui-component's delegate already separates
    /// `render_tr` from `render_td`.
    fn render_tr(&mut self, row_ix: usize, _window: &mut Window, cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let range = self.entry_range(row_ix, cx);
        let panel = self.panel.clone();
        div().id(("strings-row", row_ix)).on_hover(move |hovered, _window, cx| {
            if let Some(panel) = panel.upgrade() {
                let span = if *hovered { range } else { None };
                panel.update(cx, |panel, cx| panel.set_hover(span, cx));
            }
        })
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
        let Some(entry) = panel_ref.visible_entry(row_ix) else {
            return div().into_any_element();
        };
        match col_ix {
            // Row click (not this cell specifically) does the jump --
            // see `on_table_event` -- so this only needs link styling.
            0 => div().text_color(cx.theme().primary).child(format!("0x{:X}", entry.offset)).into_any_element(),
            1 => div().child(entry.text.clone()).into_any_element(),
            2 => div().child(entry.length().to_string()).into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn render_empty(&mut self, _window: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let text = match self.panel.upgrade() {
            Some(panel) => {
                let panel = panel.read(cx);
                if panel.last_result.is_none() {
                    if panel.running { hxy_i18n::t("strings-running") } else { hxy_i18n::t("strings-no-results-yet") }
                } else {
                    hxy_i18n::t("strings-no-matches")
                }
            }
            None => String::new(),
        };
        h_flex().size_full().justify_center().text_color(cx.theme().muted_foreground).child(text)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use hxy_core::MemorySource;

    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(gpui::component::init);
    }

    fn source(bytes: Vec<u8>) -> Arc<dyn HexSource> {
        Arc::new(MemorySource::new(bytes))
    }

    /// Builds a `StringsPanel` bound to a fresh `HexPane` inside a
    /// real `gpui::component::Root` window, mirroring `FilePanel`'s
    /// test harness (`InputState`'s focus tracking needs the `Root`
    /// layer present).
    fn build(cx: &mut TestAppContext, bytes: Vec<u8>) -> (Entity<StringsPanel>, &mut gpui::VisualTestContext) {
        let window = cx.add_window(|window, cx| {
            let pane = cx.new(|cx| HexPane::new(source(bytes), cx));
            let panel =
                cx.new(|cx| StringsPanel::new(pane, Some(PathBuf::from("/tmp/hxy-strings-fixture.bin")), window, cx));
            gpui::component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<StringsPanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        (panel, vcx)
    }

    /// A run requested while another is in flight is queued (not dropped)
    /// and replayed on completion, so a reload-triggered recompute can't be
    /// swallowed by an in-flight scan.
    #[gpui::test]
    fn in_flight_run_queues_a_rerun(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, b"\x00hello\x00".to_vec());
        cx.run_until_parked();
        panel.update(cx, |p, cx| {
            p.run(cx);
            assert!(p.running, "the first run is in flight");
            p.run(cx);
            assert!(p.pending_rerun, "a second run mid-flight is queued, not dropped");
        });
        cx.run_until_parked();
        assert!(!panel.read_with(cx, |p, _| p.pending_rerun), "the queued rerun was consumed");
        assert!(!panel.read_with(cx, |p, _| p.running), "and the panel settled");
    }

    /// `recompute_after_reload` is gated on file size like egui's
    /// `cascade_byte_change`: an empty source is skipped before `run`, so
    /// no rerun is queued even when a prior result exists.
    #[gpui::test]
    fn recompute_after_reload_skips_empty_source(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, Vec::new());
        cx.run_until_parked();
        panel.update(cx, |p, cx| {
            // `running = true` satisfies the has-result-or-running gate;
            // the len == 0 gate must still short-circuit before `run`.
            p.running = true;
            p.recompute_after_reload(cx);
            assert!(!p.pending_rerun, "an empty source is gated out, so run() never queued a rerun");
            p.running = false;
        });
    }

    /// Opening the panel on a small fixture auto-runs (under
    /// `AUTO_RUN_MAX_BYTES`) and produces the same rows
    /// `hxy_panels::strings::extract` would. This test only confirms
    /// the rows land correctly, not that `extract` ran off the UI
    /// thread -- that's established by inspection instead (`run`'s
    /// only call to `extract` is inside `cx.background_spawn`).
    #[gpui::test]
    fn run_over_fixture_produces_rows(cx: &mut TestAppContext) {
        setup(cx);
        let bytes = b"\x00hello\x00world\x00".to_vec();
        let (panel, cx) = build(cx, bytes);
        cx.run_until_parked();

        let texts: Vec<String> = panel.read_with(cx, |p, _| {
            p.last_result
                .as_ref()
                .expect("auto-run populated a result")
                .entries
                .iter()
                .map(|e| e.text.clone())
                .collect()
        });
        assert_eq!(texts, vec!["hello".to_string(), "world".to_string()]);
        assert!(!panel.read_with(cx, |p, _| p.running));
    }

    #[gpui::test]
    fn row_jump_sets_owning_pane_selection(cx: &mut TestAppContext) {
        setup(cx);
        let bytes = b"\x00hello\x00world\x00".to_vec();
        let (panel, cx) = build(cx, bytes);
        cx.run_until_parked();

        panel.update(cx, |p, cx| p.jump_to_row(0, cx));

        let selection =
            panel.read_with(cx, |p, cx| p.owning_pane.as_ref().expect("bound").read(cx).editor().selection());
        assert_eq!(selection, Some(Selection { anchor: ByteOffset::new(1), cursor: ByteOffset::new(5) }));
    }

    #[gpui::test]
    fn hover_sets_and_clears_owning_pane_hover_span(cx: &mut TestAppContext) {
        setup(cx);
        let bytes = b"\x00hello\x00world\x00".to_vec();
        let (panel, cx) = build(cx, bytes);
        cx.run_until_parked();

        let range = ByteRange::new(ByteOffset::new(1), ByteOffset::new(6)).unwrap();
        panel.update(cx, |p, cx| p.set_hover(Some(range), cx));
        assert_eq!(panel.read_with(cx, |p, cx| p.owning_pane.as_ref().unwrap().read(cx).hover_span()), Some(range));

        panel.update(cx, |p, cx| p.set_hover(None, cx));
        assert_eq!(panel.read_with(cx, |p, cx| p.owning_pane.as_ref().unwrap().read(cx).hover_span()), None);
    }

    #[test]
    fn config_from_info_round_trips_encoding_and_min_length() {
        let info = PanelInfo::panel(serde_json::json!({ "encoding": "utf16le", "min_length": 8 }));
        let (encoding, min_length) = config_from_info(&info);
        assert_eq!(encoding, Encoding::Utf16Le);
        assert_eq!(min_length, 8);

        let default_info = PanelInfo::panel(serde_json::json!({}));
        let (encoding, min_length) = config_from_info(&default_info);
        assert_eq!(encoding, Encoding::default());
        assert_eq!(min_length, DEFAULT_MIN_LENGTH);

        let corrupt_info = PanelInfo::panel(serde_json::json!({ "encoding": "ebcdic" }));
        let (encoding, _) = config_from_info(&corrupt_info);
        assert_eq!(encoding, Encoding::default());
    }

    #[gpui::test]
    fn dump_round_trips_path_encoding_and_min_length(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, vec![0u8; 16]);
        panel.update(cx, |p, cx| p.set_encoding(Encoding::Utf8, cx));
        panel.update(cx, |p, _| p.config.min_length = 6);

        let dumped = panel.read_with(cx, |p, cx| p.dump(cx));
        assert_eq!(dumped.panel_name, STRINGS_PANEL_NAME);

        let (encoding, min_length) = config_from_info(&dumped.info);
        assert_eq!(encoding, Encoding::Utf8);
        assert_eq!(min_length, 6);
        assert_eq!(path_from_info(&dumped.info), Some(PathBuf::from("/tmp/hxy-strings-fixture.bin")));
    }

    /// Restore has no live `FilePanel` to bind to yet when the
    /// `OpenFilePanels` global is already populated (the common case:
    /// files restore before -- or in the same pass as -- their
    /// strings tabs): the panel finds it immediately.
    #[gpui::test]
    fn restore_binds_immediately_when_global_already_has_the_file(cx: &mut TestAppContext) {
        setup(cx);
        let path = PathBuf::from("/tmp/hxy-strings-restore-a.bin");
        let path_for_closure = path.clone();
        let window = cx.add_window(move |window, cx| {
            let file_panel =
                cx.new(|cx| FilePanel::new(source(vec![0u8; 16]), Some(path_for_closure.clone()), window, cx));
            cx.set_global(OpenFilePanels(vec![file_panel]));
            let info = PanelInfo::panel(serde_json::json!({ "path": path_for_closure.to_string_lossy() }));
            let panel = cx.new(|cx| StringsPanel::restore(&info, window, cx));
            gpui::component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<StringsPanel>().unwrap());
        let cx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();

        assert!(panel.read_with(cx, |p, _| p.owning_pane.is_some()));
        assert_eq!(panel.read_with(cx, |p, _| p.owning_path.clone()), Some(path));
    }

    /// Restore before the file reopens: the panel starts unbound and
    /// picks up the file once `Workspace::reconcile` publishes it,
    /// via the `OpenFilePanels` global observer. Mirrors
    /// `InspectorPanel`'s equivalent boot-ordering test.
    #[gpui::test]
    fn restore_rebinds_when_global_updates_later(cx: &mut TestAppContext) {
        setup(cx);
        let path = PathBuf::from("/tmp/hxy-strings-restore-b.bin");
        let info = PanelInfo::panel(serde_json::json!({ "path": path.to_string_lossy() }));
        let window = cx.add_window(move |window, cx| {
            let panel = cx.new(|cx| StringsPanel::restore(&info, window, cx));
            gpui::component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<StringsPanel>().unwrap());
        let cx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();
        assert!(panel.read_with(cx, |p, _| p.owning_pane.is_none()), "no file open yet");

        let path_for_closure = path.clone();
        cx.update(|window, cx| {
            let file_panel = cx.new(|cx| FilePanel::new(source(vec![0u8; 16]), Some(path_for_closure), window, cx));
            cx.set_global(OpenFilePanels(vec![file_panel]));
        });
        cx.run_until_parked();

        assert!(panel.read_with(cx, |p, _| p.owning_pane.is_some()));
    }

    /// `recompute_after_reload` is a no-op for a panel with no result
    /// (mirrors egui's `has_strings` cascade gate). `build` auto-runs
    /// on bind, so the "never computed" precondition is forced by
    /// clearing the result afterward rather than by construction.
    #[gpui::test]
    fn recompute_after_reload_noop_when_never_computed(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, b"\x00hello\x00".to_vec());
        cx.run_until_parked();
        panel.update(cx, |p, _cx| p.last_result = None);

        panel.update(cx, |p, cx| p.recompute_after_reload(cx));
        cx.run_until_parked();
        assert!(panel.read_with(cx, |p, _| p.last_result.is_none()), "no prior result means no auto-recompute");
    }

    /// `recompute_after_reload` re-runs against the pane's current
    /// bytes once a result already exists -- the reload cascade's
    /// entry point after `Workspace::resolve_reload` swaps the
    /// owning file's source.
    #[gpui::test]
    fn recompute_after_reload_reruns_when_a_result_exists(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, b"\x00hello\x00".to_vec());
        cx.run_until_parked();
        let before: Vec<String> = panel
            .read_with(cx, |p, _| p.last_result.as_ref().unwrap().entries.iter().map(|e| e.text.clone()).collect());
        assert_eq!(before, vec!["hello".to_string()]);

        // Same total length as the fixture above: `config.range` is
        // set once from the file's length at bind time (`bind_pane`)
        // and reload doesn't widen it, so a same-length replacement
        // isolates what this test cares about (recompute picks up
        // the new bytes) from that separate, pre-existing behavior.
        let pane = panel.read_with(cx, |p, _| p.owning_pane.clone().unwrap());
        pane.update(cx, |pane, cx| {
            pane.editor_mut().swap_source(Arc::new(MemorySource::new(b"\x00world\x00".to_vec())));
            cx.notify();
        });

        panel.update(cx, |p, cx| p.recompute_after_reload(cx));
        cx.run_until_parked();
        let after: Vec<String> = panel
            .read_with(cx, |p, _| p.last_result.as_ref().unwrap().entries.iter().map(|e| e.text.clone()).collect());
        assert_eq!(after, vec!["world".to_string()], "recompute picked up the new bytes");
    }
}
