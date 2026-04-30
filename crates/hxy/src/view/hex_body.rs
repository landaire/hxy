//! Render the per-tab hex view body: hooks up the editor, value
//! palette, template field tinting, and patched-byte highlight.

#![cfg(not(target_arch = "wasm32"))]

use crate::files::OpenFile;
use crate::files::copy::CopyKind;
use crate::state::PersistedState;

/// Background tint for patched bytes when the user's highlight mode
/// paints glyphs. Saturated red stands out against the default cell
/// fill on both light and dark themes.
pub const MODIFIED_BYTE_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x80, 0x10, 0x10, 0xB0);
/// Foreground tint for patched bytes when the base highlight already
/// owns the cell fill (background mode or highlighting disabled).
pub const MODIFIED_BYTE_FG: egui::Color32 = egui::Color32::from_rgb(0xFF, 0x5A, 0x4A);

pub fn render_hex_body(ui: &mut egui::Ui, file: &mut OpenFile, state: &mut PersistedState) -> Option<CopyKind> {
    // Inline-resolve the active template via direct field access so
    // the resulting borrow is on `file.templates` (a field), not on
    // `&file` as a whole. Going through the `active_template()`
    // method would extend the borrow to all of `file` and conflict
    // with the later `file.editor.view()` mutable borrow.
    let active_state: Option<&crate::files::TemplateState> = file
        .active_template
        .and_then(|active_id| file.templates.iter().find(|t| t.id == active_id))
        .map(|t| &t.state);

    let template_palette_override = active_state.and_then(|s| s.byte_palette_override.clone());
    let (highlight, palette) = if let Some(table) = template_palette_override {
        (Some(state.app.byte_highlight_mode.as_view()), Some(hxy_view::HighlightPalette::Custom(table)))
    } else {
        let highlight = state.app.byte_value_highlight.then(|| state.app.byte_highlight_mode.as_view());
        (highlight, build_palette(ui.visuals().dark_mode, &state.app, highlight))
    };
    let has_sel = file.editor.selection().map(|s| !s.range().is_empty()).unwrap_or(false);
    let show_scalar_submenu =
        file.editor.selection().map(|s| matches!(s.range().len().get(), 1 | 2 | 4 | 8)).unwrap_or(false);

    let mut copy_request: Option<CopyKind> = None;
    let template_hover_span = active_state.and_then(|s| {
        let idx = s.hovered_node?;
        let node = s.tree.nodes.get(idx.0 as usize)?;
        let start = node.span.offset;
        let end = start.saturating_add(node.span.length);
        hxy_core::ByteRange::new(hxy_core::ByteOffset::new(start), hxy_core::ByteOffset::new(end)).ok()
    });
    // Strings-panel row hover paints the same highlight used by
    // template field hover; template wins on tie, since template
    // hovers are usually paired with explicit field selection.
    let hover_span = template_hover_span.or(file.strings_panel.hovered_entry);

    let field_boundaries = active_state.map(|s| s.leaf_boundaries.as_slice()).unwrap_or_default();
    let field_colors = active_state
        .filter(|s| s.show_colors && !s.leaf_boundaries.is_empty())
        .map(|s| (s.leaf_boundaries.as_slice(), s.leaf_colors.as_slice()));

    let modified_ranges = file.editor.modified_ranges();
    let tab_id = file.id.get();
    let columns = file.hex_columns_override.unwrap_or(state.app.hex_columns);
    let need_styler = field_colors.is_some() || !modified_ranges.is_empty();
    let styler_data = if need_styler {
        let text_mode = matches!(state.app.byte_highlight_mode, crate::settings::ByteHighlightMode::Text);
        let modified_style = if text_mode {
            hxy_view::ByteStyle { bg: Some(MODIFIED_BYTE_BG), fg: None }
        } else {
            hxy_view::ByteStyle { bg: None, fg: Some(MODIFIED_BYTE_FG) }
        };
        let field_data = field_colors.map(|(b, c)| (b.to_vec(), c.to_vec()));
        Some((text_mode, modified_style, field_data))
    } else {
        None
    };

    // Virtual base: when the user accepted the plugin's hint (or
    // set one via Open File with options...), every address the
    // hex view paints is shifted by the base. The address column
    // also needs to widen to fit the bigger numbers, and an
    // Alt-held overlay tags each row with the underlying file
    // offset so the user can correlate without flipping a setting.
    let virtual_base = file.virtual_base.unwrap_or(0);
    let alt_held = ui.input(|i| i.modifiers.alt);
    let source_len = file.editor.source().len().get();
    let display_len = hxy_core::ByteLen::new(source_len.saturating_add(virtual_base));
    let separator_char = state.app.address_separator_enabled.then_some(state.app.address_separator_char);
    let base_chars = hxy_view::address_hex_width(display_len);
    let chars_with_separator = if separator_char.is_some() {
        hxy_view::address_chars_with_separator(base_chars, 4)
    } else {
        base_chars
    };
    // Reserve room for the "[+0xfile_offset]" annotation when
    // virtual addressing is active and the user is holding Alt.
    // Width is approximate -- egui doesn't enforce; the column
    // gets the worst-case allocation for the file's max offset.
    let alt_extra = if virtual_base > 0 && alt_held {
        // " [+" + max(file offset hex chars) + "]"
        4 + hxy_view::address_hex_width(hxy_core::ByteLen::new(source_len))
    } else {
        0
    };
    let mut view = file
        .editor
        .view()
        .id_salt(("hxy-hex-view", tab_id))
        .columns(columns)
        .value_highlight(highlight)
        .minimap(state.app.show_minimap)
        .minimap_colored(state.app.minimap_colored)
        .hover_span(hover_span)
        .field_boundaries(field_boundaries);
    if virtual_base > 0 || separator_char.is_some() {
        view = view
            .address_chars(chars_with_separator + alt_extra)
            .address_formatter(move |offset, _| {
                let virtual_offset = hxy_core::ByteOffset::new(offset.get().saturating_add(virtual_base));
                let mut s = match separator_char {
                    Some(sep) => hxy_view::format_address_grouped(virtual_offset, base_chars, sep, 4),
                    None => format!("{:0width$X}", virtual_offset.get(), width = base_chars),
                };
                if virtual_base > 0 && alt_held {
                    use std::fmt::Write;
                    let _ = write!(&mut s, " [+{:X}]", offset.get());
                }
                s
            });
    }
    if let Some((_, colors)) = field_colors {
        view = view.field_colors(colors);
    }
    if let Some((text_mode, modified_style, field_data)) = styler_data {
        // Patched bytes win over the template field tint -- the
        // user is editing them right now, the template color can
        // wait.
        view = view.byte_styler(move |_byte, offset| {
            let b = offset.get();
            if range_contains(&modified_ranges, b) {
                return modified_style;
            }
            let Some((boundaries, colors)) = field_data.as_ref() else {
                return hxy_view::ByteStyle { bg: None, fg: None };
            };
            let idx = boundaries.partition_point(|(start, _)| start.get() <= b);
            if idx == 0 {
                return hxy_view::ByteStyle { bg: None, fg: None };
            }
            let (start, len) = boundaries[idx - 1];
            let end = start.get().saturating_add(len.get());
            if b >= end {
                return hxy_view::ByteStyle { bg: None, fg: None };
            }
            let color = colors[idx - 1];
            if text_mode {
                hxy_view::ByteStyle { bg: None, fg: Some(color) }
            } else {
                hxy_view::ByteStyle { bg: Some(color.gamma_multiply(0.45)), fg: None }
            }
        });
    }
    if let Some(p) = palette {
        view = view.palette(p);
    }
    let response = view
        .context_menu(|ui| {
            ui.add_enabled_ui(has_sel, |ui| {
                if let Some(kind) = crate::files::copy::copy_as_menu(ui, show_scalar_submenu) {
                    copy_request = Some(kind);
                }
            });
        })
        .show(ui);
    file.editor.on_response(&response, columns);
    file.hovered = response.hovered_offset;
    crate::tabs::close::sync_tab_state(state, file);

    let breadcrumb_state: Option<&crate::files::TemplateState> = file
        .active_template
        .and_then(|active_id| file.templates.iter().find(|t| t.id == active_id))
        .map(|t| &t.state);
    // Alt / Option held = full struct path; otherwise just the
    // leaf field. Cross-platform: egui maps the macOS Option key
    // to `modifiers.alt`.
    let detail = if ui.input(|i| i.modifiers.alt) {
        crate::panels::template::BreadcrumbDetail::Full
    } else {
        crate::panels::template::BreadcrumbDetail::Leaf
    };
    let template_value_formats = &state.app.template_value_formats;
    let inverse_format = ui.input(|i| i.modifiers.alt);
    if let Some(offset) = response.hovered_offset
        && let Some(tpl_state) = breadcrumb_state
        && let Some(path) = crate::panels::template::breadcrumb_for_offset(
            &tpl_state.tree,
            file.editor.source().as_ref(),
            offset.get(),
            detail,
            template_value_formats,
            inverse_format,
        )
    {
        let layer = ui.layer_id();
        egui::Tooltip::always_open(
            ui.ctx().clone(),
            layer,
            egui::Id::new("hxy_template_breadcrumb"),
            egui::PopupAnchor::Pointer,
        )
        .gap(12.0)
        .show(|ui| {
            // Let the tooltip grow to the widest row instead of
            // wrapping long type names. Each row is monospace so the
            // tree connectors align across labels.
            for (i, line) in path.iter().enumerate() {
                let text = egui::RichText::new(line).monospace();
                let text = if i + 1 == path.len() { text.strong() } else { text };
                ui.add(egui::Label::new(text).wrap_mode(egui::TextWrapMode::Extend));
            }
        });
    }

    copy_request
}

pub fn build_palette(
    dark: bool,
    settings: &crate::settings::AppSettings,
    highlight: Option<hxy_view::ValueHighlight>,
) -> Option<hxy_view::HighlightPalette> {
    let mode = highlight?;
    Some(match settings.byte_highlight_scheme {
        crate::settings::ByteHighlightScheme::Class => {
            hxy_view::HighlightPalette::Class(hxy_view::BytePalette::for_theme_and_mode(dark, mode))
        }
        crate::settings::ByteHighlightScheme::Value => {
            hxy_view::HighlightPalette::Value(hxy_view::ValueGradient::for_theme_and_mode(dark, mode))
        }
    })
}

/// Binary search a sorted, non-overlapping list of byte ranges for
/// `offset`. Used by the hex-view tinting closure -- O(log N) per
/// pixel-row instead of O(N).
pub fn range_contains(ranges: &[(u64, u64)], offset: u64) -> bool {
    let idx = ranges.partition_point(|(start, _)| *start <= offset);
    if idx == 0 {
        return false;
    }
    let (_start, end) = ranges[idx - 1];
    offset < end
}
