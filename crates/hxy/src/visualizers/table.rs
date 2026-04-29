//! `[[hex::visualize("table")]]`: render the visualized field's
//! direct children as a tabular grid (one row per child, columns
//! Name / Type / Offset / Length / Value). Useful on a struct of
//! mostly scalars where the inline tree view in the template panel
//! is harder to scan than a flat table.

use hxy_plugin_host::template::Node;
use hxy_plugin_host::template::Value;

use super::VisualizerContext;

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext) {
    // Find children of this node by walking the tree once (the
    // tree's flat `nodes` Vec doesn't carry an inline child list).
    let parent_idx = ctx
        .tree
        .nodes
        .iter()
        .position(|n| std::ptr::eq(n, ctx.node))
        .map(|i| i as u32);
    let Some(parent_idx) = parent_idx else {
        ui.weak(hxy_i18n::t("visualizer-table-no-children"));
        return;
    };
    let children: Vec<&Node> = ctx
        .tree
        .nodes
        .iter()
        .filter(|n| n.parent == Some(parent_idx))
        .collect();
    if children.is_empty() {
        ui.weak(hxy_i18n::t("visualizer-table-no-children"));
        return;
    }
    ui.label(
        egui::RichText::new(hxy_i18n::t_args(
            "visualizer-table-info",
            &[("count", &children.len().to_string())],
        ))
        .weak(),
    );
    ui.add_space(4.0);

    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new(ctx.ui_id.with("table"))
            .striped(true)
            .num_columns(5)
            .show(ui, |ui| {
                ui.strong(hxy_i18n::t("visualizer-table-col-name"));
                ui.strong(hxy_i18n::t("visualizer-table-col-type"));
                ui.strong(hxy_i18n::t("visualizer-table-col-offset"));
                ui.strong(hxy_i18n::t("visualizer-table-col-length"));
                ui.strong(hxy_i18n::t("visualizer-table-col-value"));
                ui.end_row();
                for child in &children {
                    ui.label(&child.name);
                    ui.label(hxy_plugin_host::node_display_type(child));
                    ui.monospace(format!("{:#x}", child.span.offset));
                    ui.monospace(child.span.length.to_string());
                    ui.label(format_value(child).unwrap_or_default());
                    ui.end_row();
                }
            });
    });
}

fn format_value(node: &Node) -> Option<String> {
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
        Value::BytesVal(b) => format!("[{} bytes]", b.len()),
        Value::StringVal(s) => format!("{s:?}"),
        Value::EnumVal((name, raw)) => format!("{name} ({raw})"),
    })
}
