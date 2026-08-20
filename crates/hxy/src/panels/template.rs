//! Template-result side panel. Renders the flat node tree a
//! [`TemplateState`] holds as a virtualized [`egui_table`] with
//! columns for Name, Type, Offset, Length, and Value. Row hover feeds
//! back into the hex view so the user can see where a field lives.

#![cfg(not(target_arch = "wasm32"))]

use egui_table::Column;
use egui_table::HeaderCellInfo;
use egui_table::HeaderRow;
use egui_table::Table;
use egui_table::TableDelegate;
use hxy_plugin_host::template::Node;
use hxy_templates::color::Rgba;
use hxy_templates::format::decode_scalar_bytes;
use hxy_templates::format::scalar_kind_name;
use hxy_templates::format::scalar_kind_width;
use hxy_templates::state::RowKind;
use hxy_templates::state::build_visible;
use hxy_templates::state::children_by_parent;

use crate::files::OpenFile;
use crate::files::TemplateArrayId;
use crate::files::TemplateInstanceId;
use crate::files::TemplateNodeIdx;
use crate::files::TemplateState;

pub use crate::files::copy::CopyKind;
pub use hxy_templates::breadcrumb::BreadcrumbDetail;
pub use hxy_templates::breadcrumb::breadcrumb_for_offset;
pub use hxy_templates::format::format_value;
pub use hxy_templates::state::TemplateEvent;
pub use hxy_templates::state::error_state;
pub use hxy_templates::state::expand_array;
pub use hxy_templates::state::new_state;
pub use hxy_templates::state::new_state_from;
pub use hxy_templates::state::recompute_leaf_colors;
pub use hxy_templates::state::toggle_collapse;
pub use hxy_templates::state::visible_node_indices;

/// Byte-preserving conversions between the shared [`Rgba`] newtype
/// and egui's `Color32`: both store premultiplied sRGBA in the same
/// component order, so no color math happens at this boundary.
pub fn color32_from_rgba(c: Rgba) -> egui::Color32 {
    egui::Color32::from_rgba_premultiplied(c.r, c.g, c.b, c.a)
}

