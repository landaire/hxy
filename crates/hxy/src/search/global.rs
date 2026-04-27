//! Cross-file search backing the `Tab::SearchResults` tab.

use crate::files::FileId;
use crate::search::Endian;
use crate::search::NumberWidth;
use crate::search::SearchKind;
use crate::search::SearchState;

#[derive(Clone, Debug)]
pub struct GlobalMatch {
    pub file_id: FileId,
    pub offset: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum GlobalSearchEvent {
    /// User edited the query / settings -- re-encode the pattern. The
    /// host doesn't auto-rescan; the user runs the scan explicitly via
    /// `Run`.
    Refresh,
    /// Run the scan against every open file's source.
    Run,
    /// Close the tab.
    Close,
    /// Click on a result row. Carries the index into `matches`.
    JumpTo(usize),
}

/// Aggregated cross-file search state. The query, type, width,
/// endianness, etc. mirror `SearchState` so the user sees the same UI
/// in both bars; matches are accumulated by walking every open file.
pub struct GlobalSearchState {
    pub open: bool,
    pub query_state: SearchState,
    pub matches: Vec<GlobalMatch>,
    pub active_idx: Option<usize>,
}

impl Default for GlobalSearchState {
    fn default() -> Self {
        Self {
            open: false,
            query_state: SearchState {
                kind: SearchKind::HexBytes,
                width: NumberWidth::W32,
                signed: false,
                endian: Endian::Little,
                all_results: true,
                ..SearchState::default()
            },
            matches: Vec::new(),
            active_idx: None,
        }
    }
}

/// Render the cross-file search tab. Returns events to apply post-dock.
/// `file_names` provides display names for every open file (used in
/// the result rows); `caret` is unused -- global search runs over the
/// whole file every time.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut GlobalSearchState,
    file_names: &std::collections::HashMap<FileId, String>,
) -> Vec<GlobalSearchEvent> {
    let mut events = Vec::new();

    egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label("Find in all files:");
            let kind_label = match state.query_state.kind {
                SearchKind::Text => "Text",
                SearchKind::HexBytes => "Hex Bytes",
                SearchKind::Number => "Number",
            };
            egui::ComboBox::from_id_salt("hxy_global_search_kind").selected_text(kind_label).show_ui(ui, |ui| {
                for k in [SearchKind::Text, SearchKind::HexBytes, SearchKind::Number] {
                    let label = match k {
                        SearchKind::Text => "Text",
                        SearchKind::HexBytes => "Hex Bytes",
                        SearchKind::Number => "Number",
                    };
                    if ui.selectable_value(&mut state.query_state.kind, k, label).changed() {
                        events.push(GlobalSearchEvent::Refresh);
                    }
                }
            });
            if matches!(state.query_state.kind, SearchKind::Number) {
                egui::ComboBox::from_id_salt("hxy_global_search_width")
                    .selected_text(format!("{}-bit", state.query_state.width.label()))
                    .show_ui(ui, |ui| {
                        for w in NumberWidth::ALL {
                            if ui
                                .selectable_value(&mut state.query_state.width, w, format!("{}-bit", w.label()))
                                .changed()
                            {
                                events.push(GlobalSearchEvent::Refresh);
                            }
                        }
                    });
                if ui.checkbox(&mut state.query_state.signed, "signed").changed() {
                    events.push(GlobalSearchEvent::Refresh);
                }
                if ui.selectable_label(matches!(state.query_state.endian, Endian::Little), "LE").clicked() {
                    state.query_state.endian = Endian::Little;
                    events.push(GlobalSearchEvent::Refresh);
                }
                if ui.selectable_label(matches!(state.query_state.endian, Endian::Big), "BE").clicked() {
                    state.query_state.endian = Endian::Big;
                    events.push(GlobalSearchEvent::Refresh);
                }
            }
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.query_state.query)
                    .id_salt("hxy_global_search_query")
                    .desired_width(260.0),
            );
            if resp.changed() {
                events.push(GlobalSearchEvent::Refresh);
            }
            let enter =
                resp.lost_focus() && ui.ctx().input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            if enter || ui.button(egui_phosphor::regular::MAGNIFYING_GLASS).on_hover_text("Run").clicked() {
                events.push(GlobalSearchEvent::Run);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(egui_phosphor::regular::X).on_hover_text("Close").clicked() {
                    events.push(GlobalSearchEvent::Close);
                }
                if let Some(err) = &state.query_state.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
                } else {
                    ui.weak(format!("{} matches across files", state.matches.len()));
                }
            });
        });

        ui.separator();
        if state.matches.is_empty() {
            ui.weak("No matches yet -- type a query and press Enter or click the magnifier.");
            return;
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, m) in state.matches.iter().enumerate() {
                let active = state.active_idx == Some(i);
                let name = file_names.get(&m.file_id).cloned().unwrap_or_else(|| format!("file-{}", m.file_id.get()));
                let label = format!("{}    0x{:08X}    {}", name, m.offset, m.offset);
                if ui.selectable_label(active, label).clicked() {
                    events.push(GlobalSearchEvent::JumpTo(i));
                }
            }
        });
    });

    events
}
