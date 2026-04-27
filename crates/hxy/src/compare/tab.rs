//! `Tab::Compare` body rendering: toolbar, two hex panes, diff
//! table at the bottom. Pulls in [`super::pane::render_compare_pane`]
//! for the per-side hex view.

#![cfg(not(target_arch = "wasm32"))]

use crate::compare::pane::compare_pane_ranges;
use crate::compare::pane::pane_hover_span;
use crate::compare::pane::render_compare_pane;
use crate::compare::pane::scroll_pane_to;
use crate::state::PersistedState;

/// Render the body of a `Tab::Compare`. Top toolbar, two hex panes
/// side-by-side, diff table at the bottom. Synchronized scroll with
/// gap rows / minimap leader lines are deferred to a follow-up; this
/// commit lands the wire, the table, the diff color overlay, and a
/// debounced live recompute.
pub fn render_compare_tab(ui: &mut egui::Ui, session: &mut crate::compare::CompareSession, state: &mut PersistedState) {
    use crate::compare::DebouncedDecision;

    let id = session.id;
    let tab_id = ui.id().with(("hxy-compare", id.get()));

    session.poll_recompute();
    let global_deadline = state.app.compare_recompute_deadline;
    match session.needs_recompute_debounced(std::time::Instant::now()) {
        DebouncedDecision::Idle => {}
        DebouncedDecision::WaitFor(d) => ui.ctx().request_repaint_after(d),
        DebouncedDecision::Recompute => {
            session.request_recompute(ui.ctx(), session.effective_deadline(global_deadline));
        }
    }

    egui::Panel::top(tab_id.with("toolbar")).resizable(false).show_inside(ui, |ui| {
        ui.horizontal(|ui| {
            let recomputing = session.is_recomputing();
            if ui.add_enabled(!recomputing, egui::Button::new(hxy_i18n::t("compare-recompute"))).clicked() {
                session.request_recompute(ui.ctx(), session.effective_deadline(global_deadline));
            }
            ui.checkbox(&mut session.sync_scroll_enabled, hxy_i18n::t("compare-sync-scroll"))
                .on_hover_text(hxy_i18n::t("compare-sync-scroll-tooltip"));
            render_compare_deadline_widget(ui, session, global_deadline);
            if recomputing {
                ui.spinner();
                ui.weak(hxy_i18n::t("compare-status-recomputing"));
            } else if let Some(diff) = &session.diff {
                ui.weak(hxy_i18n::t_args(
                    "compare-status",
                    &[
                        ("a", &session.a.display_name),
                        ("b", &session.b.display_name),
                        ("changes", &diff.change_count().to_string()),
                    ],
                ));
            } else {
                ui.weak(hxy_i18n::t("compare-status-pending"));
            }
        });
    });

    egui::Panel::bottom(tab_id.with("diff-table")).resizable(true).min_size(120.0).default_size(160.0).show_inside(
        ui,
        |ui| {
            render_compare_diff_table(ui, session);
        },
    );

    let cols = state.app.hex_columns.as_u64();
    let (a_ranges, b_ranges, row_maps) = match session.diff.as_ref() {
        Some(d) => (
            compare_pane_ranges(d, crate::compare::CompareSide::A),
            compare_pane_ranges(d, crate::compare::CompareSide::B),
            Some(crate::compare::build_row_maps(d, cols)),
        ),
        None => (Vec::new(), Vec::new(), None),
    };
    let (a_map, b_map): (Option<Vec<hxy_view::RowSlot>>, Option<Vec<hxy_view::RowSlot>>) = match row_maps {
        Some(m) => (Some(m.a), Some(m.b)),
        None => (None, None),
    };
    let (a_hover, b_hover) = match (session.hovered_hunk, session.diff.as_ref()) {
        (Some(idx), Some(d)) => {
            let hunk = d.hunks.get(idx).copied();
            (
                hunk.and_then(|h| pane_hover_span(h.a_offset, h.a_len)),
                hunk.and_then(|h| pane_hover_span(h.b_offset, h.b_len)),
            )
        }
        _ => (None, None),
    };

    egui::CentralPanel::default().show_inside(ui, |ui| {
        let avail = ui.available_width();
        let half = (avail * 0.5).max(160.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::Vec2::new(half, ui.available_height()),
                egui::Layout::top_down(egui::Align::LEFT),
                |ui| {
                    render_compare_pane(
                        ui,
                        &mut session.a,
                        state,
                        tab_id.with("pane-a"),
                        &a_ranges,
                        a_map.clone(),
                        a_hover,
                    );
                },
            );
            ui.separator();
            ui.allocate_ui_with_layout(
                egui::Vec2::new(ui.available_width(), ui.available_height()),
                egui::Layout::top_down(egui::Align::LEFT),
                |ui| {
                    render_compare_pane(
                        ui,
                        &mut session.b,
                        state,
                        tab_id.with("pane-b"),
                        &b_ranges,
                        b_map.clone(),
                        b_hover,
                    );
                },
            );
        });
    });

    session.sync_scroll();
}

