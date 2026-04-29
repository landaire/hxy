//! Template-result side panel. Renders the flat node tree a
//! [`TemplateState`] holds as a virtualized [`egui_table`] with
//! columns for Name, Type, Offset, Length, and Value. Row hover feeds
//! back into the hex view so the user can see where a field lives.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;
use std::collections::HashSet;

use egui_table::Column;
use egui_table::HeaderCellInfo;
use egui_table::HeaderRow;
use egui_table::Table;
use egui_table::TableDelegate;
use hxy_plugin_host::ParsedTemplate;
use hxy_plugin_host::template::Node;

use crate::files::OpenFile;
use crate::files::TemplateArrayId;
use crate::files::TemplateInstanceId;
use crate::files::TemplateNodeIdx;
use crate::files::TemplateState;

/// Events the app needs to handle after the panel renders.
pub enum TemplateEvent {
    /// User clicked the panel's close button. Hides the panel for
    /// every template on this file; doesn't drop any instances.
    HidePanel,
    /// User clicked a tab strip entry; switch to that template.
    SetActive(TemplateInstanceId),
    /// User clicked the close button on a tab; remove that instance
    /// (whether running or completed). The panel itself stays open if
    /// other instances remain.
    RemoveInstance(TemplateInstanceId),
    ExpandArray {
        array_id: TemplateArrayId,
        count: u64,
    },
    ToggleCollapse(TemplateNodeIdx),
    /// The pointer is currently over a row. `None` fires on the first
    /// frame the pointer leaves the table.
    Hover(Option<TemplateNodeIdx>),
    /// User clicked the row -- jump the hex view to this node's span
    /// and select it.
    Select(TemplateNodeIdx),
    /// User picked a copy option from the row's context menu. `kind`
    /// names what to format and how.
    Copy {
        idx: TemplateNodeIdx,
        kind: CopyKind,
    },
    /// User picked "Save bytes to file...". App should pop up a save
    /// dialog and write this node's byte span.
    SaveBytes(TemplateNodeIdx),
    /// User toggled per-field byte tinting in the hex view.
    ToggleColors(bool),
    /// User picked a new tint for `idx`'s field via the Color column
    /// swatch. The override survives across re-runs and (per
    /// [`crate::state::PersistedTemplateInstance`]) across restarts as
    /// long as the template source's BLAKE3 fingerprint matches.
    SetColor {
        idx: TemplateNodeIdx,
        color: egui::Color32,
    },
    /// User reset `idx`'s field tint back to the auto color
    /// (template-supplied attribute or hue-cycle fallback). Right-click
    /// on the swatch.
    ResetColor(TemplateNodeIdx),
    /// Keyboard arrow-key navigation: move the selected row by `delta`
    /// positions in the visible row list, skipping non-Node rows
    /// (synthesized array elements have no tree-node identity). The
    /// app handler clamps and re-fires the `Select` side effects so
    /// the hex view jumps to the new field.
    MoveSelection(i32),
    /// Left-arrow: collapse the currently selected node if expanded.
    CollapseSelected,
    /// Right-arrow: expand the currently selected node if collapsed.
    ExpandSelected,
    /// User clicked the visualizer icon on a row whose field carries
    /// a `[[hex::visualize(...)]]` attribute. Host opens / focuses
    /// the file's [`Tab::Visualizer`](crate::tabs::Tab::Visualizer)
    /// dock tab and selects this node as the active sub-tab.
    OpenVisualizer(TemplateNodeIdx),
}

pub use crate::files::copy::CopyKind;

const INDENT_STEP: f32 = 14.0;

/// Tab strip + active-instance body in one pass. The file context lets
/// us render every running and completed template's tab without
/// re-borrowing `app.files` between cells. Only the active instance's
/// node tree is rendered below the strip; the rest of the templates'
/// trees still exist on the file but are presented as tabs to swap to.
///
/// `whole_file_len` lets the strip suppress range-decoration on the
/// "default" case: a single template covering the entire file shows
/// just its name, with no `[..]` byte-range suffix.
pub fn show(ui: &mut egui::Ui, file: &OpenFile, whole_file_len: u64) -> Vec<TemplateEvent> {
    let mut events = Vec::new();
    let id_seed = file.id.get();

    let header_color_state = file.active_template().map(|t| t.state.show_colors).unwrap_or(true);
    let total_count = file.templates.len() + file.templates_running.len();
    let only_one = total_count == 1;

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{} Template", egui_phosphor::regular::SCROLL)).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(egui::Button::new(egui_phosphor::regular::X).frame(false))
                .on_hover_text("Hide template")
                .clicked()
            {
                events.push(TemplateEvent::HidePanel);
            }
            if file.active_template().is_some() {
                let mut colors_on = header_color_state;
                let resp = ui
                    .toggle_value(&mut colors_on, egui_phosphor::regular::PAINT_BUCKET)
                    .on_hover_text("Tint bytes by field");
                if resp.changed() {
                    events.push(TemplateEvent::ToggleColors(colors_on));
                }
            }
        });
    });

    render_tab_strip(ui, file, whole_file_len, only_one, &mut events);
    ui.separator();

    let Some(active_id) = file.active_template else {
        ui.weak("No template active.");
        return events;
    };
    if let Some(running) = file.templates_running.iter().find(|r| r.id == active_id) {
        render_template_running(ui, &running.run);
        return events;
    }
    let Some(active) = file.templates.iter().find(|t| t.id == active_id) else {
        ui.weak("Active template not found.");
        return events;
    };
    let state = &active.state;

    if !state.tree.diagnostics.is_empty() {
        egui::CollapsingHeader::new(format!("Diagnostics ({})", state.tree.diagnostics.len()))
            .id_salt(("hxy_tmpl_diag", id_seed))
            .default_open(true)
            .show(ui, |ui| {
                for d in &state.tree.diagnostics {
                    let icon = match d.severity {
                        hxy_plugin_host::template::Severity::Error => egui_phosphor::regular::X_CIRCLE,
                        hxy_plugin_host::template::Severity::Warning => egui_phosphor::regular::WARNING,
                        hxy_plugin_host::template::Severity::Info => egui_phosphor::regular::INFO,
                    };
                    ui.label(format!("{icon}  {}", d.message));
                }
            });
        ui.separator();
    }

    if state.tree.nodes.is_empty() {
        ui.weak("No tree produced.");
        return events;
    }

    let children = children_by_parent(&state.tree.nodes);
    let visible = build_visible(state, &children);

    let row_height = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
    let mut any_hover: Option<TemplateNodeIdx> = None;
    // Source access for synthesized ScalarArrayElement rows. Decoded
    // values come back as strings via [`decode_scalar_bytes`]. Pulled
    // up here so the per-cell render doesn't have to re-borrow `file`.
    let source: std::sync::Arc<dyn hxy_core::HexSource> = file.editor.source().clone();

    // Panel-level focus widget. Per-row interacts each have their own
    // ids and would lose focus when scrolled out of view (egui_table
    // virtualizes), so route arrow-key focus through one stable
    // widget that covers the whole table. Row clicks request focus
    // on it via the shared `focus_id`.
    let focus_id = egui::Id::new(("hxy-tmpl-focus", id_seed));
    let table_rect = ui.available_rect_before_wrap();
    let focus_resp = ui.interact(table_rect, focus_id, egui::Sense::focusable_noninteractive());
    // Tell egui not to intercept arrow keys (or Tab) for focus
    // traversal while we own focus. Without this, the first arrow
    // press is treated as a focus-direction hint and moves focus
    // off the panel widget, so subsequent presses stop reaching us.
    // No-op when the widget isn't currently focused.
    ui.memory_mut(|m| {
        m.set_focus_lock_filter(
            focus_id,
            egui::EventFilter { tab: false, horizontal_arrows: true, vertical_arrows: true, escape: false },
        );
    });

    let mut delegate = TemplateTableDelegate {
        state,
        visible: &visible,
        events: &mut events,
        any_hover: &mut any_hover,
        row_height,
        source: source.as_ref(),
        focus_id,
        pending_select: None,
    };

    // Bring the selected row into view when the selection just
    // changed (arrow-key nav, or a click that happened to land on
    // a row scrolled off-screen). We track the previous frame's
    // selected_node in egui's per-context temp data so we can
    // compare; scroll_to_row with `align: None` is a no-op when the
    // row is already visible, so click-driven selections don't
    // jitter the scroll position.
    let last_selected_id = egui::Id::new(("hxy-tmpl-last-selected", id_seed));
    let last_selected: Option<u32> = ui.ctx().data(|d| d.get_temp::<u32>(last_selected_id));
    let current_selected: Option<u32> = state.selected_node.map(|n| n.0);
    let scroll_to_row_nr: Option<u64> = current_selected
        .filter(|_| current_selected != last_selected)
        .and_then(|target| {
            visible.iter().position(|r| matches!(r, RowKind::Node { idx, .. } if idx.0 == target))
        })
        .map(|pos| pos as u64);
    ui.ctx().data_mut(|d| match current_selected {
        Some(idx) => {
            d.insert_temp(last_selected_id, idx);
        }
        None => {
            d.remove::<u32>(last_selected_id);
        }
    });

    // Initial widths get content-fitted on the first frame (egui_table runs a
    // sizing pass while state is fresh) and continuously redistributed to fill
    // the parent via AutoSizeMode::Always. Name has the most slack in its
    // range so it absorbs spare horizontal space; the fixed-glyph columns
    // (Start/End/Length) keep tight ranges so they don't balloon.
    let mut table = Table::new()
        .id_salt(("hxy_tmpl_table", id_seed))
        .num_rows(visible.len() as u64)
        .columns(vec![
            Column::new(36.0).range(32.0..=48.0).resizable(false).id(egui::Id::new("tmpl-col-color")),
            Column::new(240.0).range(80.0..=1200.0).resizable(true).id(egui::Id::new("tmpl-col-name")),
            Column::new(120.0).range(60.0..=300.0).resizable(true).id(egui::Id::new("tmpl-col-type")),
            Column::new(90.0).range(60.0..=140.0).resizable(true).id(egui::Id::new("tmpl-col-start")),
            Column::new(90.0).range(60.0..=140.0).resizable(true).id(egui::Id::new("tmpl-col-end")),
            Column::new(70.0).range(50.0..=120.0).resizable(true).id(egui::Id::new("tmpl-col-len")),
            Column::new(220.0).range(80.0..=800.0).resizable(true).id(egui::Id::new("tmpl-col-val")),
        ])
        .headers(vec![HeaderRow::new(row_height)])
        .auto_size_mode(egui_table::AutoSizeMode::Always);
    if let Some(row_nr) = scroll_to_row_nr {
        table = table.scroll_to_row(row_nr, None);
    }
    table.show(ui, &mut delegate);
    let pending_select = delegate.pending_select.take();

    if any_hover != state.hovered_node {
        events.push(TemplateEvent::Hover(any_hover));
    }
    if let Some(idx) = pending_select {
        events.push(TemplateEvent::Select(idx));
    }

    // Keyboard nav lives outside the egui_table render so it sees the
    // post-render focus state. Arrows are only consumed when the
    // panel widget actually owns focus, so they don't interfere with
    // hex-view editor input or other panels.
    if focus_resp.has_focus() && state.selected_node.is_some() {
        ui.ctx().input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                events.push(TemplateEvent::MoveSelection(1));
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                events.push(TemplateEvent::MoveSelection(-1));
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) {
                events.push(TemplateEvent::CollapseSelected);
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) {
                events.push(TemplateEvent::ExpandSelected);
            }
        });
    }

    events
}

