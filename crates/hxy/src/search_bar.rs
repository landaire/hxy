//! Bottom-anchored search panel rendered inside a file tab.
//!
//! Owns no mutable state of its own -- all state lives on
//! [`crate::file::OpenFile::search`]. `show` returns a list of
//! [`SearchEvent`]s the host loop drains to perform actual byte
//! searches, replacements, and selection moves.

use crate::search::Endian;
use crate::search::NumberWidth;
use crate::search::SearchKind;
use crate::search::SearchScope;
use crate::search::SearchState;

#[derive(Debug, Clone, Copy)]
pub enum SearchEvent {
    /// Re-encode the find pattern after a query / settings change.
    Refresh,
    /// Re-encode the replace pattern after the replace input or
    /// shared encoding settings changed.
    RefreshReplace,
    /// Find next match starting after the current caret.
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
    /// Toggle visibility of the replace input row.
    ToggleReplace,
    /// Replace the byte range matching the most recent find result
    /// with the encoded replacement.
    ReplaceCurrent,
    /// Replace every match in the current scope.
    ReplaceAll,
    /// Switch search scope (whole file vs current selection).
    SetScope(SearchScope),
}

/// Render the bar at the bottom of the file tab. Returns events to
/// apply against the file's hex source.
pub fn show(ui: &mut egui::Ui, state: &mut SearchState) -> Vec<SearchEvent> {
    let mut events = Vec::new();

    egui::Frame::new().inner_margin(egui::Margin::symmetric(6, 4)).fill(ui.visuals().extreme_bg_color).show(ui, |ui| {
        ui.horizontal(|ui| find_row(ui, state, &mut events));
        if state.replace_open {
            ui.add_space(2.0);
            ui.horizontal(|ui| replace_row(ui, state, &mut events));
        }
        if state.all_results && !state.matches.is_empty() {
            ui.add_space(4.0);
            let avail = ui.available_height().clamp(80.0, 140.0);
            egui::ScrollArea::vertical().max_height(avail).id_salt("hxy_search_results").show(ui, |ui| {
                for (i, off) in state.matches.iter().enumerate() {
                    let label = format!("0x{off:08X}    {off}");
                    let active = state.active_idx == Some(i);
                    if ui.selectable_label(active, label).clicked() {
                        events.push(SearchEvent::JumpTo(i));
                    }
                }
            });
        }
    });

    events
}

