//! Data inspector dock tab: renders the decoder table for the active
//! tab's caret.
//!
//! The decoder trait, built-in decoders, and `InspectorState` moved
//! to `hxy_panels::inspector` (framework-agnostic, shared with the
//! GPUI port); re-exported here under the original path. `show()`
//! stays -- it draws with egui.

use std::sync::Arc;

pub use hxy_panels::inspector::*;

/// Draw the inspector into `ui`. `caret_offset` is the cursor byte
/// position; `bytes` is a prefetched window of data (typically 16
/// bytes) starting at that offset.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut InspectorState,
    decoders: &[Arc<dyn Decoder>],
    caret_offset: Option<u64>,
    bytes: &[u8],
) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{} Inspector", egui_phosphor::regular::EYE)).strong());
        ui.separator();
        ui.label("Endianness:");
        ui.selectable_value(&mut state.endian, Endian::Little, "Little");
        ui.selectable_value(&mut state.endian, Endian::Big, "Big");
        ui.separator();
        ui.label("Int radix:");
        ui.selectable_value(&mut state.radix, IntRadix::Decimal, "Dec");
        ui.selectable_value(&mut state.radix, IntRadix::Hex, "Hex");
        ui.selectable_value(&mut state.radix, IntRadix::Binary, "Bin");
    });
    ui.separator();

    if caret_offset.is_none() {
        ui.weak("No caret -- click a byte in the hex view.");
        return;
    }
    ui.add_space(4.0);

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("hxy_inspector_grid").num_columns(2).striped(true).min_col_width(100.0).show(ui, |ui| {
            for dec in decoders {
                ui.label(dec.name());
                let decoded = dec.decode(bytes, state.endian, state.radix);
                match decoded {
                    Some(Decoded::Text(s)) => {
                        ui.add(egui::Label::new(egui::RichText::new(&s).monospace()).truncate());
                    }
                    Some(Decoded::Color { rgba, label }) => {
                        ui.horizontal(|ui| {
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                            let fill = egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
                            ui.painter().rect_filled(rect, 3.0, fill);
                            // Thin outline so light colors on a
                            // light background still read as a
                            // distinct swatch.
                            ui.painter().rect_stroke(
                                rect,
                                3.0,
                                egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.fg_stroke.color),
                                egui::StrokeKind::Inside,
                            );
                            ui.add(egui::Label::new(egui::RichText::new(&label).monospace()).truncate());
                        });
                    }
                    None => {
                        ui.weak("--");
                    }
                }
                ui.end_row();
            }
        });
    });
}