/// One visible row in the flattened table -- either a real node or a
/// placeholder row inside an expanded deferred array. Array elements
/// don't live in `tree.nodes`, so they get a distinct row kind.
#[derive(Clone)]
enum RowKind {
    Node {
        idx: TemplateNodeIdx,
        depth: usize,
        is_parent: bool,
        collapsed: bool,
    },
    /// "[N x type, stride bytes each]" placeholder with an Expand button.
    DeferredArray {
        array_id: TemplateArrayId,
        count: u64,
        stride: u64,
        first_offset: u64,
        element_type: String,
        depth: usize,
    },
    /// Materialised element of an expanded deferred array.
    ArrayElement {
        array_id: TemplateArrayId,
        index: usize,
        depth: usize,
    },
    /// Synthetic element of an expanded primitive `ScalarArray` node.
    /// The lang emits these arrays as a single contiguous node with no
    /// children -- expanding the row into per-element rows happens
    /// here in the panel by decoding bytes from the source on demand,
    /// so we don't pay tree-size cost on collapsed arrays.
    ScalarArrayElement {
        parent_idx: TemplateNodeIdx,
        index: u64,
        depth: usize,
    },
}

struct TemplateTableDelegate<'a> {
    state: &'a TemplateState,
    visible: &'a [RowKind],
    events: &'a mut Vec<TemplateEvent>,
    any_hover: &'a mut Option<TemplateNodeIdx>,
    row_height: f32,
    /// Byte source used to decode synthetic primitive-array element
    /// rows on demand. Borrowed for the panel's render pass only.
    source: &'a dyn hxy_core::HexSource,
    /// Stable id of the panel-level focusable widget, so per-row
    /// click handlers can request focus without each fighting for
    /// its own id (rows scroll out of view under egui_table's
    /// virtualization and would lose focus mid-navigation).
    focus_id: egui::Id,
    /// Tentative row-click selection: `row_ui` writes here when the
    /// click landed on bare row area, but cell-level widgets (caret
    /// expander, color swatch, visualizer icon) clear it back to
    /// `None` if their own widget claimed the click. After
    /// `Table::show` returns, any leftover entry becomes a
    /// [`TemplateEvent::Select`]. This avoids selecting a node just
    /// because the user clicked a child widget on its row -- the
    /// fallback `pointer.primary_clicked()` check in `row_ui` fires
    /// regardless of which widget owned the click, so without this
    /// gate clicking the caret to expand a parent would also select
    /// (and selecting a parent paints the entire span purple).
    pending_select: Option<TemplateNodeIdx>,
}