fn find_row(ui: &mut egui::Ui, state: &mut SearchState, events: &mut Vec<SearchEvent>) {
    ui.label(hxy_i18n::t("search-find-label"));

    egui::ComboBox::from_id_salt("hxy_search_kind").selected_text(kind_label(state.kind)).show_ui(ui, |ui| {
        for k in [SearchKind::Text, SearchKind::HexBytes, SearchKind::Number] {
            if ui.selectable_value(&mut state.kind, k, kind_label(k)).changed() {
                events.push(SearchEvent::Refresh);
                events.push(SearchEvent::RefreshReplace);
            }
        }
    });

    if matches!(state.kind, SearchKind::Number) {
        egui::ComboBox::from_id_salt("hxy_search_width").selected_text(width_label(state.width)).show_ui(ui, |ui| {
            for w in NumberWidth::ALL {
                if ui.selectable_value(&mut state.width, w, width_label(w)).changed() {
                    events.push(SearchEvent::Refresh);
                    events.push(SearchEvent::RefreshReplace);
                }
            }
        });
        if ui.checkbox(&mut state.signed, hxy_i18n::t("search-signed")).changed() {
            events.push(SearchEvent::Refresh);
            events.push(SearchEvent::RefreshReplace);
        }
        if ui.selectable_label(matches!(state.endian, Endian::Little), hxy_i18n::t("search-endian-little")).clicked() {
            state.endian = Endian::Little;
            events.push(SearchEvent::Refresh);
            events.push(SearchEvent::RefreshReplace);
        }
        if ui.selectable_label(matches!(state.endian, Endian::Big), hxy_i18n::t("search-endian-big")).clicked() {
            state.endian = Endian::Big;
            events.push(SearchEvent::Refresh);
            events.push(SearchEvent::RefreshReplace);
        }
    }

    let resp = ui.add(egui::TextEdit::singleline(&mut state.query).id_salt("hxy_search_query").desired_width(220.0));
    if resp.changed() {
        events.push(SearchEvent::Refresh);
    }
    if resp.lost_focus() {
        let (enter, shift_held) = ui.ctx().input(|i| (i.key_pressed(egui::Key::Enter), i.modifiers.shift));
        if enter && shift_held {
            events.push(SearchEvent::Prev);
            resp.request_focus();
        } else if enter {
            events.push(SearchEvent::Next);
            resp.request_focus();
        }
    }
    if resp.has_focus() && ui.ctx().input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        events.push(SearchEvent::Close);
    }
    if state.open && state.query.is_empty() && !resp.has_focus() && ui.ctx().memory(|m| m.focused().is_none()) {
        resp.request_focus();
    }

    if ui.button(egui_phosphor::regular::CARET_DOWN).on_hover_text(hxy_i18n::t("search-next-tooltip")).clicked() {
        events.push(SearchEvent::Next);
    }
    if ui.button(egui_phosphor::regular::CARET_UP).on_hover_text(hxy_i18n::t("search-prev-tooltip")).clicked() {
        events.push(SearchEvent::Prev);
    }

    let mut all = state.all_results;
    if ui.checkbox(&mut all, hxy_i18n::t("search-all-results")).changed() {
        state.all_results = all;
        if all {
            events.push(SearchEvent::FindAll);
        } else {
            events.push(SearchEvent::ClearAll);
        }
    }

    if matches!(state.scope, SearchScope::Selection { .. })
        && ui
            .selectable_label(true, hxy_i18n::t("search-scope-in-selection"))
            .on_hover_text(hxy_i18n::t("search-scope-in-selection-tooltip"))
            .clicked()
    {
        events.push(SearchEvent::SetScope(SearchScope::File));
    }

    let toggle_label = if state.replace_open {
        hxy_i18n::t("search-replace-toggle-hide")
    } else {
        hxy_i18n::t("search-replace-toggle-show")
    };
    if ui.button(toggle_label).on_hover_text(hxy_i18n::t("search-replace-toggle-tooltip")).clicked() {
        events.push(SearchEvent::ToggleReplace);
    }

    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if ui.button(egui_phosphor::regular::X).on_hover_text(hxy_i18n::t("search-close-tooltip")).clicked() {
            events.push(SearchEvent::Close);
        }
        if let Some(err) = &state.error {
            ui.colored_label(egui::Color32::LIGHT_RED, err);
        } else if state.all_results {
            let n = state.matches.len();
            let label = match state.active_idx {
                Some(i) => hxy_i18n::t_args(
                    "search-status-active-of-total",
                    &[("index", &(i + 1).to_string()), ("total", &n.to_string())],
                ),
                None => hxy_i18n::t_args("search-status-match-count", &[("count", &n.to_string())]),
            };
            ui.weak(label);
        } else if state.pattern.is_some() {
            ui.weak(hxy_i18n::t("search-status-press-enter"));
        }
    });
}

fn replace_row(ui: &mut egui::Ui, state: &mut SearchState, events: &mut Vec<SearchEvent>) {
    ui.label(hxy_i18n::t("search-replace-label"));

    let resp = ui.add(
        egui::TextEdit::singleline(&mut state.replace_query).id_salt("hxy_search_replace_query").desired_width(220.0),
    );
    if resp.changed() {
        events.push(SearchEvent::RefreshReplace);
    }

    let can_replace_current = state.pattern.is_some() && state.replace_pattern.is_some();
    if ui
        .add_enabled(can_replace_current, egui::Button::new(hxy_i18n::t("search-replace-once")))
        .on_hover_text(hxy_i18n::t("search-replace-once-tooltip"))
        .clicked()
    {
        events.push(SearchEvent::ReplaceCurrent);
    }
    if ui
        .add_enabled(can_replace_current, egui::Button::new(hxy_i18n::t("search-replace-all")))
        .on_hover_text(hxy_i18n::t("search-replace-all-tooltip"))
        .clicked()
    {
        events.push(SearchEvent::ReplaceAll);
    }

    if let Some(err) = &state.replace_error {
        ui.colored_label(egui::Color32::LIGHT_RED, err);
    }
}

fn kind_label(kind: SearchKind) -> String {
    let key = match kind {
        SearchKind::Text => "search-kind-text",
        SearchKind::HexBytes => "search-kind-hex-bytes",
        SearchKind::Number => "search-kind-number",
    };
    hxy_i18n::t(key)
}

fn width_label(width: NumberWidth) -> String {
    let bits = (width.bytes() * 8).to_string();
    hxy_i18n::t_args("search-number-width", &[("bits", &bits)])
}
