//! Tab-close flow: dirty-check modal, single-tab + workspace
//! teardown, cmd+W shortcut dispatch.

#![cfg(not(target_arch = "wasm32"))]

use hxy_vfs::TabSource;

use crate::app::HxyApp;
use crate::commands::shortcuts::CLOSE_TAB;
use crate::commands::shortcuts::REOPEN_CLOSED_TAB;
use crate::files::FileId;
use crate::files::OpenFile;
use crate::state::OpenTabState;
use crate::state::PersistedState;
use crate::tabs::Tab;

/// Upper bound on the in-memory ring buffer of recently-closed tabs.
/// Older entries fall off the back when this is exceeded; matches the
/// "recently closed" depth used by browsers / editors with a similar
/// shortcut.
pub const CLOSED_TABS_CAPACITY: usize = 32;

/// One closed-file capture sitting on the reopen ring buffer. Carries
/// the same [`OpenTabState`] shape session restore consumes -- so
/// reopening drives the existing restore path rather than a parallel
/// codepath. `display_name` is held alongside so the palette / menu
/// item can show what's about to be reopened without re-resolving
/// the source.
#[derive(Clone, Debug)]
pub struct ClosedTabSnapshot {
    pub state: OpenTabState,
    pub display_name: String,
}

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

/// Push a snapshot of the persisted [`OpenTabState`] for `source` onto
/// the reopen stack. Idempotent -- nothing is pushed when the source
/// has no entry in `state.open_tabs` (an in-flight open that never
/// persisted, or a non-restorable singleton tab). Carries
/// `display_name` alongside so palette / menu rows can show what's
/// about to be reopened without re-resolving the source.
///
/// Drops the oldest entry once the stack reaches
/// [`CLOSED_TABS_CAPACITY`] -- the buffer is meant to undo recent
/// closes, not to be a permanent history.
pub(crate) fn remember_closed(app: &mut HxyApp, source: &TabSource, display_name: String) {
    let state = {
        let g = app.state.read();
        g.open_tabs.iter().find(|t| &t.source == source).cloned()
    };
    let Some(state) = state else { return };
    if app.closed_tabs.len() >= CLOSED_TABS_CAPACITY {
        app.closed_tabs.pop_front();
    }
    app.closed_tabs.push_back(ClosedTabSnapshot { state, display_name });
}

/// Pop the most recently closed tab off the ring buffer and drive it
/// back through the same restore path session startup uses. Returns
/// `false` when the buffer is empty or the restore failed (parent
/// VFS mount gone, file deleted on disk, plugin uninstalled); the
/// snapshot in that case is discarded -- a single user gesture
/// pops one entry.
///
/// The `ctx` is needed to fire any persisted template auto-reruns
/// for the just-restored tab (the runner wires its result inbox
/// against an [`egui::Context`]). Call sites without a context
/// handy can use [`crate::tabs::close::dispatch_close_shortcut`]
/// which already threads the per-frame ctx.
pub fn reopen_last_closed_tab(ctx: &egui::Context, app: &mut HxyApp) -> bool {
    let Some(snapshot) = app.closed_tabs.pop_back() else { return false };
    let source = snapshot.state.source.clone();
    // Pre-seed `open_tabs` with the captured state so the standard
    // `open()` path skips its default-init branch (it only pushes a
    // fresh entry when no row matches the source). Carries the
    // captured templates / visualizer flag / virtual_base_choice
    // through to the live tab.
    {
        let mut g = app.state.write();
        g.open_tabs.retain(|t| t.source != source);
        g.open_tabs.push(snapshot.state.clone());
    }
    let must_mount = matches!(&source, TabSource::VfsEntry { .. });
    if let Err(e) = app.restore_one_tab(&snapshot.state, must_mount) {
        tracing::warn!(error = %e, "reopen closed tab");
        let mut g = app.state.write();
        g.open_tabs.retain(|t| t.source != source);
        return false;
    }
    // Auto-rerun the just-restored tab's persisted templates so the
    // tree / visualizer / colored-byte tinting come back exactly
    // where the user left them. Scoped to this one source rather
    // than the global `pending_template_restore` flag, which would
    // also re-fire every other open file's templates.
    app.restore_persisted_templates_for_source(ctx, &source);
    true
}

