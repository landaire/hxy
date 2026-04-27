//! Tab-close flow: dirty-check modal, single-tab + workspace
//! teardown, cmd+W shortcut dispatch.

#![cfg(not(target_arch = "wasm32"))]

use hxy_vfs::TabSource;

use crate::app::HxyApp;
use crate::commands::shortcuts::CLOSE_TAB;
use crate::files::FileId;
use crate::files::OpenFile;
use crate::state::PersistedState;
use crate::tabs::Tab;

/// One tab the user has asked to close, gated on its dirty buffer.
/// Carries enough metadata to render the prompt without re-reading
/// `app.files` (which would force the modal to re-borrow during
/// rendering).
#[derive(Clone, Debug)]
pub struct PendingCloseTab {
    pub file_id: FileId,
    pub display_name: String,
}

enum CloseTabAction {
    Save,
    Discard,
    Cancel,
}

/// Drop a single file tab (Tab::File or workspace entry) by id, free
/// its `OpenFile`, and clear the matching persisted `OpenTabState`
/// so the tab doesn't reappear on next launch. Callers responsible
/// for gating on dirtiness -- this helper is the unconditional path
/// the modal's "Don't Save" branch uses.
pub fn close_file_tab_by_id(app: &mut HxyApp, id: FileId) {
    if let Some(path) = app.dock.find_tab(&Tab::File(id)) {
        let _ = app.dock.remove_tab(path);
    }
    for workspace in app.workspaces.values_mut() {
        if let Some(path) = workspace.dock.find_tab(&crate::files::WorkspaceTab::Entry(id)) {
            let _ = workspace.dock.remove_tab(path);
            break;
        }
    }
    if let Some(removed) = app.files.remove(&id)
        && let Some(source) = removed.source_kind
    {
        let mut state = app.state.write();
        state.open_tabs.retain(|t| t.source != source);
    }
    if app.last_active_file == Some(id) {
        app.last_active_file = None;
    }
    app.toasts.dismiss_for_file(id);
}

/// Cmd+W entry point. Closes the currently focused tab. For File
/// tabs the dirty-check is the same one `on_close` uses: when the
/// editor has uncommitted edits the modal is staged instead of
/// dropping. Non-File tabs (Console, Inspector, Plugins, ...)
/// close immediately -- they have no save state.
pub fn request_close_active_tab(app: &mut HxyApp) {
    let Some((_, tab)) = app.dock.find_active_focused() else { return };
    let tab = *tab;
    match tab {
        Tab::File(id) => {
            if let Some(file) = app.files.get(&id)
                && file.editor.is_dirty()
            {
                app.pending_close_tab = Some(PendingCloseTab { file_id: id, display_name: file.display_name.clone() });
                return;
            }
            close_file_tab_by_id(app, id);
        }
        Tab::Welcome | Tab::Settings => {
            // Non-closeable in the TabViewer; Cmd+W matches.
        }
        Tab::Console | Tab::Inspector | Tab::Plugins => {
            if let Some(path) = app.dock.find_tab(&tab) {
                let _ = app.dock.remove_tab(path);
            }
        }
        Tab::PluginMount(mount_id) => {
            if let Some(path) = app.dock.find_tab(&tab) {
                let _ = app.dock.remove_tab(path);
            }
            if let Some(removed) = app.mounts.remove(&mount_id) {
                let target = TabSource::PluginMount {
                    plugin_name: removed.plugin_name,
                    token: removed.token,
                    title: removed.display_name,
                };
                app.state.write().open_tabs.retain(|t| t.source != target);
            }
        }
        Tab::SearchResults => {
            if let Some(path) = app.dock.find_tab(&tab) {
                let _ = app.dock.remove_tab(path);
            }
            app.global_search.open = false;
        }
        Tab::Workspace(workspace_id) => {
            close_workspace_by_id(app, workspace_id);
        }
        Tab::Compare(compare_id) => {
            if let Some(path) = app.dock.find_tab(&tab) {
                let _ = app.dock.remove_tab(path);
            }
            app.compares.remove(&compare_id);
        }
    }
}

