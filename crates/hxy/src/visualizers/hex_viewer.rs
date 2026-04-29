//! `[[hex::visualize("hex_viewer")]]`: render the field's bytes as
//! a fixed-pitch hex dump. Inline equivalent of opening the parent
//! file at the field's offset, but presented as a focused popout so
//! the user can inspect a small slice without scrolling the main
//! editor.

use std::fmt::Write;

use super::VisualizerContext;

/// Bytes per row in the dump. Matches the hex view's default width
/// so the user can mentally line up offsets without doing math.
const COLS: usize = 16;
/// Cap how much we render so a 100MB field doesn't blow up the UI
/// thread. The user opened a *visualizer*, not the main editor.
const MAX_BYTES: usize = 64 * 1024;

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

fn format_dump(base_offset: u64, bytes: &[u8]) -> String {
    let rows = bytes.len().div_ceil(COLS);
    let mut out = String::with_capacity(rows * (10 + COLS * 3 + 2 + COLS + 1));
    for (row_idx, chunk) in bytes.chunks(COLS).enumerate() {
        let off = base_offset + (row_idx * COLS) as u64;
        let _ = write!(out, "{off:08X}  ");
        for col in 0..COLS {
            if col < chunk.len() {
                let _ = write!(out, "{:02X} ", chunk[col]);
            } else {
                out.push_str("   ");
            }
            if col == 7 {
                out.push(' ');
            }
        }
        out.push(' ');
        for &b in chunk {
            out.push(if (0x20..0x7f).contains(&b) { b as char } else { '.' });
        }
        out.push('\n');
    }
    out
}
