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
use gpui::component::WindowExt;
use gpui::component::notification::Notification;
use hxy_core::ByteOffset;
use hxy_core::Selection;

use crate::palette::modes::CopyFormat;
use crate::palette::modes::PaletteAction;
use crate::plugins::PluginOp;
use crate::plugins::find_handler;
use crate::templates::FieldJump;
use crate::workspace::Workspace;

/// Route `action` into the workspace. The palette is already closed and
/// focus restored to the grid, so the caret / selection changes land on
/// the pane the user will see.
pub(crate) fn apply(ws: &mut Workspace, action: PaletteAction, window: &mut Window, cx: &mut Context<Workspace>) {
    match action {
        PaletteAction::OpenFile => ws.open_file_dialog(window, cx),
        PaletteAction::CloseTab => ws.close_active_tab(window, cx),
        PaletteAction::ToggleVim => ws.toggle_active_vim(cx),
        PaletteAction::ToggleGlobalSearch => ws.toggle_global_search(window, cx),
        PaletteAction::ToggleInspector => ws.toggle_inspector_dock(window, cx),
        PaletteAction::OpenStrings => ws.open_strings_for_active_file(window, cx),
        PaletteAction::OpenEntropy => ws.open_entropy_for_active_file(window, cx),
        PaletteAction::OpenVisualizer => ws.open_visualizer_for_active_file(window, cx),
        PaletteAction::OpenChecksums => ws.open_checksums_for_active_file(window, cx),
        PaletteAction::OpenSettings => ws.open_settings(window, cx),
        PaletteAction::OpenPlugins => ws.open_plugins(window, cx),
        PaletteAction::OpenConsole => ws.open_console(window, cx),
        PaletteAction::BrowseVfs => ws.browse_active_file_as_workspace(window, cx),
        PaletteAction::SplitPane(dir) => ws.split_active_pane(dir, window, cx),
        PaletteAction::MoveTab(dir) => ws.move_active_tab(dir, window, cx),
        PaletteAction::MergePane(dir) => ws.merge_active_pane(dir, window, cx),
        PaletteAction::TearTab => ws.tear_active_tab_into_window(window, cx),
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
        PaletteAction::RunTemplate { path, range } => ws.run_template_on_active(path, range, window, cx),
        PaletteAction::RunTemplateDialog => ws.run_template_dialog(window, cx),
        PaletteAction::InstallTemplate => ws.install_template_dialog(window, cx),
        PaletteAction::UninstallTemplate(path) => ws.uninstall_template(&path, window, cx),
        PaletteAction::JumpNextField => ws.jump_template_field(FieldJump::Next, cx),
        PaletteAction::JumpPrevField => ws.jump_template_field(FieldJump::Prev, cx),
        PaletteAction::FetchImhexPatterns => ws.fetch_imhex_patterns(window, cx),
        // Resolve the handler by name (a rescan may have dropped it since
        // the row was built) and drive the call off-thread. The outcome
        // re-enters the palette (cascade / prompt) or closes it (done).
        PaletteAction::InvokePluginCommand { plugin_name, command_id } => match find_handler(cx, &plugin_name) {
            Some(plugin) => ws.spawn_plugin_op(PluginOp::Invoke { plugin, command_id }, window, cx),
            None => tracing::warn!(plugin = %plugin_name, command = %command_id, "plugin invoke target missing"),
        },
        PaletteAction::RespondToPlugin { plugin_name, command_id, answer } => match find_handler(cx, &plugin_name) {
            Some(plugin) => ws.spawn_plugin_op(PluginOp::Respond { plugin, command_id, answer }, window, cx),
            None => tracing::warn!(plugin = %plugin_name, command = %command_id, "plugin respond target missing"),
        },
        PaletteAction::CopyText(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
        PaletteAction::CopySelection(format) => {
            let Some(pane) = ws.active_pane(cx) else { return };
            let Some((text, count)) = selection_text(&pane, format, cx) else { return };
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            let key = match format {
                CopyFormat::Hex => "gpui-toast-copied-hex",
                CopyFormat::Bytes => "gpui-toast-copied-ascii",
            };
            let message = hxy_i18n::t_args(key, &[("count", &count.to_string())]);
            window.push_notification(Notification::info(message), cx);
        }
        // Consumed by the overlay before reaching dispatch: mode
        // switches, the no-op rows, the compare cascade (routed through
        // `Workspace::open_compare` / `compare_browse`), and the QuickOpen
        // tab pick (routed through `Workspace::activate_tab`).
        PaletteAction::SwitchMode(_)
        | PaletteAction::NoOp
        | PaletteAction::CompareSelectSource { .. }
        | PaletteAction::CompareBrowse(_)
        | PaletteAction::FocusTab(_) => {}
    }
}

/// Read the active pane's current selection, render it in `format`, and
/// report the byte count (for the copied toast). `None` when there is no
/// selection or the bytes can't be read.
fn selection_text(
    pane: &gpui::Entity<hxy_view_gpui::HexPane>,
    format: CopyFormat,
    cx: &gpui::App,
) -> Option<(String, u64)> {
    let pane = pane.read(cx);
    let editor = pane.editor();
    let range = editor.selection()?.range();
    let bytes = editor.source().read(range).ok()?;
    let count = bytes.len() as u64;
    let text = match format {
        // Space-separated uppercase hex, matching the vim hex-pane yank.
        CopyFormat::Hex => bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" "),
        CopyFormat::Bytes => String::from_utf8_lossy(&bytes).into_owned(),
    };
    Some((text, count))
}
