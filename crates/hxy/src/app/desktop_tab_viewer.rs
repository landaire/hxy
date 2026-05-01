//! Desktop-target dock tab viewers.
//!
//! `HxyTabViewer` is the outer-dock viewer rendering Tab variants the
//! browser build doesn't reach (Plugins, Compare, SearchResults,
//! Visualizer, PluginMount). `WorkspaceTabViewer` is the inner-dock
//! viewer for `Tab::Workspace`. Both rely on desktop-only state
//! (toasts, plugin host, watcher, sync rfd) so the whole module is
//! gated to non-wasm. The wasm equivalents live in `app::wasm`.

use std::collections::HashMap;
use std::sync::Arc;

use egui_dock::tab_viewer::OnCloseResponse;
use hxy_vfs::MountedVfs;
use hxy_vfs::TabSource;

use super::ConsoleEntry;
use super::PENDING_VFS_OPEN_KEY;
use super::PendingVfsOpen;
use super::TabFocus;
use super::format_file_tab_title;
use super::format_workspace_tab_title;
use super::render_failed_placeholder;
use super::render_file_tab;
use super::render_loading_placeholder;
use super::settings_ui;
use super::user_plugins_dir;
use super::user_template_plugins_dir;
use super::vfs_expanded_for;
use super::welcome_ui;
use crate::files::FileId;
use crate::files::OpenFile;
use crate::state::PersistedState;
use crate::tabs::Tab;