/// Render the per-tab Myers diff deadline widget on the compare
/// toolbar. Editing the value sets the per-session override; the
/// reset button (only shown when an override is active) clears it
/// so the session falls back to the global setting again.
fn render_compare_deadline_widget(
    ui: &mut egui::Ui,
    session: &mut crate::compare::CompareSession,
    global: crate::settings::RecomputeDeadline,
) {
    use crate::settings::RecomputeDeadline;
    ui.label(hxy_i18n::t("compare-deadline-label"));
    let effective = session.effective_deadline(global);
    let mut ms = effective.as_ms();
    let response = ui.add(
        egui::DragValue::new(&mut ms)
            .range(RecomputeDeadline::MIN_MS..=RecomputeDeadline::MAX_MS)
            .speed(50.0)
            .suffix(" ms"),
    );
    response.on_hover_text(hxy_i18n::t("compare-deadline-tooltip"));
    if ms != effective.as_ms() {
        session.recompute_deadline_override = Some(RecomputeDeadline::from_ms(ms));
    }
    if session.recompute_deadline_override.is_some()
        && ui
            .small_button(hxy_i18n::t("compare-deadline-reset"))
            .on_hover_text(hxy_i18n::t("compare-deadline-reset-tooltip"))
            .clicked()
    {
        session.recompute_deadline_override = None;
    }
}

fn render_compare_diff_table(ui: &mut egui::Ui, session: &mut crate::compare::CompareSession) {
    use egui_table::Column;
    use egui_table::HeaderRow;
    use egui_table::Table;

    let Some(diff) = session.diff.clone() else {
        ui.weak(hxy_i18n::t("compare-status-pending"));
        return;
    };
    let visible: Vec<(usize, crate::compare::DiffHunk)> = diff
        .hunks
        .iter()
        .enumerate()
        .filter(|(_, h)| !matches!(h.kind, crate::compare::HunkKind::Equal))
        .map(|(i, h)| (i, *h))
        .collect();
    if visible.is_empty() {
        ui.weak(hxy_i18n::t("compare-no-differences"));
        return;
    }

    let row_height = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
    let mut any_hover: Option<usize> = None;
    let mut click: Option<crate::compare::DiffHunk> = None;
    let id_seed = ui.id().with(("hxy-compare-table", session.id.get()));

    let mut delegate = CompareTableDelegate {
        visible: &visible,
        hovered_hunk: session.hovered_hunk,
        any_hover: &mut any_hover,
        click: &mut click,
        row_height,
    };

    Table::new()
        .id_salt(id_seed)
        .num_rows(visible.len() as u64)
        .columns(vec![
            Column::new(96.0).range(60.0..=200.0).resizable(true).id(egui::Id::new("cmp-col-kind")),
            Column::new(180.0).range(80.0..=400.0).resizable(true).id(egui::Id::new("cmp-col-a")),
            Column::new(180.0).range(80.0..=400.0).resizable(true).id(egui::Id::new("cmp-col-b")),
            Column::new(160.0).range(80.0..=400.0).resizable(true).id(egui::Id::new("cmp-col-size")),
        ])
        .headers(vec![HeaderRow::new(row_height)])
        .auto_size_mode(egui_table::AutoSizeMode::OnParentResize)
        .show(ui, &mut delegate);

    if any_hover != session.hovered_hunk {
        session.hovered_hunk = any_hover;
    }
    if let Some(hunk) = click {
        if hunk.a_len > 0 {
            scroll_pane_to(&mut session.a, hunk.a_offset, hunk.a_len);
        }
        if hunk.b_len > 0 {
            scroll_pane_to(&mut session.b, hunk.b_offset, hunk.b_len);
        }
    }
}

