//! The big per-mode entry builder for the command palette.
//!
//! `build_palette_entries` walks `app.palette.mode` and emits the
//! list of `egui_palette::Entry`s the user sees. Each mode owns one
//! arm of the match. New palette modes / commands plug in here.

use hxy_vfs::TabSource;
use hxy_vfs::VfsHandler;

use crate::app::HxyApp;
use crate::commands::palette::Action;
use crate::commands::palette::Mode;
use crate::commands::palette::PaletteCommand;
use crate::commands::shortcuts::COPY_BYTES;
use crate::commands::shortcuts::COPY_HEX;
use crate::commands::shortcuts::FOCUS_PANE;
use crate::commands::shortcuts::NEW_FILE;
use crate::commands::shortcuts::PASTE;
use crate::commands::shortcuts::PASTE_AS_HEX;
use crate::commands::shortcuts::REDO;
use crate::commands::shortcuts::TOGGLE_EDIT_MODE;
use crate::commands::shortcuts::UNDO;
use crate::files::copy::CopyKind;
use crate::tabs::Tab;

use super::offset::OffsetPaletteContext;

/// Snapshot of the active selection used by the palette to decide
/// which `Copy as...` entries to expose. `None` when no file is
/// focused or the selection is empty.
#[derive(Clone, Copy)]
pub struct CopyPaletteContext {
    /// True when the selection width is a scalar integer width
    /// (1/2/4/8 bytes), meaning the `Copy value as...` options apply.
    pub scalar_width: bool,
}

#[derive(Clone, Copy, Default)]
pub struct HistoryPaletteContext {
    pub can_undo: bool,
    pub can_redo: bool,
    /// True when the active tab is mutable and would accept a paste.
    pub can_paste: bool,
    /// True when an active file tab exists, regardless of edit mode.
    /// Gates toggle-read-only and other tab-level actions.
    pub has_active_file: bool,
}

/// Snapshot of the active tab used for ranking `Run Template`
/// entries against its content. Empty when no file is active --
/// `rank_entries` falls through to the default ordering in that
/// case.
#[derive(Clone, Default)]
pub struct TemplatePaletteContext {
    pub extension: Option<String>,
    pub head_bytes: Vec<u8>,
}

pub fn copy_palette_context(app: &mut HxyApp) -> Option<CopyPaletteContext> {
    let id = crate::app::active_file_id(app)?;
    let file = app.files.get(&id)?;
    let sel = file.editor.selection()?;
    let range = sel.range();
    if range.is_empty() {
        return None;
    }
    Some(CopyPaletteContext { scalar_width: matches!(range.len().get(), 1 | 2 | 4 | 8) })
}

pub fn history_palette_context(app: &mut HxyApp) -> HistoryPaletteContext {
    let Some(id) = crate::app::active_file_id(app) else { return HistoryPaletteContext::default() };
    let Some(file) = app.files.get(&id) else { return HistoryPaletteContext::default() };
    HistoryPaletteContext {
        can_undo: file.editor.can_undo(),
        can_redo: file.editor.can_redo(),
        can_paste: file.editor.edit_mode() == crate::files::EditMode::Mutable,
        has_active_file: true,
    }
}

pub fn template_palette_context(app: &mut HxyApp) -> TemplatePaletteContext {
    let Some(id) = crate::app::active_file_id(app) else { return TemplatePaletteContext::default() };
    let Some(file) = app.files.get(&id) else { return TemplatePaletteContext::default() };
    let extension =
        file.root_path().and_then(|p| p.extension()).and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase());
    let source_len = file.editor.source().len().get();
    let window = source_len.min(crate::templates::library::DETECTION_WINDOW as u64);
    let head_bytes = if window == 0 {
        Vec::new()
    } else if let Ok(range) = hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(window))
    {
        file.editor.source().read(range).unwrap_or_default()
    } else {
        Vec::new()
    };
    TemplatePaletteContext { extension, head_bytes }
}