pub(super) struct HxyTabViewer<'a> {
    pub(super) files: &'a mut HashMap<FileId, OpenFile>,
    pub(super) state: &'a mut PersistedState,
    pub(super) compares: &'a mut std::collections::BTreeMap<crate::compare::CompareId, crate::compare::CompareSession>,
    pub(super) console: &'a std::collections::VecDeque<ConsoleEntry>,
    /// Active plugin VFS mounts. Read-only here -- closing a mount tab
    /// only flags it via `pending_close_mount` and the app drops it
    /// from the map after the dock pass.
    pub(super) mounts: &'a std::collections::BTreeMap<crate::files::MountId, crate::files::MountedPlugin>,
    /// Slot for the dock's `on_close` handler when the user X-clicks a
    /// `Tab::PluginMount`. The app drains the mount entry from
    /// `app.mounts` after the dock pass.
    pub(super) pending_close_mount: &'a mut Option<crate::files::MountId>,
    /// Cross-file search state, rendered by `Tab::SearchResults`.
    pub(super) global_search: &'a mut crate::search::global::GlobalSearchState,
    /// Events emitted by the global search tab during render. Drained
    /// after the dock pass so we can mutate `files` to focus / jump.
    pub(super) pending_global_search_events: &'a mut Vec<crate::search::global::GlobalSearchEvent>,
    pub(super) inspector: &'a mut crate::panels::inspector::InspectorState,
    pub(super) decoders: &'a [Arc<dyn crate::panels::inspector::Decoder>],
    /// FileId of the currently-active file, captured before the
    /// dock pass so the Inspector tab arm can identify which
    /// file's caret to read from. The Inspector helper does the
    /// actual read against `files` at render time, so disjoint
    /// field borrows on `self` (files immut + this immut) work.
    pub(super) active_file_id: Option<FileId>,
    /// Set to true when the Plugins tab mutated the plugin directories
    /// and needs the registry / template runtimes rebuilt. Drained at
    /// end of frame by [`HxyApp::ui`].
    pub(super) plugin_rescan: &'a mut bool,
    /// Read-only view of loaded plugin handlers so the Plugins tab
    /// can render their consent cards.
    pub(super) plugin_handlers: &'a [Arc<hxy_plugin_host::PluginHandler>],
    /// Sink for grant changes / state-wipe requests captured by the
    /// Plugins tab. Drained at end of frame by [`HxyApp::ui`].
    pub(super) pending_plugin_events: &'a mut Vec<crate::panels::plugins::PluginsEvent>,
    /// Snapshot of the persisted ImHex-Patterns hash, captured before
    /// the dock pass so the Plugins tab can render its status
    /// without re-borrowing `state`.
    pub(super) patterns_installed_hash: Option<String>,
    /// Bytes received so far on an in-flight pattern download, or
    /// None when no fetch is running. Mirrors
    /// [`HxyApp::pattern_in_flight_bytes`] for the dock viewer.
    pub(super) patterns_in_flight_bytes: Option<u64>,
    /// Slot the dock's `on_close` handler writes to when the user
    /// X-clicks a dirty File tab. The app drains this after the
    /// dock pass and renders the save-prompt modal next frame.
    pub(super) pending_close_tab: &'a mut Option<crate::tabs::close::PendingCloseTab>,
    /// Mutated whenever the user clicks an outer tab button so
    /// `Ctrl+Tab` knows to cycle the outer dock next, or hands off
    /// to a workspace inner dock when the user clicks into one.
    pub(super) tab_focus: &'a mut TabFocus,
    /// File-mounted VFS workspaces. The viewer renders each
    /// `Tab::Workspace` by spinning up an inner `DockArea` against
    /// `workspace.dock`.
    pub(super) workspaces: &'a mut std::collections::BTreeMap<crate::files::WorkspaceId, crate::files::Workspace>,
    /// Slot the inner workspace dock writes to when the user closes a
    /// `WorkspaceTab::Entry` whose file is dirty. Same shape as
    /// `pending_close_tab` (the modal handler treats them identically).
    pub(super) pending_close_workspace_entry: &'a mut Option<crate::tabs::close::PendingCloseTab>,
    /// `WorkspaceId`s the viewer drained to "no tabs left except the
    /// editor." The post-dock pass collapses these back to plain
    /// `Tab::File` entries in the outer dock.
    pub(super) pending_collapse_workspace: &'a mut Vec<crate::files::WorkspaceId>,
    /// Toast / template-prompt center, plumbed in so `render_file_tab`
    /// can render its prompts scoped to the tab's content rect rather
    /// than the app-global corner.
    pub(super) toasts: &'a mut crate::toasts::ToastCenter,
    /// Sink for "Run X.bt" toast accepts. Drained by the host loop
    /// after the dock pass.
    pub(super) pending_template_runs: &'a mut Vec<crate::toasts::PendingTemplateRun>,
    /// Sink for entropy panels' "Compute" / "Recompute" button
    /// clicks. Each panel pushes its own pinned `FileId` here
    /// when the button fires; the host drains the list after
    /// the dock pass and routes each entry through
    /// [`compute_entropy_for`]. A `Vec` (rather than a single
    /// slot) lets multiple docked entropy panels each fire a
    /// recompute in the same frame.
    pub(super) entropy_recompute: &'a mut Vec<FileId>,
    /// Sink for visualizer-panel header X-button clicks. The
    /// dock-pass borrow on `app.dock` blocks the renderer from
    /// removing the tab inline, so it queues the file id here and
    /// the post-dock drain calls `remove_tab` + sets the file's
    /// `visualizer_panel.open` flag.
    pub(super) pending_visualizer_dismiss: &'a mut Vec<FileId>,
    /// Strings panel "Run" requests captured during render.
    /// Drained post-dock-pass, where we have `&mut HxyApp` and can
    /// route through `spawn_strings_for`.
    pub(super) pending_strings_run: &'a mut Vec<FileId>,
    /// Strings panel offset-link clicks captured during render.
    /// Each entry is `(file_id, offset, end)`; the post-dock drain
    /// translates them into selection + scroll updates on the file's
    /// hex view.
    pub(super) pending_strings_jump: &'a mut Vec<(FileId, u64, u64)>,
    /// Checksum panel "Run" requests captured during render.
    pub(super) pending_checksums_run: &'a mut Vec<FileId>,
    /// Clipboard-copy requests emitted by the Checksum panel
    /// (per-row "Copy" buttons + "Copy all"). Each entry is the
    /// already-formatted string that should land on the clipboard.
    pub(super) pending_checksums_copy: &'a mut Vec<String>,
    /// Shared byte cache, plumbed through so the Settings tab can
    /// drive `set_limit` directly when the user changes the cache
    /// budget and the Memory debug panel can call `stats()`.
    pub(super) byte_cache: &'a Arc<hxy_core::ByteCache>,
}

