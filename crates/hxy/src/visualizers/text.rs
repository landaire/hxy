//! `[[hex::visualize("text", encoding?)]]`: decode the field's
//! bytes as text and render in a read-only multiline TextEdit.
//! Default encoding is UTF-8; common single-byte alternatives
//! (ASCII, Latin-1) are recognised. Non-decodable byte sequences
//! get the U+FFFD replacement so the surrounding readable text is
//! still legible.

use hxy_templates::visualize::text::MAX_BYTES;
use hxy_templates::visualize::text::decode;

use super::VisualizerContext;

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext) {
    let encoding = ctx.spec.args.first().map(|s| s.as_str()).unwrap_or("utf-8").to_ascii_lowercase();
    let truncated = ctx.bytes.len() > MAX_BYTES;
    let view = &ctx.bytes[..ctx.bytes.len().min(MAX_BYTES)];

    let decoded = match decode(view, &encoding) {
        Ok(d) => d,
        Err(e) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
            return;
        }
    };

    ui.label(
        egui::RichText::new(hxy_i18n::t_args(
            "visualizer-text-info",
            &[("encoding", &encoding), ("bytes", &ctx.bytes.len().to_string())],
        ))
        .weak(),
    );
    if truncated {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            hxy_i18n::t_args("visualizer-text-truncated", &[("max", &MAX_BYTES.to_string())]),
        );
    }
    ui.add_space(4.0);

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_sized(
            ui.available_size(),
            egui::TextEdit::multiline(&mut decoded.as_str()).font(egui::TextStyle::Monospace),
        );
    });
}