impl TableDelegate for TemplateTableDelegate<'_> {
    fn header_cell_ui(&mut self, ui: &mut egui::Ui, cell: &HeaderCellInfo) {
        let label = match cell.col_range.start {
            0 => "",
            1 => "Name",
            2 => "Type",
            3 => "Start",
            4 => "End",
            5 => "Length",
            6 => "Value",
            _ => "",
        };
        if !label.is_empty() {
            ui.add_space(6.0);
            ui.strong(label);
        }
    }

    fn row_ui(&mut self, ui: &mut egui::Ui, row_nr: u64) {
        let row_rect = ui.max_rect();
        let row_kind = self.visible.get(row_nr as usize).cloned();

        let Some(row) = row_kind else { return };
        let node_idx = match &row {
            RowKind::Node { idx, .. } => Some(*idx),
            _ => None,
        };

        // Push a row-scoped id before anything that registers a widget
        // so interact, context-menu popups, and the painter can't
        // collide with widgets further down the id tree (egui_table
        // gives cells their own salt, but the row-level interact I do
        // here needs a unique parent scope too).
        ui.push_id(("hxy-tmpl-row", row_nr), |ui| {
            let row_id = ui.id().with("interact");
            let resp = ui.interact(row_rect, row_id, egui::Sense::click());
            // Both the hover highlight and the click-to-select need
            // to fire on presses that land over cell labels, not
            // just in the gaps between cells. Labels sense hover
            // themselves (for tooltips), which blocks the row
            // interact's `hovered()`/`clicked()`. Fall back to a raw
            // "pointer in rect + pointer pressed this frame" check
            // so the whole row behaves like one click target.
            let over_row = ui.rect_contains_pointer(row_rect);
            if over_row && let Some(idx) = node_idx {
                *self.any_hover = Some(idx);
            }
            let clicked_row = resp.clicked() || (over_row && ui.input(|i| i.pointer.primary_clicked()));
            if clicked_row && let Some(idx) = node_idx {
                // Tentative -- a child widget rendered below (caret
                // expander, color swatch, visualizer icon) can clear
                // this back to None if it claimed the click. The
                // post-`Table::show` drain converts whatever's still
                // here into a real Select event.
                self.pending_select = Some(idx);
                // Pull keyboard focus to the panel-level widget so
                // arrow keys move selection from this row going
                // forward. Safe to do unconditionally even if the
                // pending_select gets cancelled -- focusing the panel
                // doesn't move the selection on its own.
                ui.ctx().memory_mut(|m| m.request_focus(self.focus_id));
            }
            if let Some(idx) = node_idx {
                resp.context_menu(|ui| self.row_context_menu(ui, idx));
            }

            // Selected row (keyboard / click cursor) draws a heavier
            // background tint than hover so the user can tell which
            // row arrows will move from. Hover stacks underneath.
            let is_hovered = node_idx == self.state.hovered_node && node_idx.is_some();
            let is_selected = node_idx == self.state.selected_node && node_idx.is_some();
            if is_selected {
                ui.painter().rect_filled(row_rect, 0.0, ui.visuals().selection.bg_fill.gamma_multiply(0.6));
            } else if is_hovered {
                ui.painter().rect_filled(row_rect, 0.0, ui.visuals().selection.bg_fill.gamma_multiply(0.35));
            }
        });
    }

    fn cell_ui(&mut self, ui: &mut egui::Ui, cell: &egui_table::CellInfo) {
        let Some(row) = self.visible.get(cell.row_nr as usize) else { return };
        ui.add_space(6.0);
        match row {
            RowKind::Node { idx, depth, is_parent, collapsed } => {
                self.render_node_cell(ui, cell.col_nr, *idx, *depth, *is_parent, *collapsed);
            }
            RowKind::DeferredArray { array_id, count, stride, first_offset, element_type, depth } => {
                self.render_deferred_cell(
                    ui,
                    cell.col_nr,
                    *array_id,
                    *count,
                    *stride,
                    *first_offset,
                    element_type,
                    *depth,
                );
            }
            RowKind::ArrayElement { array_id, index, depth } => {
                self.render_array_element_cell(ui, cell.col_nr, *array_id, *index, *depth);
            }
            RowKind::ScalarArrayElement { parent_idx, index, depth } => {
                self.render_scalar_array_element_cell(ui, cell.col_nr, *parent_idx, *index, *depth);
            }
        }
    }

    fn default_row_height(&self) -> f32 {
        self.row_height
    }
}

impl TemplateTableDelegate<'_> {
    fn row_context_menu(&mut self, ui: &mut egui::Ui, idx: TemplateNodeIdx) {
        let Some(node) = self.state.tree.nodes.get(idx.0 as usize) else { return };
        let is_scalar = node.value.as_ref().is_some_and(|v| {
            matches!(
                v,
                hxy_plugin_host::template::Value::U8Val(_)
                    | hxy_plugin_host::template::Value::U16Val(_)
                    | hxy_plugin_host::template::Value::U32Val(_)
                    | hxy_plugin_host::template::Value::U64Val(_)
                    | hxy_plugin_host::template::Value::S8Val(_)
                    | hxy_plugin_host::template::Value::S16Val(_)
                    | hxy_plugin_host::template::Value::S32Val(_)
                    | hxy_plugin_host::template::Value::S64Val(_)
            )
        });
        let is_struct = matches!(
            node.type_name,
            hxy_plugin_host::template::NodeType::StructType(_) | hxy_plugin_host::template::NodeType::StructArray(_)
        );

        ui.label(egui::RichText::new(format!("{}  ({} bytes)", node.name, node.span.length)).strong());
        ui.separator();

        if let Some(kind) = crate::files::copy::copy_as_menu_full(ui, is_scalar, is_struct) {
            self.events.push(TemplateEvent::Copy { idx, kind });
        }

        ui.separator();
        if ui.button("Save bytes to file...").clicked() {
            self.events.push(TemplateEvent::SaveBytes(idx));
            ui.close();
        }
    }

    fn render_node_cell(
        &mut self,
        ui: &mut egui::Ui,
        col_nr: usize,
        idx: TemplateNodeIdx,
        depth: usize,
        is_parent: bool,
        collapsed: bool,
    ) {
        let node = &self.state.tree.nodes[idx.0 as usize];
        match col_nr {
            0 => {
                self.render_color_swatch(ui, idx);
            }
            1 => {
                ui.add_space((depth as f32) * INDENT_STEP);
                if is_parent {
                    let icon = if collapsed {
                        egui_phosphor::regular::CARET_RIGHT
                    } else {
                        egui_phosphor::regular::CARET_DOWN
                    };
                    let r = ui.add(egui::Button::new(icon).frame(false).min_size(egui::vec2(14.0, 14.0)));
                    if r.clicked() {
                        self.events.push(TemplateEvent::ToggleCollapse(idx));
                        // Suppress the row-level Select that would
                        // otherwise fire on the same press -- selecting
                        // a parent paints its entire span with the
                        // selection color, which on a struct that
                        // covers the whole file (PNG, ZIP, ...) looks
                        // like the hex view "lost" all its tinting.
                        self.pending_select = None;
                    }
                } else {
                    ui.add_space(14.0);
                }
                let name_resp = ui.add(egui::Label::new(&node.name).truncate());
                attach_comment_tooltip(name_resp, node);
                render_comment_marker(ui, node);
                render_visualizer_marker(ui, node, idx, self.events, &mut self.pending_select);
            }
            2 => {
                let label = hxy_plugin_host::node_display_type(node);
                ui.add(egui::Label::new(egui::RichText::new(label).weak()).truncate());
            }
            3 => {
                ui.monospace(format!("{:#x}", node.span.offset));
            }
            4 => {
                let end = node.span.offset.saturating_add(node.span.length);
                ui.monospace(format!("{end:#x}"));
            }
            5 => {
                ui.monospace(node.span.length.to_string());
            }
            6 => {
                if let Some(text) = format_value(node) {
                    ui.add(egui::Label::new(text).truncate());
                }
            }
            _ => {}
        }
    }

    /// Color column for a node row. Renders a clickable swatch only
    /// for nodes that actually contribute to the hex view's tinting
    /// (leaves with a non-empty span); parent nodes and bookkeeping
    /// rows leave the cell blank. The swatch shows the resolved color
    /// (override > template attribute > hue-cycle fallback). Click
    /// opens egui's color picker; right-click resets to auto.
    fn render_color_swatch(&mut self, ui: &mut egui::Ui, idx: TemplateNodeIdx) {
        let Some(&slot) = self.state.leaf_slot_by_node.get(&idx.0) else {
            return;
        };
        let original = self.state.leaf_colors[slot];
        let mut color = original;
        let resp = ui.color_edit_button_srgba(&mut color);
        if resp.clicked() {
            // Opening the picker is a deliberate per-cell action, so
            // don't also fire the row-level Select that the bare-row
            // fallback would have produced.
            self.pending_select = None;
        }
        if color != original {
            self.events.push(TemplateEvent::SetColor { idx, color });
        }
        let has_override = self.state.node_color_overrides.contains_key(&idx.0);
        // Shift-click resets to the auto color (template attribute or
        // hue-cycle fallback). The previous design used a `.context_menu`
        // popup, but registering a second popup on the same button
        // response races the color picker's popup bookkeeping and
        // dismisses the picker on the same frame it opens; a modifier
        // click avoids the second popup entirely.
        let shift_clicked = resp.clicked() && ui.input(|i| i.modifiers.shift);
        if shift_clicked && has_override {
            self.events.push(TemplateEvent::ResetColor(idx));
        }
        let tooltip = if has_override {
            "Click to edit, shift-click to reset"
        } else {
            "Click to override color"
        };
        resp.on_hover_text(tooltip);
    }

    #[allow(clippy::too_many_arguments)]
    fn render_deferred_cell(
        &mut self,
        ui: &mut egui::Ui,
        col_nr: usize,
        array_id: TemplateArrayId,
        count: u64,
        stride: u64,
        first_offset: u64,
        element_type: &str,
        depth: usize,
    ) {
        let total_len = count.saturating_mul(stride);
        match col_nr {
            0 => {}
            1 => {
                ui.add_space((depth as f32) * INDENT_STEP + 14.0);
                ui.weak(format!("[{count} x {element_type}]"));
                if ui.small_button("Expand").clicked() {
                    self.events.push(TemplateEvent::ExpandArray { array_id, count });
                }
            }
            2 => {
                ui.add(egui::Label::new(egui::RichText::new(element_type).weak()));
            }
            3 => {
                ui.monospace(format!("{first_offset:#x}"));
            }
            4 => {
                let end = first_offset.saturating_add(total_len);
                ui.monospace(format!("{end:#x}"));
            }
            5 => {
                ui.monospace(format!("{total_len}"));
            }
            _ => {}
        }
    }

    /// Render one synthesized element row of a fixed-size primitive
    /// array. The lang emitted the parent ScalarArray as one node; we
    /// decode the per-element bytes from the source on demand. No
    /// color swatch -- the parent owns the tint (see `collect_leaves`).
    fn render_scalar_array_element_cell(
        &mut self,
        ui: &mut egui::Ui,
        col_nr: usize,
        parent_idx: TemplateNodeIdx,
        index: u64,
        depth: usize,
    ) {
        let Some(parent) = self.state.tree.nodes.get(parent_idx.0 as usize) else { return };
        let hxy_plugin_host::template::NodeType::ScalarArray((kind, _count)) = parent.type_name else { return };
        let Some(elem_width) = scalar_kind_width(kind) else { return };
        if elem_width == 0 {
            return;
        }
        let elem_offset = parent.span.offset.saturating_add(index * elem_width);
        match col_nr {
            0 => {}
            1 => {
                ui.add_space((depth as f32) * INDENT_STEP + 14.0);
                ui.label(format!("[{index}]"));
            }
            2 => {
                let label = scalar_kind_name(kind);
                ui.add(egui::Label::new(egui::RichText::new(label).weak()));
            }
            3 => {
                ui.monospace(format!("{elem_offset:#x}"));
            }
            4 => {
                let end = elem_offset.saturating_add(elem_width);
                ui.monospace(format!("{end:#x}"));
            }
            5 => {
                ui.monospace(elem_width.to_string());
            }
            6 => {
                let endian = parent
                    .attributes
                    .iter()
                    .find_map(|(k, v)| (k == hxy_plugin_host::ENDIAN_ATTR).then_some(v.as_str()))
                    .unwrap_or("little");
                let range = match hxy_core::ByteRange::new(
                    hxy_core::ByteOffset::new(elem_offset),
                    hxy_core::ByteOffset::new(elem_offset.saturating_add(elem_width)),
                ) {
                    Ok(r) => r,
                    Err(_) => return,
                };
                let bytes = match self.source.read(range) {
                    Ok(b) => b,
                    Err(_) => return,
                };
                if let Some(text) = decode_scalar_bytes(kind, &bytes, endian) {
                    ui.add(egui::Label::new(text).truncate());
                }
            }
            _ => {}
        }
    }

    fn render_array_element_cell(
        &mut self,
        ui: &mut egui::Ui,
        col_nr: usize,
        array_id: TemplateArrayId,
        index: usize,
        depth: usize,
    ) {
        let Some(elements) = self.state.expanded_arrays.get(&array_id) else { return };
        let Some(node) = elements.get(index) else { return };
        match col_nr {
            0 => {}
            1 => {
                ui.add_space((depth as f32) * INDENT_STEP + 14.0);
                let resp = ui.label(format!("[{index}]"));
                attach_comment_tooltip(resp, node);
                render_comment_marker(ui, node);
            }
            2 => {
                let label = hxy_plugin_host::node_display_type(node);
                ui.add(egui::Label::new(egui::RichText::new(label).weak()));
            }
            3 => {
                ui.monospace(format!("{:#x}", node.span.offset));
            }
            4 => {
                let end = node.span.offset.saturating_add(node.span.length);
                ui.monospace(format!("{end:#x}"));
            }
            5 => {
                ui.monospace(node.span.length.to_string());
            }
            6 => {
                if let Some(text) = format_value(node) {
                    ui.add(egui::Label::new(text).truncate());
                }
            }
            _ => {}
        }
    }
}

