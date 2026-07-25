//! Dispatch for the [`PaletteAction`] the user activates.
//!
//! Shaped like the egui app's `apply_palette_action`: the overlay
//! closes first (handled by the caller in [`super::Palette::pick`]),
//! then this routes each pure-data action into the
//! [`Workspace`](crate::workspace::Workspace). [`PaletteAction::SwitchMode`]
//! and [`PaletteAction::NoOp`] never reach here -- the overlay consumes
//! them itself.

use gpui::ClipboardItem;
use gpui::Context;
use gpui::Window;
use hxy_core::ByteOffset;
use hxy_core::Selection;

use crate::palette::modes::CopyFormat;
use crate::palette::modes::PaletteAction;
use crate::workspace::Workspace;

/// Route `action` into the workspace. The palette is already closed and
/// focus restored to the grid, so the caret / selection changes land on
/// the pane the user will see.
pub(crate) fn apply(ws: &mut Workspace, action: PaletteAction, window: &mut Window, cx: &mut Context<Workspace>) {
    match action {
        PaletteAction::OpenFile => ws.open_file_dialog(window, cx),
        PaletteAction::CloseTab => ws.close_active_tab(window, cx),
        PaletteAction::ToggleVim => ws.toggle_active_vim(cx),
        PaletteAction::ToggleInspector => ws.toggle_inspector_dock(window, cx),
        PaletteAction::OpenStrings => ws.open_strings_for_active_file(window, cx),
        PaletteAction::GoToOffset(target) => {
            let Some(pane) = ws.active_pane(cx) else { return };
            pane.update(cx, |pane, cx| {
                let max = pane.editor().source().len().get().saturating_sub(1);
                let clamped = ByteOffset::new(target.min(max));
                pane.editor_mut().set_selection(Some(Selection::caret(clamped)));
                if !pane.editor().is_offset_visible(clamped) {
                    pane.editor_mut().set_scroll_to_byte(clamped);
                }
                pane.sync_pending_scroll(cx);
                cx.notify();
            });
        }
        PaletteAction::SetSelection { start, end_exclusive } => {
            let Some(pane) = ws.active_pane(cx) else { return };
            pane.update(cx, |pane, cx| {
                let source_len = pane.editor().source().len().get();
                if source_len == 0 || end_exclusive <= start {
                    return;
                }
                let last = end_exclusive.saturating_sub(1).min(source_len.saturating_sub(1));
                let anchor = ByteOffset::new(start.min(source_len.saturating_sub(1)));
                pane.editor_mut().set_selection(Some(Selection { anchor, cursor: ByteOffset::new(last) }));
                if !pane.editor().is_offset_visible(anchor) {
                    pane.editor_mut().set_scroll_to_byte(anchor);
                }
                pane.sync_pending_scroll(cx);
                cx.notify();
            });
        }
        PaletteAction::SetColumns(count) => {
            let Some(pane) = ws.active_pane(cx) else { return };
            pane.update(cx, |pane, cx| pane.set_columns(count, cx));
        }
        PaletteAction::CopyText(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
        PaletteAction::CopySelection(format) => {
            let Some(pane) = ws.active_pane(cx) else { return };
            let Some(text) = selection_text(&pane, format, cx) else { return };
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        // Consumed by the overlay before reaching dispatch.
        PaletteAction::SwitchMode(_) | PaletteAction::NoOp => {}
    }
}

/// Read the active pane's current selection and render it in `format`.
/// `None` when there is no selection or the bytes can't be read.
fn selection_text(pane: &gpui::Entity<hxy_view_gpui::HexPane>, format: CopyFormat, cx: &gpui::App) -> Option<String> {
    let pane = pane.read(cx);
    let editor = pane.editor();
    let range = editor.selection()?.range();
    let bytes = editor.source().read(range).ok()?;
    Some(match format {
        // Space-separated uppercase hex, matching the vim hex-pane yank.
        CopyFormat::Hex => bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" "),
        CopyFormat::Bytes => String::from_utf8_lossy(&bytes).into_owned(),
    })
}
