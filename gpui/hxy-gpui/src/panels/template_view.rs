//! [`TemplateView`]: the per-file template results panel rendered
//! below a [`FilePanel`](super::FilePanel)'s hex pane.
//!
//! All template state (instances, tree, colors, selection) lives on
//! the owning [`FilePanel`]; this view is a renderer plus event
//! source. Every interaction emits a framework-neutral
//! [`TemplateEvent`] which the panel's reducer
//! (`FilePanel::apply_template_event`) applies, then pushes the
//! recomputed visible-row list back here via [`TemplateView::set_rows`].
//! The gpui counterpart of the egui panel in
//! `crates/hxy/src/panels/template.rs`.

use gpui::AnyElement;
use gpui::App;
use gpui::AppContext;
use gpui::ClickEvent;
use gpui::Context;
use gpui::Div;
use gpui::Entity;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::SharedString;
use gpui::Stateful;
use gpui::StatefulInteractiveElement;
use gpui::Styled;
use gpui::Subscription;
use gpui::WeakEntity;
use gpui::Window;
use gpui::div;
use gpui::prelude::FluentBuilder;
use gpui::px;
use gpui_component::ActiveTheme;
use gpui_component::Icon;
use gpui_component::IconName;
use gpui_component::Selectable;
use gpui_component::Sizable;
use gpui_component::button::Button;
use gpui_component::button::ButtonVariants;
use gpui_component::h_flex;
use gpui_component::label::Label;
use gpui_component::menu::PopupMenu;
use gpui_component::menu::PopupMenuItem;
use gpui_component::popover::Popover;
use gpui_component::spinner::Spinner;
use gpui_component::table::Column;
use gpui_component::table::Table;
use gpui_component::table::TableDelegate;
use gpui_component::table::TableEvent;
use gpui_component::table::TableState;
use gpui_component::v_flex;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_core::color::Rgba;
use hxy_core::format::NumericFormat;
use hxy_core::format::TemplateValueFormats;
use hxy_core::format::format_offset;
use hxy_plugin_host::template::Node;
use hxy_plugin_host::template::Severity;
use hxy_templates::breadcrumb::BreadcrumbDetail;
use hxy_templates::breadcrumb::breadcrumb_for_offset;
use hxy_templates::format::decode_scalar_bytes;
use hxy_templates::format::format_value;
use hxy_templates::format::scalar_kind_name;
use hxy_templates::format::scalar_kind_width;
use hxy_templates::state::CopyKind;
use hxy_templates::state::RowKind;
use hxy_templates::state::TemplateArrayId;
use hxy_templates::state::TemplateEvent;
use hxy_templates::state::TemplateInstanceId;
use hxy_templates::state::TemplateNodeIdx;
use hxy_templates::state::TemplateState;
use hxy_view_gpui::HexPane;

use super::FilePanel;
use crate::templates::rgba_to_hsla;

/// Key context for the results table's arrow-key bindings. Scoping
/// the bindings here keeps plain arrows inert everywhere else.
const KEY_CONTEXT: &str = "TemplateTable";

/// Horizontal indent per tree depth level in the Name column.
const INDENT_STEP: f32 = 14.0;

gpui::actions!(hxy_gpui_template, [SelectionUp, SelectionDown, CollapseRow, ExpandRow]);

/// Register the template table's arrow-key bindings. Called from
/// `workspace::init_keybindings` at startup.
pub fn init_keybindings(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("up", SelectionUp, Some(KEY_CONTEXT)),
        gpui::KeyBinding::new("down", SelectionDown, Some(KEY_CONTEXT)),
        gpui::KeyBinding::new("left", CollapseRow, Some(KEY_CONTEXT)),
        gpui::KeyBinding::new("right", ExpandRow, Some(KEY_CONTEXT)),
    ]);
}

/// Emitted when the user clicks a diagnostic's offset link; the
/// owning [`FilePanel`] jumps the hex view to that byte. Separate
/// from [`TemplateEvent`] because the shared vocabulary has no
/// "jump to raw offset" arm (egui's diagnostics list has no link).
#[derive(Clone, Copy, Debug)]
pub struct TemplateOffsetJump(pub ByteOffset);

pub struct TemplateView {
    file: WeakEntity<FilePanel>,
    /// The owning panel's hex pane, read for the hovered byte and the
    /// source the breadcrumb decodes values from. Observed so hover
    /// moves in the grid re-render the breadcrumb strip.
    pane: Entity<HexPane>,
    _pane_observe: Subscription,
    focus_handle: FocusHandle,
    /// Visible-row cache for the ACTIVE instance, recomputed by the
    /// owning panel's reducer after every state change (the panel
    /// owns the state; recomputing here would re-enter the panel's
    /// entity lease mid-update).
    rows: Vec<RowKind>,
    /// Mirror of the active instance's `selected_node`, kept to
    /// detect selection changes so keyboard moves scroll the row
    /// into view without jittering on plain clicks.
    selected: Option<TemplateNodeIdx>,
    /// Diagnostics section toggle. `None` until the user touches it:
    /// auto-open while any Error-severity diagnostic exists (mirrors
    /// egui's `default_open`), collapsed otherwise.
    diagnostics_open: Option<bool>,
    /// Which node's color-swatch popover is open, if any. Controlled
    /// state so a shift-click reset can close the popover the
    /// trigger's own mouse-down just opened.
    color_popover: Option<TemplateNodeIdx>,
    /// Table row the pointer is over. Row enter/leave callbacks have
    /// no guaranteed order when the pointer crosses rows, so a leave
    /// only clears the hover it set itself.
    hovered_row: Option<usize>,
    /// Consume-once guard armed by [`Self::set_rows`] when it moves
    /// the table's selected row purely to track a row-index shift
    /// (selection unchanged): the resulting SelectRow echo must not
    /// re-fire the hex-view jump.
    suppress_select_echo: bool,
    table: Entity<TableState<TemplateTableDelegate>>,
    _table_sub: Subscription,
    /// Refreshes the table (whose cells read the numeric/value
    /// formats from the settings global at render time) when settings
    /// change; egui gets this for free from per-frame redraw.
    _settings_observe: Subscription,
}

impl TemplateView {
    pub fn new(
        file: WeakEntity<FilePanel>,
        pane: Entity<HexPane>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let weak = cx.entity().downgrade();
        let table = cx.new(|cx| TableState::new(TemplateTableDelegate::new(weak), window, cx));
        let table_sub = cx.subscribe_in(&table, window, Self::on_table_event);
        let pane_observe = cx.observe(&pane, |_, _, cx| cx.notify());
        let settings_observe = cx.observe_global::<crate::settings::SettingsGlobal>(|this, cx| {
            this.table.update(cx, |table, cx| table.refresh(cx));
            cx.notify();
        });
        Self {
            file,
            pane,
            _pane_observe: pane_observe,
            focus_handle: cx.focus_handle(),
            rows: Vec::new(),
            selected: None,
            diagnostics_open: None,
            color_popover: None,
            hovered_row: None,
            suppress_select_echo: false,
            table,
            _table_sub: table_sub,
            _settings_observe: settings_observe,
        }
    }

