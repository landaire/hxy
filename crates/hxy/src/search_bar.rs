//! Bottom-anchored search panel rendered inside a file tab.
//!
//! Owns no mutable state of its own -- all state lives on
//! [`crate::file::OpenFile::search`]. `show` returns a list of
//! [`SearchEvent`]s the host loop drains to perform actual byte
//! searches and selection moves.

use crate::search::Endian;
use crate::search::NumberWidth;
use crate::search::SearchKind;
use crate::search::SearchState;

#[derive(Debug, Clone, Copy)]
pub enum SearchEvent {
    /// Re-encode the pattern after a query / settings change.
    Refresh,
    /// Find next match starting after the current caret. `wrap = true`
    /// wraps to the start of the file when no match is found ahead.
    Next,
    /// Find previous match before the current caret.
    Prev,
    /// Recompute the full match set and switch into "All Results" mode.
    FindAll,
    /// User toggled "All Results" off.
    ClearAll,
    /// Close the bar.
    Close,
    /// User clicked one of the results in the All Results list.
    /// Carries the index into `state.matches`.
    JumpTo(usize),
}

/// Render the bar at the bottom of the file tab. Returns events to
/// apply against the file's hex source.
pub fn show(ui: &mut egui::Ui, state: &mut SearchState) -> Vec<SearchEvent> {
    let mut events = Vec::new();

    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(6, 4))
        .fill(ui.visuals().extreme_bg_color)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Find:");

                let kind_label = match state.kind {
                    SearchKind::Text => "Text",
                    SearchKind::HexBytes => "Hex Bytes",
                    SearchKind::Number => "Number",
                };
                egui::ComboBox::from_id_salt("hxy_search_kind").selected_text(kind_label).show_ui(ui, |ui| {
                    for k in [SearchKind::Text, SearchKind::HexBytes, SearchKind::Number] {
                        let label = match k {
                            SearchKind::Text => "Text",
                            SearchKind::HexBytes => "Hex Bytes",
                            SearchKind::Number => "Number",
                        };
                        if ui.selectable_value(&mut state.kind, k, label).changed() {
                            events.push(SearchEvent::Refresh);
                        }
                    }
                });

                if matches!(state.kind, SearchKind::Number) {
                    egui::ComboBox::from_id_salt("hxy_search_width")
                        .selected_text(format!("{}-bit", state.width.label()))
                        .show_ui(ui, |ui| {
                            for w in NumberWidth::ALL {
                                if ui
                                    .selectable_value(&mut state.width, w, format!("{}-bit", w.label()))
                                    .changed()
                                {
                                    events.push(SearchEvent::Refresh);
                                }
                            }
                        });
                    if ui.checkbox(&mut state.signed, "signed").changed() {
                        events.push(SearchEvent::Refresh);
                    }
                    if ui.selectable_label(matches!(state.endian, Endian::Little), "LE").clicked() {
                        state.endian = Endian::Little;
                        events.push(SearchEvent::Refresh);
                    }
                    if ui.selectable_label(matches!(state.endian, Endian::Big), "BE").clicked() {
                        state.endian = Endian::Big;
                        events.push(SearchEvent::Refresh);
                    }
                }

                let resp = ui.add(
                    egui::TextEdit::singleline(&mut state.query).id_salt("hxy_search_query").desired_width(220.0),
                );
                if resp.changed() {
                    events.push(SearchEvent::Refresh);
                }
                // egui's singleline TextEdit unfocuses on Enter. Catch
                // the Enter via lost_focus + the input_state shortcut
                // (consume_key already fired into the field's submit
                // path, so we read pending_input_consumed via key_pressed)
                // and refocus so repeated Enter walks matches.
                if resp.lost_focus() {
                    let enter = ui.ctx().input(|i| i.key_pressed(egui::Key::Enter));
                    let shift_held = ui.ctx().input(|i| i.modifiers.shift);
                    if enter && shift_held {
                        events.push(SearchEvent::Prev);
                        resp.request_focus();
                    } else if enter {
                        events.push(SearchEvent::Next);
                        resp.request_focus();
                    }
                }
                if resp.has_focus()
                    && ui.ctx().input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
                {
                    events.push(SearchEvent::Close);
                }
                if state.open && state.query.is_empty()
                    && !resp.has_focus()
                    && ui.ctx().memory(|m| m.focused().is_none())
                {
                    resp.request_focus();
                }

                if ui.button(egui_phosphor::regular::CARET_DOWN).on_hover_text("Next match (Enter)").clicked() {
                    events.push(SearchEvent::Next);
                }
                if ui.button(egui_phosphor::regular::CARET_UP).on_hover_text("Previous match (Shift+Enter)").clicked()
                {
                    events.push(SearchEvent::Prev);
                }

                let mut all = state.all_results;
                if ui.checkbox(&mut all, "All").changed() {
                    state.all_results = all;
                    if all {
                        events.push(SearchEvent::FindAll);
                    } else {
                        events.push(SearchEvent::ClearAll);
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(egui_phosphor::regular::X)
                        .on_hover_text("Close (Esc)")
                        .clicked()
                    {
                        events.push(SearchEvent::Close);
                    }
                    if let Some(err) = &state.error {
                        ui.colored_label(egui::Color32::LIGHT_RED, err);
                    } else if state.all_results {
                        let n = state.matches.len();
                        let label = match state.active_idx {
                            Some(i) => format!("{}/{}", i + 1, n),
                            None => format!("{} matches", n),
                        };
                        ui.weak(label);
                    } else if state.pattern.is_some() {
                        ui.weak("Enter to find");
                    }
                });
            });

            if state.all_results && !state.matches.is_empty() {
                ui.add_space(4.0);
                let avail = ui.available_height().clamp(80.0, 140.0);
                egui::ScrollArea::vertical().max_height(avail).id_salt("hxy_search_results").show(ui, |ui| {
                    for (i, off) in state.matches.iter().enumerate() {
                        let label = format!("0x{:08X}    {}", off, off);
                        let active = state.active_idx == Some(i);
                        let resp = ui.selectable_label(active, label);
                        if resp.clicked() {
                            events.push(SearchEvent::JumpTo(i));
                        }
                    }
                });
            }
        });

    events
}
