//! Dispatch for the `Action` the user picks out of the palette.
//!
//! Each variant routes to whatever app-shell helper actually
//! performs the work (open file, undo, dock split, etc.), then
//! closes the palette so the next keystroke goes back to the dock.

use hxy_vfs::TabSource;
use hxy_vfs::VfsHandler;

use crate::app::HxyApp;
use crate::commands::CommandEffect;
use crate::commands::DockDir;
use crate::commands::palette::Action;
use crate::commands::palette::ColumnScope;
use crate::commands::palette::ComparePickState;
use crate::commands::palette::CompareSide;
use crate::commands::palette::Mode;
use crate::commands::palette::PaletteCommand;
use crate::files::watch::PollingPrefs;

/// Clamp a user-typed poll interval to the supported window.
/// `0` is preserved as the "disable polling" sentinel; other
/// values are pulled into [`PollingPrefs::MIN_INTERVAL`] ..=
/// [`PollingPrefs::MAX_INTERVAL`] in milliseconds so the
/// worker doesn't spin or sleep for so long that the user
/// thinks watching is broken.
fn clamp_poll_interval_ms(ms: u32) -> u32 {
    if ms == 0 {
        return 0;
    }
    let min_ms = PollingPrefs::MIN_INTERVAL.as_millis() as u32;
    let max_ms = PollingPrefs::MAX_INTERVAL.as_millis() as u32;
    ms.clamp(min_ms, max_ms)
}

use super::offset::OffsetCopy;
use super::offset::copy_formatted_offset;

