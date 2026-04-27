//! Offset / range entry builder for the palette's
//! `Go to offset` / `Select from offset` / `Select range` modes,
//! plus the formatted-offset clipboard helpers behind the
//! Copy-caret / Copy-selection / Copy-file-length entries.

use crate::app::HxyApp;
use crate::commands::palette::Action;
use crate::commands::palette::Mode;

/// Snapshot of the active tab's caret + source length, used by the
/// Go-To / Select palette modes to resolve relative offsets and
/// bounds-check resulting ranges.
#[derive(Clone, Copy, Default)]
pub struct OffsetPaletteContext {
    pub cursor: u64,
    pub source_len: u64,
    pub available: bool,
    /// `Some((start, end_exclusive))` when the active tab has a
    /// non-empty selection (including a single-byte caret). `None`
    /// means no selection exists -- caret-specific copy entries
    /// skip themselves in that case.
    pub selection: Option<(u64, u64)>,
}

#[derive(Clone, Copy, Debug)]
pub enum OffsetCopy {
    Caret,
    SelectionRange,
    SelectionLength,
    FileLength,
}

pub fn offset_palette_context(app: &mut HxyApp) -> OffsetPaletteContext {
    let Some(id) = crate::app::active_file_id(app) else { return OffsetPaletteContext::default() };
    let Some(file) = app.files.get(&id) else { return OffsetPaletteContext::default() };
    let source_len = file.editor.source().len().get();
    let sel = file.editor.selection();
    let cursor = sel.map(|s| s.cursor.get()).unwrap_or(0);
    let selection = sel.map(|s| {
        let r = s.range();
        (r.start().get(), r.end().get())
    });
    OffsetPaletteContext { cursor, source_len, available: true, selection }
}

/// Copy a formatted offset / length / range from the active tab to
/// the clipboard. Used by the palette's Copy-caret / Copy-selection
/// / Copy-file-length entries; formatting matches the status bar
/// (current `OffsetBase` setting).
pub fn copy_formatted_offset(ctx: &egui::Context, app: &mut HxyApp, kind: OffsetCopy) {
    let Some(id) = crate::app::active_file_id(app) else { return };
    let base = app.state.read().app.offset_base;
    let Some(file) = app.files.get(&id) else { return };
    let source_len = file.editor.source().len().get();
    let sel = file.editor.selection();
    let text = match kind {
        OffsetCopy::Caret => {
            let Some(sel) = sel else { return };
            crate::app::format_offset(sel.cursor.get(), base)
        }
        OffsetCopy::SelectionRange => {
            let Some(sel) = sel else { return };
            let range = sel.range();
            let last_inclusive = range.end().get().saturating_sub(1);
            format!(
                "{}-{} ({} bytes)",
                crate::app::format_offset(range.start().get(), base),
                crate::app::format_offset(last_inclusive, base),
                crate::app::format_offset(range.len().get(), base),
            )
        }
        OffsetCopy::SelectionLength => {
            let Some(sel) = sel else { return };
            crate::app::format_offset(sel.range().len().get(), base)
        }
        OffsetCopy::FileLength => crate::app::format_offset(source_len, base),
    };
    ctx.copy_text(text);
}

pub fn build_offset_entries(
    out: &mut Vec<egui_palette::Entry<Action>>,
    mode: Mode,
    query: &str,
    offset_ctx: &OffsetPaletteContext,
) {
    use egui_phosphor::regular as icon;

    if query.is_empty() {
        return;
    }
    match mode {
        Mode::GoToOffset => match crate::commands::goto::parse_number(query).and_then(|n| {
            n.resolve(offset_ctx.cursor, offset_ctx.source_len).ok_or(crate::commands::goto::ParseError::OutOfRange)
        }) {
            Ok(target) => {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t_args("palette-go-to-offset-fmt", &[("offset", &format!("0x{target:X}"))]),
                        Action::GoToOffset(target),
                    )
                    .with_icon(icon::CROSSHAIR)
                    .with_subtitle(format!("{target}")),
                );
            }
            Err(e) => super::entries::invalid_entry(out, query, &e.to_string()),
        },
        Mode::SelectFromOffset => match crate::commands::goto::parse_number(query) {
            Ok(crate::commands::goto::Number::Absolute(count)) if count > 0 => {
                let start = offset_ctx.cursor;
                let available = offset_ctx.source_len.saturating_sub(start);
                if available == 0 {
                    super::entries::invalid_entry(out, query, "at EOF");
                    return;
                }
                let clamped = count.min(available);
                let end_exclusive = start + clamped;
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t_args(
                            "palette-select-from-offset-fmt",
                            &[("count", &format!("{clamped}")), ("start", &format!("0x{start:X}"))],
                        ),
                        Action::SetSelection { start, end_exclusive },
                    )
                    .with_icon(icon::ARROWS_OUT_LINE_HORIZONTAL)
                    .with_subtitle(format!("0x{start:X} .. 0x{end_exclusive:X}")),
                );
            }
            Ok(crate::commands::goto::Number::Absolute(_)) => {
                super::entries::invalid_entry(out, query, "count must be nonzero")
            }
            Ok(crate::commands::goto::Number::Relative(_)) => {
                super::entries::invalid_entry(out, query, "count must be absolute (no + / - prefix)")
            }
            Err(e) => super::entries::invalid_entry(out, query, &e.to_string()),
        },
        Mode::SelectRange => match crate::commands::goto::parse_range(query, offset_ctx.source_len) {
            Ok(range) => {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t_args(
                            "palette-select-range-fmt",
                            &[
                                ("start", &format!("0x{:X}", range.start)),
                                ("end", &format!("0x{:X}", range.end_exclusive)),
                                ("count", &format!("{}", range.len())),
                            ],
                        ),
                        Action::SetSelection { start: range.start, end_exclusive: range.end_exclusive },
                    )
                    .with_icon(icon::BRACKETS_CURLY),
                );
            }
            Err(e) => super::entries::invalid_entry(out, query, &e.to_string()),
        },
        _ => {}
    }
}