struct CompareTableDelegate<'a> {
    visible: &'a [(usize, crate::compare::DiffHunk)],
    hovered_hunk: Option<usize>,
    any_hover: &'a mut Option<usize>,
    click: &'a mut Option<crate::compare::DiffHunk>,
    row_height: f32,
}

impl egui_table::TableDelegate for CompareTableDelegate<'_> {
    fn header_cell_ui(&mut self, ui: &mut egui::Ui, cell: &egui_table::HeaderCellInfo) {
        let key = match cell.col_range.start {
            0 => "compare-table-kind",
            1 => "compare-table-a-range",
            2 => "compare-table-b-range",
            3 => "compare-table-size",
            _ => return,
        };
        ui.add_space(6.0);
        ui.strong(hxy_i18n::t(key));
    }

    fn row_ui(&mut self, ui: &mut egui::Ui, row_nr: u64) {
        let Some((hunk_idx, hunk)) = self.visible.get(row_nr as usize).copied() else { return };
        let row_rect = ui.max_rect();
        ui.push_id(("hxy-compare-row", row_nr), |ui| {
            let row_id = ui.id().with("interact");
            let resp = ui.interact(row_rect, row_id, egui::Sense::click());
            // Mirror the template panel: cell labels eat hover /
            // click responses, so fall back to "pointer in rect"
            // checks so the whole row behaves as one target.
            let over_row = ui.rect_contains_pointer(row_rect);
            if over_row {
                *self.any_hover = Some(hunk_idx);
            }
            let clicked_row = resp.clicked() || (over_row && ui.input(|i| i.pointer.primary_clicked()));
            if clicked_row {
                *self.click = Some(hunk);
            }
            if self.hovered_hunk == Some(hunk_idx) {
                ui.painter().rect_filled(row_rect, 0.0, ui.visuals().selection.bg_fill.gamma_multiply(0.35));
            }
        });
    }

    fn cell_ui(&mut self, ui: &mut egui::Ui, cell: &egui_table::CellInfo) {
        use crate::compare::HunkKind;
        let Some((_, hunk)) = self.visible.get(cell.row_nr as usize).copied() else { return };
        ui.add_space(6.0);
        match cell.col_nr {
            0 => {
                let (key, color) = match hunk.kind {
                    HunkKind::Added => ("compare-kind-added", egui::Color32::from_rgb(60, 200, 100)),
                    HunkKind::Removed => ("compare-kind-removed", egui::Color32::from_rgb(220, 90, 90)),
                    HunkKind::Changed => ("compare-kind-changed", egui::Color32::from_rgb(220, 160, 60)),
                    HunkKind::Equal => return,
                };
                ui.label(egui::RichText::new(hxy_i18n::t(key)).color(color).strong());
            }
            1 => {
                ui.label(format_range(hunk.a_offset, hunk.a_len));
            }
            2 => {
                ui.label(format_range(hunk.b_offset, hunk.b_len));
            }
            3 => {
                ui.label(hxy_i18n::t_args(
                    "compare-table-size-fmt",
                    &[("a", &hunk.a_len.to_string()), ("b", &hunk.b_len.to_string())],
                ));
            }
            _ => {}
        }
    }

    fn default_row_height(&self) -> f32 {
        self.row_height
    }
}

fn format_range(offset: u64, len: u64) -> String {
    if len == 0 { format!("0x{offset:08X} (gap)") } else { format!("0x{offset:08X} +{len}") }
}