    /// Replace the visible-row cache after the owning panel's reducer
    /// changed template state. `selected` mirrors the active
    /// instance's keyboard selection; the table's own selected row is
    /// reconciled to it by node identity, so the highlight tracks the
    /// selected node even when rows above it appear or disappear
    /// (collapse/expand). Reconciliation is skipped when the table
    /// already sits on the right row, which is what terminates the
    /// click -> Select -> set_rows echo after one pass; a purely
    /// positional reconcile suppresses the SelectRow echo so it can't
    /// re-fire the hex-view jump for an unchanged selection.
    pub(crate) fn set_rows(&mut self, rows: Vec<RowKind>, selected: Option<TemplateNodeIdx>, cx: &mut Context<Self>) {
        self.rows = rows;
        let selection_moved = self.selected != selected;
        self.selected = selected;
        let target_row = selected
            .and_then(|target| self.rows.iter().position(|r| matches!(r, RowKind::Node { idx, .. } if *idx == target)));
        let current_row = self.table.read(cx).selected_row();
        let reconcile = match target_row {
            Some(row_ix) => current_row != Some(row_ix),
            None => current_row.is_some(),
        };
        // clear_selection emits nothing, so the echo suppression only
        // arms for a positional set_selected_row.
        if reconcile && !selection_moved && target_row.is_some() {
            self.suppress_select_echo = true;
        }
        self.table.update(cx, |table, cx| {
            table.refresh(cx);
            if reconcile {
                match target_row {
                    Some(row_ix) => table.set_selected_row(row_ix, cx),
                    None => table.clear_selection(cx),
                }
            }
        });
        cx.notify();
    }

    /// The cached visible rows, for the panel's reducer tests.
    #[cfg(test)]
    pub(crate) fn rows(&self) -> &[RowKind] {
        &self.rows
    }

    fn on_table_event(
        &mut self,
        _table: &Entity<TableState<TemplateTableDelegate>>,
        event: &TableEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let TableEvent::SelectRow(row_ix) = event {
            if std::mem::take(&mut self.suppress_select_echo) {
                return;
            }
            if let Some(RowKind::Node { idx, .. }) = self.rows.get(*row_ix) {
                cx.emit(TemplateEvent::Select(*idx));
            }
            // Keep arrow keys on this panel's own bindings: the
            // table's inner focus (taken by the click) would
            // otherwise route arrows to its column-selection
            // handlers.
            window.focus(&self.focus_handle);
        }
    }

    fn on_selection_up(&mut self, _: &SelectionUp, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TemplateEvent::MoveSelection(-1));
    }

    fn on_selection_down(&mut self, _: &SelectionDown, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TemplateEvent::MoveSelection(1));
    }

    fn on_collapse_row(&mut self, _: &CollapseRow, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TemplateEvent::CollapseSelected);
    }

    fn on_expand_row(&mut self, _: &ExpandRow, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TemplateEvent::ExpandSelected);
    }

    fn render_header(&self, show_colors: Option<bool>, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_2()
            .px_2()
            .py_1()
            .items_center()
            .child(Label::new(hxy_i18n::t("template-panel-title")))
            .child(div().flex_1())
            .when_some(show_colors, |row, on| {
                row.child(
                    Button::new("tmpl-toggle-colors")
                        .compact()
                        .icon(Icon::new(IconName::Palette))
                        .selected(on)
                        .tooltip(SharedString::from(hxy_i18n::t("template-toggle-colors")))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(TemplateEvent::ToggleColors(!on)))),
                )
            })
            .child(
                Button::new("tmpl-close")
                    .ghost()
                    .compact()
                    .icon(Icon::new(IconName::Close))
                    .tooltip(SharedString::from(hxy_i18n::t("template-close")))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(TemplateEvent::HidePanel))),
            )
    }

    fn render_tab_strip(&self, tabs: Vec<TabInfo>, cx: &Context<Self>) -> impl IntoElement {
        let mut strip = h_flex().gap_1().px_2().pb_1().flex_wrap().items_center();
        for tab in tabs {
            let id = tab.id;
            let mut entry = h_flex().gap_0p5().items_center();
            if tab.running {
                entry = entry.child(Spinner::new().xsmall());
            }
            entry = entry
                .child(
                    Button::new(("tmpl-tab", id.get() as usize))
                        .compact()
                        .label(tab.label)
                        .selected(tab.active)
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(TemplateEvent::SetActive(id)))),
                )
                .child(
                    Button::new(("tmpl-tab-close", id.get() as usize))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::Close))
                        .tooltip(SharedString::from(hxy_i18n::t("template-remove-instance")))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(TemplateEvent::RemoveInstance(id)))),
                );
            strip = strip.child(entry);
        }
        strip
    }

    fn render_diagnostics(
        &self,
        diagnostics: Vec<hxy_plugin_host::template::Diagnostic>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let has_error = diagnostics.iter().any(|d| matches!(d.severity, Severity::Error));
        // Auto-open on errors until the user picks a state (mirrors
        // egui's `default_open(has_error)`).
        let open = self.diagnostics_open.unwrap_or(has_error);
        let mut section = v_flex().px_2().gap_0p5().child(
            Button::new("tmpl-diagnostics-toggle")
                .ghost()
                .compact()
                .icon(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }))
                .label(hxy_i18n::t_args("template-diagnostics", &[("count", &diagnostics.len().to_string())]))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.diagnostics_open = Some(!open);
                    cx.notify();
                })),
        );
        if open {
            let numeric_format = crate::settings::formats(cx).0;
            for (i, diag) in diagnostics.into_iter().enumerate() {
                let (icon, color) = match diag.severity {
                    Severity::Error => (IconName::CircleX, cx.theme().danger),
                    Severity::Warning => (IconName::TriangleAlert, cx.theme().warning),
                    Severity::Info => (IconName::Info, cx.theme().muted_foreground),
                };
                let mut row = h_flex()
                    .gap_1()
                    .pl_4()
                    .items_center()
                    .child(Icon::new(icon).xsmall().text_color(color))
                    .child(Label::new(diag.message));
                if let Some(offset) = diag.file_offset {
                    row = row.child(
                        Button::new(("tmpl-diag-jump", i))
                            .link()
                            .xsmall()
                            .label(format_offset(offset, numeric_format.pick(offset)))
                            .on_click(
                                cx.listener(move |_, _, _, cx| cx.emit(TemplateOffsetJump(ByteOffset::new(offset)))),
                            ),
                    );
                }
                section = section.child(row);
            }
        }
        section
    }

    fn render_running(&self, name: &str, started: jiff::Timestamp, cx: &Context<Self>) -> impl IntoElement {
        // Non-negative clamp: a clock step backwards between spawn
        // and render would otherwise show a negative elapsed time.
        let elapsed_ms = jiff::Timestamp::now().duration_since(started).as_millis().max(0);
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Spinner::new().small())
                    .child(Label::new(hxy_i18n::t_args("template-running", &[("name", name)]))),
            )
            .child(
                Label::new(hxy_i18n::t_args("template-running-elapsed", &[("ms", &elapsed_ms.to_string())]))
                    .text_color(cx.theme().muted_foreground),
            )
    }

    fn muted_line(&self, text: String, cx: &Context<Self>) -> AnyElement {
        div().p_2().text_color(cx.theme().muted_foreground).child(text).into_any_element()
    }
}

/// One tab-strip entry, precomputed so the render pass doesn't
/// re-borrow the owning panel per tab.
struct TabInfo {
    id: TemplateInstanceId,
    label: String,
    active: bool,
    running: bool,
}

/// Port of the egui strip's label rules (`render_tab_button`): a
/// single whole-file instance shows the bare name, several
/// whole-file instances disambiguate with "(whole file)", and a
/// sliced instance carries its byte range.
fn tab_label(name: &str, range: Option<ByteRange>, whole_file_len: u64, only_one: bool) -> String {
    let Some(range) = range else { return name.to_owned() };
    let covers_whole_file = range.start().get() == 0 && range.len().get() == whole_file_len;
    if covers_whole_file && only_one {
        name.to_owned()
    } else if covers_whole_file {
        format!("{name}  {}", hxy_i18n::t("template-whole-file"))
    } else {
        format!("{name}  [{:#x}..{:#x}]", range.start().get(), range.end().get())
    }
}