/// Push a non-actionable "Invalid: {reason}" row. Activating it
/// falls through to `apply_palette_action`'s existing Invalid arm
/// which just closes the palette -- keeps a visible indication
/// that the query isn't parseable without silently showing an
/// empty list.
pub fn invalid_entry(out: &mut Vec<egui_palette::Entry<Action>>, query: &str, reason: &str) {
    use egui_phosphor::regular as icon;

    out.push(
        egui_palette::Entry::new(hxy_i18n::t_args("palette-invalid-fmt", &[("reason", reason)]), Action::NoOp)
            .with_icon(icon::WARNING)
            .with_subtitle(query.to_owned()),
    );
}

pub fn build_palette_entries(
    ctx: &egui::Context,
    app: &HxyApp,
    copy_ctx: Option<CopyPaletteContext>,
    history_ctx: HistoryPaletteContext,
    template_ctx: &TemplatePaletteContext,
    offset_ctx: &OffsetPaletteContext,
) -> Vec<egui_palette::Entry<Action>> {
    use egui_phosphor::regular as icon;

    let fmt = |sc: &egui::KeyboardShortcut| ctx.format_shortcut(sc);
    let mut out: Vec<egui_palette::Entry<Action>> = Vec::new();
    match app.palette.mode {
        Mode::Main => {
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("menu-file-new"),
                    Action::InvokeCommand(PaletteCommand::NewFile),
                )
                .with_icon(icon::FILE_PLUS)
                .with_shortcut(fmt(&NEW_FILE)),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("toolbar-open-file"),
                    Action::InvokeCommand(PaletteCommand::OpenFile),
                )
                .with_icon(icon::FOLDER_OPEN),
            );
            if !app.state.read().app.recent_files.is_empty() {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-open-recent-entry"),
                        Action::SwitchMode(Mode::Recent),
                    )
                    .with_icon(icon::CLOCK_COUNTER_CLOCKWISE),
                );
            }
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("toolbar-browse-vfs"),
                    Action::InvokeCommand(PaletteCommand::BrowseVfs),
                )
                .with_icon(icon::TREE_STRUCTURE),
            );
            let panel_subtitle = |visible: bool| -> String {
                hxy_i18n::t(if visible { "palette-subtitle-hide" } else { "palette-subtitle-show" })
            };
            let console_visible = app.dock.find_tab(&Tab::Console).is_some();
            let inspector_visible = app.dock.find_tab(&Tab::Inspector).is_some();
            let plugins_visible = app.dock.find_tab(&Tab::Plugins).is_some();
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("menu-view-console"),
                    Action::InvokeCommand(PaletteCommand::ToggleConsole),
                )
                .with_icon(icon::TERMINAL)
                .with_subtitle(panel_subtitle(console_visible)),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("menu-view-inspector"),
                    Action::InvokeCommand(PaletteCommand::ToggleInspector),
                )
                .with_icon(icon::EYE)
                .with_subtitle(panel_subtitle(inspector_visible)),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("menu-view-plugins"),
                    Action::InvokeCommand(PaletteCommand::TogglePlugins),
                )
                .with_icon(icon::PUZZLE_PIECE)
                .with_subtitle(panel_subtitle(plugins_visible)),
            );

            let workspace_tree_visible = app
                .dock
                .focused_leaf()
                .and_then(|p| app.dock.leaf(p).ok())
                .and_then(|leaf| leaf.tabs().get(leaf.active.0))
                .and_then(|tab| match tab {
                    Tab::Workspace(workspace_id) => app
                        .workspaces
                        .get(workspace_id)
                        .map(|w| w.dock.find_tab(&crate::files::WorkspaceTab::VfsTree).is_some()),
                    _ => None,
                })
                .unwrap_or(false);
            out.push(
                egui_palette::Entry::new(
                    "Toggle VFS panel",
                    Action::InvokeCommand(PaletteCommand::ToggleWorkspaceVfs),
                )
                .with_icon(icon::TREE_STRUCTURE)
                .with_subtitle(panel_subtitle(workspace_tree_visible)),
            );

            let tool_panel_visible =
                app.hidden_tool_tabs.is_empty() && app.dock.iter_all_tabs().any(|(_, t)| crate::tabs::dock_ops::is_tool_tab(t));
            out.push(
                egui_palette::Entry::new(
                    "Toggle tool panel",
                    Action::InvokeCommand(PaletteCommand::ToggleToolPanel),
                )
                .with_icon(icon::SQUARES_FOUR)
                .with_subtitle(panel_subtitle(tool_panel_visible)),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-run-template-entry"),
                    Action::SwitchMode(Mode::Templates),
                )
                .with_icon(icon::SCROLL),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-uninstall-template"),
                    Action::SwitchMode(Mode::Uninstall),
                )
                .with_icon(icon::TRASH),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-uninstall-plugin"),
                    Action::SwitchMode(Mode::UninstallPlugin),
                )
                .with_subtitle(hxy_i18n::t("palette-delete-plugin-subtitle"))
                .with_icon(icon::TRASH),
            );
            if history_ctx.has_active_file {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-go-to-offset-entry"),
                        Action::SwitchMode(Mode::GoToOffset),
                    )
                    .with_icon(icon::CROSSHAIR),
                );
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-select-from-offset-entry"),
                        Action::SwitchMode(Mode::SelectFromOffset),
                    )
                    .with_icon(icon::ARROWS_OUT_LINE_HORIZONTAL),
                );
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-select-range-entry"),
                        Action::SwitchMode(Mode::SelectRange),
                    )
                    .with_icon(icon::BRACKETS_CURLY),
                );
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-set-columns-local-entry"),
                        Action::SwitchMode(Mode::SetColumnsLocal),
                    )
                    .with_icon(icon::COLUMNS),
                );
            }
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-set-columns-global-entry"),
                    Action::SwitchMode(Mode::SetColumnsGlobal),
                )
                .with_icon(icon::COLUMNS_PLUS_RIGHT),
            );
            if history_ctx.can_undo {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("menu-edit-undo"),
                        Action::InvokeCommand(PaletteCommand::Undo),
                    )
                    .with_icon(icon::ARROW_COUNTER_CLOCKWISE)
                    .with_shortcut(fmt(&UNDO)),
                );
            }
            if history_ctx.can_redo {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("menu-edit-redo"),
                        Action::InvokeCommand(PaletteCommand::Redo),
                    )
                    .with_icon(icon::ARROW_CLOCKWISE)
                    .with_shortcut(fmt(&REDO)),
                );
            }
            if history_ctx.has_active_file {
                let (result_key, toggle_icon) = if history_ctx.can_paste {
                    ("palette-toggle-readonly-result-readonly", icon::LOCK)
                } else {
                    ("palette-toggle-readonly-result-mutable", icon::LOCK_OPEN)
                };
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-toggle-readonly"),
                        Action::InvokeCommand(PaletteCommand::ToggleEditMode),
                    )
                    .with_subtitle(hxy_i18n::t(result_key))
                    .with_icon(toggle_icon)
                    .with_shortcut(fmt(&TOGGLE_EDIT_MODE)),
                );
            }
            if history_ctx.can_paste {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("menu-edit-paste"),
                        Action::InvokeCommand(PaletteCommand::Paste),
                    )
                    .with_icon(icon::CLIPBOARD_TEXT)
                    .with_shortcut(fmt(&PASTE)),
                );
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("menu-edit-paste-as-hex"),
                        Action::InvokeCommand(PaletteCommand::PasteAsHex),
                    )
                    .with_icon(icon::CLIPBOARD_TEXT)
                    .with_shortcut(fmt(&PASTE_AS_HEX)),
                );
            }
            if history_ctx.has_active_file {
                let base = app.state.read().app.offset_base;
                if let Some((start, end_exclusive)) = offset_ctx.selection {
                    let last_inclusive = end_exclusive.saturating_sub(1);
                    let len = end_exclusive.saturating_sub(start);
                    let caret_preview = crate::app::format_offset(offset_ctx.cursor, base);
                    out.push(
                        egui_palette::Entry::new(
                            hxy_i18n::t("palette-copy-caret-offset"),
                            Action::InvokeCommand(PaletteCommand::CopyCaretOffset),
                        )
                        .with_icon(icon::COPY)
                        .with_subtitle(caret_preview),
                    );
                    if len > 1 {
                        let len_preview = crate::app::format_offset(len, base);
                        let range_preview = format!(
                            "{}-{} ({} bytes)",
                            crate::app::format_offset(start, base),
                            crate::app::format_offset(last_inclusive, base),
                            len_preview,
                        );
                        out.push(
                            egui_palette::Entry::new(
                                hxy_i18n::t("palette-copy-selection-range"),
                                Action::InvokeCommand(PaletteCommand::CopySelectionRange),
                            )
                            .with_icon(icon::COPY)
                            .with_subtitle(range_preview),
                        );
                        out.push(
                            egui_palette::Entry::new(
                                hxy_i18n::t("palette-copy-selection-length"),
                                Action::InvokeCommand(PaletteCommand::CopySelectionLength),
                            )
                            .with_icon(icon::COPY)
                            .with_subtitle(len_preview),
                        );
                    }
                }
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-copy-file-length"),
                        Action::InvokeCommand(PaletteCommand::CopyFileLength),
                    )
                    .with_icon(icon::COPY)
                    .with_subtitle(crate::app::format_offset(offset_ctx.source_len, base)),
                );
            }
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-compare-files"),
                    Action::InvokeCommand(PaletteCommand::CompareFiles),
                )
                .with_icon(icon::COLUMNS)
                .with_subtitle(hxy_i18n::t("palette-compare-files-subtitle")),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-compare-files-dialog"),
                    Action::InvokeCommand(PaletteCommand::CompareFilesDialog),
                )
                .with_icon(icon::COLUMNS)
                .with_subtitle(hxy_i18n::t("palette-compare-files-dialog-subtitle")),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-split-right"),
                    Action::InvokeCommand(PaletteCommand::SplitRight),
                )
                .with_icon(icon::ARROW_SQUARE_RIGHT),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-split-left"),
                    Action::InvokeCommand(PaletteCommand::SplitLeft),
                )
                .with_icon(icon::ARROW_SQUARE_LEFT),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-split-down"),
                    Action::InvokeCommand(PaletteCommand::SplitDown),
                )
                .with_icon(icon::ARROW_SQUARE_DOWN),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-split-up"),
                    Action::InvokeCommand(PaletteCommand::SplitUp),
                )
                .with_icon(icon::ARROW_SQUARE_UP),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-merge-right"),
                    Action::InvokeCommand(PaletteCommand::MergeRight),
                )
                .with_icon(icon::ARROW_LINE_RIGHT),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-merge-left"),
                    Action::InvokeCommand(PaletteCommand::MergeLeft),
                )
                .with_icon(icon::ARROW_LINE_LEFT),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-merge-down"),
                    Action::InvokeCommand(PaletteCommand::MergeDown),
                )
                .with_icon(icon::ARROW_LINE_DOWN),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-merge-up"),
                    Action::InvokeCommand(PaletteCommand::MergeUp),
                )
                .with_icon(icon::ARROW_LINE_UP),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-move-tab-right"),
                    Action::InvokeCommand(PaletteCommand::MoveTabRight),
                )
                .with_icon(icon::ARROW_FAT_RIGHT),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-move-tab-left"),
                    Action::InvokeCommand(PaletteCommand::MoveTabLeft),
                )
                .with_icon(icon::ARROW_FAT_LEFT),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-move-tab-down"),
                    Action::InvokeCommand(PaletteCommand::MoveTabDown),
                )
                .with_icon(icon::ARROW_FAT_DOWN),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-move-tab-up"),
                    Action::InvokeCommand(PaletteCommand::MoveTabUp),
                )
                .with_icon(icon::ARROW_FAT_UP),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-move-tab-visual"),
                    Action::InvokeCommand(PaletteCommand::MoveTabVisual),
                )
                .with_icon(icon::CROSSHAIR_SIMPLE)
                .with_subtitle(hxy_i18n::t("palette-pane-pick-subtitle")),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-merge-visual"),
                    Action::InvokeCommand(PaletteCommand::MergeVisual),
                )
                .with_icon(icon::CROSSHAIR_SIMPLE)
                .with_subtitle(hxy_i18n::t("palette-pane-pick-subtitle")),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-focus-pane"),
                    Action::InvokeCommand(PaletteCommand::FocusPane),
                )
                .with_icon(icon::CROSSHAIR_SIMPLE)
                .with_subtitle(hxy_i18n::t("palette-pane-pick-subtitle"))
                .with_shortcut(fmt(&FOCUS_PANE)),
            );
            let vim_active = matches!(app.state.read().app.input_mode, hxy_view::InputMode::Vim);
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-toggle-vim"),
                    Action::InvokeCommand(PaletteCommand::ToggleVim),
                )
                .with_icon(icon::KEYBOARD)
                .with_subtitle(hxy_i18n::t(if vim_active {
                    "palette-toggle-vim-subtitle-on"
                } else {
                    "palette-toggle-vim-subtitle-off"
                })),
            );
            if let Some(copy) = copy_ctx {
                for (label, kind) in crate::files::copy::BYTES_MENU {
                    let mut entry = egui_palette::Entry::new(format!("Copy bytes: {label}"), Action::Copy(*kind))
                        .with_icon(icon::COPY);
                    if matches!(kind, CopyKind::BytesLossyUtf8) {
                        entry = entry.with_shortcut(fmt(&COPY_BYTES));
                    } else if matches!(kind, CopyKind::BytesHexSpaced) {
                        entry = entry.with_shortcut(fmt(&COPY_HEX));
                    }
                    out.push(entry);
                }
                if copy.scalar_width {
                    for (label, kind) in crate::files::copy::VALUE_MENU {
                        out.push(
                            egui_palette::Entry::new(format!("Copy value: {label}"), Action::Copy(*kind))
                                .with_icon(icon::COPY),
                        );
                    }
                }
            }
            for plugin in &app.plugin_handlers {
                let plugin_name = plugin.name().to_owned();
                for cmd in plugin.list_commands() {
                    let mut entry = egui_palette::Entry::new(
                        format!("{plugin_name}: {}", cmd.label),
                        Action::InvokePluginCommand { plugin_name: plugin_name.clone(), command_id: cmd.id },
                    );
                    if let Some(s) = cmd.subtitle {
                        entry = entry.with_subtitle(s);
                    }
                    entry = entry.with_icon(cmd.icon.unwrap_or_else(|| icon::PUZZLE_PIECE.to_string()));
                    out.push(entry);
                }
            }
        }
        Mode::QuickOpen => {
            for (id, file) in &app.files {
                let mut entry =
                    egui_palette::Entry::new(file.display_name.clone(), Action::FocusFile(*id)).with_icon(icon::FILE);
                if let Some(parent) = file.root_path().and_then(|p| p.parent()) {
                    entry = entry.with_subtitle(parent.display().to_string());
                }
                out.push(entry);
            }
            let open_paths: std::collections::HashSet<std::path::PathBuf> =
                app.files.values().filter_map(|f| f.root_path().cloned()).collect();
            for recent in &app.state.read().app.recent_files {
                if open_paths.contains(&recent.path) {
                    continue;
                }
                let name = recent
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| recent.path.display().to_string());
                let mut entry = egui_palette::Entry::new(name, Action::OpenRecent(recent.path.clone()))
                    .with_icon(icon::CLOCK_COUNTER_CLOCKWISE);
                if let Some(parent) = recent.path.parent() {
                    entry = entry.with_subtitle(parent.display().to_string());
                }
                out.push(entry);
            }
        }
        Mode::Templates => {
            let ranked = app.templates.rank_entries(template_ctx.extension.as_deref(), &template_ctx.head_bytes);
            for entry in ranked {
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t_args("palette-run-template-fmt", &[("name", &entry.name)]),
                        Action::RunTemplate(entry.path.clone()),
                    )
                    .with_subtitle(entry.path.display().to_string())
                    .with_icon(icon::SCROLL),
                );
            }
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-install-template"), Action::InstallTemplate)
                    .with_subtitle(hxy_i18n::t("palette-install-template-subtitle"))
                    .with_icon(icon::DOWNLOAD),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-uninstall-template"),
                    Action::SwitchMode(Mode::Uninstall),
                )
                .with_icon(icon::TRASH),
            );
        }
        Mode::Uninstall => {
            if let Some(dir) = crate::app::user_templates_dir() {
                for path in crate::templates::library::list_installed_templates(&dir) {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    out.push(
                        egui_palette::Entry::new(
                            hxy_i18n::t_args("palette-delete-template-fmt", &[("name", &name)]),
                            Action::UninstallTemplate(path.clone()),
                        )
                        .with_subtitle(path.display().to_string())
                        .with_icon(icon::TRASH),
                    );
                }
            }
        }
        Mode::UninstallPlugin => {
            for dir in [crate::app::user_plugins_dir(), crate::app::user_template_plugins_dir()].into_iter().flatten()
            {
                let Ok(read) = std::fs::read_dir(&dir) else { continue };
                let mut wasms: Vec<std::path::PathBuf> = read
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wasm"))
                    .collect();
                wasms.sort();
                for path in wasms {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    out.push(
                        egui_palette::Entry::new(
                            hxy_i18n::t_args("palette-delete-plugin-fmt", &[("name", &name)]),
                            Action::UninstallPlugin(path.clone()),
                        )
                        .with_subtitle(path.display().to_string())
                        .with_icon(icon::TRASH),
                    );
                }
            }
        }
        Mode::Recent => {
            let open_paths: std::collections::HashSet<std::path::PathBuf> =
                app.files.values().filter_map(|f| f.root_path().cloned()).collect();
            for recent in &app.state.read().app.recent_files {
                if open_paths.contains(&recent.path) {
                    continue;
                }
                let name = recent
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| recent.path.display().to_string());
                let mut entry = egui_palette::Entry::new(name, Action::OpenRecent(recent.path.clone()))
                    .with_icon(icon::CLOCK_COUNTER_CLOCKWISE);
                if let Some(parent) = recent.path.parent() {
                    entry = entry.with_subtitle(parent.display().to_string());
                }
                out.push(entry);
            }
        }
        Mode::CompareSideA | Mode::CompareSideB => {
            let side = if matches!(app.palette.mode, Mode::CompareSideA) {
                crate::commands::palette::CompareSide::A
            } else {
                crate::commands::palette::CompareSide::B
            };
            let picked_a = app.palette.compare_pick.as_ref().and_then(|p| p.picked_a.clone());
            for file in app.files.values() {
                let Some(source) = file.source_kind.clone() else {
                    continue;
                };
                if matches!(side, crate::commands::palette::CompareSide::B)
                    && picked_a.as_ref().is_some_and(|a| a == &source)
                {
                    continue;
                }
                let mut entry =
                    egui_palette::Entry::new(file.display_name.clone(), Action::CompareSelectSource { side, source })
                        .with_icon(icon::FILE);
                if let Some(parent) = file.root_path().and_then(|p| p.parent()) {
                    entry = entry.with_subtitle(parent.display().to_string());
                }
                out.push(entry);
            }
            let recent_mode = match side {
                crate::commands::palette::CompareSide::A => Mode::CompareSideARecent,
                crate::commands::palette::CompareSide::B => Mode::CompareSideBRecent,
            };
            if !app.state.read().app.recent_files.is_empty() {
                out.push(
                    egui_palette::Entry::new(hxy_i18n::t("palette-open-recent-entry"), Action::SwitchMode(recent_mode))
                        .with_icon(icon::CLOCK_COUNTER_CLOCKWISE),
                );
            }
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("compare-picker-browse"), Action::CompareBrowse(side))
                    .with_icon(icon::FOLDER_OPEN),
            );
        }
        Mode::CompareSideARecent | Mode::CompareSideBRecent => {
            let side = if matches!(app.palette.mode, Mode::CompareSideARecent) {
                crate::commands::palette::CompareSide::A
            } else {
                crate::commands::palette::CompareSide::B
            };
            let picked_a_path =
                app.palette.compare_pick.as_ref().and_then(|p| p.picked_a.as_ref()).and_then(|s| match s {
                    TabSource::Filesystem(p) => Some(p.clone()),
                    _ => None,
                });
            let open_paths: std::collections::HashSet<std::path::PathBuf> =
                app.files.values().filter_map(|f| f.root_path().cloned()).collect();
            for recent in &app.state.read().app.recent_files {
                if open_paths.contains(&recent.path) {
                    continue;
                }
                if matches!(side, crate::commands::palette::CompareSide::B)
                    && picked_a_path.as_ref().is_some_and(|a| a == &recent.path)
                {
                    continue;
                }
                let name = recent
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| recent.path.display().to_string());
                let mut entry = egui_palette::Entry::new(
                    name,
                    Action::CompareSelectSource { side, source: TabSource::Filesystem(recent.path.clone()) },
                )
                .with_icon(icon::CLOCK_COUNTER_CLOCKWISE);
                if let Some(parent) = recent.path.parent() {
                    entry = entry.with_subtitle(parent.display().to_string());
                }
                out.push(entry);
            }
        }
        Mode::GoToOffset | Mode::SelectFromOffset | Mode::SelectRange => {
            let query = app.palette.inner.query.trim();
            if !offset_ctx.available {
                invalid_entry(&mut out, query, &hxy_i18n::t("palette-invalid-no-active-file"));
            } else {
                super::offset::build_offset_entries(&mut out, app.palette.mode, query, offset_ctx);
            }
        }
        Mode::SetColumnsLocal | Mode::SetColumnsGlobal => {
            let query = app.palette.inner.query.trim();
            if matches!(app.palette.mode, Mode::SetColumnsLocal) && !offset_ctx.available {
                invalid_entry(&mut out, query, &hxy_i18n::t("palette-invalid-no-active-file"));
            } else {
                super::columns::build_columns_entries(&mut out, app.palette.mode, query);
            }
        }
        Mode::PluginCascade => {
            if let Some(cascade) = app.palette.plugin_cascade.as_ref() {
                let plugin_name = &cascade.plugin_name;
                for cmd in &cascade.commands {
                    let mut entry = egui_palette::Entry::new(
                        cmd.label.clone(),
                        Action::InvokePluginCommand { plugin_name: plugin_name.clone(), command_id: cmd.id.clone() },
                    );
                    if let Some(s) = cmd.subtitle.clone() {
                        entry = entry.with_subtitle(s);
                    }
                    entry = entry.with_icon(cmd.icon.clone().unwrap_or_else(|| icon::PUZZLE_PIECE.to_string()));
                    out.push(entry);
                }
            }
        }
        Mode::PluginPrompt => {
            if let Some(prompt) = app.palette.plugin_prompt.as_ref() {
                let answer = app.palette.inner.query.clone();
                let label = if answer.is_empty() { hxy_i18n::t("palette-plugin-prompt-empty") } else { answer.clone() };
                let mut entry = egui_palette::Entry::new(
                    label,
                    Action::RespondToPlugin {
                        plugin_name: prompt.plugin_name.clone(),
                        command_id: prompt.command_id.clone(),
                        answer,
                    },
                )
                .with_icon(icon::ARROW_BEND_DOWN_LEFT);
                entry = entry.with_subtitle(prompt.title.clone());
                out.push(entry);
            }
        }
    }
    out
}
