//! `[[hex::visualize("3d", ...)]]`: 3D mesh preview.
//!
//! ImHex spins up a real GL viewer for STL / OBJ-like vertex
//! buffers. Doing the same in egui needs a wgpu compositor + camera
//! controls + lighting -- a milestone-sized undertaking on its own.
//! For this initial pass we surface the size + a one-line
//! interpretation hint so the user knows the visualizer "saw" the
//! attribute and isn't silently dropping data.

use super::VisualizerContext;

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext) {
    ui.heading(hxy_i18n::t("visualizer-3d-heading"));
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(hxy_i18n::t_args(
            "visualizer-3d-info",
            &[("bytes", &ctx.bytes.len().to_string())],
        ))
        .weak(),
    );
    ui.add_space(8.0);
    ui.colored_label(ui.visuals().warn_fg_color, hxy_i18n::t("visualizer-3d-not-yet"));
}