impl Focusable for TemplateView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<TemplateEvent> for TemplateView {}
impl EventEmitter<TemplateOffsetJump> for TemplateView {}

impl Render for TemplateView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = v_flex().size_full().bg(cx.theme().background);
        let Some(panel) = self.file.upgrade() else {
            return root;
        };

        // Breadcrumb for the byte under the pointer in the hex grid.
        // Alt (Option on macOS) switches to the full struct path and
        // flips the numeric format, mirroring the egui tooltip
        // (crates/hxy/src/view/hex_body.rs).
        let alt = window.modifiers().alt;
        let value_formats = crate::settings::formats(cx).1;
        let breadcrumb = {
            let pane_ref = self.pane.read(cx);
            pane_ref.hovered_offset().and_then(|byte| {
                let panel_ref = panel.read(cx);
                let instance = panel_ref.active_template()?;
                breadcrumb_line(
                    &instance.state.tree,
                    pane_ref.editor().source().as_ref(),
                    byte.get(),
                    alt,
                    &value_formats,
                )
            })
        };

        // Extract owned render data up front so the panel borrow
        // doesn't extend into element construction.
        struct ActiveBody {
            diagnostics: Vec<hxy_plugin_host::template::Diagnostic>,
            has_nodes: bool,
        }
        let (tabs, show_colors, active_running, active_body) = {
            let panel_ref = panel.read(cx);
            let whole_file_len = panel_ref.pane().read(cx).editor().source().len().get();
            let only_one = panel_ref.templates.len() + panel_ref.templates_running.len() == 1;
            let active = panel_ref.active_template;
            let mut tabs = Vec::new();
            for instance in &panel_ref.templates {
                tabs.push(TabInfo {
                    id: instance.id,
                    label: tab_label(&instance.display_name, Some(instance.range), whole_file_len, only_one),
                    active: active == Some(instance.id),
                    running: false,
                });
            }
            for running in &panel_ref.templates_running {
                tabs.push(TabInfo {
                    id: running.id,
                    label: tab_label(&running.display_name, None, whole_file_len, only_one),
                    active: active == Some(running.id),
                    running: true,
                });
            }
            let show_colors = panel_ref.active_template().map(|t| t.state.show_colors);
            let active_running = active.and_then(|id| {
                panel_ref.templates_running.iter().find(|r| r.id == id).map(|r| (r.display_name.clone(), r.started))
            });
            let active_body = panel_ref.active_template().map(|t| ActiveBody {
                diagnostics: t.state.tree.diagnostics.clone(),
                has_nodes: !t.state.tree.nodes.is_empty(),
            });
            (tabs, show_colors, active_running, active_body)
        };

        let mut root = root.child(self.render_header(show_colors, cx));
        if let Some(text) = breadcrumb {
            root = root.child(
                div().px_2().pb_1().font_family(cx.theme().mono_font_family.clone()).text_sm().truncate().child(text),
            );
        }
        let mut root =
            root.child(self.render_tab_strip(tabs, cx)).child(div().border_t_1().border_color(cx.theme().border));

        if let Some((name, started)) = active_running {
            return root.child(self.render_running(&name, started, cx));
        }
        let Some(body) = active_body else {
            return root.child(self.muted_line(hxy_i18n::t("template-no-template"), cx));
        };
        if !body.diagnostics.is_empty() {
            root = root.child(self.render_diagnostics(body.diagnostics, cx));
        }
        if !body.has_nodes {
            return root.child(self.muted_line(hxy_i18n::t("template-no-tree"), cx));
        }
        root.child(
            div()
                .id("tmpl-table-wrap")
                .key_context(KEY_CONTEXT)
                .track_focus(&self.focus_handle)
                .on_action(cx.listener(Self::on_selection_up))
                .on_action(cx.listener(Self::on_selection_down))
                .on_action(cx.listener(Self::on_collapse_row))
                .on_action(cx.listener(Self::on_expand_row))
                .flex_1()
                .min_h_0()
                .child(Table::new(&self.table)),
        )
    }
}

/// Single-line breadcrumb for the template field covering `byte`:
/// the [`breadcrumb_for_offset`] path with its tree decorations
/// stripped, joined with " > ". `alt` selects the full root-to-leaf
/// chain (and the inverse numeric format); the default is the compact
/// leaf line. `None` when no field covers the offset.
fn breadcrumb_line(
    tree: &hxy_plugin_host::template::ResultTree,
    source: &dyn HexSource,
    byte: u64,
    alt: bool,
    fmts: &TemplateValueFormats,
) -> Option<String> {
    let detail = if alt { BreadcrumbDetail::Full } else { BreadcrumbDetail::Leaf };
    let path = breadcrumb_for_offset(tree, source, byte, detail, fmts, alt)?;
    let line =
        path.iter().map(|row| row.trim_start_matches([' ', '\u{2514}', '\u{2500}'])).collect::<Vec<_>>().join(" > ");
    Some(line)
}

/// Emit a [`TemplateEvent`] on the view from a widget callback that
/// only has an `&mut App` (button clicks, popover entries).
fn emit_event(view: &WeakEntity<TemplateView>, cx: &mut App, event: TemplateEvent) {
    if let Some(view) = view.upgrade() {
        view.update(cx, |_, cx| cx.emit(event));
    }
}

/// Fixed palette offered by the color-swatch popover: twelve evenly
/// spaced hues through the same HSV ramp the auto-assignment uses,
/// so picked colors sit naturally next to the fallback ones.
fn swatch_palette() -> [Rgba; 12] {
    std::array::from_fn(|i| Rgba::from_hsv(i as f32 / 12.0, 0.6, 0.9))
}

/// Pull a non-empty `hxy_comment` off the node, or `None`.
fn node_comment(node: &Node) -> Option<&str> {
    node.attributes
        .iter()
        .find_map(|(k, v)| (k == hxy_plugin_host::COMMENT_ATTR && !v.is_empty()).then_some(v.as_str()))
}

/// The visualizer name from a `hxy_visualize` / `hxy_inline_visualize`
/// attribute, if the node carries one (the part before the first
/// argument separator). Feeds the marker icon's tooltip; the click
/// emits `TemplateEvent::OpenVisualizer`, which the owning `FilePanel`
/// forwards to the workspace to open the visualizer panel.
fn node_visualizer_name(node: &Node) -> Option<String> {
    let raw = node.attributes.iter().find_map(|(k, v)| {
        ((k == hxy_plugin_host::VISUALIZE_ATTR || k == hxy_plugin_host::INLINE_VISUALIZE_ATTR) && !v.is_empty())
            .then_some(v.as_str())
    })?;
    raw.split(hxy_plugin_host::VISUALIZE_ARG_SEP).next().map(str::to_owned)
}

/// Whether the node's decoded value is a plain integer (the kinds the
/// scalar copy formatter accepts).
fn is_scalar_int(node: &Node) -> bool {
    use hxy_plugin_host::template::Value;
    node.value.as_ref().is_some_and(|v| {
        matches!(
            v,
            Value::U8Val(_)
                | Value::U16Val(_)
                | Value::U32Val(_)
                | Value::U64Val(_)
                | Value::S8Val(_)
                | Value::S16Val(_)
                | Value::S32Val(_)
                | Value::S64Val(_)
        )
    })
}