impl egui_dock::TabViewer for HxyTabViewer<'_> {
    type Tab = Tab;

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        match tab {
            Tab::Welcome => hxy_i18n::t("tab-welcome").into(),
            Tab::Settings => hxy_i18n::t("tab-settings").into(),
            Tab::Console => hxy_i18n::t("tab-console").into(),
            Tab::Inspector => hxy_i18n::t("tab-inspector").into(),
            Tab::Plugins => hxy_i18n::t("tab-plugins").into(),
            Tab::Memory => hxy_i18n::t("tab-memory").into(),
            Tab::Entropy(id) => {
                let name = self.files.get(id).map(|f| f.display_name.as_str()).unwrap_or("");
                hxy_i18n::t_args("tab-entropy", &[("name", name)]).into()
            }
            Tab::Visualizer(id) => {
                let name = self.files.get(id).map(|f| f.display_name.as_str()).unwrap_or("");
                hxy_i18n::t_args("tab-visualizer", &[("name", name)]).into()
            }
            Tab::Strings(id) => {
                let name = self.files.get(id).map(|f| f.display_name.as_str()).unwrap_or("");
                hxy_i18n::t_args("tab-strings", &[("name", name)]).into()
            }
            Tab::Checksums(id) => {
                let name = self.files.get(id).map(|f| f.display_name.as_str()).unwrap_or("");
                hxy_i18n::t_args("tab-checksums", &[("name", name)]).into()
            }
            Tab::File(id) => match self.files.get(id) {
                Some(f) => format_file_tab_title(f).into(),
                None => format!("file-{}", id.get()).into(),
            },
            Tab::PluginMount(mount_id) => match self.mounts.get(mount_id) {
                Some(m) => format!("{} {}", egui_phosphor::regular::TREE_STRUCTURE, m.display_name).into(),
                None => format!("mount-{}", mount_id.get()).into(),
            },
            Tab::SearchResults => {
                format!("{} {}", egui_phosphor::regular::MAGNIFYING_GLASS, hxy_i18n::t("tab-search-results")).into()
            }
            Tab::Workspace(workspace_id) => match self.workspaces.get(workspace_id) {
                Some(w) => match self.files.get(&w.editor_id) {
                    Some(f) => format_workspace_tab_title(f).into(),
                    None => format!("workspace-{}", workspace_id.get()).into(),
                },
                None => format!("workspace-{}", workspace_id.get()).into(),
            },
            Tab::Compare(compare_id) => match self.compares.get(compare_id) {
                Some(s) => {
                    hxy_i18n::t_args("tab-compare-title", &[("a", &s.a.display_name), ("b", &s.b.display_name)]).into()
                }
                None => format!("compare-{}", compare_id.get()).into(),
            },
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            Tab::Welcome => welcome_ui(ui, self.state),
            Tab::Settings => settings_ui(ui, &mut self.state.app, self.files, self.byte_cache),
            Tab::Console => super::render_console_tab(ui, self.console),
            Tab::Inspector => {
                super::render_inspector_tab(ui, self.inspector, self.decoders, self.files, self.active_file_id)
            }
            Tab::Entropy(file_id) => {
                super::render_entropy_tab(ui, self.files, *file_id, self.entropy_recompute);
            }
            Tab::Strings(file_id) => {
                super::render_strings_tab(
                    ui,
                    self.files,
                    *file_id,
                    self.pending_strings_run,
                    self.pending_strings_jump,
                );
            }
            Tab::Checksums(file_id) => {
                super::render_checksums_tab(
                    ui,
                    self.files,
                    *file_id,
                    self.pending_checksums_run,
                    self.pending_checksums_copy,
                );
            }
            Tab::Visualizer(file_id) => {
                let pinned = *file_id;
                // Split-borrow: the visualizer renderer needs both
                // `OpenFile` (template trees, byte source) and a
                // mutable handle to the file's `visualizer_panel`
                // field. Take ownership of the panel for the render
                // pass and slot it back afterwards so the renderer
                // can borrow `OpenFile` immutably without contending
                // with the `&mut` on the panel field.
                let numeric_format = self.state.app.numeric_format;
                let template_value_formats = self.state.app.template_value_formats;
                if let Some(file) = self.files.get_mut(&pinned) {
                    let mut taken = std::mem::take(&mut file.visualizer_panel);
                    let events =
                        crate::visualizers::show(ui, Some(&*file), &mut taken, numeric_format, &template_value_formats);
                    file.visualizer_panel = taken;
                    for ev in events {
                        match ev {
                            crate::visualizers::VisualizerEvent::Dismiss => {
                                self.pending_visualizer_dismiss.push(pinned);
                            }
                        }
                    }
                } else {
                    let mut empty = crate::visualizers::VisualizerPanel::default();
                    let _ = crate::visualizers::show(ui, None, &mut empty, numeric_format, &template_value_formats);
                }
            }
            Tab::Memory => super::render_memory_tab(ui, self.files, self.byte_cache),
            Tab::Plugins => {
                let handlers_dir = user_plugins_dir();
                let templates_dir = user_template_plugins_dir();
                let patterns_info = crate::panels::plugins::PatternsTabInfo {
                    installed_hash: self.patterns_installed_hash.as_deref(),
                    in_flight_bytes: self.patterns_in_flight_bytes,
                };
                let events = crate::panels::plugins::show(
                    ui,
                    handlers_dir.as_ref(),
                    templates_dir.as_ref(),
                    self.plugin_handlers,
                    patterns_info,
                );
                for e in events {
                    match e {
                        crate::panels::plugins::PluginsEvent::Rescan => *self.plugin_rescan = true,
                        // Grant + wipe events apply to mutable state
                        // the viewer doesn't own; queue them for the
                        // app's post-dock drain.
                        other => self.pending_plugin_events.push(other),
                    }
                }
            }
            Tab::File(id) => match self.files.get_mut(id) {
                Some(file) => match &file.load_status {
                    crate::files::LoadStatus::Loading => {
                        render_loading_placeholder(ui, &file.display_name);
                    }
                    crate::files::LoadStatus::Failed(message) => {
                        render_failed_placeholder(ui, &file.display_name, message);
                    }
                    crate::files::LoadStatus::Ready => {
                        render_file_tab(
                            ui,
                            *id,
                            file,
                            self.state,
                            *self.tab_focus,
                            self.toasts,
                            self.pending_template_runs,
                        );
                    }
                },
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing file {id:?}"));
                }
            },
            Tab::PluginMount(mount_id) => match self.mounts.get(mount_id) {
                Some(m) => {
                    let key = TabSource::PluginMount {
                        plugin_name: m.plugin_name.clone(),
                        token: m.token.clone(),
                        title: m.display_name.clone(),
                    };
                    let expanded = vfs_expanded_for(&mut self.state.vfs_tree_expanded, &key);
                    crate::plugins::mount::render_plugin_mount_tab(ui, *mount_id, m, expanded);
                }
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing mount {mount_id:?}"));
                }
            },
            Tab::SearchResults => {
                super::render_search_results_tab(ui, self.files, self.global_search, self.pending_global_search_events);
            }
            Tab::Workspace(workspace_id) => {
                render_workspace_tab(
                    ui,
                    *workspace_id,
                    self.workspaces,
                    self.files,
                    self.state,
                    self.pending_close_workspace_entry,
                    self.pending_collapse_workspace,
                    self.tab_focus,
                    self.toasts,
                    self.pending_template_runs,
                );
            }
            Tab::Compare(compare_id) => super::render_compare_tab(ui, self.compares, *compare_id, self.state),
        }
    }

    fn on_tab_button(&mut self, _tab: &mut Self::Tab, response: &egui::Response) {
        // Mouse clicks on an outer tab snap focus to the outer dock
        // so the next Ctrl+Tab cycles top-level tabs. Hover / drag
        // don't count -- only an actual click is a focus event.
        if response.clicked() {
            *self.tab_focus = TabFocus::Outer;
        }
    }
    fn closeable(&mut self, tab: &mut Self::Tab) -> bool {
        matches!(
            tab,
            Tab::File(_)
                | Tab::Console
                | Tab::Inspector
                | Tab::Plugins
                | Tab::Workspace(_)
                | Tab::Memory
                | Tab::PluginMount(_)
                | Tab::SearchResults
                | Tab::Entropy(_)
                | Tab::Visualizer(_)
        )
    }

    fn scroll_bars(&self, tab: &Self::Tab) -> [bool; 2] {
        // File tabs and the console/inspector manage their own
        // scrolling; outer dock scrollbar off for those. Plugin mount
        // tabs render the VFS tree's own scroll area. Workspace tabs
        // host an inner DockArea that takes the full body.
        match tab {
            Tab::File(_) | Tab::Console | Tab::Inspector | Tab::Workspace(_) => [false, false],
            Tab::PluginMount(_) | Tab::SearchResults => [false, false],
            Tab::Entropy(_) | Tab::Visualizer(_) => [false, false],
            Tab::Memory => [false, true],
            _ => [true, true],
        }
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> OnCloseResponse {
        if let Tab::File(id) = tab {
            // Stage the close in the pending-modal slot when the
            // editor has uncommitted edits. The dock keeps the tab
            // (Ignore response) and we let the modal next frame
            // decide: Save -> close, Don't Save -> close, Cancel ->
            // do nothing. Without this gate, the X button silently
            // discards unsaved bytes.
            if let Some(file) = self.files.get(id)
                && file.editor.is_dirty()
            {
                *self.pending_close_tab =
                    Some(crate::tabs::close::PendingCloseTab { file_id: *id, display_name: file.display_name.clone() });
                return OnCloseResponse::Ignore;
            }
            if let Some(removed) = self.files.remove(id) {
                removed.release_cache();
                if let Some(source) = &removed.source_kind {
                    self.state.open_tabs.retain(|t| &t.source != source);
                }
            }
        }
        if let Tab::PluginMount(mount_id) = tab {
            // Defer the actual removal -- the mounts map is borrowed
            // immutably here. The post-dock drain in `HxyApp::ui`
            // matches on this slot and drops the mount entry plus the
            // matching `state.open_tabs` record.
            *self.pending_close_mount = Some(*mount_id);
        }
        if let Tab::Visualizer(file_id) = tab {
            // Clear the user's "open" flag so the next template re-run
            // doesn't auto-pop the panel back. Mirrored to the
            // persisted tab state so the closure survives a restart.
            if let Some(file) = self.files.get_mut(file_id) {
                file.visualizer_panel.open = false;
                if let Some(source) = file.source_kind.clone()
                    && let Some(entry) = self.state.open_tabs.iter_mut().find(|t| t.source == source)
                {
                    entry.visualizer_open = false;
                }
            }
        }
        if let Tab::Strings(file_id) = tab {
            // Drop the cached row hover so the hex view doesn't keep
            // painting a stale highlight after the panel disappears.
            if let Some(file) = self.files.get_mut(file_id) {
                file.strings_panel.hovered_entry = None;
            }
        }
        if let Tab::Workspace(workspace_id) = tab {
            // Workspace close = editor + every entry sub-tab. Bail to
            // the modal if any of them is dirty; the modal handler is
            // responsible for tearing down the workspace once the
            // user confirms.
            let Some(workspace) = self.workspaces.get(workspace_id) else {
                return OnCloseResponse::Close;
            };
            let mut dirty: Option<(FileId, String)> = None;
            if let Some(editor) = self.files.get(&workspace.editor_id)
                && editor.editor.is_dirty()
            {
                dirty = Some((workspace.editor_id, editor.display_name.clone()));
            } else {
                for (_, t) in workspace.dock.iter_all_tabs() {
                    if let crate::files::WorkspaceTab::Entry(file_id) = t
                        && let Some(f) = self.files.get(file_id)
                        && f.editor.is_dirty()
                    {
                        dirty = Some((*file_id, f.display_name.clone()));
                        break;
                    }
                }
            }
            if let Some((file_id, display_name)) = dirty {
                *self.pending_close_tab = Some(crate::tabs::close::PendingCloseTab { file_id, display_name });
                return OnCloseResponse::Ignore;
            }
            // Drain workspace contents from `app.files` + persistence;
            // the modal handler does the same on confirmed close.
            let workspace = self.workspaces.remove(workspace_id).expect("just looked up");
            let mut to_drop: Vec<FileId> = vec![workspace.editor_id];
            for (_, t) in workspace.dock.iter_all_tabs() {
                if let crate::files::WorkspaceTab::Entry(file_id) = t {
                    to_drop.push(*file_id);
                }
            }
            for file_id in &to_drop {
                if let Some(removed) = self.files.remove(file_id) {
                    removed.release_cache();
                    if let Some(source) = &removed.source_kind {
                        self.state.open_tabs.retain(|t| &t.source != source);
                    }
                }
            }
        }
        OnCloseResponse::Close
    }
}