/// Drop a single file tab (Tab::File or workspace entry) by id, free
/// its `OpenFile`, and clear the matching persisted `OpenTabState`
/// so the tab doesn't reappear on next launch. Callers responsible
/// for gating on dirtiness -- this helper is the unconditional path
/// the modal's "Don't Save" branch uses.
pub fn close_file_tab_by_id(app: &mut HxyApp, id: FileId) {
    // Snapshot the live OpenTabState before any of the dock teardown
    // below clears `state.open_tabs`. The reopen-last-closed stack
    // pops this back when the user hits Cmd+Shift+T, which then
    // re-runs `restore_one_tab` against the same shape the launch
    // path uses. Sync first so the snapshot reflects "right now"
    // rather than the most recent per-frame sync_tab_state pass.
    if let Some(file) = app.files.get(&id)
        && let Some(source) = file.source_kind.clone()
    {
        let display_name = file.display_name.clone();
        {
            let mut g = app.state.write();
            sync_tab_state(&mut g, file);
        }
        remember_closed(app, &source, display_name);
    }
    if let Some(path) = app.dock.find_tab(&Tab::File(id)) {
        let _ = app.dock.remove_tab(path);
    }
    // The entropy panel is keyed by FileId, so a closing file
    // takes its panel with it -- otherwise the panel would
    // render an empty "no active file" placeholder against a
    // FileId that no longer exists.
    if let Some(path) = app.dock.find_tab(&Tab::Entropy(id)) {
        let _ = app.dock.remove_tab(path);
    }
    // Strings panel is also keyed by FileId, so it leaves with the
    // file too.
    if let Some(path) = app.dock.find_tab(&Tab::Strings(id)) {
        let _ = app.dock.remove_tab(path);
    }
    if let Some(path) = app.dock.find_tab(&Tab::Checksums(id)) {
        let _ = app.dock.remove_tab(path);
    }
    // Same lifetime story for the visualizer panel: keyed on
    // FileId, so it goes when its file does.
    if let Some(path) = app.dock.find_tab(&Tab::Visualizer(id)) {
        let _ = app.dock.remove_tab(path);
    }
    for workspace in app.workspaces.values_mut() {
        if let Some(path) = workspace.dock.find_tab(&crate::files::WorkspaceTab::Entry(id)) {
            let _ = workspace.dock.remove_tab(path);
            break;
        }
    }
    let removed_root: Option<std::path::PathBuf> = app.files.remove(&id).and_then(|removed| {
        removed.release_cache();
        let root = removed.root_path().cloned();
        if let Some(source) = &removed.source_kind {
            let mut state = app.state.write();
            state.open_tabs.retain(|t| &t.source != source);
        }
        root
    });
    if let Some(path) = removed_root {
        app.unwatch_path_if_unused(&path);
    }
    app.unwatch_vfs_for_file(id);
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
///
/// When the active outer tab is a workspace, the close targets the
/// workspace's inner active tab instead: closing an Entry closes
/// that file, closing the Editor closes the entire workspace, and
/// closing the VfsTree just removes that sub-tab. We dispatch on
/// the active outer tab rather than `app.tab_focus` because
/// `tab_focus` only flips on tab-header clicks; clicking into the
/// hex body leaves it on `Outer` even though the user is plainly
/// "inside" the workspace.
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
        Tab::Console | Tab::Inspector | Tab::Plugins | Tab::Entropy(_) | Tab::Memory | Tab::Checksums(_) => {
            if let Some(path) = app.dock.find_tab(&tab) {
                let _ = app.dock.remove_tab(path);
            }
        }
        Tab::Strings(file_id) => {
            if let Some(path) = app.dock.find_tab(&tab) {
                let _ = app.dock.remove_tab(path);
            }
            // Drop the cached row hover so the hex view doesn't
            // keep painting a stale highlight after the panel
            // disappears.
            if let Some(file) = app.files.get_mut(&file_id) {
                file.strings_panel.hovered_entry = None;
            }
        }
        Tab::Visualizer(file_id) => {
            if let Some(path) = app.dock.find_tab(&tab) {
                let _ = app.dock.remove_tab(path);
            }
            // Clear the user's "open" flag so a re-run on the same
            // file doesn't pop the panel back. Mirrored into the
            // persisted tab state so the closure also survives a
            // restart, not just a template re-run within the same
            // session.
            set_visualizer_open(app, file_id, false);
        }
        Tab::PluginMount(mount_id) => {
            if let Some(path) = app.dock.find_tab(&tab) {
                let _ = app.dock.remove_tab(path);
            }
            if let Some(removed) = app.mounts.remove(&mount_id) {
                let display_name = removed.display_name.clone();
                let target = TabSource::PluginMount {
                    plugin_name: removed.plugin_name,
                    token: removed.token,
                    title: removed.display_name,
                };
                remember_closed(app, &target, display_name);
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
            close_active_workspace_inner(app, workspace_id);
        }
        Tab::Compare(compare_id) => {
            if let Some(path) = app.dock.find_tab(&tab) {
                let _ = app.dock.remove_tab(path);
            }
            app.compares.remove(&compare_id);
        }
    }
}