fn is_struct_node(node: &Node) -> bool {
    matches!(
        node.type_name,
        hxy_plugin_host::template::NodeType::StructType(_) | hxy_plugin_host::template::NodeType::StructArray(_)
    )
}

struct TemplateTableDelegate {
    columns: [Column; 7],
    view: WeakEntity<TemplateView>,
}

impl TemplateTableDelegate {
    fn new(view: WeakEntity<TemplateView>) -> Self {
        Self {
            columns: [
                Column::new("color", hxy_i18n::t("template-col-color")).width(px(40.0)).resizable(false),
                Column::new("name", hxy_i18n::t("template-col-name")).width(px(240.0)),
                Column::new("type", hxy_i18n::t("template-col-type")).width(px(120.0)),
                Column::new("start", hxy_i18n::t("template-col-start")).width(px(90.0)),
                Column::new("end", hxy_i18n::t("template-col-end")).width(px(90.0)),
                Column::new("length", hxy_i18n::t("template-col-length")).width(px(70.0)),
                Column::new("value", hxy_i18n::t("template-col-value")).width(px(220.0)),
            ],
            view,
        }
    }

    /// The active instance's node for a tree index, cloned out so cell
    /// construction doesn't hold entity borrows.
    fn node(&self, idx: TemplateNodeIdx, cx: &App) -> Option<Node> {
        let view = self.view.upgrade()?;
        let panel = view.read(cx).file.upgrade()?;
        let node = panel.read(cx).active_template()?.state.tree.nodes.get(idx.0 as usize)?.clone();
        Some(node)
    }

    fn row(&self, row_ix: usize, cx: &App) -> Option<RowKind> {
        Some(self.view.upgrade()?.read(cx).rows.get(row_ix)?.clone())
    }

    /// The user's numeric/value formats, straight from the settings
    /// global (replaces the M4a `Default::default()` placeholders).
    fn formats(&self, cx: &App) -> (NumericFormat, TemplateValueFormats) {
        crate::settings::formats(cx)
    }

    fn numeric_cell(&self, value: u64, cx: &App) -> AnyElement {
        let (numeric_format, _) = self.formats(cx);
        let text = format_offset(value, numeric_format.pick(value));
        div().font_family(cx.theme().mono_font_family.clone()).truncate().child(text).into_any_element()
    }

    fn type_cell(&self, label: String, cx: &App) -> AnyElement {
        div().truncate().text_color(cx.theme().muted_foreground).child(label).into_any_element()
    }