fn children_by_parent(nodes: &[Node]) -> HashMap<Option<TemplateNodeIdx>, Vec<TemplateNodeIdx>> {
    let mut map: HashMap<Option<TemplateNodeIdx>, Vec<TemplateNodeIdx>> = HashMap::new();
    for (idx, node) in nodes.iter().enumerate() {
        let parent = node.parent.map(TemplateNodeIdx);
        map.entry(parent).or_default().push(TemplateNodeIdx(idx as u32));
    }
    map
}

/// Flatten the tree into the exact list of rows we want the table
/// to render, respecting collapsed subtrees and expanded deferred
/// arrays. Done up-front so egui_table can virtualize with accurate
/// row counts.
fn build_visible(
    state: &TemplateState,
    children: &HashMap<Option<TemplateNodeIdx>, Vec<TemplateNodeIdx>>,
) -> Vec<RowKind> {
    let mut out = Vec::new();
    let roots = children.get(&None).cloned().unwrap_or_default();
    for root in roots {
        emit_node(state, children, root, 0, &mut out);
    }
    out
}

fn emit_node(
    state: &TemplateState,
    children: &HashMap<Option<TemplateNodeIdx>, Vec<TemplateNodeIdx>>,
    idx: TemplateNodeIdx,
    depth: usize,
    out: &mut Vec<RowKind>,
) {
    let node = &state.tree.nodes[idx.0 as usize];
    let kids = children.get(&Some(idx)).cloned().unwrap_or_default();
    let has_array = node.array.is_some();
    // Fixed-size primitive arrays (`u32 length[4]`, `char name[N]`)
    // come back as a single ScalarArray node with no children. Treat
    // them as parents anyway so the user can drill into individual
    // elements; the rows themselves get synthesized lazily when the
    // user expands.
    let scalar_array_count = match node.type_name {
        hxy_plugin_host::template::NodeType::ScalarArray((_, n)) if n > 0 => Some(n),
        _ => None,
    };
    let is_parent = !kids.is_empty() || has_array || scalar_array_count.is_some();
    let collapsed = state.collapsed.contains(&idx);

    out.push(RowKind::Node { idx, depth, is_parent, collapsed });

    if collapsed {
        return;
    }
    for cid in kids {
        emit_node(state, children, cid, depth + 1, out);
    }
    if let Some(arr) = node.array.as_ref() {
        let array_id = TemplateArrayId(arr.id);
        if let Some(elements) = state.expanded_arrays.get(&array_id) {
            for i in 0..elements.len() {
                out.push(RowKind::ArrayElement { array_id, index: i, depth: depth + 1 });
            }
        } else {
            out.push(RowKind::DeferredArray {
                array_id,
                count: arr.count,
                stride: arr.stride,
                first_offset: arr.first_offset,
                element_type: arr.element_type.clone(),
                depth: depth + 1,
            });
        }
    }
    if let Some(count) = scalar_array_count {
        for i in 0..count {
            out.push(RowKind::ScalarArrayElement { parent_idx: idx, index: i, depth: depth + 1 });
        }
    }
}