/// Render a `Tab::Workspace` body: spin up an inner DockArea against
/// the workspace's `dock` and dispatch to `WorkspaceTabViewer` for
/// each sub-tab (Editor, VfsTree, Entry).
#[allow(clippy::too_many_arguments)]
fn render_workspace_tab(
    ui: &mut egui::Ui,
    workspace_id: crate::files::WorkspaceId,
    workspaces: &mut std::collections::BTreeMap<crate::files::WorkspaceId, crate::files::Workspace>,
    files: &mut HashMap<FileId, OpenFile>,
    state: &mut PersistedState,
    pending_close_workspace_entry: &mut Option<crate::tabs::close::PendingCloseTab>,
    pending_collapse_workspace: &mut Vec<crate::files::WorkspaceId>,
    tab_focus: &mut TabFocus,
    toasts: &mut crate::toasts::ToastCenter,
    pending_template_runs: &mut Vec<crate::toasts::PendingTemplateRun>,
) {
    let Some(workspace) = workspaces.get_mut(&workspace_id) else {
        ui.colored_label(egui::Color32::RED, format!("missing workspace {workspace_id:?}"));
        return;
    };
    let editor_id = workspace.editor_id;
    let mount = workspace.mount.clone();
    let inner_dock = &mut workspace.dock;

    let mut viewer = WorkspaceTabViewer {
        files,
        state,
        editor_id,
        workspace_id,
        mount: &mount,
        pending_close_workspace_entry,
        tab_focus,
        toasts,
        pending_template_runs,
    };
    let style = crate::style::hxy_dock_style(ui.style());
    egui_dock::DockArea::new(inner_dock)
        .id(egui::Id::new(("hxy-workspace-dock", workspace_id.get())))
        .style(style)
        .show_leaf_collapse_buttons(false)
        .show_inside(ui, &mut viewer);

    // Collapse-back trigger: if the workspace is left with only its
    // Editor sub-tab (user closed the tree + every entry), schedule a
    // post-dock collapse to a plain `Tab::File`.
    let only_editor = workspace.dock.iter_all_tabs().count() == 1
        && workspace.dock.iter_all_tabs().all(|(_, t)| matches!(t, crate::files::WorkspaceTab::Editor));
    if only_editor && !pending_collapse_workspace.contains(&workspace_id) {
        pending_collapse_workspace.push(workspace_id);
    }
}

