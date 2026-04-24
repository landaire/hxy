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

use crate::file::TemplateArrayId;
use crate::file::TemplateNodeIdx;
use crate::file::TemplateState;

/// Events the app needs to handle after the panel renders.
pub enum TemplateEvent {
    Close,
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
}

pub use crate::copy_format::CopyKind;

const INDENT_STEP: f32 = 14.0;

pub fn show(ui: &mut egui::Ui, id_seed: u64, state: &TemplateState) -> Vec<TemplateEvent> {
    let mut events = Vec::new();

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{} Template", egui_phosphor::regular::SCROLL)).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(egui::Button::new(egui_phosphor::regular::X).frame(false))
                .on_hover_text("Hide template")
                .clicked()
            {
                events.push(TemplateEvent::Close);
            }
            let mut colors_on = state.show_colors;
            let resp = ui
                .toggle_value(&mut colors_on, egui_phosphor::regular::PAINT_BUCKET)
                .on_hover_text("Tint bytes by field");
            if resp.changed() {
                events.push(TemplateEvent::ToggleColors(colors_on));
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

    let mut delegate =
        TemplateTableDelegate { state, visible: &visible, events: &mut events, any_hover: &mut any_hover, row_height };

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
        element_type: String,
        depth: usize,
    },
    /// Materialised element of an expanded deferred array.
    ArrayElement {
        array_id: TemplateArrayId,
        index: usize,
        depth: usize,
    },
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
            if over_row
                && let Some(idx) = node_idx
            {
                *self.any_hover = Some(idx);
            }
            let clicked_row = resp.clicked() || (over_row && ui.input(|i| i.pointer.primary_clicked()));
            if clicked_row
                && let Some(idx) = node_idx
            {
                self.events.push(TemplateEvent::Select(idx));
            }
            if let Some(idx) = node_idx {
                resp.context_menu(|ui| self.row_context_menu(ui, idx));
            }

            let this_row_highlighted = node_idx == self.state.hovered_node && node_idx.is_some();
            if this_row_highlighted {
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
            hxy_plugin_host::template::NodeType::StructType(_)
                | hxy_plugin_host::template::NodeType::StructArray(_)
        );

        ui.label(egui::RichText::new(format!("{}  ({} bytes)", node.name, node.span.length)).strong());
        ui.separator();

        if let Some(kind) = crate::copy_format::copy_as_menu_full(ui, is_scalar, is_struct) {
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
                let label = hxy_plugin_host::node_display_type(node);
                ui.add(egui::Label::new(egui::RichText::new(label).weak()).truncate());
            }
            2 => {
                ui.monospace(format!("{:#x}", node.span.offset));
            }
            3 => {
                ui.monospace(node.span.length.to_string());
            }
            4 => {
                if let Some(text) = format_value(node) {
                    ui.add(egui::Label::new(text).truncate());
                }
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
                ui.weak(format!("[{count} x {element_type}]"));
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
                let label = hxy_plugin_host::node_display_type(node);
                ui.add(egui::Label::new(egui::RichText::new(label).weak()));
            }
            2 => {
                ui.monospace(format!("{:#x}", node.span.offset));
            }
            3 => {
                ui.monospace(node.span.length.to_string());
            }
            4 => {
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

/// Character budget for rendering a template field's string / bytes
/// value. Fields wider than this collapse to `... (N bytes)` so a
/// multi-megabyte `uchar[N] data` doesn't blow up the row or tooltip.
const STRING_VALUE_PREVIEW_BUDGET: usize = 64;

fn summarise_string(s: &str) -> String {
    if s.is_empty() {
        // Render empty strings as `""` so the cell still has visible
        // glyphs. An empty Label with `.truncate()` produces a galley
        // of size `(0, line_height)`; the fractional line_height
        // pulls the enclosing Ui's min_rect off the pixel grid and
        // trips egui's `show_unaligned` debug overlay.
        "\"\"".to_owned()
    } else if s.len() <= STRING_VALUE_PREVIEW_BUDGET {
        s.to_owned()
    } else {
        format!("... ({} bytes)", s.len())
    }
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
        Value::BytesVal(b) => format!("{} bytes", b.len()),
        Value::StringVal(s) => summarise_string(s),
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

pub fn toggle_collapse(state: &mut TemplateState, idx: TemplateNodeIdx) {
    if !state.collapsed.remove(&idx) {
        state.collapsed.insert(idx);
    }
}

pub fn new_state(parsed: std::sync::Arc<dyn ParsedTemplate>) -> Result<TemplateState, hxy_vfs::HandlerError> {
    let tree = parsed.execute(&[])?;
    Ok(new_state_from(parsed, tree))
}

/// Build a [`TemplateState`] from an already-computed tree. Used by
/// the background-run path where the worker thread executes the
/// template and sends the result back to the UI.
pub fn new_state_from(
    parsed: std::sync::Arc<dyn ParsedTemplate>,
    tree: hxy_plugin_host::template::ResultTree,
) -> TemplateState {
    let leaf_boundaries = collect_leaf_boundaries(&tree);
    let leaf_colors = generate_leaf_colors(leaf_boundaries.len());
    let byte_palette_override = build_byte_palette_override(tree.byte_palette.as_deref());
    TemplateState {
        parsed: Some(parsed),
        tree,
        show_panel: true,
        expanded_arrays: HashMap::new(),
        collapsed: HashSet::new(),
        hovered_node: None,
        leaf_boundaries,
        leaf_colors,
        show_colors: true,
        byte_palette_override,
    }
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
        show_panel: true,
        expanded_arrays: HashMap::new(),
        collapsed: HashSet::new(),
        hovered_node: None,
        leaf_boundaries: Vec::new(),
        leaf_colors: Vec::new(),
        show_colors: true,
        byte_palette_override: None,
    }
}

/// Pick `n` distinct hues using the golden angle so neighbouring
/// leaves don't land on similar colors. The base colors are vivid
/// enough to read as glyphs in `ValueHighlight::Text` mode; callers
/// that paint them as backgrounds apply `gamma_multiply` to mute
/// them on the fly.
fn generate_leaf_colors(n: usize) -> Vec<egui::Color32> {
    (0..n)
        .map(|i| {
            let hue = (i as f32 * 0.381966) % 1.0;
            egui::Color32::from(egui::ecolor::Hsva::new(hue, 0.6, 0.9, 1.0))
        })
        .collect()
}

/// Walk `tree` to find the deepest node whose span contains `byte`
/// and return a top-down path of "{type} {name}[ = {value}]" strings.
/// `None` when no template field covers the offset.
///
/// When the deepest node is a primitive `ScalarArray`, the breadcrumb
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
    // Find the deepest containing node by scanning once; later (more
    // specific) nodes in the flat pre-order list win over ancestors.
    let mut deepest: Option<u32> = None;
    for (idx, node) in tree.nodes.iter().enumerate() {
        let start = node.span.offset;
        let end = start.saturating_add(node.span.length);
        if byte >= start && byte < end {
            deepest = Some(idx as u32);
        }
    }
    let leaf = deepest?;

    // Walk parent chain leaf -> root.
    let mut chain: Vec<u32> = Vec::new();
    let mut cursor = Some(leaf);
    while let Some(idx) = cursor {
        chain.push(idx);
        cursor = tree.nodes.get(idx as usize).and_then(|n| n.parent);
    }
    chain.reverse();

    let mut lines: Vec<String> = chain
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
        lines.push(row);
    }
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
    let endian =
        leaf.attributes.iter().find_map(|(k, v)| (k == "hxy_endian").then_some(v.as_str())).unwrap_or("little");
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
        Value::BytesVal(b) => format!("{} bytes", b.len()),
        Value::StringVal(s) => format!("{:?}", summarise_string(s)),
        Value::EnumVal((name, raw)) => format!("{name} ({raw})"),
    })
}

/// Collect (offset, length) for every leaf node -- one with no
/// children and no deferred array -- and sort by offset. This is the
/// list the hex view uses to draw per-field outlines.
fn collect_leaf_boundaries(
    tree: &hxy_plugin_host::template::ResultTree,
) -> Vec<(hxy_core::ByteOffset, hxy_core::ByteLen)> {
    let mut has_children = vec![false; tree.nodes.len()];
    for node in &tree.nodes {
        if let Some(parent) = node.parent
            && (parent as usize) < has_children.len()
        {
            has_children[parent as usize] = true;
        }
    }
    let mut out: Vec<(hxy_core::ByteOffset, hxy_core::ByteLen)> = tree
        .nodes
        .iter()
        .enumerate()
        .filter(|(idx, node)| !has_children[*idx] && node.array.is_none() && node.span.length > 0)
        .map(|(_, node)| (hxy_core::ByteOffset::new(node.span.offset), hxy_core::ByteLen::new(node.span.length)))
        .collect();
    out.sort_by_key(|(start, _)| start.get());
    out
}