/// Character budget for rendering a string value's preview before it
/// collapses to `"head..." (N bytes)`. Keeps a multi-megabyte
/// `string` field from blowing up a single row.
const STRING_VALUE_PREVIEW_CHARS: usize = 64;

/// Byte budget for rendering a byte value's hex-escaped preview before
/// it collapses to `'\xAB\xCD...' (N bytes)`. Smaller than the string
/// budget because each byte expands to four characters (`\xHH`).
const BYTES_VALUE_PREVIEW_BYTES: usize = 16;

/// Render a string value with surrounding double quotes and Rust-style
/// debug escaping. Empty strings come out as `""`, which is enough to
/// give the surrounding label a non-zero galley (an empty galley would
/// trip egui's `show_unaligned` overlay).
fn quote_string_preview(s: &str) -> String {
    let mut chars = s.chars();
    let preview: String = chars.by_ref().take(STRING_VALUE_PREVIEW_CHARS).collect();
    if chars.next().is_none() {
        format!("{preview:?}")
    } else {
        format!("{preview:?}... ({} bytes)", s.len())
    }
}

/// Render a byte slice as `'\xAB\xCD...'` so the user can tell it apart
/// from a string at a glance. Long buffers truncate to
/// [`BYTES_VALUE_PREVIEW_BYTES`] with a `... (N bytes)` tail.
fn quote_bytes_preview(b: &[u8]) -> String {
    use std::fmt::Write as _;
    let head_len = BYTES_VALUE_PREVIEW_BYTES.min(b.len());
    let mut out = String::with_capacity(head_len * 4 + 16);
    out.push('\'');
    for byte in &b[..head_len] {
        let _ = write!(out, "\\x{byte:02X}");
    }
    out.push('\'');
    if b.len() > head_len {
        let _ = write!(out, "... ({} bytes)", b.len());
    }
    out
}

/// Returns `Some(text)` for a scalar value to render in the Value
/// column, or `None` for composite rows (struct headers, bitfield
/// parents) that have no value of their own. Callers must skip the
/// Label widget on `None` -- adding `Label::new("")` produces a
/// zero-width galley whose `line_height` is font-dependent and
/// often sub-pixel, which trips egui's `show_unaligned` debug
/// overlay on the cell's enclosing `Ui`.
fn format_value(node: &Node) -> Option<String> {
    use hxy_plugin_host::template::Value;
    let v = node.value.as_ref()?;
    Some(match v {
        Value::U8Val(x) => format!("{x}"),
        Value::U16Val(x) => format!("{x}"),
        Value::U32Val(x) => match node.display {
            Some(hxy_plugin_host::template::DisplayHint::Hex) => format!("0x{x:08X}"),
            _ => format!("{x}"),
        },
        Value::U64Val(x) => match node.display {
            Some(hxy_plugin_host::template::DisplayHint::Hex) => format!("0x{x:016X}"),
            _ => format!("{x}"),
        },
        Value::S8Val(x) => format!("{x}"),
        Value::S16Val(x) => format!("{x}"),
        Value::S32Val(x) => format!("{x}"),
        Value::S64Val(x) => format!("{x}"),
        Value::F32Val(x) => format!("{x}"),
        Value::F64Val(x) => format!("{x}"),
        Value::BoolVal(b) => format!("{b}"),
        Value::BytesVal(b) => quote_bytes_preview(b),
        Value::StringVal(s) => quote_string_preview(s),
        Value::EnumVal((name, raw)) => format!("{name} ({raw})"),
    })
}

pub fn expand_array(state: &mut TemplateState, array_id: TemplateArrayId, count: u64) {
    const MAX_INITIAL: u64 = 512;
    let Some(parsed) = state.parsed.as_ref() else { return };
    let end = count.min(MAX_INITIAL);
    match parsed.expand_array(array_id.0, 0, end) {
        Ok(elements) => {
            state.expanded_arrays.insert(array_id, elements);
        }
        Err(e) => tracing::warn!(error = %e, "expand array"),
    }
}

/// Tree-node indices for every visible Node row, in panel display
/// order. Non-Node rows (deferred-array placeholders, expanded array
/// elements, synthetic primitive-array elements) are filtered out
/// because they don't have a stable `TemplateNodeIdx`. Used by the
/// arrow-key navigation handler to step from one selectable row to
/// the next.
pub fn visible_node_indices(state: &TemplateState) -> Vec<TemplateNodeIdx> {
    let children = children_by_parent(&state.tree.nodes);
    let visible = build_visible(state, &children);
    visible
        .into_iter()
        .filter_map(|r| match r {
            RowKind::Node { idx, .. } => Some(idx),
            _ => None,
        })
        .collect()
}

pub fn toggle_collapse(state: &mut TemplateState, idx: TemplateNodeIdx) {
    if !state.collapsed.remove(&idx) {
        state.collapsed.insert(idx);
    }
}

pub fn new_state(parsed: std::sync::Arc<dyn ParsedTemplate>) -> Result<TemplateState, hxy_vfs::HandlerError> {
    let tree = parsed.execute(&[])?;
    Ok(new_state_from(parsed, tree, HashMap::new()))
}

/// Build a [`TemplateState`] from an already-computed tree. Used by
/// the background-run path where the worker thread executes the
/// template and sends the result back to the UI. `node_color_overrides`
/// is non-empty when the run is a restart-time auto-rerun replaying
/// the user's previously persisted picks.
pub fn new_state_from(
    parsed: std::sync::Arc<dyn ParsedTemplate>,
    tree: hxy_plugin_host::template::ResultTree,
    node_color_overrides: HashMap<u32, egui::Color32>,
) -> TemplateState {
    let children_of = build_children_index(&tree);
    let (leaf_boundaries, leaf_node_indices) = collect_leaves(&tree, &children_of);
    let leaf_slot_by_node: HashMap<u32, usize> =
        leaf_node_indices.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    let leaf_colors = resolve_leaf_colors(&tree, &leaf_node_indices, &node_color_overrides);
    let collapsed = initial_collapsed(&tree, &children_of);
    let byte_palette_override = build_byte_palette_override(tree.byte_palette.as_deref());
    TemplateState {
        parsed: Some(parsed),
        tree,
        expanded_arrays: HashMap::new(),
        collapsed,
        hovered_node: None,
        selected_node: None,
        leaf_boundaries,
        leaf_colors,
        leaf_node_indices,
        leaf_slot_by_node,
        node_color_overrides,
        show_colors: true,
        byte_palette_override,
    }
}

/// Recompute `leaf_colors` after a change to `node_color_overrides`.
/// Cheap (O(leaves)) and called from the SetColor / ResetColor event
/// handlers so the hex view picks up the new tint on the next frame
/// without a full template re-run.
pub fn recompute_leaf_colors(state: &mut TemplateState) {
    state.leaf_colors = resolve_leaf_colors(&state.tree, &state.leaf_node_indices, &state.node_color_overrides);
}

/// Unpack the runtime's optional 256-entry `0xAARRGGBB` table into an
/// `Arc<[Color32; 256]>`. Any length other than 256 is rejected -- we
/// keep the contract tight so the hex view can index without bounds
/// checks. Returns `None` when the runtime didn't supply a palette.
fn build_byte_palette_override(palette: Option<&[u32]>) -> Option<std::sync::Arc<[egui::Color32; 256]>> {
    let raw = palette?;
    if raw.len() != 256 {
        return None;
    }
    let mut out = [egui::Color32::TRANSPARENT; 256];
    for (i, packed) in raw.iter().enumerate() {
        let a = (packed >> 24) as u8;
        let r = (packed >> 16) as u8;
        let g = (packed >> 8) as u8;
        let b = *packed as u8;
        out[i] = egui::Color32::from_rgba_unmultiplied(r, g, b, a);
    }
    Some(std::sync::Arc::new(out))
}