/// Inner-dock viewer for `Tab::Workspace`. Renders the editor (the
/// workspace's underlying file), the VFS tree, and any opened VFS
/// entries. Dirty closes funnel through `pending_close_workspace_entry`
/// the same way top-level dirty closes funnel through
/// `pending_close_tab`.
struct WorkspaceTabViewer<'a> {
    pub(super) files: &'a mut HashMap<FileId, OpenFile>,
    pub(super) state: &'a mut PersistedState,
    pub(super) editor_id: FileId,
    pub(super) workspace_id: crate::files::WorkspaceId,
    pub(super) mount: &'a Arc<MountedVfs>,
    pub(super) pending_close_workspace_entry: &'a mut Option<crate::tabs::close::PendingCloseTab>,
    /// Updated by `on_tab_button` when the user clicks an inner tab,
    /// so subsequent `Ctrl+Tab` cycles cycle this workspace's dock.
    pub(super) tab_focus: &'a mut TabFocus,
    /// Plumbed through so the workspace's inner File-tabs can render
    /// their template prompts scoped to the tab body.
    pub(super) toasts: &'a mut crate::toasts::ToastCenter,
    pub(super) pending_template_runs: &'a mut Vec<crate::toasts::PendingTemplateRun>,
}

impl egui_dock::TabViewer for WorkspaceTabViewer<'_> {
    type Tab = crate::files::WorkspaceTab;

    fn id(&mut self, tab: &mut Self::Tab) -> egui::Id {
        // Distinct ids per workspace so two open workspaces don't
        // share `WorkspaceTab::Editor` when egui_dock interns the tab.
        match tab {
            crate::files::WorkspaceTab::Editor => egui::Id::new(("ws-editor", self.workspace_id.get())),
            crate::files::WorkspaceTab::VfsTree => egui::Id::new(("ws-tree", self.workspace_id.get())),
            crate::files::WorkspaceTab::Entry(file_id) => {
                egui::Id::new(("ws-entry", self.workspace_id.get(), file_id.get()))
            }
        }
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        match tab {
            crate::files::WorkspaceTab::Editor => match self.files.get(&self.editor_id) {
                Some(f) => {
                    // House icon marks the workspace's parent file --
                    // visually distinct from Entry sub-tabs (which are
                    // unprefixed), so the user can spot the root tab
                    // even when several entries are open beside it.
                    let mut prefix = String::from(egui_phosphor::regular::HOUSE);
                    prefix.push(' ');
                    if matches!(f.editor.edit_mode(), crate::files::EditMode::Readonly) {
                        prefix.push_str(egui_phosphor::regular::LOCK);
                        prefix.push(' ');
                    }
                    if f.editor.is_dirty() {
                        prefix.push_str("\u{2022} ");
                    }
                    format!("{prefix}{}", f.display_name).into()
                }
                None => format!("file-{}", self.editor_id.get()).into(),
            },
            crate::files::WorkspaceTab::VfsTree => format!("{} VFS", egui_phosphor::regular::TREE_STRUCTURE).into(),
            crate::files::WorkspaceTab::Entry(file_id) => match self.files.get(file_id) {
                Some(f) => {
                    let mut prefix = String::new();
                    if f.editor.is_dirty() {
                        prefix.push_str("\u{2022} ");
                    }
                    format!("{prefix}{}", f.display_name).into()
                }
                None => format!("entry-{}", file_id.get()).into(),
            },
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            crate::files::WorkspaceTab::Editor => match self.files.get_mut(&self.editor_id) {
                Some(file) => render_file_tab(
                    ui,
                    self.editor_id,
                    file,
                    self.state,
                    *self.tab_focus,
                    self.toasts,
                    self.pending_template_runs,
                ),
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing editor {:?}", self.editor_id));
                }
            },
            crate::files::WorkspaceTab::VfsTree => {
                let scope = egui::Id::new(("hxy-workspace-vfs", self.workspace_id.get()));
                // Key the persisted expansion list by the parent
                // file's source so it survives across restarts and
                // even across closing / reopening the workspace.
                let parent_source = self.files.get(&self.editor_id).and_then(|f| f.source_kind.clone());
                let events = match parent_source {
                    Some(key) => {
                        let expanded = vfs_expanded_for(&mut self.state.vfs_tree_expanded, &key);
                        crate::panels::vfs::show(ui, scope, &*self.mount.fs, expanded)
                    }
                    None => {
                        let mut scratch = Vec::new();
                        crate::panels::vfs::show(ui, scope, &*self.mount.fs, &mut scratch)
                    }
                };
                let mut to_open: Vec<String> = Vec::new();
                for e in events {
                    let crate::panels::vfs::VfsPanelEvent::OpenEntry(path) = e;
                    to_open.push(path);
                }
                if !to_open.is_empty() {
                    let workspace_id = self.workspace_id;
                    ui.ctx().data_mut(|d| {
                        let queue: &mut Vec<PendingVfsOpen> =
                            d.get_temp_mut_or_default(egui::Id::new(PENDING_VFS_OPEN_KEY));
                        for p in to_open {
                            queue.push(PendingVfsOpen::Workspace { workspace_id, entry_path: p });
                        }
                    });
                }
            }
            crate::files::WorkspaceTab::Entry(file_id) => match self.files.get_mut(file_id) {
                Some(file) => render_file_tab(
                    ui,
                    *file_id,
                    file,
                    self.state,
                    *self.tab_focus,
                    self.toasts,
                    self.pending_template_runs,
                ),
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing entry {file_id:?}"));
                }
            },
        }
    }

    fn closeable(&mut self, tab: &mut Self::Tab) -> bool {
        // Editor is non-closeable from inside the workspace -- the
        // user closes the whole workspace via the outer Tab::Workspace
        // tab. Tree / Entry are individually closeable.
        !matches!(tab, crate::files::WorkspaceTab::Editor)
    }

    fn scroll_bars(&self, _tab: &Self::Tab) -> [bool; 2] {
        [false, false]
    }

    fn on_tab_button(&mut self, _tab: &mut Self::Tab, response: &egui::Response) {
        // Click on an inner sub-tab snaps focus to this workspace's
        // inner dock so Ctrl+Tab starts cycling Editor / VfsTree /
        // Entry instead of the outer dock.
        if response.clicked() {
            *self.tab_focus = TabFocus::Workspace(self.workspace_id);
        }
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> OnCloseResponse {
        if let crate::files::WorkspaceTab::Entry(file_id) = tab {
            if let Some(f) = self.files.get(file_id)
                && f.editor.is_dirty()
            {
                *self.pending_close_workspace_entry = Some(crate::tabs::close::PendingCloseTab {
                    file_id: *file_id,
                    display_name: f.display_name.clone(),
                });
                return OnCloseResponse::Ignore;
            }
            if let Some(removed) = self.files.remove(file_id) {
                removed.release_cache();
                if let Some(source) = &removed.source_kind {
                    self.state.open_tabs.retain(|t| &t.source != source);
                }
            }
        }
        OnCloseResponse::Close
    }
}