/// Close whatever sub-tab is active in the workspace's inner dock.
/// Editor -> close the whole workspace; Entry -> close that file
/// (dirty-check via the same prompt as the outer flow); VfsTree ->
/// just remove the sub-tab. Falls back to closing the workspace
/// outright when no inner active tab is resolvable, so a wedged
/// workspace is still closeable.
fn close_active_workspace_inner(app: &mut HxyApp, workspace_id: crate::files::WorkspaceId) {
    let active = match app.workspaces.get_mut(&workspace_id) {
        Some(w) => w.dock.find_active_focused().map(|(_, t)| *t),
        None => return,
    };
    match active {
        Some(crate::files::WorkspaceTab::Editor) | None => {
            close_workspace_by_id(app, workspace_id);
            app.tab_focus = crate::app::TabFocus::Outer;
        }
        Some(crate::files::WorkspaceTab::VfsTree) => {
            if let Some(workspace) = app.workspaces.get_mut(&workspace_id)
                && let Some(path) = workspace.dock.find_tab(&crate::files::WorkspaceTab::VfsTree)
            {
                let _ = workspace.dock.remove_tab(path);
            }
        }
        Some(crate::files::WorkspaceTab::Entry(file_id)) => {
            if let Some(file) = app.files.get(&file_id)
                && file.editor.is_dirty()
            {
                app.pending_close_workspace_entry =
                    Some(PendingCloseTab { file_id, display_name: file.display_name.clone() });
                return;
            }
            close_file_tab_by_id(app, file_id);
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
    // Snapshot the editor file onto the reopen stack so Cmd+Shift+T
    // can bring the workspace back. Inner-entry tabs aren't captured
    // individually -- they ride along with the editor's
    // `as_workspace = true` restore, which re-mounts the parent and
    // re-grafts any persisted child entries automatically. Capturing
    // each child separately would queue up duplicates in the reopen
    // buffer for what the user perceives as a single close.
    if let Some(file) = app.files.get(&workspace.editor_id)
        && let Some(source) = file.source_kind.clone()
    {
        let display_name = file.display_name.clone();
        {
            let mut g = app.state.write();
            sync_tab_state(&mut g, file);
        }
        remember_closed(app, &source, display_name);
    }
    let mut paths_to_recheck: Vec<std::path::PathBuf> = Vec::new();
    {
        let mut state = app.state.write();
        for file_id in &to_drop {
            if let Some(removed) = app.files.remove(file_id) {
                removed.release_cache();
                if let Some(p) = removed.root_path().cloned() {
                    paths_to_recheck.push(p);
                }
                if let Some(source) = &removed.source_kind {
                    state.open_tabs.retain(|t| &t.source != source);
                }
            }
        }
    }
    for path in paths_to_recheck {
        app.unwatch_path_if_unused(&path);
    }
}

/// Cmd+W (close active tab) and Cmd+Shift+T (reopen last closed
/// tab) dispatchers. Both consumed in this pass so the close /
/// reopen pair owns its keystrokes regardless of frame ordering.
pub fn dispatch_close_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    let (close, reopen) = ctx.input_mut(|i| (i.consume_shortcut(&CLOSE_TAB), i.consume_shortcut(&REOPEN_CLOSED_TAB)));
    if close {
        request_close_active_tab(app);
    }
    if reopen {
        reopen_last_closed_tab(ctx, app);
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

/// Set `visualizer_panel.open` on the file and mirror it into the
/// matching `OpenTabState` so the choice survives a restart, not
/// just a template re-run within the same session.
pub fn set_visualizer_open(app: &mut HxyApp, file_id: FileId, open: bool) {
    let Some(file) = app.files.get_mut(&file_id) else { return };
    file.visualizer_panel.open = open;
    let Some(source) = file.source_kind.clone() else { return };
    let mut state = app.state.write();
    if let Some(entry) = state.open_tabs.iter_mut().find(|t| t.source == source) {
        entry.visualizer_open = open;
    }
}

/// Sync the current editor selection / scroll back into the
/// persisted [`crate::state::OpenTabState`] entry so the next
/// session restores the user's view. Also mirrors the file's
/// completed template runs (path, range, fingerprint, color
/// overrides) so the next launch can auto-rerun them.
pub fn sync_tab_state(state: &mut PersistedState, file: &OpenFile) {
    let Some(source) = &file.source_kind else { return };
    let Some(entry) = state.open_tabs.iter_mut().find(|t| &t.source == source) else { return };
    entry.selection = file.editor.selection();
    entry.scroll_offset = file.editor.scroll_offset();
    entry.templates = file
        .templates
        .iter()
        .map(|t| crate::state::PersistedTemplateInstance {
            source_path: t.source_path.clone(),
            display_name: t.display_name.clone(),
            range: t.range,
            source_fingerprint: t.source_fingerprint,
            node_color_overrides: t.state.node_color_overrides.iter().map(|(&k, &v)| (k, v)).collect(),
        })
        .collect();
    entry.active_template_idx =
        file.active_template.and_then(|active| file.templates.iter().position(|t| t.id == active));
    entry.visualizer_open = file.visualizer_panel.open;
}