pub fn error_state(message: String) -> TemplateState {
    TemplateState {
        parsed: None,
        tree: hxy_plugin_host::template::ResultTree {
            nodes: Vec::new(),
            diagnostics: vec![hxy_plugin_host::template::Diagnostic {
                message,
                severity: hxy_plugin_host::template::Severity::Error,
                file_offset: None,
                template_line: None,
            }],
            byte_palette: None,
        },
        expanded_arrays: HashMap::new(),
        collapsed: HashSet::new(),
        hovered_node: None,
        selected_node: None,
        leaf_boundaries: Vec::new(),
        leaf_colors: Vec::new(),
        leaf_node_indices: Vec::new(),
        leaf_slot_by_node: HashMap::new(),
        node_color_overrides: HashMap::new(),
        show_colors: true,
        byte_palette_override: None,
    }
}

/// Centered "Running `<name>`..." spinner block. Shown in place of
/// the body when the active tab is an in-flight run.
fn render_template_running(ui: &mut egui::Ui, run: &crate::files::TemplateRun) {
    ui.vertical_centered(|ui| {
        ui.add_space(24.0);
        ui.label(egui::RichText::new(format!("{} Template", egui_phosphor::regular::SCROLL)).strong());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!("Running `{}`...", run.template_name));
        });
        let elapsed_ms = jiff::Timestamp::now().duration_since(run.started).as_millis().max(0);
        ui.add_space(4.0);
        ui.weak(format!("{} ms", elapsed_ms));
    });
}

/// Render the row of selectable tab labels above the tree. Hidden when
/// only one template covers the whole file (no point in a single-tab
/// strip with no range to disambiguate). Each tab carries a close (X)
/// button so the user can drop a single instance without affecting
/// the rest.
fn render_tab_strip(
    ui: &mut egui::Ui,
    file: &OpenFile,
    whole_file_len: u64,
    only_one: bool,
    events: &mut Vec<TemplateEvent>,
) {
    if file.templates.is_empty() && file.templates_running.is_empty() {
        return;
    }
    let active = file.active_template;
    let suppress_range_for_single = only_one;

    egui::ScrollArea::horizontal().id_salt(("hxy-tmpl-tab-strip", file.id.get())).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for instance in &file.templates {
                render_tab_button(
                    ui,
                    instance.id,
                    &instance.display_name,
                    instance.range,
                    whole_file_len,
                    suppress_range_for_single,
                    /* running = */ false,
                    active == Some(instance.id),
                    events,
                );
            }
            for running in &file.templates_running {
                render_tab_button(
                    ui,
                    running.id,
                    &running.display_name,
                    running.range,
                    whole_file_len,
                    suppress_range_for_single,
                    /* running = */ true,
                    active == Some(running.id),
                    events,
                );
            }
        });
    });
}

/// Single tab button: `<icon> Name [0xS..0xE]  X`. Active tab is
/// styled as a `SelectableLabel`-selected; running tabs prefix a
/// spinner glyph so the user sees the work still in flight.
#[allow(clippy::too_many_arguments)]
fn render_tab_button(
    ui: &mut egui::Ui,
    id: TemplateInstanceId,
    name: &str,
    range: hxy_core::ByteRange,
    whole_file_len: u64,
    suppress_range_for_single: bool,
    running: bool,
    active: bool,
    events: &mut Vec<TemplateEvent>,
) {
    let covers_whole_file = range.start().get() == 0 && range.len().get() == whole_file_len;
    let label = if covers_whole_file && suppress_range_for_single {
        name.to_owned()
    } else if covers_whole_file {
        format!("{name}  (whole file)")
    } else {
        format!("{name}  [{:#x}..{:#x}]", range.start().get(), range.end().get())
    };
    let prefix = if running { format!("{}  ", egui_phosphor::regular::CIRCLE_NOTCH) } else { String::new() };
    let resp = ui.add(egui::Button::selectable(active, format!("{prefix}{label}")));
    if resp.clicked() {
        events.push(TemplateEvent::SetActive(id));
    }
    let close = ui.add(egui::Button::new(egui_phosphor::regular::X).frame(false).small());
    if close.clicked() {
        events.push(TemplateEvent::RemoveInstance(id));
    }
}

/// Pick `n` distinct hues using the golden angle so neighbouring
/// leaves don't land on similar colors. The base colors are vivid
/// enough to read as glyphs in `ValueHighlight::Text` mode; callers
/// that paint them as backgrounds apply `gamma_multiply` to mute
/// them on the fly. Used as the per-leaf fallback when neither a
/// user override nor a template-supplied `hxy_color` attribute
/// applies.
fn fallback_leaf_color(slot: usize) -> egui::Color32 {
    let hue = (slot as f32 * 0.381966) % 1.0;
    egui::Color32::from(egui::ecolor::Hsva::new(hue, 0.6, 0.9, 1.0))
}

/// Per-leaf color resolution: user override > template
/// `hxy_color` attribute > hue-cycle fallback. The fallback's slot
/// index is just the leaf's position in `leaf_node_indices`, which
/// keeps the auto colors stable across runs of the same template
/// (so a field that previously sat at slot 7 still gets slot 7's
/// hue if no override is set).
fn resolve_leaf_colors(
    tree: &hxy_plugin_host::template::ResultTree,
    leaf_node_indices: &[u32],
    overrides: &HashMap<u32, egui::Color32>,
) -> Vec<egui::Color32> {
    leaf_node_indices
        .iter()
        .enumerate()
        .map(|(slot, &node_idx)| {
            if let Some(c) = overrides.get(&node_idx) {
                return *c;
            }
            if let Some(node) = tree.nodes.get(node_idx as usize)
                && let Some(c) = parse_color_attr(node)
            {
                return c;
            }
            fallback_leaf_color(slot)
        })
        .collect()
}

/// Pull a non-empty `hxy_comment` off the node, or `None`.
fn node_comment(node: &Node) -> Option<&str> {
    node.attributes
        .iter()
        .find_map(|(k, v)| (k == hxy_plugin_host::COMMENT_ATTR && !v.is_empty()).then_some(v.as_str()))
}

/// Render a dim INFO icon directly after the field name when the node
/// carries a `hxy_comment`. Hovering the icon shows the full comment
/// in a tooltip; hovering the name label does the same. The icon
/// makes the comment discoverable (otherwise the user would have to
/// know to hover) and also gives us a guaranteed-hoverable widget --
/// `Label` tooltips can be flaky inside the densely-overlapping
/// row layout.
fn render_comment_marker(ui: &mut egui::Ui, node: &Node) {
    let Some(comment) = node_comment(node) else {
        return;
    };
    let icon = egui::RichText::new(egui_phosphor::regular::INFO).weak();
    ui.add(egui::Label::new(icon)).on_hover_text(comment);
}

/// Attach a hover tooltip carrying the node's `hxy_comment` to a
/// just-rendered widget response. Used on the row's name label so
/// the user gets the tooltip whether they hover the name text or
/// the marker icon next to it.
fn attach_comment_tooltip(resp: egui::Response, node: &Node) {
    if let Some(comment) = node_comment(node) {
        resp.on_hover_text(comment);
    }
}

