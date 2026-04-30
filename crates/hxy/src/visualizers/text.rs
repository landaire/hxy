//! `[[hex::visualize("text", encoding?)]]`: decode the field's
//! bytes as text and render in a read-only multiline TextEdit.
//! Default encoding is UTF-8; common single-byte alternatives
//! (ASCII, Latin-1) are recognised. Non-decodable byte sequences
//! get the U+FFFD replacement so the surrounding readable text is
//! still legible.

use super::VisualizerContext;

const MAX_BYTES: usize = 1024 * 1024;

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext) {
    let encoding = ctx.spec.args.first().map(|s| s.as_str()).unwrap_or("utf-8").to_ascii_lowercase();
    let truncated = ctx.bytes.len() > MAX_BYTES;
    let view = &ctx.bytes[..ctx.bytes.len().min(MAX_BYTES)];

    let decoded = match encoding.as_str() {
        "utf-8" | "utf8" => String::from_utf8_lossy(view).into_owned(),
        "ascii" => view.iter().map(|&b| if b < 0x80 { b as char } else { '\u{FFFD}' }).collect(),
        "latin-1" | "latin1" | "iso-8859-1" => view.iter().map(|&b| b as char).collect(),
        "utf-16-le" | "utf-16le" | "utf16le" => decode_utf16(view, true),
        "utf-16-be" | "utf-16be" | "utf16be" => decode_utf16(view, false),
        other => {
            ui.colored_label(
                ui.visuals().error_fg_color,
                hxy_i18n::t_args("visualizer-text-unknown-encoding", &[("name", other)]),
            );
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

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let mut units: Vec<u16> = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let v = if little_endian {
            u16::from_le_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_be_bytes([chunk[0], chunk[1]])
        };
        units.push(v);
    }
    String::from_utf16_lossy(&units)
}