pub fn rgba_from_color32(c: egui::Color32) -> Rgba {
    let [r, g, b, a] = c.to_array();
    Rgba::rgba(r, g, b, a)
}

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
pub fn show(
    ui: &mut egui::Ui,
    file: &OpenFile,
    whole_file_len: u64,
    numeric_format: crate::settings::NumericFormat,
    template_value_formats: &crate::settings::TemplateValueFormats,
) -> Vec<TemplateEvent> {
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
        // Auto-expand only when at least one diagnostic is at the
        // Error level. Info / Warning runs are usually noise (a
        // template that "completed with N notes" doesn't deserve
        // a panel takeover); errors get the eyeball treatment
        // because they probably indicate a malformed file or a
        // template bug worth reading. The Console auto-opens for
        // the same severity threshold via console_log, so the
        // user gets a coherent "something went wrong" surface.
        let has_error =
            state.tree.diagnostics.iter().any(|d| matches!(d.severity, hxy_plugin_host::template::Severity::Error));
        egui::CollapsingHeader::new(format!("Diagnostics ({})", state.tree.diagnostics.len()))
            .id_salt(("hxy_tmpl_diag", id_seed))
            .default_open(has_error)
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

    // Round to the GUI grid (multiples of 1/32 pt) so consecutive
    // row tops stay aligned with the device pixel grid. Without
    // this, a fractional body-text height (zoom != 1.0 produces
    // these readily) accumulates across rows and every other row
    // ends up sub-pixel-offset, which makes egui's debug overlay
    // light up the cells with "Unaligned" watermarks.
    let row_height = {
        use egui::emath::GuiRounding;
        (ui.text_style_height(&egui::TextStyle::Body) + 4.0).round_ui()
    };
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

    // Inverse-format modifier (Alt / Option) flips the picked
    // base for every cell rendered this frame. Read once so all
    // cells make the same call.
    let inverse_format = ui.input(|i| i.modifiers.alt);
    let mut delegate = TemplateTableDelegate {
        state,
        visible: &visible,
        events: &mut events,
        any_hover: &mut any_hover,
        row_height,
        source: source.as_ref(),
        focus_id,
        pending_select: None,
        numeric_format,
        template_value_formats,
        inverse_format,
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
        .and_then(|target| visible.iter().position(|r| matches!(r, RowKind::Node { idx, .. } if idx.0 == target)))
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
    /// User-configured base / threshold for offset / length /
    /// end columns. Looked up per cell so the threshold form
    /// can pick a different base for tiny indices and big
    /// addresses on the same row.
    numeric_format: crate::settings::NumericFormat,
    /// Per-integer-type formats for the Value column. Kept as
    /// a borrow so the table delegate can hand one slot to each
    /// integer arm without re-cloning the bundle.
    template_value_formats: &'a crate::settings::TemplateValueFormats,
    /// True while the inverse-format modifier (Alt / Option) is
    /// held -- flips every rendered base for the duration of
    /// the frame.
    inverse_format: bool,
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
        // egui_table's `auto_size_mode = Always` redistributes the
        // remaining width across resizable columns, which produces
        // fractional column widths when the parent width doesn't
        // divide cleanly. Each cell's max_rect inherits those
        // fractional bounds, child widgets get fractional rects,
        // and egui's debug overlay watermarks them as "Unaligned".
        // Snap the cell's bounds to the GUI grid here so every
        // child widget allocates from aligned coordinates.
        let aligned = {
            use egui::emath::GuiRounding;
            ui.max_rect().round_ui()
        };
        ui.scope_builder(egui::UiBuilder::new().max_rect(aligned).layout(*ui.layout()), |ui| {
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
        });
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
                numeric_cell(ui, node.span.offset, self.numeric_format, self.inverse_format);
            }
            4 => {
                let end = node.span.offset.saturating_add(node.span.length);
                numeric_cell(ui, end, self.numeric_format, self.inverse_format);
            }
            5 => {
                numeric_cell(ui, node.span.length, self.numeric_format, self.inverse_format);
            }
            6 => {
                if let Some(text) = format_value(node, self.template_value_formats, self.inverse_format) {
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
        let original = color32_from_rgba(self.state.leaf_colors[slot]);
        let mut color = original;
        let resp = ui.color_edit_button_srgba(&mut color);
        if resp.clicked() {
            // Opening the picker is a deliberate per-cell action, so
            // don't also fire the row-level Select that the bare-row
            // fallback would have produced.
            self.pending_select = None;
        }
        if color != original {
            self.events.push(TemplateEvent::SetColor { idx, color: rgba_from_color32(color) });
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
        let tooltip = if has_override { "Click to edit, shift-click to reset" } else { "Click to override color" };
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
                numeric_cell(ui, first_offset, self.numeric_format, self.inverse_format);
            }
            4 => {
                let end = first_offset.saturating_add(total_len);
                numeric_cell(ui, end, self.numeric_format, self.inverse_format);
            }
            5 => {
                numeric_cell(ui, total_len, self.numeric_format, self.inverse_format);
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
                numeric_cell(ui, elem_offset, self.numeric_format, self.inverse_format);
            }
            4 => {
                let end = elem_offset.saturating_add(elem_width);
                numeric_cell(ui, end, self.numeric_format, self.inverse_format);
            }
            5 => {
                numeric_cell(ui, elem_width, self.numeric_format, self.inverse_format);
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
                if let Some(text) = decode_scalar_bytes(
                    kind,
                    &bytes,
                    endian,
                    parent.display,
                    self.template_value_formats,
                    self.inverse_format,
                ) {
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
                numeric_cell(ui, node.span.offset, self.numeric_format, self.inverse_format);
            }
            4 => {
                let end = node.span.offset.saturating_add(node.span.length);
                numeric_cell(ui, end, self.numeric_format, self.inverse_format);
            }
            5 => {
                numeric_cell(ui, node.span.length, self.numeric_format, self.inverse_format);
            }
            6 => {
                if let Some(text) = format_value(node, self.template_value_formats, self.inverse_format) {
                    ui.add(egui::Label::new(text).truncate());
                }
            }
            _ => {}
        }
    }
}

/// Render a numeric span value (offset / length / end) using
/// the user's [`crate::settings::NumericFormat`] in a monospace
/// label that truncates on narrow columns. Truncation is what
/// avoids wrapping into a multi-line cell, which is what tripped
/// egui's `show_unaligned` debug overlay -- a wrapped value
/// produces a sub-pixel-tall galley. `inverse` flips the picked
/// base while the user holds the inverse-format modifier.
fn numeric_cell(ui: &mut egui::Ui, value: u64, fmt: crate::settings::NumericFormat, inverse: bool) {
    let base = if inverse { fmt.pick(value).toggle() } else { fmt.pick(value) };
    let text = crate::view::format::format_offset(value, base);
    ui.add(egui::Label::new(egui::RichText::new(text).monospace()).truncate());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_serde_wire_format_matches_color32() {
        // Persisted color overrides used to serialize egui::Color32
        // ([r, g, b, a] array); Rgba must keep the same wire form in
        // both directions so old state files load unchanged.
        let c32 = egui::Color32::from_rgba_premultiplied(1, 2, 3, 4);
        let rgba = Rgba::rgba(1, 2, 3, 4);
        assert_eq!(serde_json::to_string(&c32).unwrap(), "[1,2,3,4]");
        assert_eq!(serde_json::to_string(&rgba).unwrap(), "[1,2,3,4]");
        let cross: Rgba = serde_json::from_str(&serde_json::to_string(&c32).unwrap()).unwrap();
        assert_eq!(cross, rgba);
        let back: egui::Color32 = serde_json::from_str(&serde_json::to_string(&rgba).unwrap()).unwrap();
        assert_eq!(back, c32);
    }

    #[test]
    fn rgba_unmultiplied_matches_color32() {
        // parse_hex_color / from_argb_u32 replicate ecolor's
        // unmultiplied -> premultiplied conversion; verify the bytes
        // agree with egui across the alpha range.
        for a in 0..=255u32 {
            for v in [0u8, 1, 7, 13, 40, 90, 128, 200, 254, 255] {
                let a = a as u8;
                let ours = Rgba::from_rgba_unmultiplied(v, v / 2, v.wrapping_add(31), a);
                let theirs = rgba_from_color32(egui::Color32::from_rgba_unmultiplied(v, v / 2, v.wrapping_add(31), a));
                assert_eq!(ours, theirs, "r/g/b from {v}, a = {a}");
            }
        }
    }

    #[test]
    fn rgba_from_hsv_matches_egui_hue_fallback() {
        // The hue-cycle fallback colors must stay identical to the
        // old egui-side Hsva conversion so untinted templates render
        // the exact same palette.
        for slot in 0..512usize {
            let hue = (slot as f32 * 0.381966) % 1.0;
            let expected = egui::Color32::from(egui::ecolor::Hsva::new(hue, 0.6, 0.9, 1.0));
            let ours = color32_from_rgba(Rgba::from_hsv(hue, 0.6, 0.9));
            assert_eq!(ours, expected, "slot {slot}");
        }
    }
}