/// Collapse a workspace whose inner dock has been emptied of
/// everything except the Editor sub-tab back to a plain `Tab::File`
/// in the outer dock. The workspace entry is dropped from
/// `app.workspaces` and the persisted `as_workspace` flag is cleared.
pub fn collapse_workspace_to_file(app: &mut HxyApp, workspace_id: crate::files::WorkspaceId) {
    let Some(workspace) = app.workspaces.remove(&workspace_id) else { return };
    if app.last_active_workspace == Some(workspace_id) {
        app.last_active_workspace = None;
    }
    let editor_id = workspace.editor_id;

    if let Some(path) = app.dock.find_tab(&Tab::Workspace(workspace_id)) {
        let _ = app.dock.remove_tab(path);
    }
    app.dock.push_to_focused_leaf(Tab::File(editor_id));
    if let Some(path) = app.dock.find_tab(&Tab::File(editor_id)) {
        let _ = app.dock.set_active_tab(path);
    }

    if let Some(source) = app.files.get(&editor_id).and_then(|f| f.source_kind.clone()) {
        let mut g = app.state.write();
        if let Some(entry) = g.open_tabs.iter_mut().find(|t| t.source == source) {
            entry.as_workspace = false;
        }
    }
}

/// Close the entire `Tab::Workspace(workspace_id)` -- the editor
/// itself plus any open VFS entries inside the inner dock.
pub fn close_workspace_by_id(app: &mut HxyApp, workspace_id: crate::files::WorkspaceId) {
    let workspace = match app.workspaces.remove(&workspace_id) {
        Some(w) => w,
        None => return,
    };
    if app.last_active_workspace == Some(workspace_id) {
        app.last_active_workspace = None;
    }
    if let Some(path) = app.dock.find_tab(&Tab::Workspace(workspace_id)) {
        let _ = app.dock.remove_tab(path);
    }

    let mut to_drop: Vec<FileId> = vec![workspace.editor_id];
    for (_, t) in workspace.dock.iter_all_tabs() {
        if let crate::files::WorkspaceTab::Entry(file_id) = t {
            to_drop.push(*file_id);
        }
    }
    let mut state = app.state.write();
    for file_id in &to_drop {
        if let Some(removed) = app.files.remove(file_id)
            && let Some(source) = removed.source_kind
        {
            state.open_tabs.retain(|t| t.source != source);
        }
    }
}

/// Cmd+W shortcut dispatcher.
pub fn dispatch_close_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    if ctx.input_mut(|i| i.consume_shortcut(&CLOSE_TAB)) {
        request_close_active_tab(app);
    }
}

/// Render the "Save before closing?" modal when a close request
/// is staged in `pending_close_tab`. Three terminal actions: Save
/// -> save then close (only if save actually wrote bytes; a
/// cancelled save dialog leaves the tab open and the staged
/// request is cleared so the user starts fresh next press),
/// Don't Save -> close immediately, Cancel -> do nothing.
pub fn render_close_tab_dialog(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(pending) = app.pending_close_tab.as_ref().cloned() else { return };

    let mut action: Option<CloseTabAction> = None;
    let mut open = true;
    egui::Window::new(hxy_i18n::t("close-prompt-title"))
        .id(egui::Id::new("hxy_close_tab_dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(hxy_i18n::t_args("close-prompt-body", &[("name", &pending.display_name)]));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(hxy_i18n::t("close-prompt-save")).clicked() {
                    action = Some(CloseTabAction::Save);
                }
                if ui.button(hxy_i18n::t("close-prompt-discard")).clicked() {
                    action = Some(CloseTabAction::Discard);
                }
                if ui.button(hxy_i18n::t("close-prompt-cancel")).clicked() {
                    action = Some(CloseTabAction::Cancel);
                }
            });
        });
    if !open && action.is_none() {
        action = Some(CloseTabAction::Cancel);
    }

    let Some(action) = action else { return };
    app.pending_close_tab = None;
    match action {
        CloseTabAction::Save => {
            if crate::files::save::save_file_by_id(app, pending.file_id, false) {
                close_file_tab_by_id(app, pending.file_id);
            }
        }
        CloseTabAction::Discard => close_file_tab_by_id(app, pending.file_id),
        CloseTabAction::Cancel => {}
    }
}

/// Sync the current editor selection / scroll back into the
/// persisted [`crate::state::OpenTabState`] entry so the next
/// session restores the user's view.
pub fn sync_tab_state(state: &mut PersistedState, file: &OpenFile) {
    let Some(source) = &file.source_kind else { return };
    if let Some(entry) = state.open_tabs.iter_mut().find(|t| &t.source == source) {
        entry.selection = file.editor.selection();
        entry.scroll_offset = file.editor.scroll_offset();
    }
}
