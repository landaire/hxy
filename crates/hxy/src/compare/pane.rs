//! Compare pane: one side's hex view rendering plus the
//! diff-color overlay machinery.

#![cfg(not(target_arch = "wasm32"))]

use crate::state::PersistedState;

/// Sorted-by-start `(start, end_exclusive, kind)` ranges for one
/// side's coloring. Skips hunks that don't have any bytes on that
/// side (Added on A, Removed on B) so the styler's binary search
/// only inspects ranges with non-zero length.
pub fn compare_pane_ranges(
    diff: &crate::compare::DiffResult,
    side: crate::compare::CompareSide,
) -> Vec<(u64, u64, crate::compare::HunkKind)> {
    use crate::compare::CompareSide;
    use crate::compare::HunkKind;

    diff.changes()
        .filter_map(|h| {
            let (offset, len) = match side {
                CompareSide::A => (h.a_offset, h.a_len),
                CompareSide::B => (h.b_offset, h.b_len),
            };
            if len == 0 {
                return None;
            }
            Some((offset, offset + len, h.kind))
        })
        .filter(|(_, _, kind)| !matches!(kind, HunkKind::Equal))
        .collect()
}

/// Convert a hunk's `(offset, len)` for one side into a
/// [`hxy_core::ByteRange`] for the hex view's `hover_span`. Returns
/// `None` for zero-length sides (Added on A / Removed on B), which
/// leaves the corresponding pane unhighlighted.
pub fn pane_hover_span(offset: u64, len: u64) -> Option<hxy_core::ByteRange> {
    if len == 0 {
        return None;
    }
    hxy_core::ByteRange::new(hxy_core::ByteOffset::new(offset), hxy_core::ByteOffset::new(offset + len)).ok()
}

pub fn render_compare_pane(
    ui: &mut egui::Ui,
    pane: &mut crate::compare::ComparePane,
    state: &mut PersistedState,
    salt: egui::Id,
    diff_ranges: &[(u64, u64, crate::compare::HunkKind)],
    row_map: Option<Vec<hxy_view::RowSlot>>,
    hover_span: Option<hxy_core::ByteRange>,
) {
    ui.horizontal(|ui| {
        ui.strong(&pane.display_name);
        ui.checkbox(&mut pane.diff_colors, hxy_i18n::t("compare-diff-colors-toggle"));
    });
    let columns = state.app.hex_columns;
    let highlight = state.app.byte_value_highlight.then(|| state.app.byte_highlight_mode.as_view());
    let mut diff_field_bounds: Vec<(hxy_core::ByteOffset, hxy_core::ByteLen)> = Vec::new();
    let mut diff_field_colors: Vec<egui::Color32> = Vec::new();
    if pane.diff_colors {
        for (start, end_exclusive, kind) in diff_ranges {
            let len = hxy_core::ByteLen::new(end_exclusive - start);
            diff_field_bounds.push((hxy_core::ByteOffset::new(*start), len));
            diff_field_colors.push(compare_kind_color(*kind));
        }
    }
    let mut view = pane
        .editor
        .view()
        .id_salt(salt)
        .columns(columns)
        .value_highlight(highlight)
        .minimap(state.app.show_minimap)
        .minimap_colored(state.app.minimap_colored)
        .field_boundaries(&diff_field_bounds)
        .field_colors(&diff_field_colors);
    if let Some(span) = hover_span {
        view = view.hover_span(Some(span));
    }
    if let Some(map) = row_map {
        view = view.row_map(map);
    }
    if pane.diff_colors && !diff_ranges.is_empty() {
        let ranges = diff_ranges.to_vec();
        let text_mode = matches!(state.app.byte_highlight_mode, crate::settings::ByteHighlightMode::Text);
        view = view.byte_styler(move |_byte, offset| {
            let off = offset.get();
            let idx = ranges.partition_point(|(start, _, _)| *start <= off);
            if idx == 0 {
                return hxy_view::ByteStyle { bg: None, fg: None };
            }
            let (_, end_exclusive, kind) = ranges[idx - 1];
            if off >= end_exclusive {
                return hxy_view::ByteStyle { bg: None, fg: None };
            }
            compare_kind_style(kind, text_mode)
        });
    }
    let response = view.show(ui);
    pane.editor.on_response(&response, columns);
}

pub fn compare_kind_color(kind: crate::compare::HunkKind) -> egui::Color32 {
    use crate::compare::HunkKind;
    match kind {
        HunkKind::Added => egui::Color32::from_rgb(60, 200, 100),
        HunkKind::Removed => egui::Color32::from_rgb(220, 90, 90),
        HunkKind::Changed => egui::Color32::from_rgb(220, 160, 60),
        HunkKind::Equal => egui::Color32::TRANSPARENT,
    }
}

/// Pick the diff color for `kind`, applied to either the byte fill
/// or the text depending on the user's global byte-highlight-mode
/// setting -- so compare colors land in the same channel as
/// template colors.
pub fn compare_kind_style(kind: crate::compare::HunkKind, text_mode: bool) -> hxy_view::ByteStyle {
    use crate::compare::HunkKind;
    let color = match kind {
        HunkKind::Added => egui::Color32::from_rgb(60, 200, 100),
        HunkKind::Removed => egui::Color32::from_rgb(220, 90, 90),
        HunkKind::Changed => egui::Color32::from_rgb(220, 160, 60),
        HunkKind::Equal => return hxy_view::ByteStyle { bg: None, fg: None },
    };
    if text_mode {
        hxy_view::ByteStyle { bg: None, fg: Some(color) }
    } else {
        hxy_view::ByteStyle { bg: Some(color), fg: None }
    }
}

pub fn scroll_pane_to(pane: &mut crate::compare::ComparePane, offset: u64, len: u64) {
    let end_inclusive = offset.saturating_add(len.max(1)).saturating_sub(1);
    pane.editor.set_selection(Some(hxy_core::Selection {
        anchor: hxy_core::ByteOffset::new(offset),
        cursor: hxy_core::ByteOffset::new(end_inclusive),
    }));
    pane.editor.set_scroll_to_byte(hxy_core::ByteOffset::new(offset));
}