/// Render a small "visualize" icon after the field name when the
/// node carries a `[[hex::visualize(...)]]` or
/// `[[hex::inline_visualize(...)]]` attribute. Click pushes
/// [`TemplateEvent::OpenVisualizer`] so the host can pop the
/// visualizer panel + select this field. Hovering the icon shows the
/// visualizer name as a tooltip so the user can see what kind of
/// renderer they'll get without clicking through.
///
/// `pending_select` is the row-level tentative selection slot the
/// delegate threads through every cell widget; we clear it when the
/// icon claims a click so the row's bare-area fallback doesn't ALSO
/// fire a `Select(idx)` for the same press.
fn render_visualizer_marker(
    ui: &mut egui::Ui,
    node: &Node,
    idx: TemplateNodeIdx,
    events: &mut Vec<TemplateEvent>,
    pending_select: &mut Option<TemplateNodeIdx>,
) {
    let Some((spec, _inline)) = crate::visualizers::read_node_visualizer(node) else {
        return;
    };
    let icon = egui::RichText::new(egui_phosphor::regular::IMAGE_SQUARE).weak();
    let resp = ui.add(egui::Button::new(icon).frame(false).small());
    let tooltip = hxy_i18n::t_args("visualizer-row-tooltip", &[("name", spec.kind.label())]);
    if resp.on_hover_text(tooltip).clicked() {
        events.push(TemplateEvent::OpenVisualizer(idx));
        *pending_select = None;
    }
}

/// Pull a `hxy_color` attribute off `node` and parse it as an sRGB(A)
/// hex string. Accepted shapes (case-insensitive, optional `#` /
/// `0x` prefix): `RRGGBB` and `AARRGGBB`. `None` when the attribute
/// is missing or doesn't parse.
fn parse_color_attr(node: &Node) -> Option<egui::Color32> {
    let raw = node
        .attributes
        .iter()
        .find_map(|(k, v)| (k == hxy_plugin_host::COLOR_ATTR).then_some(v.as_str()))?;
    parse_hex_color(raw)
}

fn parse_hex_color(s: &str) -> Option<egui::Color32> {
    let s = s.trim();
    let s = s.strip_prefix('#').unwrap_or(s);
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some(egui::Color32::from_rgb(r, g, b))
        }
        8 => {
            let a = u8::from_str_radix(&s[0..2], 16).ok()?;
            let r = u8::from_str_radix(&s[2..4], 16).ok()?;
            let g = u8::from_str_radix(&s[4..6], 16).ok()?;
            let b = u8::from_str_radix(&s[6..8], 16).ok()?;
            Some(egui::Color32::from_rgba_unmultiplied(r, g, b, a))
        }
        _ => None,
    }
}

/// Walk `tree` to find the first leaf node whose span contains `byte`
/// and return a top-down path of "{type} {name}[ = {value}]" strings.
/// `None` when no template field covers the offset.
///
/// "First" matters because some templates declare a trailing
/// visualizer / peek field that overlaps the whole struct
/// (`u8 v[length] @ addressof(this) [[no_unique_address]]`); a
/// last-emitted-wins walk would always end on it instead of the
/// structural field the user is hovering. "Leaf" matters because
/// otherwise the root struct (which contains every byte) would win
/// before we reach anything specific.
///
/// When the chosen leaf is a primitive `ScalarArray`, the breadcrumb
/// gets an extra leaf row showing the specific element under the
/// cursor -- e.g. `uchar [77] = 120` -- decoded on the fly from
/// `source`. That's the reason the source is taken as an argument:
/// primitive arrays are emitted as a single contiguous node, so
/// individual element values aren't in the tree.
pub fn breadcrumb_for_offset(
    tree: &hxy_plugin_host::template::ResultTree,
    source: &dyn hxy_core::HexSource,
    byte: u64,
) -> Option<Vec<String>> {
    let mut has_children = vec![false; tree.nodes.len()];
    for node in &tree.nodes {
        if let Some(parent) = node.parent
            && (parent as usize) < has_children.len()
        {
            has_children[parent as usize] = true;
        }
    }
    let leaf = tree.nodes.iter().enumerate().find_map(|(idx, node)| {
        if has_children[idx] {
            return None;
        }
        let start = node.span.offset;
        let end = start.saturating_add(node.span.length);
        (byte >= start && byte < end).then_some(idx as u32)
    })?;

    // Walk parent chain leaf -> root.
    let mut chain: Vec<u32> = Vec::new();
    let mut cursor = Some(leaf);
    while let Some(idx) = cursor {
        chain.push(idx);
        cursor = tree.nodes.get(idx as usize).and_then(|n| n.parent);
    }
    chain.reverse();

    let mut raw: Vec<String> = chain
        .iter()
        .map(|idx| {
            let node = &tree.nodes[*idx as usize];
            let is_leaf = *idx == leaf;
            let ty = hxy_plugin_host::node_display_type(node);
            let value_str = if is_leaf { format_node_value(node) } else { None };
            match value_str {
                Some(v) => format!("{} {} = {}", ty, node.name, v),
                None => format!("{} {}", ty, node.name),
            }
        })
        .collect();

    if let Some(row) = array_element_row(&tree.nodes[leaf as usize], source, byte) {
        raw.push(row);
    }

    // Decorate as a degenerate (linear) tree. Root has no connector;
    // every deeper row gets `└─ ` prefixed by 3 spaces per ancestor
    // depth so the indents line up with `tree` / `exa -T` output.
    let lines: Vec<String> = raw
        .into_iter()
        .enumerate()
        .map(|(depth, label)| {
            if depth == 0 {
                label
            } else {
                let indent = "   ".repeat(depth - 1);
                format!("{indent}\u{2514}\u{2500} {label}")
            }
        })
        .collect();
    Some(lines)
}

/// Produce the per-element breadcrumb row for a primitive scalar
/// array. Returns `None` when the leaf isn't a scalar array, the
/// byte lands in the array's padding, or the source read fails.
fn array_element_row(
    leaf: &hxy_plugin_host::template::Node,
    source: &dyn hxy_core::HexSource,
    byte: u64,
) -> Option<String> {
    use hxy_plugin_host::template::NodeType;

    let (kind, count) = match &leaf.type_name {
        NodeType::ScalarArray((k, n)) => (*k, *n),
        _ => return None,
    };
    let elem_width = scalar_kind_width(kind)?;
    if elem_width == 0 || count == 0 {
        return None;
    }
    let array_start = leaf.span.offset;
    let relative = byte.checked_sub(array_start)?;
    let index = relative / elem_width;
    if index >= count {
        return None;
    }
    let elem_offset = array_start + index * elem_width;
    let range = hxy_core::ByteRange::new(
        hxy_core::ByteOffset::new(elem_offset),
        hxy_core::ByteOffset::new(elem_offset + elem_width),
    )
    .ok()?;
    let bytes = source.read(range).ok()?;
    let endian = leaf
        .attributes
        .iter()
        .find_map(|(k, v)| (k == hxy_plugin_host::ENDIAN_ATTR).then_some(v.as_str()))
        .unwrap_or("little");
    let value = decode_scalar_bytes(kind, &bytes, endian)?;
    let type_label = scalar_kind_name(kind);
    Some(format!("{type_label} [{index}] = {value}"))
}

fn scalar_kind_width(kind: hxy_plugin_host::template::ScalarKind) -> Option<u64> {
    use hxy_plugin_host::template::ScalarKind as K;
    Some(match kind {
        K::U8K | K::S8K | K::BoolK => 1,
        K::U16K | K::S16K => 2,
        K::U32K | K::S32K | K::F32K => 4,
        K::U64K | K::S64K | K::F64K => 8,
        K::U128K | K::S128K => 16,
        K::BytesK | K::StringK => return None,
    })
}

fn scalar_kind_name(kind: hxy_plugin_host::template::ScalarKind) -> &'static str {
    use hxy_plugin_host::template::ScalarKind as K;
    match kind {
        K::U8K => "uchar",
        K::S8K => "char",
        K::U16K => "uint16",
        K::S16K => "int16",
        K::U32K => "uint32",
        K::S32K => "int32",
        K::U64K => "uint64",
        K::S64K => "int64",
        K::U128K => "uint128",
        K::S128K => "int128",
        K::F32K => "float",
        K::F64K => "double",
        K::BoolK => "bool",
        K::BytesK => "bytes",
        K::StringK => "string",
    }
}