    /// Name-column cell for a real tree node: indent, expand/collapse
    /// chevron on parents, the name, and the comment / visualizer
    /// markers.
    #[allow(clippy::too_many_arguments)]
    fn name_cell(
        &self,
        row_ix: usize,
        idx: TemplateNodeIdx,
        node: &Node,
        depth: usize,
        is_parent: bool,
        collapsed: bool,
    ) -> AnyElement {
        let mut row = h_flex().gap_1().items_center().pl(px(depth as f32 * INDENT_STEP));
        if is_parent {
            let weak = self.view.clone();
            row = row.child(
                Button::new(("tmpl-chevron", row_ix))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(if collapsed { IconName::ChevronRight } else { IconName::ChevronDown }))
                    .on_click(move |_, _, cx| {
                        // Expanding is a deliberate per-cell action;
                        // don't also select the row (selecting a
                        // parent paints its whole span).
                        cx.stop_propagation();
                        emit_event(&weak, cx, TemplateEvent::ToggleCollapse(idx));
                    }),
            );
        } else {
            row = row.child(div().w(px(14.0)));
        }
        row = row.child(div().truncate().child(node.name.clone()));
        if let Some(comment) = node_comment(node) {
            row = row.child(
                Button::new(("tmpl-comment", row_ix))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Info))
                    .tooltip(SharedString::from(comment.to_owned())),
            );
        }
        if let Some(name) = node_visualizer_name(node) {
            let weak = self.view.clone();
            row = row.child(
                Button::new(("tmpl-visualize", row_ix))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Eye))
                    .tooltip(SharedString::from(hxy_i18n::t_args("visualizer-row-tooltip", &[("name", &name)])))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        emit_event(&weak, cx, TemplateEvent::OpenVisualizer(idx));
                    }),
            );
        }
        row.into_any_element()
    }

    /// Color-column swatch for a leaf node: shows the resolved tint,
    /// click opens a fixed-palette popover, shift-click resets an
    /// override back to the auto color.
    fn swatch_cell(&self, idx: TemplateNodeIdx, state: &TemplateState, cx: &App) -> AnyElement {
        let Some(&slot) = state.leaf_slot_by_node.get(&idx.0) else {
            return div().into_any_element();
        };
        let color = rgba_to_hsla(state.leaf_colors[slot]);
        let has_override = state.node_color_overrides.contains_key(&idx.0);
        let is_open = self.view.upgrade().is_some_and(|view| view.read(cx).color_popover == Some(idx));
        let tooltip = if has_override {
            hxy_i18n::t("template-swatch-tooltip-override")
        } else {
            hxy_i18n::t("template-swatch-tooltip")
        };
        let weak = self.view.clone();
        let trigger = Button::new(("tmpl-swatch", idx.0 as usize))
            .w(px(22.0))
            .h(px(14.0))
            .bg(color)
            .border_1()
            .border_color(cx.theme().border)
            .tooltip(SharedString::from(tooltip))
            .on_click(move |event: &ClickEvent, _, cx| {
                cx.stop_propagation();
                if event.modifiers().shift
                    && let Some(view) = weak.upgrade()
                {
                    view.update(cx, |view, cx| {
                        view.color_popover = None;
                        if has_override {
                            cx.emit(TemplateEvent::ResetColor(idx));
                        }
                        cx.notify();
                    });
                }
            });
        let weak = self.view.clone();
        let weak_content = self.view.clone();
        Popover::new(("tmpl-swatch-pop", idx.0 as usize))
            .open(is_open)
            .on_open_change(move |open, _, cx| {
                let open = *open;
                if let Some(view) = weak.upgrade() {
                    view.update(cx, |view, cx| {
                        view.color_popover = if open { Some(idx) } else { None };
                        cx.notify();
                    });
                }
            })
            .trigger(trigger)
            .content(move |_, _, _| {
                let mut grid = h_flex().gap_1().flex_wrap().w(px(4.0 * 26.0));
                for (i, color) in swatch_palette().into_iter().enumerate() {
                    let weak = weak_content.clone();
                    grid = grid.child(
                        Button::new(("tmpl-swatch-pick", i)).w(px(22.0)).h(px(22.0)).bg(rgba_to_hsla(color)).on_click(
                            move |_, _, cx| {
                                cx.stop_propagation();
                                if let Some(view) = weak.upgrade() {
                                    view.update(cx, |view, cx| {
                                        view.color_popover = None;
                                        cx.emit(TemplateEvent::SetColor { idx, color });
                                        cx.notify();
                                    });
                                }
                            },
                        ),
                    );
                }
                grid.into_any_element()
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_node_cell(
        &self,
        row_ix: usize,
        col_ix: usize,
        idx: TemplateNodeIdx,
        depth: usize,
        is_parent: bool,
        collapsed: bool,
        cx: &App,
    ) -> AnyElement {
        let Some(node) = self.node(idx, cx) else {
            return div().into_any_element();
        };
        match col_ix {
            0 => {
                let Some(view) = self.view.upgrade() else {
                    return div().into_any_element();
                };
                let Some(panel) = view.read(cx).file.upgrade() else {
                    return div().into_any_element();
                };
                let panel_ref = panel.read(cx);
                let Some(instance) = panel_ref.active_template() else {
                    return div().into_any_element();
                };
                self.swatch_cell(idx, &instance.state, cx)
            }
            1 => self.name_cell(row_ix, idx, &node, depth, is_parent, collapsed),
            2 => self.type_cell(hxy_plugin_host::node_display_type(&node), cx),
            3 => self.numeric_cell(node.span.offset, cx),
            4 => self.numeric_cell(node.span.offset.saturating_add(node.span.length), cx),
            5 => self.numeric_cell(node.span.length, cx),
            6 => {
                let (_, fmts) = self.formats(cx);
                // Always the primary base: the egui table flips to the
                // inverse numeric base while Alt is held; here Alt only
                // affects the breadcrumb strip.
                match format_value(&node, &fmts, false) {
                    Some(text) => div().truncate().child(text).into_any_element(),
                    None => div().into_any_element(),
                }
            }
            _ => div().into_any_element(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_deferred_cell(
        &self,
        row_ix: usize,
        col_ix: usize,
        array_id: TemplateArrayId,
        count: u64,
        stride: u64,
        first_offset: u64,
        element_type: &str,
        depth: usize,
        cx: &App,
    ) -> AnyElement {
        let total_len = count.saturating_mul(stride);
        match col_ix {
            1 => {
                let weak = self.view.clone();
                h_flex()
                    .gap_1()
                    .items_center()
                    .pl(px(depth as f32 * INDENT_STEP + 14.0))
                    .child(div().truncate().text_color(cx.theme().muted_foreground).child(hxy_i18n::t_args(
                        "template-deferred-array",
                        &[("count", &count.to_string()), ("type", element_type)],
                    )))
                    .child(
                        Button::new(("tmpl-expand", row_ix))
                            .compact()
                            .xsmall()
                            .label(hxy_i18n::t("template-expand"))
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                emit_event(&weak, cx, TemplateEvent::ExpandArray { array_id, count });
                            }),
                    )
                    .into_any_element()
            }
            2 => self.type_cell(element_type.to_owned(), cx),
            3 => self.numeric_cell(first_offset, cx),
            4 => self.numeric_cell(first_offset.saturating_add(total_len), cx),
            5 => self.numeric_cell(total_len, cx),
            _ => div().into_any_element(),
        }
    }

    fn render_array_element_cell(
        &self,
        row_ix: usize,
        col_ix: usize,
        array_id: TemplateArrayId,
        index: usize,
        depth: usize,
        cx: &App,
    ) -> AnyElement {
        let node = (|| {
            let view = self.view.upgrade()?;
            let panel = view.read(cx).file.upgrade()?;
            let node = panel.read(cx).active_template()?.state.expanded_arrays.get(&array_id)?.get(index)?.clone();
            Some(node)
        })();
        let Some(node) = node else {
            return div().into_any_element();
        };
        match col_ix {
            1 => {
                let mut row = h_flex()
                    .gap_1()
                    .items_center()
                    .pl(px(depth as f32 * INDENT_STEP + 14.0))
                    .child(div().truncate().child(format!("[{index}]")));
                if let Some(comment) = node_comment(&node) {
                    row = row.child(
                        Button::new(("tmpl-elem-comment", row_ix))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Info))
                            .tooltip(SharedString::from(comment.to_owned())),
                    );
                }
                row.into_any_element()
            }
            2 => self.type_cell(hxy_plugin_host::node_display_type(&node), cx),
            3 => self.numeric_cell(node.span.offset, cx),
            4 => self.numeric_cell(node.span.offset.saturating_add(node.span.length), cx),
            5 => self.numeric_cell(node.span.length, cx),
            6 => {
                let (_, fmts) = self.formats(cx);
                // Always the primary base: the egui table flips to the
                // inverse numeric base while Alt is held; here Alt only
                // affects the breadcrumb strip.
                match format_value(&node, &fmts, false) {
                    Some(text) => div().truncate().child(text).into_any_element(),
                    None => div().into_any_element(),
                }
            }
            _ => div().into_any_element(),
        }
    }

    /// One synthesized element row of a fixed-size primitive array:
    /// the parent ScalarArray is a single node, so element values
    /// decode from the source on demand (port of the egui panel's
    /// `render_scalar_array_element_cell`).
    fn render_scalar_array_element_cell(
        &self,
        col_ix: usize,
        parent_idx: TemplateNodeIdx,
        index: u64,
        depth: usize,
        cx: &App,
    ) -> AnyElement {
        let Some(parent) = self.node(parent_idx, cx) else {
            return div().into_any_element();
        };
        let hxy_plugin_host::template::NodeType::ScalarArray((kind, _count)) = parent.type_name else {
            return div().into_any_element();
        };
        let Some(elem_width) = scalar_kind_width(kind) else {
            return div().into_any_element();
        };
        if elem_width == 0 {
            return div().into_any_element();
        }
        let elem_offset = parent.span.offset.saturating_add(index * elem_width);
        match col_ix {
            1 => h_flex()
                .items_center()
                .pl(px(depth as f32 * INDENT_STEP + 14.0))
                .child(div().truncate().child(format!("[{index}]")))
                .into_any_element(),
            2 => self.type_cell(scalar_kind_name(kind).to_owned(), cx),
            3 => self.numeric_cell(elem_offset, cx),
            4 => self.numeric_cell(elem_offset.saturating_add(elem_width), cx),
            5 => self.numeric_cell(elem_width, cx),
            6 => {
                let source: Option<std::sync::Arc<dyn HexSource>> = (|| {
                    let view = self.view.upgrade()?;
                    let panel = view.read(cx).file.upgrade()?;
                    Some(panel.read(cx).pane().read(cx).editor().source().clone())
                })();
                let Some(source) = source else {
                    return div().into_any_element();
                };
                // serde(default)-style rule from the egui panel: the
                // runtime omits `hxy_endian` for little-endian data.
                let endian = parent
                    .attributes
                    .iter()
                    .find_map(|(k, v)| (k == hxy_plugin_host::ENDIAN_ATTR).then_some(v.as_str()))
                    .unwrap_or("little");
                let range = match ByteRange::new(
                    hxy_core::ByteOffset::new(elem_offset),
                    hxy_core::ByteOffset::new(elem_offset.saturating_add(elem_width)),
                ) {
                    Ok(r) => r,
                    Err(_) => return div().into_any_element(),
                };
                let bytes = match source.read(range) {
                    Ok(b) => b,
                    Err(_) => return div().into_any_element(),
                };
                let (_, fmts) = self.formats(cx);
                match decode_scalar_bytes(kind, &bytes, endian, parent.display, &fmts, false) {
                    Some(text) => div().truncate().child(text).into_any_element(),
                    None => div().into_any_element(),
                }
            }
            _ => div().into_any_element(),
        }
    }
}

impl TableDelegate for TemplateTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, cx: &App) -> usize {
        self.view.upgrade().map(|view| view.read(cx).rows.len()).unwrap_or(0)
    }

    fn column(&self, col_ix: usize, _cx: &App) -> &Column {
        &self.columns[col_ix]
    }

    /// Row hover feeds the hex view's hover band (via the reducer's
    /// `Hover` arm). Only real tree-node rows have a span to
    /// highlight; hovering any other row clears it.
    fn render_tr(&mut self, row_ix: usize, _window: &mut Window, cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let node_idx = match self.row(row_ix, cx) {
            Some(RowKind::Node { idx, .. }) => Some(idx),
            _ => None,
        };
        let weak = self.view.clone();
        div().id(("tmpl-row", row_ix)).on_hover(move |hovered, _window, cx| {
            let Some(view) = weak.upgrade() else { return };
            view.update(cx, |view, cx| {
                if *hovered {
                    view.hovered_row = Some(row_ix);
                    cx.emit(TemplateEvent::Hover(node_idx));
                } else if view.hovered_row == Some(row_ix) {
                    view.hovered_row = None;
                    cx.emit(TemplateEvent::Hover(None));
                }
            });
        })
    }

    // Compact three-entry menu; egui additionally offers per-base
    // copy-as submenus (bytes-as / value-as / C struct variants).
    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(RowKind::Node { idx, .. }) = self.row(row_ix, cx) else {
            return menu;
        };
        let Some(node) = self.node(idx, cx) else {
            return menu;
        };
        // Scalar ints copy their decoded value; everything else
        // copies the node's byte span as spaced hex.
        let value_kind = if is_scalar_int(&node) { CopyKind::ValueHex } else { CopyKind::BytesHexSpaced };
        let weak = self.view.clone();
        let weak_struct = self.view.clone();
        let weak_save = self.view.clone();
        let mut menu = menu
            .label(hxy_i18n::t_args(
                "template-row-bytes",
                &[("name", &node.name), ("len", &node.span.length.to_string())],
            ))
            .separator()
            .item(PopupMenuItem::new(hxy_i18n::t("template-copy-value")).on_click(move |_, _, cx| {
                emit_event(&weak, cx, TemplateEvent::Copy { idx, kind: value_kind });
            }));
        if is_struct_node(&node) {
            menu = menu.item(PopupMenuItem::new(hxy_i18n::t("template-copy-struct")).on_click(move |_, _, cx| {
                emit_event(&weak_struct, cx, TemplateEvent::Copy { idx, kind: CopyKind::StructRust });
            }));
        }
        menu.separator().item(PopupMenuItem::new(hxy_i18n::t("template-save-bytes")).on_click(move |_, _, cx| {
            emit_event(&weak_save, cx, TemplateEvent::SaveBytes(idx));
        }))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.row(row_ix, cx) else {
            return div().into_any_element();
        };
        match row {
            RowKind::Node { idx, depth, is_parent, collapsed } => {
                self.render_node_cell(row_ix, col_ix, idx, depth, is_parent, collapsed, cx)
            }
            RowKind::DeferredArray { array_id, count, stride, first_offset, element_type, depth } => self
                .render_deferred_cell(row_ix, col_ix, array_id, count, stride, first_offset, &element_type, depth, cx),
            RowKind::ArrayElement { array_id, index, depth } => {
                self.render_array_element_cell(row_ix, col_ix, array_id, index, depth, cx)
            }
            RowKind::ScalarArrayElement { parent_idx, index, depth } => {
                self.render_scalar_array_element_cell(col_ix, parent_idx, index, depth, cx)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use gpui::TestAppContext;
    use hxy_core::ByteOffset;
    use hxy_core::MemorySource;
    use hxy_core::Selection;
    use hxy_plugin_host::ParsedTemplate;
    use hxy_plugin_host::template::Arg;
    use hxy_plugin_host::template::DeferredArray;
    use hxy_plugin_host::template::NodeType;
    use hxy_plugin_host::template::ResultTree;
    use hxy_plugin_host::template::ScalarKind;
    use hxy_plugin_host::template::Span;
    use hxy_plugin_host::template::Value;
    use hxy_templates::state::TemplateInstance;
    use hxy_templates::state::new_state_from;
    use hxy_vfs::HandlerError;

    use super::*;
    use crate::panels::FilePanel;

    /// Deferred-array-capable stand-in for a parsed template, so the
    /// tests can drive `ExpandArray` without a real runtime.
    struct FakeParsed {
        elements: Vec<Node>,
    }

    impl ParsedTemplate for FakeParsed {
        fn execute(&self, _args: &[Arg]) -> Result<ResultTree, HandlerError> {
            Err(HandlerError::Unsupported("test template never re-executes".into()))
        }

        fn expand_array(&self, _array_id: u64, start: u64, end: u64) -> Result<Vec<Node>, HandlerError> {
            let end = (end as usize).min(self.elements.len());
            Ok(self.elements[start as usize..end].to_vec())
        }
    }

    fn tree_node(name: &str, type_name: NodeType, parent: Option<u32>, value: Option<Value>, span: (u64, u64)) -> Node {
        Node {
            name: name.to_owned(),
            type_name,
            span: Span { offset: span.0, length: span.1 },
            value,
            parent,
            array: None,
            display: None,
            attributes: Vec::new(),
        }
    }

    /// Fixture: a 2-level struct (all-scalar children), a deferred
    /// array, and a fixed-size primitive array, all root-level.
    ///
    /// Node indices: 0 = hdr (struct, children 1-2), 1 = magic,
    /// 2 = len, 3 = entries (deferred array id 7, 3 x 2 bytes),
    /// 4 = name (uchar[4]).
    fn fixture_tree() -> ResultTree {
        let mut entries = tree_node("entries", NodeType::Unknown("Entry[]".to_owned()), None, None, (8, 6));
        entries.array =
            Some(DeferredArray { id: 7, element_type: "Entry".to_owned(), count: 3, stride: 2, first_offset: 8 });
        ResultTree {
            nodes: vec![
                tree_node("hdr", NodeType::StructType("Header".to_owned()), None, None, (0, 8)),
                tree_node(
                    "magic",
                    NodeType::Scalar(ScalarKind::U32K),
                    Some(0),
                    Some(Value::U32Val(0xAABBCCDD)),
                    (0, 4),
                ),
                tree_node("len", NodeType::Scalar(ScalarKind::U32K), Some(0), Some(Value::U32Val(4)), (4, 4)),
                entries,
                tree_node("name", NodeType::ScalarArray((ScalarKind::U8K, 4)), None, None, (14, 4)),
            ],
            diagnostics: Vec::new(),
            byte_palette: None,
        }
    }

    fn fixture_elements() -> Vec<Node> {
        (0..3u64)
            .map(|i| {
                tree_node(
                    "entry",
                    NodeType::Scalar(ScalarKind::U16K),
                    None,
                    Some(Value::U16Val(i as u16)),
                    (8 + i * 2, 2),
                )
            })
            .collect()
    }

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::workspace::init_keybindings(cx);
        });
    }

    fn build(cx: &mut TestAppContext) -> (gpui::Entity<FilePanel>, &mut gpui::VisualTestContext) {
        let window = cx.add_window(|window, cx| {
            let source: Arc<dyn HexSource> = Arc::new(MemorySource::new((0u8..32).collect::<Vec<_>>()));
            let panel = cx.new(|cx| FilePanel::new(source, None, window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<FilePanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        (panel, vcx)
    }

    /// Install the fixture tree as a completed instance on the panel
    /// and return its id.
    fn install_fixture(panel: &gpui::Entity<FilePanel>, cx: &mut gpui::VisualTestContext) -> TemplateInstanceId {
        install_tree(panel, cx, fixture_tree())
    }

    fn install_tree(
        panel: &gpui::Entity<FilePanel>,
        cx: &mut gpui::VisualTestContext,
        tree: ResultTree,
    ) -> TemplateInstanceId {
        let id = panel.update(cx, |panel, cx| {
            let state = new_state_from(Arc::new(FakeParsed { elements: fixture_elements() }), tree, HashMap::new());
            let id = panel.fresh_template_instance_id();
            panel.upsert_template_instance(TemplateInstance {
                id,
                source_path: PathBuf::from("/tmp/fixture.bt"),
                display_name: "fixture.bt".to_owned(),
                range: ByteRange::new(ByteOffset::new(0), ByteOffset::new(18)).unwrap(),
                source_fingerprint: None,
                state,
            });
            panel.active_template = Some(id);
            panel.template_panel_visible = true;
            panel.sync_template_rows(cx);
            panel.sync_pane_overlays(cx);
            id
        });
        cx.run_until_parked();
        id
    }

    fn view_rows(panel: &gpui::Entity<FilePanel>, cx: &gpui::VisualTestContext) -> usize {
        panel.read_with(cx, |panel, cx| panel.template_view().read(cx).rows().len())
    }

    fn apply(panel: &gpui::Entity<FilePanel>, cx: &mut gpui::VisualTestContext, event: TemplateEvent) {
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| panel.apply_template_event(&event, window, cx));
        });
        cx.run_until_parked();
    }

    /// The visible-row cache mirrors `build_visible`: expandable
    /// parents start collapsed; expanding the struct, the deferred
    /// array (placeholder -> Expand -> elements), and the primitive
    /// array each reveal their rows; re-collapsing hides them again.
    #[gpui::test]
    fn visible_rows_round_trip_through_the_reducer(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        install_fixture(&panel, cx);

        // Every expandable parent starts collapsed: 3 root rows.
        assert_eq!(view_rows(&panel, cx), 3);

        apply(&panel, cx, TemplateEvent::ToggleCollapse(TemplateNodeIdx(0)));
        assert_eq!(view_rows(&panel, cx), 5, "struct children revealed");

        apply(&panel, cx, TemplateEvent::ToggleCollapse(TemplateNodeIdx(3)));
        assert_eq!(view_rows(&panel, cx), 6, "deferred array shows its placeholder row");
        let has_placeholder = panel.read_with(cx, |panel, cx| {
            panel
                .template_view()
                .read(cx)
                .rows()
                .iter()
                .any(|r| matches!(r, RowKind::DeferredArray { array_id, count: 3, .. } if array_id.0 == 7))
        });
        assert!(has_placeholder);

        apply(&panel, cx, TemplateEvent::ExpandArray { array_id: TemplateArrayId(7), count: 3 });
        assert_eq!(view_rows(&panel, cx), 8, "placeholder replaced by 3 element rows");

        apply(&panel, cx, TemplateEvent::ToggleCollapse(TemplateNodeIdx(4)));
        assert_eq!(view_rows(&panel, cx), 12, "primitive array reveals 4 synthetic element rows");

        apply(&panel, cx, TemplateEvent::ToggleCollapse(TemplateNodeIdx(0)));
        assert_eq!(view_rows(&panel, cx), 10, "collapsing the struct hides only its children");
    }

    /// Select drives the editor selection to the node's span (end
    /// inclusive) and Hover mirrors the node span into the pane's
    /// hover band; Hover(None) clears it.
    #[gpui::test]
    fn select_and_hover_drive_the_pane(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        install_fixture(&panel, cx);

        apply(&panel, cx, TemplateEvent::Select(TemplateNodeIdx(2)));
        let selection = panel.read_with(cx, |panel, cx| panel.pane().read(cx).editor().selection());
        assert_eq!(selection, Some(Selection { anchor: ByteOffset::new(4), cursor: ByteOffset::new(7) }));

        apply(&panel, cx, TemplateEvent::Hover(Some(TemplateNodeIdx(2))));
        let hover = panel.read_with(cx, |panel, cx| panel.pane().read(cx).hover_span());
        assert_eq!(hover, ByteRange::new(ByteOffset::new(4), ByteOffset::new(8)).ok());

        apply(&panel, cx, TemplateEvent::Hover(None));
        assert_eq!(panel.read_with(cx, |panel, cx| panel.pane().read(cx).hover_span()), None);
    }

    /// MoveSelection steps through visible Node rows only, re-firing
    /// the Select side effects for each step.
    #[gpui::test]
    fn move_selection_steps_visible_nodes(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        install_fixture(&panel, cx);
        apply(&panel, cx, TemplateEvent::ToggleCollapse(TemplateNodeIdx(0)));
        apply(&panel, cx, TemplateEvent::Select(TemplateNodeIdx(1)));

        apply(&panel, cx, TemplateEvent::MoveSelection(1));
        let selected = panel.read_with(cx, |panel, _| panel.active_template().and_then(|t| t.state.selected_node));
        assert_eq!(selected, Some(TemplateNodeIdx(2)));
        let selection = panel.read_with(cx, |panel, cx| panel.pane().read(cx).editor().selection());
        assert_eq!(selection, Some(Selection { anchor: ByteOffset::new(4), cursor: ByteOffset::new(7) }));
    }

    /// Removing the active instance reselects the first remaining one;
    /// removing the last leaves no active template (and the panel
    /// unmounts via the render gate).
    #[gpui::test]
    fn remove_instance_reselects_first_remaining(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        let first = install_fixture(&panel, cx);
        let second = install_fixture(&panel, cx);
        assert_ne!(first, second);
        panel.read_with(cx, |panel, _| assert_eq!(panel.active_template, Some(second)));

        apply(&panel, cx, TemplateEvent::RemoveInstance(second));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.active_template, Some(first), "first remaining instance becomes active");
            assert_eq!(panel.templates.len(), 1);
        });

        apply(&panel, cx, TemplateEvent::RemoveInstance(first));
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.active_template, None);
            assert!(panel.templates.is_empty());
        });
        assert_eq!(view_rows(&panel, cx), 0);
    }

    /// SetColor overrides the leaf's slot in `leaf_colors` (the hdr
    /// struct absorbs its all-scalar children, so node 0 is slot 0);
    /// ResetColor restores the auto color.
    #[gpui::test]
    fn set_and_reset_color_recompute_leaf_colors(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        install_fixture(&panel, cx);

        let (auto_color, slot) = panel.read_with(cx, |panel, _| {
            let state = &panel.active_template().unwrap().state;
            let slot = state.leaf_slot_by_node[&0];
            (state.leaf_colors[slot], slot)
        });

        let picked = Rgba::rgb(10, 20, 30);
        apply(&panel, cx, TemplateEvent::SetColor { idx: TemplateNodeIdx(0), color: picked });
        panel.read_with(cx, |panel, _| {
            let state = &panel.active_template().unwrap().state;
            assert_eq!(state.leaf_colors[slot], picked);
            assert_eq!(state.node_color_overrides.get(&0), Some(&picked));
        });

        apply(&panel, cx, TemplateEvent::ResetColor(TemplateNodeIdx(0)));
        panel.read_with(cx, |panel, _| {
            let state = &panel.active_template().unwrap().state;
            assert_eq!(state.leaf_colors[slot], auto_color, "reset restores the auto color");
            assert!(state.node_color_overrides.is_empty());
        });
    }

    /// Copy formats through the shared formatters: scalar value as
    /// hex, struct as a Rust literal.
    #[gpui::test]
    fn copy_writes_formatted_text_to_the_clipboard(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        install_fixture(&panel, cx);

        apply(&panel, cx, TemplateEvent::Copy { idx: TemplateNodeIdx(1), kind: CopyKind::ValueHex });
        let text = cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(text.as_deref(), Some("0xAABBCCDD"));

        apply(&panel, cx, TemplateEvent::Copy { idx: TemplateNodeIdx(0), kind: CopyKind::StructRust });
        let text = cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(
            text.as_deref(),
            Some("let hdr: Header = Header {\n    magic: 0xAABBCCDD,\n    len: 0x00000004,\n};")
        );
    }

    /// ToggleColors flips the active instance's flag and HidePanel
    /// hides the section without dropping instances.
    #[gpui::test]
    fn toggle_colors_and_hide_panel_flip_flags(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        install_fixture(&panel, cx);

        panel.read_with(cx, |panel, _| assert!(panel.active_template().unwrap().state.show_colors));
        apply(&panel, cx, TemplateEvent::ToggleColors(false));
        panel.read_with(cx, |panel, _| assert!(!panel.active_template().unwrap().state.show_colors));

        apply(&panel, cx, TemplateEvent::HidePanel);
        panel.read_with(cx, |panel, _| {
            assert!(!panel.template_panel_visible);
            assert_eq!(panel.templates.len(), 1, "instances survive hiding the panel");
        });
    }

    /// Arrow keys reach the reducer through the "TemplateTable" key
    /// context when the table wrapper has focus.
    #[gpui::test]
    fn arrow_keys_move_the_selection_when_table_focused(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        install_fixture(&panel, cx);
        apply(&panel, cx, TemplateEvent::ToggleCollapse(TemplateNodeIdx(0)));
        apply(&panel, cx, TemplateEvent::Select(TemplateNodeIdx(1)));

        let focus = panel.read_with(cx, |panel, cx| panel.template_view().read(cx).focus_handle.clone());
        cx.update(|window, _| window.focus(&focus));
        cx.run_until_parked();

        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        let selected = panel.read_with(cx, |panel, _| panel.active_template().and_then(|t| t.state.selected_node));
        assert_eq!(selected, Some(TemplateNodeIdx(2)), "down moves to the next visible node");

        cx.simulate_keystrokes("left");
        cx.run_until_parked();
        let collapsed = panel
            .read_with(cx, |panel, _| panel.active_template().unwrap().state.collapsed.contains(&TemplateNodeIdx(2)));
        assert!(collapsed, "left collapses the selected node");
    }

    /// Installing an instance composes the pane's byte styler: field
    /// bytes get a background tint, a patched byte flips to the
    /// modified foreground mark (the pane observer rebuilds the stale
    /// styler snapshot after the edit), and toggling colors off drops
    /// the styler entirely once no patches remain to mark.
    #[gpui::test]
    fn overlays_tint_fields_and_patches_win(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        install_fixture(&panel, cx);

        let styler = panel.read_with(cx, |panel, cx| panel.pane().read(cx).byte_styler()).expect("styler installed");
        let field = styler(0x01, ByteOffset::new(1));
        assert!(field.bg.is_some(), "field byte gets a background tint");
        assert!(field.fg.is_none());
        let outside = styler(0x00, ByteOffset::new(20));
        assert_eq!(outside, hxy_view_gpui::ByteStyleOverride::default(), "no field covers byte 20");

        panel.update(cx, |panel, cx| {
            panel.pane().update(cx, |pane, cx| {
                pane.editor_mut().splice(1, 1, vec![0xAA]).unwrap();
                cx.notify();
            });
        });
        cx.run_until_parked();
        let styler = panel.read_with(cx, |panel, cx| panel.pane().read(cx).byte_styler()).expect("styler rebuilt");
        let patched = styler(0xAA, ByteOffset::new(1));
        assert!(patched.fg.is_some(), "patched byte wins over the field tint");
        assert!(patched.bg.is_none());
        assert!(styler(0x02, ByteOffset::new(2)).bg.is_some(), "neighboring field byte keeps its tint");

        apply(&panel, cx, TemplateEvent::ToggleColors(false));
        let styler = panel.read_with(cx, |panel, cx| panel.pane().read(cx).byte_styler()).expect("patch marks remain");
        assert!(styler(0x02, ByteOffset::new(2)).bg.is_none(), "colors off drops the field tint");
        assert!(styler(0xAA, ByteOffset::new(1)).fg.is_some(), "patch mark survives colors off");
    }

    /// A tree-supplied byte palette lands on the pane as the custom
    /// highlight table (0xAARRGGBB unpacked per byte value) in the
    /// user's highlight mode; removing the instance falls back to the
    /// settings-derived palette (default settings: Class scheme,
    /// Background mode) rather than the custom entries.
    #[gpui::test]
    fn palette_override_installs_and_clears(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        let mut tree = fixture_tree();
        tree.byte_palette = Some(vec![0xFF336699u32; 256]);
        let id = install_tree(&panel, cx, tree);

        let installed = panel.read_with(cx, |panel, cx| panel.pane().read(cx).highlight().map(|hl| hl.table[0x41]));
        let expected = rgba_to_hsla(hxy_core::color::Rgba::from_argb_u32(0xFF336699));
        assert_eq!(installed, Some(expected), "palette entry converted through rgba_to_hsla");

        apply(&panel, cx, TemplateEvent::RemoveInstance(id));
        let (fallback, expected_fallback) = panel.read_with(cx, |panel, cx| {
            let fallback = panel.pane().read(cx).highlight().cloned();
            let expected = crate::settings::highlight_palette(
                &crate::settings::settings(cx),
                gpui_component::ActiveTheme::theme(cx).mode.is_dark(),
            );
            (fallback, expected)
        });
        assert_eq!(fallback, expected_fallback, "removal falls back to the settings-derived palette");
        assert_ne!(fallback.map(|hl| hl.table[0x41]), Some(expected), "custom entry no longer installed");
    }

    /// Leaf detail renders the single hovered field; Full (Alt)
    /// renders the root-to-leaf chain joined with " > " with the tree
    /// decorations stripped; uncovered offsets yield nothing.
    #[test]
    fn breadcrumb_line_leaf_and_full() {
        let tree = fixture_tree();
        let source = MemorySource::new((0u8..32).collect::<Vec<_>>());
        let fmts = TemplateValueFormats::default();

        let leaf = breadcrumb_line(&tree, &source, 5, false, &fmts).unwrap();
        assert!(leaf.contains("len"), "leaf line names the hovered field, got {leaf:?}");
        assert!(!leaf.contains(" > "), "leaf detail is a single segment, got {leaf:?}");

        let full = breadcrumb_line(&tree, &source, 5, true, &fmts).unwrap();
        assert!(full.contains("hdr") && full.contains("len"), "full detail chains root to leaf, got {full:?}");
        assert!(full.contains(" > "));
        assert!(!full.contains('\u{2514}'), "tree decorations are stripped, got {full:?}");

        // Byte 16 sits in `name` (u8[4] at 14): the compact line is
        // the per-element row decoded from the source.
        let elem = breadcrumb_line(&tree, &source, 16, false, &fmts).unwrap();
        assert!(elem.contains("name[2]"), "scalar arrays render the element under the cursor, got {elem:?}");

        assert_eq!(breadcrumb_line(&tree, &source, 30, false, &fmts), None, "no field covers byte 30");
    }

    #[test]
    fn tab_labels_follow_the_egui_strip_rules() {
        let range = |start: u64, end: u64| ByteRange::new(ByteOffset::new(start), ByteOffset::new(end)).unwrap();
        assert_eq!(tab_label("a.bt", Some(range(0, 16)), 16, true), "a.bt");
        assert!(tab_label("a.bt", Some(range(0, 16)), 16, false).contains("(whole file)"));
        assert_eq!(tab_label("a.bt", Some(range(4, 8)), 16, true), "a.bt  [0x4..0x8]");
        assert_eq!(tab_label("run.bt", None, 16, false), "run.bt", "running entries have no range yet");
    }
}
