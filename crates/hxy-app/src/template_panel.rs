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
use hxy_plugin_host::Node;
use hxy_plugin_host::ParsedTemplate;

use crate::file::TemplateArrayId;
use crate::file::TemplateNodeIdx;
use crate::file::TemplateState;

/// Events the app needs to handle after the panel renders.
pub enum TemplateEvent {
    Close,
    ExpandArray { array_id: TemplateArrayId, count: u64 },
    ToggleCollapse(TemplateNodeIdx),
    /// The pointer is currently over a row. `None` fires on the first
    /// frame the pointer leaves the table.
    Hover(Option<TemplateNodeIdx>),
}

const INDENT_STEP: f32 = 14.0;

pub fn show(ui: &mut egui::Ui, id_seed: u64, state: &TemplateState) -> Vec<TemplateEvent> {
    let mut events = Vec::new();

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{} Template", egui_phosphor::regular::SCROLL)).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.add(egui::Button::new(egui_phosphor::regular::X).frame(false)).on_hover_text("Hide template").clicked()
            {
                events.push(TemplateEvent::Close);
            }
        });
    });
    ui.separator();

    if !state.tree.diagnostics.is_empty() {
        egui::CollapsingHeader::new(format!("Diagnostics ({})", state.tree.diagnostics.len()))
            .id_salt(("hxy_tmpl_diag", id_seed))
            .default_open(true)
            .show(ui, |ui| {
                for d in &state.tree.diagnostics {
                    let icon = match d.severity {
                        hxy_plugin_host::Severity::Error => egui_phosphor::regular::X_CIRCLE,
                        hxy_plugin_host::Severity::Warning => egui_phosphor::regular::WARNING,
                        hxy_plugin_host::Severity::Info => egui_phosphor::regular::INFO,
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

    let mut delegate = TemplateTableDelegate {
        state,
        visible: &visible,
        events: &mut events,
        any_hover: &mut any_hover,
        row_height,
    };

    Table::new()
        .id_salt(("hxy_tmpl_table", id_seed))
        .num_rows(visible.len() as u64)
        .columns(vec![
            Column::new(240.0).range(80.0..=600.0).resizable(true).id(egui::Id::new("tmpl-col-name")),
            Column::new(110.0).range(60.0..=300.0).resizable(true).id(egui::Id::new("tmpl-col-type")),
            Column::new(90.0).range(60.0..=160.0).resizable(true).id(egui::Id::new("tmpl-col-off")),
            Column::new(70.0).range(50.0..=140.0).resizable(true).id(egui::Id::new("tmpl-col-len")),
            Column::new(220.0).range(80.0..=800.0).resizable(true).id(egui::Id::new("tmpl-col-val")),
        ])
        .headers(vec![HeaderRow::new(row_height)])
        .auto_size_mode(egui_table::AutoSizeMode::OnParentResize)
        .show(ui, &mut delegate);

    if any_hover != state.hovered_node {
        events.push(TemplateEvent::Hover(any_hover));
    }

    events
}

/// One visible row in the flattened table — either a real node or a
/// placeholder row inside an expanded deferred array. Array elements
/// don't live in `tree.nodes`, so they get a distinct row kind.
#[derive(Clone)]
enum RowKind {
    Node { idx: TemplateNodeIdx, depth: usize, is_parent: bool, collapsed: bool },
    /// "[N × type, stride bytes each]" placeholder with an Expand button.
    DeferredArray { array_id: TemplateArrayId, count: u64, stride: u64, element_type: String, depth: usize },
    /// Materialised element of an expanded deferred array.
    ArrayElement { array_id: TemplateArrayId, index: usize, depth: usize },
}

struct TemplateTableDelegate<'a> {
    state: &'a TemplateState,
    visible: &'a [RowKind],
    events: &'a mut Vec<TemplateEvent>,
    any_hover: &'a mut Option<TemplateNodeIdx>,
    row_height: f32,
}

impl TableDelegate for TemplateTableDelegate<'_> {
    fn header_cell_ui(&mut self, ui: &mut egui::Ui, cell: &HeaderCellInfo) {
        let label = match cell.col_range.start {
            0 => "Name",
            1 => "Type",
            2 => "Offset",
            3 => "Length",
            4 => "Value",
            _ => "",
        };
        ui.add_space(6.0);
        ui.strong(label);
    }

    fn row_ui(&mut self, ui: &mut egui::Ui, row_nr: u64) {
        // The ui handed to us has max_rect set to the full row rect.
        // Use that to detect hover on the whole row, regardless of
        // which cell the pointer is over.
        let row_rect = ui.max_rect();
        if ui.rect_contains_pointer(row_rect) {
            let Some(row) = self.visible.get(row_nr as usize) else { return };
            if let RowKind::Node { idx, .. } = row {
                *self.any_hover = Some(*idx);
            }
        }
        // Highlight the row background when it's this-frame's active
        // hover (what the hex view is using).
        let this_row_highlighted = match self.visible.get(row_nr as usize) {
            Some(RowKind::Node { idx, .. }) => self.state.hovered_node == Some(*idx),
            _ => false,
        };
        if this_row_highlighted {
            ui.painter().rect_filled(
                row_rect,
                0.0,
                ui.visuals().selection.bg_fill.gamma_multiply(0.35),
            );
        }
    }

    fn cell_ui(&mut self, ui: &mut egui::Ui, cell: &egui_table::CellInfo) {
        let Some(row) = self.visible.get(cell.row_nr as usize) else { return };
        ui.add_space(6.0);
        match row {
            RowKind::Node { idx, depth, is_parent, collapsed } => {
                self.render_node_cell(ui, cell.col_nr, *idx, *depth, *is_parent, *collapsed);
            }
            RowKind::DeferredArray { array_id, count, stride, element_type, depth } => {
                self.render_deferred_cell(ui, cell.col_nr, *array_id, *count, *stride, element_type, *depth);
            }
            RowKind::ArrayElement { array_id, index, depth } => {
                self.render_array_element_cell(ui, cell.col_nr, *array_id, *index, *depth);
            }
        }
    }

    fn default_row_height(&self) -> f32 {
        self.row_height
    }
}

impl TemplateTableDelegate<'_> {
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
                    }
                } else {
                    ui.add_space(14.0);
                }
                ui.add(egui::Label::new(&node.name).truncate());
            }
            1 => {
                ui.add(egui::Label::new(egui::RichText::new(&node.type_name).weak()).truncate());
            }
            2 => {
                ui.monospace(format!("{:#x}", node.span.offset));
            }
            3 => {
                ui.monospace(node.span.length.to_string());
            }
            4 => {
                ui.add(egui::Label::new(format_value(node)).truncate());
            }
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_deferred_cell(
        &mut self,
        ui: &mut egui::Ui,
        col_nr: usize,
        array_id: TemplateArrayId,
        count: u64,
        stride: u64,
        element_type: &str,
        depth: usize,
    ) {
        match col_nr {
            0 => {
                ui.add_space((depth as f32) * INDENT_STEP + 14.0);
                ui.weak(format!("[{count} × {element_type}]"));
                if ui.small_button("Expand").clicked() {
                    self.events.push(TemplateEvent::ExpandArray { array_id, count });
                }
            }
            1 => {
                ui.add(egui::Label::new(egui::RichText::new(element_type).weak()));
            }
            3 => {
                ui.monospace(format!("{}", count.saturating_mul(stride)));
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
            0 => {
                ui.add_space((depth as f32) * INDENT_STEP + 14.0);
                ui.label(format!("[{index}]"));
            }
            1 => {
                ui.add(egui::Label::new(egui::RichText::new(&node.type_name).weak()));
            }
            2 => {
                ui.monospace(format!("{:#x}", node.span.offset));
            }
            3 => {
                ui.monospace(node.span.length.to_string());
            }
            4 => {
                ui.add(egui::Label::new(format_value(node)).truncate());
            }
            _ => {}
        }
    }
}