fn decode_scalar_bytes(kind: hxy_plugin_host::template::ScalarKind, bytes: &[u8], endian: &str) -> Option<String> {
    use hxy_plugin_host::template::ScalarKind as K;
    let big = endian == "big";
    let read_u = |b: &[u8]| -> u64 {
        let mut buf = [0u8; 8];
        if big {
            buf[8 - b.len()..].copy_from_slice(b);
            u64::from_be_bytes(buf)
        } else {
            buf[..b.len()].copy_from_slice(b);
            u64::from_le_bytes(buf)
        }
    };
    let read_i = |b: &[u8]| -> i64 {
        let raw = read_u(b);
        let shift = 64 - (b.len() as u32) * 8;
        ((raw << shift) as i64) >> shift
    };
    Some(match kind {
        K::U8K => format!("{}", bytes.first()?),
        K::S8K => format!("{}", *bytes.first()? as i8),
        K::U16K | K::U32K | K::U64K => format!("{}", read_u(bytes)),
        K::S16K | K::S32K | K::S64K => format!("{}", read_i(bytes)),
        K::U128K | K::S128K => {
            // 128-bit ints don't have a u128/i128 path through `read_u`
            // / `read_i`. Render as `0x` + raw bytes in source-endian
            // order so the inspector still shows the bits the user
            // wrote, just without a typed numeric form.
            let mut out = String::with_capacity(2 + bytes.len() * 2);
            out.push_str("0x");
            let iter: Box<dyn Iterator<Item = &u8>> =
                if big { Box::new(bytes.iter()) } else { Box::new(bytes.iter().rev()) };
            for b in iter {
                out.push_str(&format!("{b:02X}"));
            }
            out
        }
        K::F32K => {
            let arr: [u8; 4] = bytes.try_into().ok()?;
            let v = if big { f32::from_be_bytes(arr) } else { f32::from_le_bytes(arr) };
            format!("{v}")
        }
        K::F64K => {
            let arr: [u8; 8] = bytes.try_into().ok()?;
            let v = if big { f64::from_be_bytes(arr) } else { f64::from_le_bytes(arr) };
            format!("{v}")
        }
        K::BoolK => format!("{}", bytes.first()? != &0),
        K::BytesK | K::StringK => return None,
    })
}

fn format_node_value(node: &hxy_plugin_host::template::Node) -> Option<String> {
    use hxy_plugin_host::template::Value;
    let v = node.value.as_ref()?;
    Some(match v {
        Value::U8Val(x) => format!("{x}"),
        Value::U16Val(x) => format!("{x}"),
        Value::U32Val(x) => format!("{x}"),
        Value::U64Val(x) => format!("{x}"),
        Value::S8Val(x) => format!("{x}"),
        Value::S16Val(x) => format!("{x}"),
        Value::S32Val(x) => format!("{x}"),
        Value::S64Val(x) => format!("{x}"),
        Value::F32Val(x) => format!("{x}"),
        Value::F64Val(x) => format!("{x}"),
        Value::BoolVal(b) => format!("{b}"),
        Value::BytesVal(b) => quote_bytes_preview(b),
        Value::StringVal(s) => quote_string_preview(s),
        Value::EnumVal((name, raw)) => format!("{name} ({raw})"),
    })
}

/// Per-parent child index. Built once at TemplateState construction
/// and reused for both leaf detection and the initial collapse set.
fn build_children_index(tree: &hxy_plugin_host::template::ResultTree) -> Vec<Vec<u32>> {
    let mut out: Vec<Vec<u32>> = vec![Vec::new(); tree.nodes.len()];
    for (idx, node) in tree.nodes.iter().enumerate() {
        if let Some(parent) = node.parent
            && (parent as usize) < out.len()
        {
            out[parent as usize].push(idx as u32);
        }
    }
    out
}

/// Collect "color leaves" -- the nodes whose byte spans should receive
/// distinct tints in the hex view (and whose rows show a swatch in the
/// panel's Color column). Returns parallel vectors of spans and tree
/// node indices, sorted by offset.
///
/// A node is a color leaf when:
/// - it has no children (the typical scalar field), or
/// - it's the parent of a primitive-element array (every child has
///   `Scalar(_)` type). In that case the children are *excluded*: a
///   `char keyword[]` should paint as one continuous teal block, not
///   eighteen rainbow bytes, even though the lang did emit eighteen
///   per-element nodes for browsing.
///
/// Deferred arrays and zero-length nodes are always excluded.
fn collect_leaves(
    tree: &hxy_plugin_host::template::ResultTree,
    children_of: &[Vec<u32>],
) -> (Vec<(hxy_core::ByteOffset, hxy_core::ByteLen)>, Vec<u32>) {
    // A node is "absorbed by a primitive-array parent" when its
    // immediate parent has all-scalar children. The parent owns the
    // tint for the whole span; the absorbed child contributes
    // nothing of its own to leaf coloring even though it would
    // otherwise pass the no-children filter below.
    let absorbed: Vec<bool> = (0..tree.nodes.len())
        .map(|idx| {
            let Some(parent) = tree.nodes[idx].parent else {
                return false;
            };
            let parent = parent as usize;
            if parent >= children_of.len() {
                return false;
            }
            all_children_scalar(tree, &children_of[parent])
        })
        .collect();
    // Walk in declaration / tree order and accept a leaf only when
    // its span doesn't overlap any leaf we've already accepted.
    // First-emitted wins on overlap. Drops trailing "visualizer"
    // fields some templates declare as
    // `u8 v[length] @ addressof(this) [[no_unique_address]]` --
    // those would otherwise claim every byte's tint and overshadow
    // the structural fields. Same shape works for any other late
    // peek field declared with the same overlap pattern, no
    // hardcoding to a name.
    let mut accepted: Vec<(hxy_core::ByteOffset, hxy_core::ByteLen, u32)> = Vec::new();
    for (idx, node) in tree.nodes.iter().enumerate() {
        if node.array.is_some() || node.span.length == 0 || absorbed[idx] {
            continue;
        }
        let kids = &children_of[idx];
        if !(kids.is_empty() || all_children_scalar(tree, kids)) {
            continue;
        }
        let new_start = node.span.offset;
        let new_end = new_start.saturating_add(node.span.length);
        let overlaps = accepted.iter().any(|(s, l, _)| {
            let s_start = s.get();
            let s_end = s_start.saturating_add(l.get());
            new_start < s_end && s_start < new_end
        });
        if overlaps {
            continue;
        }
        accepted.push((
            hxy_core::ByteOffset::new(new_start),
            hxy_core::ByteLen::new(node.span.length),
            idx as u32,
        ));
    }
    accepted.sort_by_key(|(start, _, _)| start.get());
    let boundaries = accepted.iter().map(|(s, l, _)| (*s, *l)).collect();
    let node_indices = accepted.into_iter().map(|(_, _, n)| n).collect();
    (boundaries, node_indices)
}

fn all_children_scalar(tree: &hxy_plugin_host::template::ResultTree, kids: &[u32]) -> bool {
    kids.iter().all(|&c| {
        tree.nodes
            .get(c as usize)
            .is_some_and(|n| matches!(n.type_name, hxy_plugin_host::template::NodeType::Scalar(_)))
    })
}

/// Initial set of collapsed nodes: every node that *can* be expanded
/// (parent of children, deferred array, or fixed-size primitive
/// scalar array). Templates can be deep enough that landing on the
/// fully-expanded tree is overwhelming; the user opens what they
/// need.
fn initial_collapsed(
    tree: &hxy_plugin_host::template::ResultTree,
    children_of: &[Vec<u32>],
) -> std::collections::HashSet<TemplateNodeIdx> {
    (0..tree.nodes.len() as u32)
        .filter(|&idx| {
            let node = &tree.nodes[idx as usize];
            !children_of[idx as usize].is_empty()
                || node.array.is_some()
                || matches!(node.type_name, hxy_plugin_host::template::NodeType::ScalarArray((_, n)) if n > 0)
        })
        .map(TemplateNodeIdx)
        .collect()
}
