//! Template-result side panel. Renders the flat node tree a
//! [`TemplateState`](crate::file::TemplateState) holds, with deferred
//! arrays expandable by clicking.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;

use hxy_plugin_host::Node;
use hxy_plugin_host::ParsedTemplate;
use hxy_plugin_host::ResultTree;

use crate::file::TemplateState;

/// Events the app needs to handle after the panel renders.
pub enum TemplateEvent {
    Close,
    ExpandArray { array_id: u64, count: u64 },
}

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

    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        let children = children_by_parent(&state.tree.nodes);
        render_node(ui, id_seed, &state.tree, &children, &state.expanded_arrays, 0, &mut events);
    });
    events
}

fn children_by_parent(nodes: &[Node]) -> HashMap<Option<u32>, Vec<u32>> {
    let mut map: HashMap<Option<u32>, Vec<u32>> = HashMap::new();
    for (idx, node) in nodes.iter().enumerate() {
        map.entry(node.parent).or_default().push(idx as u32);
    }
    map
}

fn render_node(
    ui: &mut egui::Ui,
    id_seed: u64,
    tree: &ResultTree,
    children: &HashMap<Option<u32>, Vec<u32>>,
    expanded: &HashMap<u64, Vec<Node>>,
    idx: u32,
    events: &mut Vec<TemplateEvent>,
) {
    let node = &tree.nodes[idx as usize];
    let header = format_row(node);
    let kid_ids = children.get(&Some(idx)).cloned().unwrap_or_default();
    let array = node.array.as_ref();

    let leaf = kid_ids.is_empty() && array.is_none();
    if leaf {
        ui.label(header);
        return;
    }

    egui::CollapsingHeader::new(header)
        .id_salt(("hxy_tmpl_node", id_seed, idx))
        .default_open(kid_ids.len() <= 8)
        .show(ui, |ui| {
            for cid in kid_ids {
                render_node(ui, id_seed, tree, children, expanded, cid, events);
            }
            if let Some(arr) = array {
                if let Some(elements) = expanded.get(&arr.id) {
                    for (i, el) in elements.iter().enumerate() {
                        ui.label(format_row_with_index(el, i));
                    }
                } else {
                    ui.horizontal(|ui| {
                        ui.weak(format!(
                            "[{} × {}, {} bytes each]",
                            arr.element_type, arr.count, arr.stride
                        ));
                        if ui.small_button("Expand").clicked() {
                            events.push(TemplateEvent::ExpandArray { array_id: arr.id, count: arr.count });
                        }
                    });
                }
            }
        });
}

fn format_row(node: &Node) -> String {
    let value = format_value(node);
    match value {
        Some(v) => format!("{}: {} = {}", node.name, node.type_name, v),
        None => format!("{}: {}", node.name, node.type_name),
    }
}

fn format_row_with_index(node: &Node, index: usize) -> String {
    let value = format_value(node);
    match value {
        Some(v) => format!("[{index}]: {} = {}", node.type_name, v),
        None => format!("[{index}]: {}", node.type_name),
    }
}

fn format_value(node: &Node) -> Option<String> {
    use hxy_plugin_host::Value;
    let v = node.value.as_ref()?;
    Some(match v {
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
        Value::StringVal(s) => format!("{s:?}"),
        Value::EnumVal((name, raw)) => format!("{name} ({raw})"),
    })
}

pub fn expand_array(state: &mut TemplateState, array_id: u64, count: u64) {
    // Cap materialisation to keep the UI responsive. User can re-invoke
    // to grow the visible range if needed later.
    const MAX_INITIAL: u64 = 512;
    let Some(parsed) = state.parsed.as_ref() else { return };
    let end = count.min(MAX_INITIAL);
    match parsed.expand_array(array_id, 0, end) {
        Ok(elements) => {
            state.expanded_arrays.insert(array_id, elements);
        }
        Err(e) => tracing::warn!(error = %e, "expand array"),
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
    }
}
