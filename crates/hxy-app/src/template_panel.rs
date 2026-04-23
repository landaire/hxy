//! Template-result side panel. Renders the flat node tree a
//! [`TemplateState`] holds as a table with columns for Name, Type,
//! Offset, Length, and Value. Row hover feeds back into the hex view
//! so the user can see where in the data a field lives.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;
use std::collections::HashSet;

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

/// Pixel width of one indent step in the Name column.
const INDENT_STEP: f32 = 14.0;
/// Fixed widths for numeric + type columns. Value column is elastic
/// and consumes whatever remains.
const TYPE_COL_WIDTH: f32 = 120.0;
const OFFSET_COL_WIDTH: f32 = 80.0;
const LENGTH_COL_WIDTH: f32 = 64.0;

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

    // Header row.
    let header_rect_start = ui.cursor().min;
    ui.horizontal(|ui| {
        ui.add_space(0.0);
        ui.allocate_ui_with_layout(
            egui::vec2(
                ui.available_width() - TYPE_COL_WIDTH - OFFSET_COL_WIDTH - LENGTH_COL_WIDTH - 24.0,
                0.0,
            ),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.strong("Name");
            },
        );
        ui.allocate_ui_with_layout(
            egui::vec2(TYPE_COL_WIDTH, 0.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.strong("Type");
            },
        );
        ui.allocate_ui_with_layout(
            egui::vec2(OFFSET_COL_WIDTH, 0.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.strong("Offset");
            },
        );
        ui.allocate_ui_with_layout(
            egui::vec2(LENGTH_COL_WIDTH, 0.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.strong("Length");
            },
        );
        ui.strong("Value");
    });
    let _ = header_rect_start;
    ui.separator();

    let children = children_by_parent(&state.tree.nodes);

    let mut any_hover: Option<TemplateNodeIdx> = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let roots = children.get(&None).cloned().unwrap_or_default();
        for root_idx in roots {
            walk_and_render(ui, id_seed, state, &children, root_idx, 0, &mut events, &mut any_hover);
        }
    });

    // Fire Hover(None) when the pointer isn't over any row this frame
    // but was over one last frame. Subsequent frames with no hover
    // stay quiet.
    let prev = state.hovered_node;
    if any_hover != prev {
        events.push(TemplateEvent::Hover(any_hover));
    }

    events
}

fn children_by_parent(nodes: &[Node]) -> HashMap<Option<TemplateNodeIdx>, Vec<TemplateNodeIdx>> {
    let mut map: HashMap<Option<TemplateNodeIdx>, Vec<TemplateNodeIdx>> = HashMap::new();
    for (idx, node) in nodes.iter().enumerate() {
        let parent = node.parent.map(TemplateNodeIdx);
        map.entry(parent).or_default().push(TemplateNodeIdx(idx as u32));
    }
    map
}

#[allow(clippy::too_many_arguments)]
fn walk_and_render(
    ui: &mut egui::Ui,
    id_seed: u64,
    state: &TemplateState,
    children: &HashMap<Option<TemplateNodeIdx>, Vec<TemplateNodeIdx>>,
    idx: TemplateNodeIdx,
    depth: usize,
    events: &mut Vec<TemplateEvent>,
    any_hover: &mut Option<TemplateNodeIdx>,
) {
    let node = &state.tree.nodes[idx.0 as usize];
    let kids = children.get(&Some(idx)).cloned().unwrap_or_default();
    let has_array = node.array.is_some();
    let is_parent = !kids.is_empty() || has_array;
    let collapsed = state.collapsed.contains(&idx);

    render_row(ui, id_seed, idx, node, depth, is_parent, collapsed, state, events, any_hover);

    if collapsed {
        return;
    }
    for cid in kids {
        walk_and_render(ui, id_seed, state, children, cid, depth + 1, events, any_hover);
    }
    // Deferred-array elements render after the struct children, under
    // the array's own parent node.
    if let Some(arr) = node.array.as_ref() {
        let array_id = TemplateArrayId(arr.id);
        if let Some(elements) = state.expanded_arrays.get(&array_id) {
            for (i, el) in elements.iter().enumerate() {
                render_array_element_row(ui, id_seed, idx, i, el, depth + 1, any_hover, events);
            }
        } else {
            render_expand_array_row(ui, id_seed, arr, depth + 1, events);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_row(
    ui: &mut egui::Ui,
    id_seed: u64,
    idx: TemplateNodeIdx,
    node: &Node,
    depth: usize,
    is_parent: bool,
    collapsed: bool,
    state: &TemplateState,
    events: &mut Vec<TemplateEvent>,
    any_hover: &mut Option<TemplateNodeIdx>,
) {
    let row_resp = ui
        .scope(|ui| {
            let is_hovered_last_frame = state.hovered_node == Some(idx);
            if is_hovered_last_frame {
                ui.style_mut().visuals.widgets.noninteractive.bg_fill =
                    ui.visuals().widgets.active.bg_fill;
            }
            ui.horizontal(|ui| {
                // Name column: indent + expand caret + name
                let name_width = ui
                    .available_width()
                    .saturating_sub_f32(TYPE_COL_WIDTH + OFFSET_COL_WIDTH + LENGTH_COL_WIDTH);
                ui.allocate_ui_with_layout(
                    egui::vec2(name_width, 0.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add_space((depth as f32) * INDENT_STEP);
                        if is_parent {
                            let icon = if collapsed {
                                egui_phosphor::regular::CARET_RIGHT
                            } else {
                                egui_phosphor::regular::CARET_DOWN
                            };
                            let r = ui.add(
                                egui::Button::new(icon).frame(false).min_size(egui::vec2(14.0, 14.0)),
                            );
                            if r.clicked() {
                                events.push(TemplateEvent::ToggleCollapse(idx));
                            }
                        } else {
                            ui.add_space(14.0);
                        }
                        ui.label(&node.name);
                    },
                );
                ui.allocate_ui_with_layout(
                    egui::vec2(TYPE_COL_WIDTH, 0.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(&node.type_name).weak()).truncate());
                    },
                );
                ui.allocate_ui_with_layout(
                    egui::vec2(OFFSET_COL_WIDTH, 0.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(format!("{:#x}", node.span.offset)).monospace()));
                    },
                );
                ui.allocate_ui_with_layout(
                    egui::vec2(LENGTH_COL_WIDTH, 0.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(node.span.length.to_string()).monospace()));
                    },
                );
                ui.add(egui::Label::new(format_value(node)).truncate());
            });
        })
        .response
        .interact(egui::Sense::hover());

    if row_resp.hovered() {
        *any_hover = Some(idx);
    }
    let _ = id_seed;
}