pub fn apply_palette_action(ctx: &egui::Context, app: &mut HxyApp, action: Action) {
    match action {
        Action::InvokeCommand(id) => {
            app.palette.close();
            match id {
                PaletteCommand::NewFile => crate::files::new::handle_new_file(app),
                PaletteCommand::OpenFile => crate::app::apply_command_effect(ctx, app, CommandEffect::OpenFileDialog),
                PaletteCommand::BrowseVfs => crate::app::apply_command_effect(ctx, app, CommandEffect::MountActiveFile),
                PaletteCommand::ToggleWorkspaceVfs => crate::tabs::dock_ops::toggle_workspace_vfs(app),
                PaletteCommand::ToggleToolPanel => crate::tabs::dock_ops::toggle_tool_panel(app),
                PaletteCommand::ToggleConsole => app.toggle_console(),
                PaletteCommand::ToggleInspector => app.toggle_inspector(),
                PaletteCommand::TogglePlugins => app.toggle_plugins(),
                PaletteCommand::Undo => crate::app::apply_command_effect(ctx, app, CommandEffect::UndoActiveFile),
                PaletteCommand::Redo => crate::app::apply_command_effect(ctx, app, CommandEffect::RedoActiveFile),
                PaletteCommand::Paste => crate::app::paste_active_file(app, false),
                PaletteCommand::PasteAsHex => crate::app::paste_active_file(app, true),
                PaletteCommand::SplitRight => {
                    crate::app::apply_command_effect(ctx, app, CommandEffect::DockSplit(DockDir::Right))
                }
                PaletteCommand::SplitLeft => {
                    crate::app::apply_command_effect(ctx, app, CommandEffect::DockSplit(DockDir::Left))
                }
                PaletteCommand::SplitUp => {
                    crate::app::apply_command_effect(ctx, app, CommandEffect::DockSplit(DockDir::Up))
                }
                PaletteCommand::SplitDown => {
                    crate::app::apply_command_effect(ctx, app, CommandEffect::DockSplit(DockDir::Down))
                }
                PaletteCommand::MergeRight => {
                    crate::app::apply_command_effect(ctx, app, CommandEffect::DockMerge(DockDir::Right))
                }
                PaletteCommand::MergeLeft => {
                    crate::app::apply_command_effect(ctx, app, CommandEffect::DockMerge(DockDir::Left))
                }
                PaletteCommand::MergeUp => {
                    crate::app::apply_command_effect(ctx, app, CommandEffect::DockMerge(DockDir::Up))
                }
                PaletteCommand::MergeDown => {
                    crate::app::apply_command_effect(ctx, app, CommandEffect::DockMerge(DockDir::Down))
                }
                PaletteCommand::MoveTabRight => {
                    crate::app::apply_command_effect(ctx, app, CommandEffect::DockMoveTab(DockDir::Right))
                }
                PaletteCommand::MoveTabLeft => {
                    crate::app::apply_command_effect(ctx, app, CommandEffect::DockMoveTab(DockDir::Left))
                }
                PaletteCommand::MoveTabUp => {
                    crate::app::apply_command_effect(ctx, app, CommandEffect::DockMoveTab(DockDir::Up))
                }
                PaletteCommand::MoveTabDown => {
                    crate::app::apply_command_effect(ctx, app, CommandEffect::DockMoveTab(DockDir::Down))
                }
                PaletteCommand::MoveTabVisual => {
                    crate::app::start_pane_pick(app, crate::tabs::pane_pick::PaneOp::MoveTab)
                }
                PaletteCommand::MergeVisual => crate::app::start_pane_pick(app, crate::tabs::pane_pick::PaneOp::Merge),
                PaletteCommand::FocusPane => crate::app::start_pane_focus(app),
                PaletteCommand::ToggleVim => crate::app::toggle_vim_mode(app),
                PaletteCommand::ToggleEditMode => crate::app::toggle_active_edit_mode(app),
                PaletteCommand::CopyCaretOffset => copy_formatted_offset(ctx, app, OffsetCopy::Caret),
                PaletteCommand::CopySelectionRange => copy_formatted_offset(ctx, app, OffsetCopy::SelectionRange),
                PaletteCommand::CopySelectionLength => copy_formatted_offset(ctx, app, OffsetCopy::SelectionLength),
                PaletteCommand::CopyFileLength => copy_formatted_offset(ctx, app, OffsetCopy::FileLength),
                PaletteCommand::CompareFiles => crate::compare::picker::start_compare_palette_flow(app),
                PaletteCommand::CompareFilesDialog => crate::compare::picker::start_compare_picker(app),
                PaletteCommand::JumpNextField => crate::app::jump_to_template_field(app, true),
                PaletteCommand::JumpPrevField => crate::app::jump_to_template_field(app, false),
                PaletteCommand::ReloadActiveFile => crate::app::request_reload_active_file(app),
                PaletteCommand::TakeSnapshot => crate::app::take_snapshot_active_file(app),
                PaletteCommand::OpenSnapshots => crate::app::open_snapshots_active_file(app),
                PaletteCommand::WatchAlways => {
                    crate::app::set_active_file_watch_pref(app, crate::settings::AutoReloadMode::Always);
                }
                PaletteCommand::WatchAsk => {
                    crate::app::set_active_file_watch_pref(app, crate::settings::AutoReloadMode::Ask);
                }
                PaletteCommand::WatchNever => {
                    crate::app::set_active_file_watch_pref(app, crate::settings::AutoReloadMode::Never);
                }
            }
        }
        Action::FocusFile(id) => {
            app.palette.close();
            app.focus_file_tab(id);
        }
        Action::RunTemplate(path) => {
            app.palette.close();
            if let Some(id) = crate::app::active_file_id(app) {
                crate::templates::runner::run_template_from_path(ctx, app, id, path);
            }
        }
        Action::SwitchMode(mode) => {
            app.palette.open_at(mode);
        }
        Action::InstallTemplate => {
            app.palette.close();
            crate::app::install_template_from_dialog(app);
        }
        Action::UninstallTemplate(path) => {
            app.palette.close();
            crate::app::uninstall_template(app, &path);
        }
        Action::UninstallPlugin(path) => {
            app.palette.close();
            crate::app::uninstall_plugin(app, &path);
        }
        Action::Copy(kind) => {
            app.palette.close();
            if let Some(id) = crate::app::active_file_id(app)
                && let Some(file) = app.files.get(&id)
            {
                crate::app::do_copy(ctx, file, kind);
            }
        }
        Action::OpenRecent(path) => {
            app.palette.close();
            crate::app::apply_command_effect(ctx, app, CommandEffect::OpenRecent(path));
        }
        Action::GoToOffset(target) => {
            app.palette.close();
            if let Some(id) = crate::app::active_file_id(app)
                && let Some(file) = app.files.get_mut(&id)
            {
                let max = file.editor.source().len().get().saturating_sub(1);
                let clamped = hxy_core::ByteOffset::new(target.min(max));
                file.editor.set_selection(Some(hxy_core::Selection::caret(clamped)));
                if !file.editor.is_offset_visible(clamped) {
                    file.editor.set_scroll_to_byte(clamped);
                }
            }
        }
        Action::SetSelection { start, end_exclusive } => {
            app.palette.close();
            if let Some(id) = crate::app::active_file_id(app)
                && let Some(file) = app.files.get_mut(&id)
            {
                let source_len = file.editor.source().len().get();
                if source_len == 0 || end_exclusive <= start {
                    return;
                }
                let last = end_exclusive.saturating_sub(1).min(source_len.saturating_sub(1));
                let anchor = hxy_core::ByteOffset::new(start.min(source_len.saturating_sub(1)));
                file.editor
                    .set_selection(Some(hxy_core::Selection { anchor, cursor: hxy_core::ByteOffset::new(last) }));
                if !file.editor.is_offset_visible(anchor) {
                    file.editor.set_scroll_to_byte(anchor);
                }
            }
        }
        Action::SetColumns { scope, count } => {
            app.palette.close();
            match scope {
                ColumnScope::Local => {
                    if let Some(id) = crate::app::active_file_id(app)
                        && let Some(file) = app.files.get_mut(&id)
                    {
                        file.hex_columns_override = Some(count);
                    }
                }
                ColumnScope::Global => {
                    app.state.write().app.hex_columns = count;
                }
            }
        }
        Action::InvokePluginCommand { plugin_name, command_id } => {
            let Some(plugin) = app.plugin_handlers.iter().find(|p| p.name() == plugin_name).cloned() else {
                tracing::warn!(plugin = %plugin_name, command = %command_id, "plugin invoke target missing");
                app.palette.close();
                return;
            };
            app.palette.close();
            let repaint = ctx.clone();
            let mut ops = std::mem::take(&mut app.pending_plugin_ops);
            crate::plugins::runner::spawn_invoke(&mut ops, app, repaint, plugin, plugin_name, command_id);
            app.pending_plugin_ops = ops;
        }
        Action::RespondToPlugin { plugin_name, command_id, answer } => {
            let Some(plugin) = app.plugin_handlers.iter().find(|p| p.name() == plugin_name).cloned() else {
                tracing::warn!(plugin = %plugin_name, command = %command_id, "plugin respond target missing");
                app.palette.close();
                return;
            };
            app.palette.close();
            let repaint = ctx.clone();
            let mut ops = std::mem::take(&mut app.pending_plugin_ops);
            crate::plugins::runner::spawn_respond(&mut ops, app, repaint, plugin, plugin_name, command_id, answer);
            app.pending_plugin_ops = ops;
        }
        Action::NoOp => {
            // Placeholder rows (e.g. "Invalid: ..." in the Go-To
            // cascade) pick to this. Close the palette so repeated
            // Enter presses don't get the user stuck on an inert
            // row, but don't dispatch any other effect.
            app.palette.close();
        }
        Action::CompareSelectSource { side, source } => match side {
            CompareSide::A => {
                app.palette.compare_pick = Some(ComparePickState { picked_a: Some(source) });
                app.palette.open_at(Mode::CompareSideB);
            }
            CompareSide::B => {
                let Some(pick) = app.palette.compare_pick.take() else {
                    app.palette.close();
                    return;
                };
                let Some(a) = pick.picked_a else {
                    app.palette.close();
                    return;
                };
                app.palette.close();
                crate::compare::picker::spawn_compare_from_palette(app, ctx, a, source);
            }
        },
        Action::SetPollInterval(ms) => {
            app.palette.close();
            let clamped = clamp_poll_interval_ms(ms);
            app.state.write().app.file_poll_interval_ms = clamped;
        }
        Action::CompareBrowse(side) => {
            let Some(path) = rfd::FileDialog::new().pick_file() else {
                return;
            };
            let source = TabSource::Filesystem(path);
            match side {
                CompareSide::A => {
                    app.palette.compare_pick = Some(ComparePickState { picked_a: Some(source) });
                    app.palette.open_at(Mode::CompareSideB);
                }
                CompareSide::B => {
                    let Some(pick) = app.palette.compare_pick.take() else {
                        app.palette.close();
                        return;
                    };
                    let Some(a) = pick.picked_a else {
                        app.palette.close();
                        return;
                    };
                    app.palette.close();
                    crate::compare::picker::spawn_compare_from_palette(app, ctx, a, source);
                }
            }
        }
    }
}