// ---- visibility computation ------------------------------------------------

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
    let is_parent = !kids.is_empty() || has_array;
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
                element_type: arr.element_type.clone(),
                depth: depth + 1,
            });
        }
    }
}

// ---- value formatting ------------------------------------------------------

fn format_value(node: &Node) -> String {
    use hxy_plugin_host::Value;
    let Some(v) = node.value.as_ref() else { return String::new() };
    match v {
        Value::U8Val(x) => format!("{x}"),
        Value::U16Val(x) => format!("{x}"),
        Value::U32Val(x) => match node.display {
            Some(hxy_plugin_host::DisplayHint::Hex) => format!("0x{x:08X}"),
            _ => format!("{x}"),
        },
        Value::U64Val(x) => match node.display {
            Some(hxy_plugin_host::DisplayHint::Hex) => format!("0x{x:016X}"),
            _ => format!("{x}"),
        },
        Value::S8Val(x) => format!("{x}"),
        Value::S16Val(x) => format!("{x}"),
        Value::S32Val(x) => format!("{x}"),
        Value::S64Val(x) => format!("{x}"),
        Value::F32Val(x) => format!("{x}"),
        Value::F64Val(x) => format!("{x}"),
        Value::BytesVal(b) => format!("{} bytes", b.len()),
        Value::StringVal(s) => s.clone(),
        Value::EnumVal((name, raw)) => format!("{name} ({raw})"),
    }
}

// ---- state helpers ---------------------------------------------------------

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

pub fn toggle_collapse(state: &mut TemplateState, idx: TemplateNodeIdx) {
    if !state.collapsed.remove(&idx) {
        state.collapsed.insert(idx);
    }
}

pub fn new_state(parsed: std::sync::Arc<ParsedTemplate>) -> Result<TemplateState, hxy_vfs::HandlerError> {
    let tree = parsed.execute(&[])?;
    Ok(new_state_from(parsed, tree))
}

/// Build a [`TemplateState`] from an already-computed tree. Used by
/// the background-run path where the worker thread executes the
/// template and sends the result back to the UI.
pub fn new_state_from(
    parsed: std::sync::Arc<ParsedTemplate>,
    tree: hxy_plugin_host::ResultTree,
) -> TemplateState {
    TemplateState {
        parsed: Some(parsed),
        tree,
        show_panel: true,
        expanded_arrays: HashMap::new(),
        collapsed: HashSet::new(),
        hovered_node: None,
    }
}

pub fn error_state(message: String) -> TemplateState {
    TemplateState {
        parsed: None,
        tree: hxy_plugin_host::ResultTree {
            nodes: Vec::new(),
            diagnostics: vec![hxy_plugin_host::Diagnostic {
                message,
                severity: hxy_plugin_host::Severity::Error,
                file_offset: None,
                template_line: None,
            }],
        },
        show_panel: true,
        expanded_arrays: HashMap::new(),
        collapsed: HashSet::new(),
        hovered_node: None,
    }
}