#[allow(clippy::too_many_arguments)]
fn render_array_element_row(
    ui: &mut egui::Ui,
    id_seed: u64,
    _parent_idx: TemplateNodeIdx,
    index: usize,
    node: &Node,
    depth: usize,
    any_hover: &mut Option<TemplateNodeIdx>,
    _events: &mut Vec<TemplateEvent>,
) {
    // Array elements don't live in the main node list, so they can't
    // be "hovered" via an index into it. We still render them but
    // don't report hover-to-highlight for now (a later pass can give
    // them synthetic indexes).
    let _ = id_seed;
    let _ = any_hover;
    ui.horizontal(|ui| {
        let name_width = ui
            .available_width()
            .saturating_sub_f32(TYPE_COL_WIDTH + OFFSET_COL_WIDTH + LENGTH_COL_WIDTH);
        ui.allocate_ui_with_layout(
            egui::vec2(name_width, 0.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add_space((depth as f32) * INDENT_STEP + 14.0);
                ui.label(format!("[{index}]"));
            },
        );
        ui.allocate_ui_with_layout(
            egui::vec2(TYPE_COL_WIDTH, 0.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add(egui::Label::new(egui::RichText::new(&node.type_name).weak()));
            },
        );
        ui.allocate_ui_with_layout(
            egui::vec2(OFFSET_COL_WIDTH, 0.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.monospace(format!("{:#x}", node.span.offset));
            },
        );
        ui.allocate_ui_with_layout(
            egui::vec2(LENGTH_COL_WIDTH, 0.0),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.monospace(node.span.length.to_string());
            },
        );
        ui.label(format_value(node));
    });
}

fn render_expand_array_row(
    ui: &mut egui::Ui,
    id_seed: u64,
    arr: &hxy_plugin_host::DeferredArray,
    depth: usize,
    events: &mut Vec<TemplateEvent>,
) {
    let _ = id_seed;
    ui.horizontal(|ui| {
        ui.add_space((depth as f32) * INDENT_STEP + 14.0);
        ui.weak(format!("[{} × {}, {} bytes each]", arr.element_type, arr.count, arr.stride));
        if ui.small_button("Expand").clicked() {
            events.push(TemplateEvent::ExpandArray {
                array_id: TemplateArrayId(arr.id),
                count: arr.count,
            });
        }
    });
}

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

pub fn expand_array(state: &mut TemplateState, array_id: TemplateArrayId, count: u64) {
    // Cap materialisation to keep the UI responsive. User can re-invoke
    // to grow the visible range if needed later.
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

/// Build a fresh [`TemplateState`] by running a parsed template.
pub fn new_state(parsed: std::sync::Arc<ParsedTemplate>) -> Result<TemplateState, hxy_vfs::HandlerError> {
    let tree = parsed.execute(&[])?;
    Ok(TemplateState {
        parsed: Some(parsed),
        tree,
        show_panel: true,
        expanded_arrays: HashMap::new(),
        collapsed: HashSet::new(),
        hovered_node: None,
    })
}

/// Build a state that carries only a fatal diagnostic message — used
/// when we can't even get to the point of parsing (no runtime, source
/// unreadable, etc.). Surfaces the panel so the user sees the message
/// instead of the command appearing to do nothing.
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

trait SubF32 {
    fn saturating_sub_f32(self, other: f32) -> f32;
}

impl SubF32 for f32 {
    fn saturating_sub_f32(self, other: f32) -> f32 {
        (self - other).max(0.0)
    }
}
