//! `[[hex::visualize("hex_viewer")]]`: render the field's bytes as
//! a fixed-pitch hex dump. Inline equivalent of opening the parent
//! file at the field's offset, but presented as a focused popout so
//! the user can inspect a small slice without scrolling the main
//! editor.

use hxy_templates::visualize::hex_dump::MAX_BYTES;
use hxy_templates::visualize::hex_dump::format_dump;

use super::VisualizerContext;

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext) {
    let truncated = ctx.bytes.len() > MAX_BYTES;
    let view_bytes = &ctx.bytes[..ctx.bytes.len().min(MAX_BYTES)];

    ui.label(
        egui::RichText::new(hxy_i18n::t_args(
            "visualizer-hex-info",
            &[("offset", &format!("{:#x}", ctx.node.span.offset)), ("len", &ctx.bytes.len().to_string())],
        ))
        .weak(),
    );
    if truncated {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            hxy_i18n::t_args("visualizer-hex-truncated", &[("max", &MAX_BYTES.to_string())]),
        );
    }
    ui.add_space(4.0);

    let base_offset = ctx.node.span.offset;
    let dump = format_dump(base_offset, view_bytes);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add(egui::TextEdit::multiline(&mut dump.as_str()).font(egui::TextStyle::Monospace).code_editor());
    });
}
