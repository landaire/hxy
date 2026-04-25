//! Main application type.

use std::collections::HashMap;
use std::sync::Arc;

use egui_dock::DockArea;
use egui_dock::DockState;
use egui_dock::Style;
use egui_dock::TabViewer;
use egui_dock::tab_viewer::OnCloseResponse;
use hxy_plugin_host::TemplateRuntime as _;
use hxy_vfs::TabSource;
use hxy_vfs::VfsHandler;
use hxy_vfs::VfsRegistry;
use hxy_vfs::handlers::ZipHandler;

use crate::APP_NAME;
use crate::file::FileId;
use crate::file::OpenFile;
use crate::state::PersistedState;
use crate::state::SharedPersistedState;
use crate::tabs::Tab;
use crate::window::WindowSettings;

use hxy_vfs::MountedVfs;

/// Where `open_with_target` should push the new tab.
#[derive(Clone, Copy, Debug)]
pub enum OpenTarget {
    /// Push as a top-level `Tab::File` in the main dock.
    Toplevel,
    /// Push as `WorkspaceTab::Entry` inside the named workspace's
    /// inner dock.
    Workspace(crate::file::WorkspaceId),
}

/// Which set of tabs the next `Ctrl+Tab` / `Ctrl+Shift+Tab` keypress
/// should cycle. Updated by mouse clicks (`on_tab_button`) and by
/// `Alt+Tab`. Persisted only for the running session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TabFocus {
    /// Cycling targets the outer dock's focused leaf -- top-level
    /// tabs (File, Workspace, Inspector, ...).
    #[default]
    Outer,
    /// Cycling targets the inner dock of this workspace -- its
    /// editor / VFS tree / opened entries.
    Workspace(crate::file::WorkspaceId),
}

pub struct HxyApp {
    dock: DockState<Tab>,
    files: HashMap<FileId, OpenFile>,
    /// File-mounted VFS workspaces, keyed by `WorkspaceId`. Each entry
    /// backs a `Tab::Workspace` and owns a nested `DockState` plus the
    /// `MountedVfs` that supplies child entries.
    workspaces: std::collections::BTreeMap<crate::file::WorkspaceId, crate::file::Workspace>,
    next_workspace_id: u64,
    /// Active plugin VFS mounts, keyed by `MountId`. Each entry backs a
    /// `Tab::PluginMount` and supplies the byte source for child VFS
    /// entry tabs the user opens from the tree.
    #[cfg(not(target_arch = "wasm32"))]
    mounts: std::collections::BTreeMap<crate::file::MountId, crate::file::MountedPlugin>,
    #[cfg(not(target_arch = "wasm32"))]
    next_mount_id: u64,
    state: SharedPersistedState,
    next_file_id: u64,
    registry: VfsRegistry,
    #[cfg(not(target_arch = "wasm32"))]
    template_plugins: Vec<Arc<dyn hxy_plugin_host::TemplateRuntime>>,
    /// Loaded VFS plugin handlers, kept alongside the
    /// `VfsRegistry` so the palette can ask each one for its
    /// command contributions without going through the trait-
    /// object erasure the registry stores.
    #[cfg(not(target_arch = "wasm32"))]
    plugin_handlers: Vec<Arc<hxy_plugin_host::PluginHandler>>,
    /// Shared per-plugin blob persistence. `None` means no SQLite
    /// pool was wired (e.g. db open failed at startup); plugins
    /// granted `persist` then see `denied` from the state interface.
    /// Grants themselves live in [`PersistedState::plugin_grants`].
    #[cfg(not(target_arch = "wasm32"))]
    plugin_state_store: Option<Arc<dyn hxy_plugin_host::StateStore>>,

    #[cfg(not(target_arch = "wasm32"))]
    sink: Option<crate::persist::SaveSink>,

    /// Window geometry captured last frame, used to detect drag-end: the
    /// first frame where `prev_window == current_window` and the saved
    /// value still differs triggers the persistence write.
    prev_window: Option<WindowSettings>,
    last_saved_window: Option<WindowSettings>,

    /// Zoom factor we last applied to the egui context. Used to push
    /// settings changes into the live context without re-running every
    /// frame.
    applied_zoom: f32,

    /// An open request that collided with an already-open tab. Held
    /// here while the modal asks the user whether to focus the
    /// existing tab or open a second copy. `None` outside that window.
    pending_duplicate: Option<PendingDuplicate>,

    /// Set when an open hit a sidecar from a previous session that
    /// still matches the file on disk. The modal asks the user
    /// whether to restore the saved patch or discard it; rendering
    /// happens in `update()` next to the duplicate-open dialog.
    #[cfg(not(target_arch = "wasm32"))]
    pending_patch_restore: Option<PendingPatchRestore>,

    /// Bounded ring buffer of plugin / template log entries. Rendered
    /// by the Console dock tab when it's open; entries accumulate
    /// regardless so opening the tab later reveals back-scroll.
    console: std::collections::VecDeque<ConsoleEntry>,

    /// Data-inspector dock tab state. Endianness + radix preferences
    /// and the `show_panel` flag that's only consulted when the
    /// Inspector tab is closed and re-opened.
    #[cfg(not(target_arch = "wasm32"))]
    inspector: crate::inspector::InspectorState,
    /// Registered decoders for the inspector. Defaults to the
    /// built-in set; user-registered decoders will be additive.
    #[cfg(not(target_arch = "wasm32"))]
    decoders: Vec<Arc<dyn crate::inspector::Decoder>>,
    /// The most recently focused File tab. Remembered across frames
    /// so panels like the Inspector (which take keyboard focus when
    /// clicked) keep showing data from the file the user was last
    /// reading, not from themselves.
    last_active_file: Option<FileId>,
    /// Same idea as `last_active_file` but for workspace context:
    /// remembers which workspace was most recently focused so
    /// "Toggle VFS panel" / "Browse VFS" don't silently no-op when
    /// the user happens to have clicked into the inspector or
    /// console. Cleared when the corresponding workspace closes.
    last_active_workspace: Option<crate::file::WorkspaceId>,
    /// Native macOS menu bar. `None` until the app is constructed on
    /// the main thread. Dropping it tears the NSMenu down.
    #[cfg(target_os = "macos")]
    menu: Option<crate::menu::MenuState>,
    /// Set by the Plugins tab when the user installs or deletes a
    /// file in the plugin directories. Drained at end of `ui()`.
    #[cfg(not(target_arch = "wasm32"))]
    plugin_rescan: bool,
    /// Per-plugin grant changes / state-wipe requests captured by
    /// the Plugins tab. Drained at end of `ui()`; each entry is
    /// applied to `PersistedState::plugin_grants` (or the state
    /// store) and triggers a plugin reload.
    #[cfg(not(target_arch = "wasm32"))]
    pending_plugin_events: Vec<crate::plugins_tab::PluginsEvent>,
    /// Plugin operations (invoke / respond / mount-by-token) that
    /// were dispatched to a worker thread and are awaiting a result.
    /// Drained each frame; ready ops dispatch their outcome through
    /// the same paths the synchronous calls used to take.
    #[cfg(not(target_arch = "wasm32"))]
    pending_plugin_ops: Vec<crate::plugin_runner::PendingOp>,
    /// Auto-detected template library loaded from the user's
    /// `templates/` dir. Consulted when a file is opened so the
    /// toolbar can offer `Run ZIP.bt` directly.
    #[cfg(not(target_arch = "wasm32"))]
    templates: crate::template_library::TemplateLibrary,
    /// Cmd+P / Ctrl+P unified palette. Outlives individual opens so
    /// toggling off and back on feels continuous; the state is reset
    /// explicitly when switching modes.
    #[cfg(not(target_arch = "wasm32"))]
    palette: crate::command_palette::PaletteState,
    /// Visual pane picker session. `Some` after the user activates
    /// the visual move/merge palette commands and before they
    /// either press a target letter (op fires) or Escape (cancel).
    /// Mutually exclusive with `palette` -- entering the picker
    /// closes the palette, opening the palette cancels the picker.
    #[cfg(not(target_arch = "wasm32"))]
    pending_pane_pick: Option<crate::pane_pick::PendingPanePick>,
    /// Persistent letter assignments for the visual pane picker,
    /// keyed by a content hash of each leaf's tabs. Lets a leaf
    /// keep the same letter across pick sessions even when other
    /// leaves around it open / close. Stale entries (whose leaf
    /// no longer exists) are evicted by `pane_pick::tick` so the
    /// freed letter is available for the next new leaf.
    #[cfg(not(target_arch = "wasm32"))]
    pane_pick_letters: std::collections::BTreeMap<u64, char>,
    /// Set when the user tries to close a tab that has unsaved
    /// edits -- via Cmd+W or by clicking the tab's X. The modal
    /// renders next frame and asks Save / Don't Save / Cancel;
    /// only `Save`-then-success or `Don't Save` actually close the
    /// tab, the third does nothing.
    pending_close_tab: Option<PendingCloseTab>,
    /// Tracks which dock the user's last tab-bar interaction was in,
    /// so `Ctrl+Tab` / `Ctrl+Shift+Tab` cycle the correct surface
    /// (outer dock vs a specific workspace's inner dock). Toggled
    /// directly by `Alt+Tab`. Updated on mouse click via the
    /// viewer's `on_tab_button` hook.
    tab_focus: TabFocus,
    /// Same shape as `pending_close_tab` but written from the inner
    /// workspace dock's `on_close`. Drained alongside the regular
    /// pending-close slot; the modal treats them identically.
    pending_close_workspace_entry: Option<PendingCloseTab>,
    /// `WorkspaceId`s the inner dock drained to "no tabs left except
    /// the editor". Drained post-dock to collapse the workspace back
    /// to a plain `Tab::File` in the outer dock.
    pending_collapse_workspace: Vec<crate::file::WorkspaceId>,
    /// Set when the user X-clicks a `Tab::PluginMount`; drained after
    /// the dock pass to remove the mount entry from `mounts` and any
    /// matching record from `state.open_tabs`.
    #[cfg(not(target_arch = "wasm32"))]
    pending_close_mount: Option<crate::file::MountId>,
    /// Tool tabs the user has stashed via `toggle_tool_panel`. While
    /// non-empty, the right-hand tool panel is hidden -- the dock has
    /// no leaf for these tabs at all, so the surrounding panes get
    /// their horizontal space back. Toggling again recreates the
    /// right-split leaf and pushes these tabs into it.
    hidden_tool_tabs: Vec<Tab>,
    /// Shared cross-file search state. Backs the `Tab::SearchResults`
    /// dock tab; lives on the app so query / matches survive the user
    /// closing and reopening the tab.
    #[cfg(not(target_arch = "wasm32"))]
    global_search: crate::global_search::GlobalSearchState,
    /// Events the global search tab emitted this frame. Drained at the
    /// end of `ui()` so we can mutate `files` (focus / jump) after the
    /// dock has released its borrow.
    #[cfg(not(target_arch = "wasm32"))]
    pending_global_search_events: Vec<crate::global_search::GlobalSearchEvent>,
    /// Most-recently-focused leaf that holds a content tab (File /
    /// Welcome / Settings). Used to route file opens that originate
    /// from inside a tool panel (e.g. clicking a VFS entry inside a
    /// `Tab::PluginMount`) back into the user's main editing area
    /// instead of the tool panel itself. Refreshed each frame.
    #[cfg(not(target_arch = "wasm32"))]
    last_content_leaf: Option<egui_dock::NodePath>,
    /// File paths from the launch's command line. Drained on the
    /// first frame and turned into open-file requests, so a
    /// `hxy a.bin b.bin` invocation lands two tabs as soon as the
    /// window comes up.
    #[cfg(not(target_arch = "wasm32"))]
    pending_cli_paths: Vec<std::path::PathBuf>,
    /// Inbox carrying path batches forwarded from second-instance
    /// invocations over the local IPC socket. `None` when the
    /// listener failed to bind (the GUI still works -- it just
    /// can't accept forwarded opens until next launch). Drained
    /// every frame.
    #[cfg(not(target_arch = "wasm32"))]
    ipc_inbox: Option<egui_inbox::UiInbox<Vec<std::path::PathBuf>>>,
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

/// One entry in the Console tab. `context` identifies the plugin run
/// that produced the message -- typically `<data-file> / <template-file>`.
#[derive(Clone, Debug)]
pub struct ConsoleEntry {
    pub timestamp: jiff::Timestamp,
    pub severity: ConsoleSeverity,
    pub context: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleSeverity {
    Info,
    Warning,
    Error,
}

/// A deferred filesystem-open request awaiting the user's choice in
/// the duplicate-open dialog. Retains the bytes we already read so we
/// don't hit the disk twice.
struct PendingDuplicate {
    display_name: String,
    path: std::path::PathBuf,
    bytes: Vec<u8>,
    existing: FileId,
}

/// A sidecar patch found on open that the user hasn't decided what
/// to do with yet. The modal renders next frame; either side resets
/// `pending_patch_restore` to `None`.
#[cfg(not(target_arch = "wasm32"))]
struct PendingPatchRestore {
    file_id: FileId,
    sidecar: crate::patch_persist::PatchSidecar,
    /// Classification captured at open time so the modal can reuse
    /// the reason string without re-stating the filesystem.
    integrity: crate::patch_persist::RestoreIntegrity,
}

impl HxyApp {
    pub fn new(cc: &eframe::CreationContext<'_>, state: SharedPersistedState) -> Self {
        install_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        let (initial_zoom, initial_window) = {
            let s = state.read();
            (s.app.zoom_factor, s.window)
        };
        cc.egui_ctx.set_zoom_factor(initial_zoom);
        let mut registry = VfsRegistry::new();
        registry.register(Arc::new(ZipHandler::new()));
        // Plugins load with empty grants + no state store at
        // construction time. The runtime-owned `with_plugin_persistence`
        // builder reloads them once the SQLite-backed grants and
        // state store are available; without that call (e.g. db open
        // failed at startup) every requested permission stays denied.
        #[cfg(not(target_arch = "wasm32"))]
        let plugin_handlers =
            register_user_plugins(&mut registry, &hxy_plugin_host::PluginGrants::default(), None);
        #[cfg(not(target_arch = "wasm32"))]
        let template_plugins = load_user_template_plugins();
        Self {
            dock: DockState::new(vec![Tab::Welcome, Tab::Settings]),
            files: HashMap::new(),
            workspaces: std::collections::BTreeMap::new(),
            next_workspace_id: 1,
            #[cfg(not(target_arch = "wasm32"))]
            mounts: std::collections::BTreeMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            next_mount_id: 1,
            state,
            next_file_id: 1,
            registry,
            #[cfg(not(target_arch = "wasm32"))]
            template_plugins,
            #[cfg(not(target_arch = "wasm32"))]
            plugin_handlers,
            #[cfg(not(target_arch = "wasm32"))]
            plugin_state_store: None,
            #[cfg(not(target_arch = "wasm32"))]
            sink: None,
            prev_window: None,
            last_saved_window: Some(initial_window),
            applied_zoom: initial_zoom,
            pending_duplicate: None,
            #[cfg(not(target_arch = "wasm32"))]
            pending_patch_restore: None,
            console: std::collections::VecDeque::new(),
            #[cfg(not(target_arch = "wasm32"))]
            inspector: crate::inspector::InspectorState::default(),
            #[cfg(not(target_arch = "wasm32"))]
            decoders: crate::inspector::default_decoders(),
            last_active_file: None,
            last_active_workspace: None,
            #[cfg(target_os = "macos")]
            menu: Some(crate::menu::MenuState::install()),
            #[cfg(not(target_arch = "wasm32"))]
            plugin_rescan: false,
            #[cfg(not(target_arch = "wasm32"))]
            pending_plugin_events: Vec::new(),
            pending_plugin_ops: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            templates: crate::template_library::TemplateLibrary::load_from(user_templates_dir().as_deref()),
            #[cfg(not(target_arch = "wasm32"))]
            palette: crate::command_palette::PaletteState::default(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_pane_pick: None,
            #[cfg(not(target_arch = "wasm32"))]
            pane_pick_letters: std::collections::BTreeMap::new(),
            pending_close_tab: None,
            tab_focus: TabFocus::Outer,
            pending_close_workspace_entry: None,
            pending_collapse_workspace: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_close_mount: None,
            hidden_tool_tabs: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            global_search: crate::global_search::GlobalSearchState::default(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_global_search_events: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            last_content_leaf: None,
            #[cfg(not(target_arch = "wasm32"))]
            pending_cli_paths: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            ipc_inbox: None,
        }
    }

    /// Rebuild the VFS registry + template runtime list from the
    /// user's plugin directories. Called by the Plugins tab after the
    /// user installs or deletes a file.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn reload_plugins(&mut self) {
        let mut registry = VfsRegistry::new();
        registry.register(Arc::new(ZipHandler::new()));
        let grants = self.state.read().plugin_grants.clone();
        self.plugin_handlers =
            register_user_plugins(&mut registry, &grants, self.plugin_state_store.clone());
        self.registry = registry;
        self.template_plugins = load_user_template_plugins();
        self.templates = crate::template_library::TemplateLibrary::load_from(user_templates_dir().as_deref());
    }

    /// Drain a batch of grant / wipe events captured by the
    /// Plugins tab. Mutates `PersistedState::plugin_grants` for
    /// any `SetGrant`, calls the state store for any `WipeState`,
    /// then triggers a single `reload_plugins` at the end so the
    /// linker reflects the new grant set.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_plugin_events(&mut self, events: Vec<crate::plugins_tab::PluginsEvent>) {
        let mut grants_changed = false;
        for ev in events {
            match ev {
                crate::plugins_tab::PluginsEvent::Rescan => {
                    self.plugin_rescan = true;
                }
                crate::plugins_tab::PluginsEvent::SetGrant { key, grants: g } => {
                    self.state.write().plugin_grants.set(key, g);
                    grants_changed = true;
                }
                crate::plugins_tab::PluginsEvent::WipeState { plugin_name } => {
                    if let Some(store) = self.plugin_state_store.as_ref()
                        && let Err(e) = store.clear(&plugin_name)
                    {
                        tracing::warn!(error = %e, plugin = %plugin_name, "wipe plugin state");
                    }
                }
            }
        }
        if grants_changed {
            // Persist immediately so a crash before the next save
            // tick doesn't lose the user's consent decision.
            if let Some(sink) = self.sink.as_ref() {
                let snapshot = self.state.read().clone();
                if let Err(e) = sink.save(&snapshot) {
                    tracing::warn!(error = %e, "save plugin grants");
                }
            }
            self.reload_plugins();
        }
    }

    /// Show the Plugins tab. Focuses if already open; otherwise routes
    /// to the shared tool leaf (creating it as a right split if no
    /// other plugin tab is already docked there).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn show_plugins(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Plugins) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        let node_path = push_tool_tab(&mut self.dock, Tab::Plugins);
        self.dock.set_focused_node_and_surface(node_path);
    }

    /// Close the Plugins tab if present; otherwise show it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn toggle_plugins(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Plugins) {
            let _ = self.dock.remove_tab(path);
        } else {
            self.show_plugins();
        }
    }

    /// Open the data inspector as a right-side split of the main
    /// dock area, matching 010 Editor's layout. If already docked
    /// anywhere (including after the user drags it elsewhere),
    /// focus the existing tab instead of creating a second split.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn show_inspector(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Inspector) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        self.dock.main_surface_mut().split_right(egui_dock::NodeIndex::root(), 0.72, vec![Tab::Inspector]);
    }

    /// Close the Inspector tab if present; otherwise show it.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn toggle_inspector(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Inspector) {
            let _ = self.dock.remove_tab(path);
        } else {
            self.show_inspector();
        }
    }

    /// Append a message to the Console tab. Caps the buffer at
    /// [`Self::CONSOLE_CAPACITY`] entries; older entries are dropped
    /// first so long-running sessions don't accumulate unbounded RAM.
    ///
    /// Errors auto-open the Console at the bottom of the main dock
    /// so the user notices them without having to hunt through the
    /// View menu.
    pub fn console_log(&mut self, severity: ConsoleSeverity, context: impl Into<String>, message: impl Into<String>) {
        let entry = ConsoleEntry {
            timestamp: jiff::Timestamp::now(),
            severity,
            context: context.into(),
            message: message.into(),
        };
        while self.console.len() >= Self::CONSOLE_CAPACITY {
            self.console.pop_front();
        }
        self.console.push_back(entry);
        if severity == ConsoleSeverity::Error {
            self.show_console();
        }
    }

    pub const CONSOLE_CAPACITY: usize = 2048;

    /// Drain any plugin operations that have completed since last
    /// frame and dispatch their outcomes. Background-threaded calls
    /// register themselves on `pending_plugin_ops` via the helpers in
    /// [`crate::plugin_runner`]; the outcome dispatch matches what
    /// the synchronous calls used to do (palette dispatch / open
    /// tab) plus a "completed in N ms" log entry.
    #[cfg(not(target_arch = "wasm32"))]
    fn drain_pending_plugin_ops(&mut self, ctx: &egui::Context) {
        // `try_take` consumes the op and returns either the result
        // or the unchanged op (still pending). Re-collect the
        // not-yet-ready ones into the queue; ready ones are
        // dispatched immediately.
        //
        // Critical: dispatching a ready op may itself spawn a new
        // op (e.g. a `respond` outcome of `Mount` spawns
        // `mount_by_token`), so the dispatch path pushes onto
        // `self.pending_plugin_ops`. We must NOT overwrite that
        // vec with our locally-collected `still_pending` at the
        // end -- that would discard the newly-spawned ops and the
        // mount tab would never open. Instead, prepend the still-
        // pending ones back so any new ops dispatch added during
        // this drain are preserved.
        let drained: Vec<_> = self.pending_plugin_ops.drain(..).collect();
        let mut still_pending: Vec<crate::plugin_runner::PendingOp> = Vec::new();
        for op in drained {
            let plugin_name = op.plugin_name.clone();
            let label = op.label.clone();
            let started = op.started;
            match op.try_take() {
                Err(unfinished) => still_pending.push(unfinished),
                Ok(crate::plugin_runner::DrainResult::Pending) => {}
                Ok(crate::plugin_runner::DrainResult::InvokeReady {
                    plugin,
                    command_id,
                    outcome,
                }) => {
                    self.log_plugin_completion(&plugin_name, &label, started, outcome.is_some());
                    dispatch_plugin_outcome(ctx, self, plugin, &plugin_name, &command_id, outcome);
                }
                Ok(crate::plugin_runner::DrainResult::RespondReady {
                    plugin,
                    command_id,
                    outcome,
                }) => {
                    self.log_plugin_completion(&plugin_name, &label, started, outcome.is_some());
                    dispatch_plugin_outcome(ctx, self, plugin, &plugin_name, &command_id, outcome);
                }
                Ok(crate::plugin_runner::DrainResult::MountReady {
                    plugin,
                    token,
                    title,
                    result,
                }) => match result {
                    Ok(mount) => {
                        self.log_plugin_completion(&plugin_name, &label, started, true);
                        install_mount_tab(self, plugin, mount, token, title);
                    }
                    Err(e) => {
                        crate::plugin_runner::log_completion(
                            self,
                            &plugin_name,
                            &label,
                            started,
                            ConsoleSeverity::Error,
                            &format!("failed: {e}"),
                        );
                    }
                },
            }
        }
        // Push the still-pending ops back, preserving any new ops
        // the dispatch loop appended (e.g. a `mount_by_token`
        // spawned by a `respond -> Mount` outcome).
        for op in still_pending {
            self.pending_plugin_ops.push(op);
        }
    }

    fn log_plugin_completion(
        &mut self,
        plugin_name: &str,
        label: &str,
        started: std::time::Instant,
        ok: bool,
    ) {
        let (sev, detail) = if ok { (ConsoleSeverity::Info, "ok") } else {
            (ConsoleSeverity::Warning, "no outcome (call trapped or grant denied)")
        };
        crate::plugin_runner::log_completion(self, plugin_name, label, started, sev, detail);
    }

    /// Open the Console tab as a horizontal split at the bottom of
    /// the main dock area. If the tab is already docked anywhere,
    /// just focus it. Called both from View > Show Console and
    /// automatically by `console_log` when an error lands.
    pub fn show_console(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Console) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        // Split the main surface's root so the console always docks
        // at the bottom regardless of whatever layout the user is
        // running with.
        self.dock.main_surface_mut().split_below(egui_dock::NodeIndex::root(), 0.75, vec![Tab::Console]);
    }

    /// Close the Console tab if present; otherwise show it.
    pub fn toggle_console(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Console) {
            let _ = self.dock.remove_tab(path);
        } else {
            self.show_console();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn template_runtime_for(&self, extension: &str) -> Option<Arc<dyn hxy_plugin_host::TemplateRuntime>> {
        self.template_plugins.iter().find(|r| r.extensions().iter().any(|e| e.eq_ignore_ascii_case(extension))).cloned()
    }

    pub fn registry(&self) -> &VfsRegistry {
        &self.registry
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_sink(mut self, sink: crate::persist::SaveSink) -> Self {
        self.sink = Some(sink);
        self.restore_open_tabs();
        self
    }

    /// Hand the app a SQLite-backed state store so plugins granted
    /// `persist` actually persist. Grants themselves come from the
    /// shared [`PersistedState::plugin_grants`] populated at
    /// startup. Triggers a plugin reload so the in-memory
    /// `PluginHandler` instances pick up the new state-store
    /// reference; without this call (e.g. db open failed), every
    /// plugin's permission requests are treated as denied.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_plugin_persistence(mut self, state_store: Arc<dyn hxy_plugin_host::StateStore>) -> Self {
        self.plugin_state_store = Some(state_store);
        self.reload_plugins();
        self
    }

    /// Stash file paths captured from the process command line so
    /// the first frame opens them. Resolution to absolute form
    /// happens in [`crate::cli::Cli::resolved_files`] before this
    /// is called -- we don't want to re-resolve against the
    /// running instance's CWD on the receiving end of an IPC
    /// forward.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_cli_paths(mut self, paths: Vec<std::path::PathBuf>) -> Self {
        self.pending_cli_paths = paths;
        self
    }

    /// Hand off the IPC listener's inbox so the running instance
    /// can pick up forwarded paths from later `hxy <file>...`
    /// invocations. `None` is fine: the GUI just won't accept
    /// forwarded opens.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_ipc_inbox(mut self, inbox: egui_inbox::UiInbox<Vec<std::path::PathBuf>>) -> Self {
        self.ipc_inbox = Some(inbox);
        self
    }

    fn fresh_file_id(&mut self) -> FileId {
        let id = FileId::new(self.next_file_id);
        self.next_file_id += 1;
        id
    }

    pub fn open_in_memory(&mut self, display_name: impl Into<String>, bytes: Vec<u8>) -> FileId {
        self.open(display_name, None, bytes, None, None, false)
    }

    pub fn open_filesystem(
        &mut self,
        display_name: impl Into<String>,
        path: std::path::PathBuf,
        bytes: Vec<u8>,
        restore_selection: Option<hxy_core::Selection>,
        restore_scroll: Option<f32>,
    ) -> FileId {
        self.open(display_name, Some(TabSource::Filesystem(path)), bytes, restore_selection, restore_scroll, false)
    }

    /// User-facing open: if the path is already in another tab, stash
    /// the request and show a "focus existing vs open duplicate"
    /// modal on the next frame. Otherwise opens straight away.
    ///
    /// Restore paths deliberately bypass this -- reopening a file
    /// across restarts shouldn't prompt.
    pub fn request_open_filesystem(
        &mut self,
        display_name: impl Into<String>,
        path: std::path::PathBuf,
        bytes: Vec<u8>,
    ) {
        let display_name = display_name.into();
        if let Some(existing) = self.existing_filesystem_tab(&path) {
            self.pending_duplicate = Some(PendingDuplicate { display_name, path, bytes, existing });
            return;
        }
        self.open_filesystem(display_name, path, bytes, None, None);
    }

    fn existing_filesystem_tab(&self, path: &std::path::Path) -> Option<FileId> {
        self.files.iter().find_map(|(id, f)| match &f.source_kind {
            Some(TabSource::Filesystem(p)) if p == path => Some(*id),
            _ => None,
        })
    }

    /// Move dock focus to the tab backing `file_id`, if found.
    fn focus_file_tab(&mut self, file_id: FileId) {
        if let Some(path) = self.dock.find_tab(&Tab::File(file_id)) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        // The file might live inside a workspace either as the
        // editor or as an opened entry. Focus the workspace tab in
        // the outer dock and the matching sub-tab in the inner dock.
        let workspace_target: Option<(crate::file::WorkspaceId, crate::file::WorkspaceTab)> = self
            .workspaces
            .values()
            .find_map(|w| {
                if w.editor_id == file_id {
                    Some((w.id, crate::file::WorkspaceTab::Editor))
                } else if w.dock.find_tab(&crate::file::WorkspaceTab::Entry(file_id)).is_some() {
                    Some((w.id, crate::file::WorkspaceTab::Entry(file_id)))
                } else {
                    None
                }
            });
        if let Some((workspace_id, sub_tab)) = workspace_target {
            if let Some(path) = self.dock.find_tab(&Tab::Workspace(workspace_id)) {
                let node_path = path.node_path();
                let _ = self.dock.set_active_tab(path);
                self.dock.set_focused_node_and_surface(node_path);
            }
            if let Some(workspace) = self.workspaces.get_mut(&workspace_id)
                && let Some(inner_path) = workspace.dock.find_tab(&sub_tab)
            {
                let _ = workspace.dock.set_active_tab(inner_path);
            }
        }
    }

    /// Open a new top-level file tab with the given display name,
    /// persistent source identity, and byte contents. Runs format
    /// detection against the source's first bytes and caches the
    /// matching handler (if any) on the tab so the "Browse VFS"
    /// command can enable itself. When `as_workspace` is true and a
    /// handler matches, the file is mounted and the tab is wrapped in
    /// a `Tab::Workspace` immediately; otherwise it lands as a plain
    /// `Tab::File`.
    pub fn open(
        &mut self,
        display_name: impl Into<String>,
        source_kind: Option<TabSource>,
        bytes: Vec<u8>,
        restore_selection: Option<hxy_core::Selection>,
        restore_scroll: Option<f32>,
        as_workspace: bool,
    ) -> FileId {
        let id = self.create_open_file(display_name, source_kind.clone(), bytes, restore_selection, restore_scroll);
        self.apply_readonly_for_source(id);

        let pushed_workspace = if as_workspace { self.try_push_as_workspace(id) } else { false };
        if !pushed_workspace {
            self.dock.push_to_focused_leaf(Tab::File(id));
            if let Some(path) = self.dock.find_tab(&Tab::File(id)) {
                remove_welcome_from_leaf(&mut self.dock, path.surface, path.node);
            }
        }

        // Look for an unsaved-edits sidecar from a previous session
        // and offer it back to the user. The actual restore happens
        // after the modal returns; this just stages the prompt.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(TabSource::Filesystem(path)) = source_kind.as_ref()
            && let Some(dir) = unsaved_edits_dir()
        {
            match crate::patch_persist::load(&dir, path) {
                Ok(Some(sidecar)) => {
                    let integrity = sidecar.integrity();
                    self.pending_patch_restore =
                        Some(PendingPatchRestore { file_id: id, sidecar, integrity });
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, path = %path.display(), "load patch sidecar"),
            }
        }

        if let Some(source) = source_kind {
            let mut g = self.state.write();
            if let TabSource::Filesystem(p) = &source {
                g.app.record_recent(p.clone());
            }
            if !g.open_tabs.iter().any(|t| t.source == source) {
                g.open_tabs.push(crate::state::OpenTabState {
                    source,
                    selection: restore_selection,
                    scroll_offset: restore_scroll.unwrap_or(0.0),
                    as_workspace: pushed_workspace,
                });
            }
        }
        id
    }

    /// Allocate a `FileId`, build an `OpenFile`, run handler / template
    /// detection against the first ~4 KiB, and insert into `app.files`.
    /// Doesn't touch the dock -- callers decide whether to push a
    /// `Tab::File`, wrap in a `Tab::Workspace`, or graft into an
    /// existing workspace's inner dock.
    fn create_open_file(
        &mut self,
        display_name: impl Into<String>,
        source_kind: Option<TabSource>,
        bytes: Vec<u8>,
        restore_selection: Option<hxy_core::Selection>,
        restore_scroll: Option<f32>,
    ) -> FileId {
        let id = self.fresh_file_id();
        let mut file = OpenFile::from_bytes(id, display_name, source_kind, bytes);
        file.editor.set_selection(restore_selection);
        if let Some(s) = restore_scroll {
            file.editor.set_scroll_to(s);
        }

        // Detect a matching VFS handler against the first ~4 KiB.
        if let Ok(range) = hxy_core::ByteRange::new(
            hxy_core::ByteOffset::new(0),
            hxy_core::ByteOffset::new(file.editor.source().len().get().min(4096)),
        ) && let Ok(head) = file.editor.source().read(range)
        {
            file.detected_handler = self.registry.detect(&head);
            #[cfg(not(target_arch = "wasm32"))]
            {
                let ext = file
                    .source_kind
                    .as_ref()
                    .and_then(|s| s.root_path().cloned())
                    .as_ref()
                    .and_then(|p| p.extension())
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_ascii_lowercase());
                file.suggested_template = self.templates.suggest(ext.as_deref(), &head).map(|entry| {
                    crate::file::SuggestedTemplate { path: entry.path.clone(), display_name: entry.name.clone() }
                });
            }
        }

        self.files.insert(id, file);
        id
    }

    /// Attempt to wrap the freshly-created file `id` in a `Tab::Workspace`
    /// by mounting its detected handler. Returns `true` if the workspace
    /// was created and pushed; `false` falls back to the plain
    /// `Tab::File` path (no detected handler, or mount failed).
    fn try_push_as_workspace(&mut self, id: FileId) -> bool {
        let Some(file) = self.files.get(&id) else { return false };
        let Some(handler) = file.detected_handler.clone() else { return false };
        let source = file.editor.source().clone();
        let mount = match handler.mount(source) {
            Ok(m) => Arc::new(m),
            Err(e) => {
                tracing::warn!(error = %e, handler = handler.name(), "mount as workspace");
                return false;
            }
        };
        let workspace_id = self.spawn_workspace(id, mount);
        self.dock.push_to_focused_leaf(Tab::Workspace(workspace_id));
        if let Some(path) = self.dock.find_tab(&Tab::Workspace(workspace_id)) {
            remove_welcome_from_leaf(&mut self.dock, path.surface, path.node);
        }
        true
    }

    /// Allocate a `WorkspaceId`, build a `Workspace`, and register it.
    /// Does not push a tab -- the caller decides whether the workspace
    /// is fresh (push `Tab::Workspace`) or replacing an existing
    /// `Tab::File` for the same `editor_id` (swap the dock tab).
    fn spawn_workspace(&mut self, editor_id: FileId, mount: Arc<MountedVfs>) -> crate::file::WorkspaceId {
        let id = crate::file::WorkspaceId::new(self.next_workspace_id);
        self.next_workspace_id += 1;
        let workspace = crate::file::Workspace::new(id, editor_id, mount);
        self.workspaces.insert(id, workspace);
        id
    }

    /// Try to open each saved tab. Filesystem tabs are read directly
    /// from disk; VFS-entry tabs require their parent tab to be open
    /// with a materialised mount. We sort tabs by `TabSource` depth so
    /// parents are restored before their children. Failures (file
    /// missing, parent failed to mount, entry path gone) drop the tab
    /// from the persisted list.
    #[cfg(not(target_arch = "wasm32"))]
    fn restore_open_tabs(&mut self) {
        let mut tabs = self.state.read().open_tabs.clone();
        // Topologically order: shallower depth first so parents load
        // before any child that references them.
        tabs.sort_by_key(|t| t.source.depth());

        // Any tab that is a `parent` of another persisted tab must mount
        // on restore, otherwise the child can't find its source bytes.
        let parent_sources: std::collections::HashSet<TabSource> = tabs
            .iter()
            .filter_map(|t| match &t.source {
                TabSource::VfsEntry { parent, .. } => Some((**parent).clone()),
                _ => None,
            })
            .collect();

        let mut surviving: Vec<crate::state::OpenTabState> = Vec::new();
        for tab in tabs {
            let must_mount = parent_sources.contains(&tab.source);
            let result = self.restore_one_tab(&tab, must_mount);
            match result {
                Ok(()) => surviving.push(tab),
                Err(e) => {
                    tracing::warn!(error = %e, "restore open tab");
                }
            }
        }
        self.state.write().open_tabs = surviving;
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn restore_one_tab(
        &mut self,
        tab: &crate::state::OpenTabState,
        must_mount: bool,
    ) -> Result<(), crate::file::FileOpenError> {
        // A parent of any persisted VfsEntry must restore as a
        // workspace so the children can find a mount; user-saved
        // workspace state forces the same path.
        let as_workspace = tab.as_workspace || must_mount;
        match &tab.source {
            TabSource::Filesystem(path) => {
                let bytes = std::fs::read(path)
                    .map_err(|source| crate::file::FileOpenError::Read { path: path.clone(), source })?;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                self.open(name, Some(tab.source.clone()), bytes, tab.selection, Some(tab.scroll_offset), as_workspace);
                Ok(())
            }
            TabSource::VfsEntry { parent, entry_path } => {
                let parent_mount = self.find_mount_for_source(parent.as_ref())
                    .ok_or_else(|| parent_missing(parent.as_ref()))?;
                let bytes = read_vfs_entry(&*parent_mount.fs, entry_path)
                    .map_err(|e| crate::file::FileOpenError::Read { path: entry_path.into(), source: e })?;
                let name = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(entry_path).to_owned();
                let target = self
                    .workspace_for_source(parent.as_ref())
                    .map(OpenTarget::Workspace)
                    .unwrap_or(OpenTarget::Toplevel);
                self.open_with_target(
                    name,
                    Some(tab.source.clone()),
                    bytes,
                    tab.selection,
                    Some(tab.scroll_offset),
                    target,
                );
                Ok(())
            }
            TabSource::Anonymous { id, title } => {
                let path = anonymous_file_path(*id).ok_or_else(|| crate::file::FileOpenError::Read {
                    path: std::path::PathBuf::from(format!("anonymous/{}", id.get())),
                    source: std::io::Error::other("no data dir"),
                })?;
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // Sidecar gone; fall back to a fresh zero buffer
                        // so the tab still opens rather than dropping the
                        // entry silently.
                        vec![0u8; ANONYMOUS_DEFAULT_SIZE]
                    }
                    Err(e) => {
                        return Err(crate::file::FileOpenError::Read { path, source: e });
                    }
                };
                self.open(title.clone(), Some(tab.source.clone()), bytes, tab.selection, Some(tab.scroll_offset), false);
                Ok(())
            }
            TabSource::PluginMount { plugin_name, token, title } => {
                let plugin = self
                    .plugin_handlers
                    .iter()
                    .find(|p| p.name() == plugin_name)
                    .cloned()
                    .ok_or_else(|| crate::file::FileOpenError::PluginMount {
                        plugin_name: plugin_name.clone(),
                        token: token.clone(),
                        reason: "plugin no longer installed".to_owned(),
                    })?;
                let mount = plugin.mount_by_token(token).map_err(|e| crate::file::FileOpenError::PluginMount {
                    plugin_name: plugin_name.clone(),
                    token: token.clone(),
                    reason: e.to_string(),
                })?;
                let mount_id = crate::file::MountId::new(self.next_mount_id);
                self.next_mount_id += 1;
                self.mounts.insert(
                    mount_id,
                    crate::file::MountedPlugin {
                        display_name: title.clone(),
                        plugin_name: plugin_name.clone(),
                        token: token.clone(),
                        mount: Arc::new(mount),
                    },
                );
                let _ = as_workspace; // plugin mount tabs always show the tree
                let _ = push_tool_tab(&mut self.dock, Tab::PluginMount(mount_id));
                if let Some(path) = self.dock.find_tab(&Tab::PluginMount(mount_id)) {
                    remove_welcome_from_leaf(&mut self.dock, path.surface, path.node);
                }
                Ok(())
            }
        }
    }

    /// If `id`'s source is a `TabSource::VfsEntry` whose mount has
    /// no writer, force the file's editor into Readonly and stamp
    /// the reason on the file. The user cannot then toggle to
    /// mutable from the lock icon -- saving is structurally
    /// impossible, not a soft hint. Filesystem-readonly stays soft
    /// (the user can still edit in-memory and save-as elsewhere).
    fn apply_readonly_for_source(&mut self, id: FileId) {
        let parent_source = match self.files.get(&id).and_then(|f| f.source_kind.as_ref()) {
            Some(TabSource::VfsEntry { parent, .. }) => (**parent).clone(),
            _ => return,
        };
        let parent_writable = self
            .find_mount_for_source(&parent_source)
            .map(|m| m.writer.is_some())
            // Mount missing right now (shouldn't happen at open time)
            // -- be conservative and leave the file alone rather
            // than force-locking it.
            .unwrap_or(true);
        if parent_writable {
            return;
        }
        if let Some(file) = self.files.get_mut(&id) {
            file.read_only_reason = Some(crate::file::ReadOnlyReason::VfsNoWriter);
            file.editor.set_edit_mode(crate::file::EditMode::Readonly);
        }
    }

    /// Locate the `MountedVfs` for the given source, regardless of
    /// where the mount lives -- workspace (file-rooted) or plugin
    /// (rootless). Returns `None` if no live mount currently provides
    /// that source. Plugin mounts only exist on desktop (the
    /// wasm-side runtime can't host wasmtime), but workspaces work
    /// everywhere -- so the function itself is universal and the
    /// `PluginMount` arm is the only desktop-only piece.
    fn find_mount_for_source(&self, source: &TabSource) -> Option<Arc<MountedVfs>> {
        match source {
            #[cfg(not(target_arch = "wasm32"))]
            TabSource::PluginMount { plugin_name, token, .. } => self
                .mounts
                .values()
                .find(|m| m.plugin_name == *plugin_name && m.token == *token)
                .map(|m| m.mount.clone()),
            #[cfg(target_arch = "wasm32")]
            TabSource::PluginMount { .. } => None,
            other => {
                let editor_id = self
                    .files
                    .iter()
                    .find_map(|(id, f)| (f.source_kind.as_ref() == Some(other)).then_some(*id))?;
                self.workspaces
                    .values()
                    .find(|w| w.editor_id == editor_id)
                    .map(|w| w.mount.clone())
            }
        }
    }

    /// Find the `WorkspaceId` whose editor file has the given source,
    /// if any. Used by VfsEntry restore to graft the entry into the
    /// parent's workspace's inner dock instead of opening it as a
    /// top-level tab.
    fn workspace_for_source(&self, source: &TabSource) -> Option<crate::file::WorkspaceId> {
        let editor_id = self
            .files
            .iter()
            .find_map(|(id, f)| (f.source_kind.as_ref() == Some(source)).then_some(*id))?;
        self.workspaces
            .values()
            .find(|w| w.editor_id == editor_id)
            .map(|w| w.id)
    }

    /// `app.open` plus an explicit target: top-level dock leaf or a
    /// specific workspace's inner dock. Used by VfsEntry restore +
    /// runtime VFS-tree clicks to push entries inside their parent
    /// workspace rather than fragmenting them out as siblings.
    pub fn open_with_target(
        &mut self,
        display_name: impl Into<String>,
        source_kind: Option<TabSource>,
        bytes: Vec<u8>,
        restore_selection: Option<hxy_core::Selection>,
        restore_scroll: Option<f32>,
        target: OpenTarget,
    ) -> FileId {
        match target {
            OpenTarget::Toplevel => {
                self.open(display_name, source_kind, bytes, restore_selection, restore_scroll, false)
            }
            OpenTarget::Workspace(workspace_id) => {
                let id = self.create_open_file(
                    display_name,
                    source_kind.clone(),
                    bytes,
                    restore_selection,
                    restore_scroll,
                );
                self.apply_readonly_for_source(id);
                if let Some(workspace) = self.workspaces.get_mut(&workspace_id) {
                    push_workspace_entry(workspace, id);
                } else {
                    // Workspace vanished between schedule and apply.
                    // Fall back to a top-level tab so the user doesn't
                    // lose the file.
                    self.dock.push_to_focused_leaf(Tab::File(id));
                }
                if let Some(source) = source_kind {
                    let mut g = self.state.write();
                    if !g.open_tabs.iter().any(|t| t.source == source) {
                        g.open_tabs.push(crate::state::OpenTabState {
                            source,
                            selection: restore_selection,
                            scroll_offset: restore_scroll.unwrap_or(0.0),
                            as_workspace: false,
                        });
                    }
                }
                id
            }
        }
    }

    /// Save the current state if it has drifted from what was last written.
    /// No-op on wasm (no sink yet).
    fn save_if_dirty(&mut self, snapshot_before: &PersistedState) {
        let after = self.state.read().clone();
        if *snapshot_before == after {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(sink) = &self.sink {
            if let Err(e) = sink.save(&after) {
                tracing::warn!(error = %e, "save persisted state");
            } else {
                self.last_saved_window = Some(after.window);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl crate::plugin_runner::Logger for HxyApp {
    fn log(&mut self, severity: ConsoleSeverity, context: String, message: String) {
        self.console_log(severity, context, message);
    }
}

impl eframe::App for HxyApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snapshot_before = self.state.read().clone();

        // Drain any background-threaded plugin operations that
        // completed since the previous frame. Outcomes dispatch
        // through the same paths the synchronous calls used to take
        // (palette dispatch, mount-tab open) plus a "completed in N
        // ms" log entry.
        #[cfg(not(target_arch = "wasm32"))]
        self.drain_pending_plugin_ops(ui.ctx());

        #[cfg(target_os = "macos")]
        drain_native_menu(ui.ctx(), self);
        #[cfg(target_os = "macos")]
        sync_native_menu_state(self);

        #[cfg(not(target_os = "macos"))]
        top_menu_bar(ui, self);

        // Pre-read the 16-byte window at the active file's caret so
        // the Inspector tab can render without needing to reborrow
        // self.files while the dock is rendering.
        #[cfg(not(target_arch = "wasm32"))]
        let inspector_data = snapshot_inspector_bytes(self);

        {
            let mut state_guard = self.state.write();
            let mut viewer = HxyTabViewer {
                files: &mut self.files,
                state: &mut state_guard,
                console: &self.console,
                #[cfg(not(target_arch = "wasm32"))]
                mounts: &self.mounts,
                #[cfg(not(target_arch = "wasm32"))]
                pending_close_mount: &mut self.pending_close_mount,
                #[cfg(not(target_arch = "wasm32"))]
                global_search: &mut self.global_search,
                #[cfg(not(target_arch = "wasm32"))]
                pending_global_search_events: &mut self.pending_global_search_events,
                #[cfg(not(target_arch = "wasm32"))]
                inspector: &mut self.inspector,
                #[cfg(not(target_arch = "wasm32"))]
                decoders: &self.decoders,
                #[cfg(not(target_arch = "wasm32"))]
                inspector_data,
                #[cfg(not(target_arch = "wasm32"))]
                plugin_rescan: &mut self.plugin_rescan,
                #[cfg(not(target_arch = "wasm32"))]
                plugin_handlers: &self.plugin_handlers,
                #[cfg(not(target_arch = "wasm32"))]
                pending_plugin_events: &mut self.pending_plugin_events,
                pending_close_tab: &mut self.pending_close_tab,
                tab_focus: &mut self.tab_focus,
                workspaces: &mut self.workspaces,
                pending_close_workspace_entry: &mut self.pending_close_workspace_entry,
                pending_collapse_workspace: &mut self.pending_collapse_workspace,
            };
            let style = Style::from_egui(ui.style());
            DockArea::new(&mut self.dock)
                .style(style)
                .show_leaf_collapse_buttons(false)
                .show_inside(ui, &mut viewer);
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let events = std::mem::take(&mut self.pending_plugin_events);
            if !events.is_empty() {
                self.apply_plugin_events(events);
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        track_content_leaf(self);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(mount_id) = self.pending_close_mount.take()
            && let Some(removed) = self.mounts.remove(&mount_id)
        {
            let target = TabSource::PluginMount {
                plugin_name: removed.plugin_name,
                token: removed.token,
                title: removed.display_name,
            };
            self.state.write().open_tabs.retain(|t| t.source != target);
        }

        // Workspace entry close that hit a dirty file gets folded into
        // the regular pending-close-tab modal slot. The modal handler
        // already drives `close_file_tab_by_id`, which now also walks
        // workspace inner docks.
        if let Some(pending) = self.pending_close_workspace_entry.take()
            && self.pending_close_tab.is_none()
        {
            self.pending_close_tab = Some(pending);
        }

        // Collapse-back: any workspace whose inner dock now contains
        // only the Editor sub-tab gets unwrapped to a plain Tab::File.
        let to_collapse = std::mem::take(&mut self.pending_collapse_workspace);
        for workspace_id in to_collapse {
            collapse_workspace_to_file(self, workspace_id);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let events = std::mem::take(&mut self.pending_global_search_events);
            if !events.is_empty() {
                apply_global_search_events(self, events);
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        if std::mem::take(&mut self.plugin_rescan) {
            self.reload_plugins();
        }

        apply_zoom_change(ui.ctx(), &self.state, &mut self.applied_zoom);

        capture_window_on_drag_end(ui.ctx(), &self.state, &mut self.prev_window, &self.last_saved_window);

        paint_drop_overlay(ui.ctx());
        consume_dropped_files(ui.ctx(), self);
        consume_welcome_open_request(ui.ctx(), self);
        drain_pending_vfs_opens(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        drain_external_open_requests(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        drain_template_runs(ui.ctx(), self);
        // Visual pane picker takes priority over the palette and
        // any other keyboard consumer: while a pick is staged it
        // owns Escape (cancel) and a..z (target letters). It runs
        // after the dock has rendered so leaf rects are this
        // frame's, not last frame's.
        #[cfg(not(target_arch = "wasm32"))]
        handle_pane_pick(ui.ctx(), self);
        // Palette runs first so it gets first crack at keyboard
        // events. egui clears focus on plain Escape during its own
        // event preprocessing, so egui_wants_keyboard_input() reads
        // false by the time dispatch_hex_edit_keys runs -- if the
        // hex editor ran first it would drain Escape for its own
        // clear-selection handler before the palette could use it
        // to dismiss.
        #[cfg(not(target_arch = "wasm32"))]
        handle_command_palette(ui.ctx(), self);
        dispatch_copy_shortcut(ui.ctx(), self);
        dispatch_save_shortcut(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        dispatch_close_shortcut(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        dispatch_paste_shortcut(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        dispatch_find_shortcut(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        dispatch_focus_pane_shortcut(ui.ctx(), self);
        dispatch_tab_focus_toggle(ui.ctx(), self);
        dispatch_tab_cycle(ui.ctx(), self);
        dispatch_hex_edit_keys(ui.ctx(), self);
        render_duplicate_open_dialog(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        render_patch_restore_dialog(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        render_close_tab_dialog(ui.ctx(), self);

        self.save_if_dirty(&snapshot_before);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn on_exit(&mut self) {
        // Persist every dirty tab's patch to a sidecar so restart
        // can offer to restore it. Best-effort: errors only log.
        if let Some(dir) = unsaved_edits_dir() {
            for file in self.files.values() {
                let Some(path) = file.root_path().cloned() else { continue };
                if !file.editor.is_dirty() {
                    // Clear any lingering sidecar from a previous session
                    // -- the in-memory state for this file is clean now.
                    let _ = crate::patch_persist::discard(&dir, &path);
                    continue;
                }
                let patch = file.editor.patch().read().expect("patch lock poisoned").clone();
                let Some(sidecar) = crate::patch_persist::snapshot(
                    path.clone(),
                    file.editor.source().as_ref(),
                    patch,
                    file.editor.undo_stack().to_vec(),
                    file.editor.redo_stack().to_vec(),
                ) else {
                    continue;
                };
                if let Err(e) = crate::patch_persist::store(&dir, &sidecar) {
                    tracing::warn!(error = %e, path = %path.display(), "store patch sidecar");
                } else {
                    tracing::info!(path = %path.display(), "saved unsaved-edits sidecar");
                }
            }
        }

        // Anonymous (scratch) tabs have no on-disk origin, so the
        // full patched buffer is what we persist. One file per tab
        // under `anonymous_files_dir()`, keyed by the tab's
        // AnonymousId.
        for file in self.files.values() {
            let Some(TabSource::Anonymous { id, .. }) = file.source_kind.as_ref() else { continue };
            let Some(path) = anonymous_file_path(*id) else { continue };
            let len = file.editor.source().len().get();
            let bytes = if len == 0 {
                Vec::new()
            } else {
                let range = match hxy_core::ByteRange::new(
                    hxy_core::ByteOffset::new(0),
                    hxy_core::ByteOffset::new(len),
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(error = %e, "anonymous tab range invalid");
                        continue;
                    }
                };
                match file.editor.source().read(range) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = %e, "read anonymous tab bytes");
                        continue;
                    }
                }
            };
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&path, &bytes) {
                tracing::warn!(error = %e, path = %path.display(), "write anonymous tab");
            }
        }
    }
}

/// Modal dialog shown when a user-facing open request hit a path
/// that's already open in another tab. Offers to focus the existing
/// tab, open a duplicate, or cancel. Held in `app.pending_duplicate`
/// so the decision survives across frames.
fn render_duplicate_open_dialog(ctx: &egui::Context, app: &mut HxyApp) {
    if app.pending_duplicate.is_none() {
        return;
    }
    // Local copy of the display name so we can borrow `app` mutably
    // inside the modal body.
    let (name, path_display) = {
        let p = app.pending_duplicate.as_ref().unwrap();
        (p.display_name.clone(), p.path.display().to_string())
    };

    let mut action: Option<DuplicateAction> = None;
    let mut open = true;
    egui::Window::new(hxy_i18n::t("duplicate-open-title"))
        .id(egui::Id::new("hxy_duplicate_open_dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(hxy_i18n::t("duplicate-open-body"));
            ui.add_space(4.0);
            ui.label(egui::RichText::new(&name).strong());
            ui.label(egui::RichText::new(&path_display).weak());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(hxy_i18n::t("duplicate-open-focus")).clicked() {
                    action = Some(DuplicateAction::Focus);
                }
                if ui.button(hxy_i18n::t("duplicate-open-new-tab")).clicked() {
                    action = Some(DuplicateAction::OpenNewTab);
                }
                if ui.button(hxy_i18n::t("duplicate-open-cancel")).clicked() {
                    action = Some(DuplicateAction::Cancel);
                }
            });
        });

    // Closing the window via its X button counts as cancel.
    if !open && action.is_none() {
        action = Some(DuplicateAction::Cancel);
    }

    let Some(action) = action else { return };
    let pending = app.pending_duplicate.take().unwrap();
    match action {
        DuplicateAction::Focus => {
            app.focus_file_tab(pending.existing);
        }
        DuplicateAction::OpenNewTab => {
            app.open_filesystem(pending.display_name, pending.path, pending.bytes, None, None);
        }
        DuplicateAction::Cancel => {}
    }
}

enum DuplicateAction {
    Focus,
    OpenNewTab,
    Cancel,
}

#[cfg(not(target_arch = "wasm32"))]
fn render_patch_restore_dialog(ctx: &egui::Context, app: &mut HxyApp) {
    use crate::patch_persist::RestoreIntegrity;

    if app.pending_patch_restore.is_none() {
        return;
    }
    let (path_display, op_count, integrity) = {
        let p = app.pending_patch_restore.as_ref().unwrap();
        (p.sidecar.source_path.display().to_string(), p.sidecar.patch.len(), p.integrity.clone())
    };

    let mut action: Option<RestoreAction> = None;
    let mut open = true;
    egui::Window::new(hxy_i18n::t("restore-patch-title"))
        .id(egui::Id::new("hxy_restore_patch_dialog"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(hxy_i18n::t_args("restore-patch-body", &[("ops", &op_count.to_string())]));
            ui.label(egui::RichText::new(&path_display).weak());

            // Warning banner for anything other than a clean match.
            // Clean sidecars get the short path; modified / unknown
            // ones get a yellow-highlighted reason plus a worded
            // "restore anyway" button so the user can't miss the
            // risk they're taking.
            match &integrity {
                RestoreIntegrity::Clean => {}
                RestoreIntegrity::Modified { reason } => {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(hxy_i18n::t("restore-patch-warn-modified"))
                            .color(ui.visuals().warn_fg_color)
                            .strong(),
                    );
                    ui.label(egui::RichText::new(reason).weak());
                }
                RestoreIntegrity::Unknown { reason } => {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(hxy_i18n::t("restore-patch-warn-unknown"))
                            .color(ui.visuals().warn_fg_color)
                            .strong(),
                    );
                    ui.label(egui::RichText::new(reason).weak());
                }
            }

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let restore_label = match &integrity {
                    RestoreIntegrity::Clean => hxy_i18n::t("restore-patch-restore"),
                    _ => hxy_i18n::t("restore-patch-restore-anyway"),
                };
                if ui.button(restore_label).clicked() {
                    action = Some(RestoreAction::Restore);
                }
                if ui.button(hxy_i18n::t("restore-patch-discard")).clicked() {
                    action = Some(RestoreAction::Discard);
                }
            });
        });
    // Closing via the X is "decide later": leave the patch on disk
    // and re-prompt next time the file is opened.
    if !open {
        app.pending_patch_restore = None;
        return;
    }
    let Some(action) = action else { return };

    let pending = app.pending_patch_restore.take().unwrap();
    let path = pending.sidecar.source_path.clone();
    let dir = unsaved_edits_dir();
    match action {
        RestoreAction::Restore => {
            let ctx_label = format!("Restore {}", path.display());
            let integrity_clean = matches!(pending.integrity, RestoreIntegrity::Clean);
            // Gather the decision + any log lines without holding a
            // live borrow on `app`, so we can hand the patch to
            // `file` and then fire `console_log` (which also
            // borrows `app`) in sequence.
            let mut log_lines: Vec<(ConsoleSeverity, String)> = Vec::new();
            let accept = if let Some(file) = app.files.get_mut(&pending.file_id) {
                // Clean sidecar: re-verify the full SourceMetadata
                // (including BLAKE3 digest if recorded) so a subtle
                // tamper still aborts.  Modified / Unknown: user
                // already opted in, skip verification and adopt.
                let verified = if integrity_clean {
                    let len = file.editor.source().len().get();
                    match file.editor.source().read(
                        hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len))
                            .expect("range valid"),
                    ) {
                        Ok(bytes) => match pending.sidecar.metadata.verify(&bytes) {
                            Ok(()) => true,
                            Err(e) => {
                                log_lines.push((
                                    ConsoleSeverity::Warning,
                                    format!("source verification failed; not restoring: {e}"),
                                ));
                                false
                            }
                        },
                        Err(e) => {
                            log_lines.push((ConsoleSeverity::Error, format!("re-read source: {e}")));
                            false
                        }
                    }
                } else {
                    true
                };

                if verified {
                    *file.editor.patch().write().expect("patch lock poisoned") = pending.sidecar.patch;
                    file.editor.set_undo_stack(pending.sidecar.undo_stack);                    file.editor.set_redo_stack(pending.sidecar.redo_stack);                    file.editor.push_history_boundary();                    file.editor.set_edit_mode(crate::file::EditMode::Mutable);                    if integrity_clean {
                        log_lines.push((ConsoleSeverity::Info, "restored unsaved edits".to_owned()));
                    } else {
                        log_lines.push((
                            ConsoleSeverity::Warning,
                            "restored unsaved edits onto a file whose on-disk state has changed".to_owned(),
                        ));
                    }
                }
                verified
            } else {
                false
            };
            let _ = accept;
            for (severity, message) in log_lines {
                app.console_log(severity, &ctx_label, message);
            }
            if let Some(dir) = dir {
                let _ = crate::patch_persist::discard(&dir, &path);
            }
        }
        RestoreAction::Discard => {
            if let Some(dir) = dir {
                let _ = crate::patch_persist::discard(&dir, &path);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
enum RestoreAction {
    Restore,
    Discard,
}

/// App-level keypress -> nibble write + arrow-key cursor navigation
/// dispatcher. Runs late in the frame so other widgets (palette
/// text input, settings fields, dialogs) get first crack at typed
/// keys via egui's normal focus path; only un-consumed presses
/// reach the active hex-edit cursor.
fn dispatch_hex_edit_keys(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else { return };
    if let Some(file) = app.files.get_mut(&id) {
        file.editor.handle_input(ctx);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn dispatch_save_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    let (new_file, save, save_as, toggle, undo, redo) = ctx.input_mut(|i| {
        (
            i.consume_shortcut(&NEW_FILE),
            i.consume_shortcut(&SAVE_FILE),
            i.consume_shortcut(&SAVE_FILE_AS),
            i.consume_shortcut(&TOGGLE_EDIT_MODE),
            i.consume_shortcut(&UNDO),
            i.consume_shortcut(&REDO),
        )
    });
    if new_file {
        handle_new_file(app);
    }
    if save_as {
        save_active_file(app, true);
    } else if save {
        save_active_file(app, false);
    }
    if toggle {
        toggle_active_edit_mode(app);
    }
    if redo {
        redo_active_file(app);
    } else if undo {
        undo_active_file(app);
    }
}

#[cfg(target_arch = "wasm32")]
fn dispatch_save_shortcut(_ctx: &egui::Context, _app: &mut HxyApp) {}

/// Clipboard paste dispatcher. Consumes Cmd+V and Cmd+Shift+V plus any
/// matching `Event::Paste` eframe auto-generated, reads the clipboard
/// through `arboard`, parses as hex when the shift variant fired, and
/// writes the result at the active tab's cursor.
#[cfg(not(target_arch = "wasm32"))]
fn dispatch_paste_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    if ctx.egui_wants_keyboard_input() {
        return;
    }
    let (paste, paste_hex, paste_event_text) = ctx.input_mut(|i| {
        let paste = i.consume_shortcut(&PASTE);
        let paste_hex = i.consume_shortcut(&PASTE_AS_HEX);
        // Drain any Event::Paste too: eframe generates one on plain
        // Cmd+V in addition to the Key event, so consuming only the
        // shortcut leaves the text event behind.
        let mut event_text = None;
        i.events.retain(|event| {
            if let egui::Event::Paste(text) = event
                && event_text.is_none()
            {
                event_text = Some(text.clone());
                return false;
            }
            true
        });
        (paste, paste_hex, event_text)
    });
    if !paste && !paste_hex {
        return;
    }
    let Some(id) = active_file_id(app) else { return };
    let Some(file) = app.files.get_mut(&id) else { return };
    if file.editor.edit_mode() != crate::file::EditMode::Mutable {
        return;
    }
    let text = match paste_event_text {
        Some(t) if !t.is_empty() => t,
        _ => match crate::paste::read_text() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "read clipboard");
                return;
            }
        },
    };
    let bytes = if paste_hex {
        match crate::paste::parse_hex_clipboard(&text) {
            Ok(b) => b,
            Err(e) => {
                app.console_log(
                    ConsoleSeverity::Warning,
                    "Paste as hex",
                    format!("clipboard text is not valid hex: {e}"),
                );
                return;
            }
        }
    } else {
        text.into_bytes()
    };
    if bytes.is_empty() {
        return;
    }
    let Some(file) = app.files.get_mut(&id) else { return };
    paste_bytes_at_cursor(file, bytes);
}

/// Apply a paste buffer at the tab's cursor. Length-preserving: the
/// write is truncated to what fits before EOF, leaves an empty
/// clipboard as a no-op, and parks the caret just past the last
/// written byte so the next paste / keystroke lands after it.
#[cfg(not(target_arch = "wasm32"))]
fn paste_bytes_at_cursor(file: &mut crate::file::OpenFile, bytes: Vec<u8>) {
    let source_len = file.editor.source().len().get();
    if source_len == 0 {
        return;
    }
    let start = file.editor.selection().map(|s| s.range().start().get()).unwrap_or(0);
    let available = source_len.saturating_sub(start);
    if available == 0 {
        return;
    }
    let n = (bytes.len() as u64).min(available) as usize;
    let bytes = if n == bytes.len() { bytes } else { bytes[..n].to_vec() };
    file.editor.push_history_boundary();
    if let Err(e) = file.editor.request_write(start, bytes) {
        tracing::warn!(error = %e, "paste write");
        return;
    }
    let new_cursor = (start + n as u64).min(source_len.saturating_sub(1));
    file.editor.set_selection(Some(hxy_core::Selection::caret(hxy_core::ByteOffset::new(new_cursor))));    file.editor.reset_edit_nibble();
    file.editor.push_history_boundary();
}

/// App-level copy shortcut handler. Runs after the dock renders, so
/// per-widget hover-copy (status bar labels) has already had a chance
/// to consume the event. Whatever's left dispatches to the currently
/// active file tab.
fn dispatch_copy_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    let kind = ctx.input_mut(|i| {
        if i.consume_shortcut(&COPY_HEX) {
            Some(CopyKind::BytesHexSpaced)
        } else if consume_copy_event(i) {
            Some(CopyKind::BytesLossyUtf8)
        } else {
            None
        }
    });
    let Some(kind) = kind else { return };
    let Some(id) = active_file_id(app) else { return };
    if let Some(file) = app.files.get(&id) {
        do_copy(ctx, file, kind);
    }
}

/// Consume the plain "copy" shortcut in all the forms the integration
/// might deliver it: as an `Event::Copy` (winit on macOS converts Cmd+C
/// to a semantic copy event), or as a normal `Event::Key` with the
/// Command modifier on platforms that pass it through.
fn consume_copy_event(input: &mut egui::InputState) -> bool {
    // winit on macOS sends Cmd+C as BOTH an `Event::Copy` (the
    // semantic copy) AND a regular Cmd+C `Event::Key`. A previous
    // version of this function returned after draining the semantic
    // form, which left the Key event for the hex-view's dispatcher
    // to grab -- so the status-bar label would copy its value and
    // the hex view would immediately overwrite the clipboard with
    // the current selection. Drain BOTH so a single "copy" click
    // produces one clipboard write.
    let mut any = false;
    let before = input.events.len();
    input.events.retain(|e| !matches!(e, egui::Event::Copy));
    if input.events.len() != before {
        any = true;
    }
    if input.consume_shortcut(&COPY_BYTES) {
        any = true;
    }
    any
}

fn consume_welcome_open_request(ctx: &egui::Context, app: &mut HxyApp) {
    let req = ctx.data_mut(|d| d.remove_temp::<std::path::PathBuf>(egui::Id::new(WELCOME_OPEN_RECENT)));
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(path) = req {
        match std::fs::read(&path) {
            Ok(bytes) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                app.request_open_filesystem(name, path, bytes);
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "open recent file");
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    let _ = (req, app);
}

fn paint_drop_overlay(ctx: &egui::Context) {
    let hovered_count = ctx.input(|i| i.raw.hovered_files.len());
    if hovered_count == 0 {
        return;
    }
    let text = ctx.input(|i| {
        if i.raw.hovered_files.len() > 1 {
            return "Drop one file at a time".to_owned();
        }
        let Some(file) = i.raw.hovered_files.first() else {
            return "Drop a file".to_owned();
        };
        match file.path.as_deref().and_then(|p| p.file_name()).and_then(|n| n.to_str()) {
            Some(name) => format!("Drop to open\n{name}"),
            None => "Drop to open".to_owned(),
        }
    });
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("hxy_drop_target")));
    let screen = ctx.content_rect();
    painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(192));
    painter.text(
        screen.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::TextStyle::Heading.resolve(&ctx.global_style()),
        egui::Color32::WHITE,
    );
}

/// Drain CLI paths captured at launch and any path batches forwarded
/// by second-instance invocations over the IPC socket. Both routes
/// land here so the open path is identical -- read bytes, hand the
/// file off to the same `request_open_filesystem` the file dialog
/// uses (which dedupes via the existing duplicate-open modal).
#[cfg(not(target_arch = "wasm32"))]
fn drain_external_open_requests(ctx: &egui::Context, app: &mut HxyApp) {
    let mut batch = std::mem::take(&mut app.pending_cli_paths);
    if let Some(inbox) = app.ipc_inbox.as_ref() {
        for forwarded in inbox.read(ctx) {
            // A second-instance invocation may try to raise the
            // running window to the front. eframe doesn't expose a
            // direct "focus the OS window" call we can rely on
            // cross-platform, but a request_repaint is cheap and
            // ensures the new tab paints right away.
            ctx.request_repaint();
            batch.extend(forwarded);
        }
    }
    for path in batch {
        match std::fs::read(&path) {
            Ok(bytes) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                app.request_open_filesystem(name, path, bytes);
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "open external path");
            }
        }
    }
}

fn consume_dropped_files(ctx: &egui::Context, app: &mut HxyApp) {
    let dropped = ctx.input(|i| i.raw.dropped_files.clone());
    for file in dropped {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = file.path {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    app.request_open_filesystem(name, path, bytes);
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "open dropped file");
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        if let Some(bytes) = file.bytes.as_deref() {
            if !bytes.is_empty() {
                let name = if file.name.is_empty() { "dropped".to_string() } else { file.name.clone() };
                app.open_in_memory(name, bytes.to_vec());
            }
        }
    }
}

/// Two-way sync between `settings.zoom_factor` and egui's own zoom.
/// The user can change zoom from the Settings slider (settings ->
/// context) or via Cmd+= / Cmd+- / Cmd+0 (context -> settings). The
/// direction is determined by comparing both against `applied`, the
/// value we last pushed in either direction.
fn apply_zoom_change(ctx: &egui::Context, state: &SharedPersistedState, applied: &mut f32) {
    let target = state.read().app.zoom_factor;
    let ctx_zoom = ctx.zoom_factor();
    let setting_drift = (target - *applied).abs() > f32::EPSILON;
    let ctx_drift = (ctx_zoom - *applied).abs() > f32::EPSILON;
    if setting_drift {
        ctx.set_zoom_factor(target);
        *applied = target;
    } else if ctx_drift {
        state.write().app.zoom_factor = ctx_zoom;
        *applied = ctx_zoom;
    }
}

/// Read the current viewport's window geometry; push it into the shared
/// state only when geometry has been stable for at least one frame and
/// differs from the last persisted value. This is the drag-end signal.
fn capture_window_on_drag_end(
    ctx: &egui::Context,
    state: &SharedPersistedState,
    prev_window: &mut Option<WindowSettings>,
    last_saved_window: &Option<WindowSettings>,
) {
    let zoom = state.read().app.zoom_factor;
    let current = ctx
        .input(|i| i.raw.viewports.get(&i.raw.viewport_id).map(|info| WindowSettings::from_viewport_info(info, zoom)));
    let Some(current) = current else {
        return;
    };
    let stable = prev_window.as_ref() == Some(&current);
    *prev_window = Some(current);
    if !stable {
        return;
    }
    if last_saved_window.as_ref() == Some(&current) {
        return;
    }
    let mut g = state.write();
    if g.window != current {
        g.window = current;
    }
}

/// Key used to stash pending VFS-entry open requests between tab
/// rendering (which only has `&mut PersistedState`) and the app-level
/// drain loop (which can open new tabs).
const PENDING_VFS_OPEN_KEY: &str = "hxy_pending_vfs_open";

/// One pending "open this entry as a new tab" request, queued from a
/// VFS panel during render. `Workspace` carries a `WorkspaceId` (the
/// file-rooted workspaces like zip / minidump); `PluginMount` carries
/// a `MountId` (plugin VFS tabs whose mount lives in `app.mounts`,
/// not in any file).
#[derive(Clone, Debug)]
pub enum PendingVfsOpen {
    Workspace { workspace_id: crate::file::WorkspaceId, entry_path: String },
    #[cfg(not(target_arch = "wasm32"))]
    PluginMount { mount_id: crate::file::MountId, entry_path: String },
}

#[cfg(not(target_arch = "wasm32"))]
fn drain_pending_vfs_opens(ctx: &egui::Context, app: &mut HxyApp) {
    let pending: Vec<PendingVfsOpen> = ctx
        .data_mut(|d| d.remove_temp::<Vec<PendingVfsOpen>>(egui::Id::new(PENDING_VFS_OPEN_KEY)))
        .unwrap_or_default();
    for req in pending {
        match req {
            PendingVfsOpen::Workspace { workspace_id, entry_path } => {
                let Some(workspace) = app.workspaces.get(&workspace_id) else { continue };
                let parent_id = workspace.editor_id;
                let mount = workspace.mount.clone();
                let parent_source = match app.files.get(&parent_id).and_then(|f| f.source_kind.clone()) {
                    Some(s) => s,
                    None => continue,
                };
                let bytes = match read_vfs_entry(&*mount.fs, &entry_path) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = %e, entry = %entry_path, "open vfs entry");
                        continue;
                    }
                };
                let name = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&entry_path).to_owned();
                let source = TabSource::VfsEntry { parent: Box::new(parent_source), entry_path };
                app.open_with_target(name, Some(source), bytes, None, None, OpenTarget::Workspace(workspace_id));
            }
            PendingVfsOpen::PluginMount { mount_id, entry_path } => {
                let Some(entry) = app.mounts.get(&mount_id) else { continue };
                let parent_source = TabSource::PluginMount {
                    plugin_name: entry.plugin_name.clone(),
                    token: entry.token.clone(),
                    title: entry.display_name.clone(),
                };
                let mount = entry.mount.clone();
                let bytes = match read_vfs_entry(&*mount.fs, &entry_path) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(error = %e, entry = %entry_path, "open vfs entry");
                        continue;
                    }
                };
                let name = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&entry_path).to_owned();
                let source = TabSource::VfsEntry { parent: Box::new(parent_source), entry_path };
                // The click happened in the tool panel, so focus is
                // there too. Move focus back to the editing area
                // before `open` -- it routes via push_to_focused_leaf.
                focus_content_leaf(app);
                app.open(name, Some(source), bytes, None, None, false);
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn drain_pending_vfs_opens(_ctx: &egui::Context, _app: &mut HxyApp) {}

fn apply_command_effect(ctx: &egui::Context, app: &mut HxyApp, effect: crate::commands::CommandEffect) {
    use crate::commands::CommandEffect;
    match effect {
        CommandEffect::OpenFileDialog => handle_open_file(app),
        CommandEffect::MountActiveFile => mount_active_file(app),
        CommandEffect::RunTemplateDialog => {
            #[cfg(not(target_arch = "wasm32"))]
            run_template_dialog(ctx, app);
        }
        CommandEffect::RunTemplateDirect(path) => {
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(id) = active_file_id(app) {
                run_template_from_path(ctx, app, id, path);
            }
            #[cfg(target_arch = "wasm32")]
            let _ = path;
        }
        CommandEffect::OpenRecent(path) => {
            #[cfg(not(target_arch = "wasm32"))]
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    app.request_open_filesystem(name, path, bytes);
                }
                Err(e) => tracing::warn!(error = %e, path = %path.display(), "open recent"),
            }
            #[cfg(target_arch = "wasm32")]
            let _ = path;
        }
        CommandEffect::UndoActiveFile => {
            #[cfg(not(target_arch = "wasm32"))]
            undo_active_file(app);
        }
        CommandEffect::RedoActiveFile => {
            #[cfg(not(target_arch = "wasm32"))]
            redo_active_file(app);
        }
        CommandEffect::DockSplit(dir) => dock_split_focused(app, dir),
        CommandEffect::DockMerge(dir) => dock_merge_focused(app, dir),
        CommandEffect::DockMoveTab(dir) => dock_move_focused_tab(app, dir),
    }
}

/// Resolve which dock leaf a split / merge command should act on.
/// Prefers the focused leaf; falls back to the leaf containing the
/// active file so the user doesn't have to click into a tab first.
fn resolve_target_leaf(app: &mut HxyApp) -> Option<egui_dock::NodePath> {
    if let Some(path) = app.dock.focused_leaf() {
        return Some(path);
    }
    let id = active_file_id(app)?;
    let tab = app.dock.find_tab(&Tab::File(id))?;
    Some(egui_dock::NodePath { surface: tab.surface, node: tab.node })
}

/// Split the target leaf in `dir` and seed the new pane with a
/// fresh Welcome placeholder. The new leaf becomes focused so the
/// next file the user opens (or tab they drag in) lands there and
/// replaces the placeholder. Duplicating the focused tab instead
/// would clone its identity (e.g. two `Tab::File(id)` pointing at
/// the same underlying file) and break close-tab semantics.
fn dock_split_focused(app: &mut HxyApp, dir: crate::commands::DockDir) {
    use crate::commands::DockDir;
    let Some(path) = resolve_target_leaf(app) else { return };
    let tree = &mut app.dock[path.surface];
    let [_, new_node] = match dir {
        DockDir::Right => tree.split_right(path.node, 0.5, vec![Tab::Welcome]),
        DockDir::Left => tree.split_left(path.node, 0.5, vec![Tab::Welcome]),
        DockDir::Up => tree.split_above(path.node, 0.5, vec![Tab::Welcome]),
        DockDir::Down => tree.split_below(path.node, 0.5, vec![Tab::Welcome]),
    };
    app.dock.set_focused_node_and_surface(egui_dock::NodePath { surface: path.surface, node: new_node });
}

/// Collapse the target leaf into its neighbour on `dir`: move every
/// tab into the neighbour's leaf and let egui_dock drop the now-
/// empty leaf + collapse the parent split. No-op when there's no
/// neighbour on that side (e.g. merge-left from the leftmost pane).
fn dock_merge_focused(app: &mut HxyApp, dir: crate::commands::DockDir) {
    let Some(path) = resolve_target_leaf(app) else { return };
    let tree = &app.dock[path.surface];
    let Some(neighbor_node) = find_neighbor_leaf(tree, path.node, dir) else { return };
    let target = egui_dock::NodePath { surface: path.surface, node: neighbor_node };
    dock_merge_to(app, path, target);
}

/// Pour every tab from `source` into `target`, then remove `source`
/// so the parent split collapses. Operations across surfaces are
/// supported -- each surface's tree is mutated independently. No-op
/// when source equals target or source has no tabs.
fn dock_merge_to(app: &mut HxyApp, source: egui_dock::NodePath, target: egui_dock::NodePath) {
    if source == target {
        return;
    }
    let tabs: Vec<_> = match &mut app.dock[source.surface][source.node] {
        egui_dock::Node::Leaf(leaf) => std::mem::take(&mut leaf.tabs),
        _ => return,
    };
    if tabs.is_empty() {
        return;
    }
    let moved_real_tab = tabs.iter().any(|t| !matches!(t, Tab::Welcome));
    // Stash one of the tabs we're about to move so we can find the
    // destination leaf again after remove_leaf -- it rewires node
    // indices, so `target` is not safe to index into the tree
    // after the remove. Looking it up by tab is robust.
    let refocus_tab = tabs[0];
    for tab in tabs {
        app.dock[target.surface][target.node].append_tab(tab);
    }
    if moved_real_tab {
        remove_welcome_from_leaf(&mut app.dock, target.surface, target.node);
    }
    app.dock[source.surface].remove_leaf(source.node);
    if let Some(found) = app.dock.find_tab(&refocus_tab) {
        app.dock.set_focused_node_and_surface(egui_dock::NodePath { surface: found.surface, node: found.node });
    }
}

/// Pop just the focused tab from its leaf and append it to the
/// neighbour leaf in `dir`. Sibling tabs stay where they are -- this
/// is the single-tab counterpart to [`dock_merge_focused`]. If the
/// source leaf ends up empty after the move it gets removed the same
/// way merge does so the parent split collapses.
fn dock_move_focused_tab(app: &mut HxyApp, dir: crate::commands::DockDir) {
    let Some(path) = resolve_target_leaf(app) else { return };
    let tree = &app.dock[path.surface];
    let Some(neighbor_node) = find_neighbor_leaf(tree, path.node, dir) else { return };
    let target = egui_dock::NodePath { surface: path.surface, node: neighbor_node };
    dock_move_tab_to(app, path, target);
}

/// Move just the source leaf's active tab into `target`. Sibling
/// tabs stay put. If the source leaf ends up empty it's removed so
/// the parent split collapses, the same way [`dock_merge_to`] does
/// when the merge drains the leaf. Cross-surface moves are supported.
fn dock_move_tab_to(app: &mut HxyApp, source: egui_dock::NodePath, target: egui_dock::NodePath) {
    if source == target {
        return;
    }
    let moved_tab = match &mut app.dock[source.surface][source.node] {
        egui_dock::Node::Leaf(leaf) => {
            if leaf.tabs.is_empty() {
                return;
            }
            let idx = leaf.active.0.min(leaf.tabs.len().saturating_sub(1));
            let tab = leaf.tabs.remove(idx);
            // Keep the source leaf's active selection sane after the
            // remove: clamp to the new last index, falling back to 0
            // when the leaf is now empty (it'll be removed below).
            if !leaf.tabs.is_empty() {
                let new_active = idx.min(leaf.tabs.len() - 1);
                leaf.active = egui_dock::TabIndex(new_active);
            }
            tab
        }
        _ => return,
    };
    let refocus_tab = moved_tab;
    let moved_real_tab = !matches!(moved_tab, Tab::Welcome);
    app.dock[target.surface][target.node].append_tab(moved_tab);
    if moved_real_tab {
        remove_welcome_from_leaf(&mut app.dock, target.surface, target.node);
    }
    let source_empty =
        matches!(&app.dock[source.surface][source.node], egui_dock::Node::Leaf(leaf) if leaf.tabs.is_empty());
    if source_empty {
        app.dock[source.surface].remove_leaf(source.node);
    }
    // Refocus on the moved tab. `target` may have been rewired by
    // remove_leaf, so look it up by the moved tab itself.
    if let Some(found) = app.dock.find_tab(&refocus_tab) {
        app.dock.set_focused_node_and_surface(egui_dock::NodePath { surface: found.surface, node: found.node });
    }
}

/// Remove any [`Tab::Welcome`] entry from the given leaf. Used by
/// the file-open and tab-move paths so the Welcome placeholder
/// quietly steps aside whenever a real tab takes over its pane.
/// Toggle visibility of the right-hand tool panel (the Plugins
/// manager and any plugin mount tabs). When visible, drains every
/// tool-class tab out of the dock into `hidden_tool_tabs`. The
/// now-empty leaf is removed by egui_dock and adjacent panes reflow
/// to take the space. When hidden, recreates the right-split leaf
/// at the standard 28% width and refills it from the stash.
#[cfg(not(target_arch = "wasm32"))]
fn toggle_tool_panel(app: &mut HxyApp) {
    if !app.hidden_tool_tabs.is_empty() {
        // Restore.
        let to_restore = std::mem::take(&mut app.hidden_tool_tabs);
        let mut iter = to_restore.into_iter();
        let Some(first) = iter.next() else { return };
        app.dock.main_surface_mut().split_right(egui_dock::NodeIndex::root(), 0.72, vec![first]);
        if let Some(path) = app.dock.find_tab(&first) {
            for tab in iter {
                if let Ok(leaf) = app.dock.leaf_mut(path.node_path()) {
                    leaf.append_tab(tab);
                }
            }
        }
        return;
    }
    // Hide. Walk every tool-class tab; collect them in dock order so
    // restore preserves the visual sequence the user had. Remove
    // from highest tab index per leaf first to avoid index shifts
    // invalidating subsequent paths.
    let mut to_hide: Vec<(egui_dock::TabPath, Tab)> = Vec::new();
    for (path, tab) in app.dock.iter_all_tabs() {
        if is_tool_tab(tab) {
            to_hide.push((path, *tab));
        }
    }
    if to_hide.is_empty() {
        return;
    }
    // Sort descending so removing earlier indices doesn't shift later ones.
    to_hide.sort_by(|a, b| b.0.tab.0.cmp(&a.0.tab.0));
    let stash: Vec<Tab> = to_hide.iter().rev().map(|(_, t)| *t).collect();
    for (path, _) in to_hide {
        let _ = app.dock.remove_tab(path);
    }
    app.hidden_tool_tabs = stash;
}

/// Toggle visibility of the workspace VFS tree sub-tab. Hide just
/// removes `WorkspaceTab::VfsTree` from the workspace's inner dock
/// (the leaf that hosted it auto-cleans if it was the only tab,
/// returning that horizontal slice to the editor + entries leaf).
/// Show re-adds the tree as a fresh left split at the same default
/// fraction we use for new workspaces.
fn toggle_workspace_vfs(app: &mut HxyApp) {
    let Some(workspace_id) = active_workspace_id(app) else { return };
    let Some(workspace) = app.workspaces.get_mut(&workspace_id) else { return };
    if let Some(path) = workspace.dock.find_tab(&crate::file::WorkspaceTab::VfsTree) {
        let _ = workspace.dock.remove_tab(path);
    } else {
        workspace.dock.main_surface_mut().split_left(
            egui_dock::NodeIndex::root(),
            0.3,
            vec![crate::file::WorkspaceTab::VfsTree],
        );
    }
}

/// Push a freshly-opened VFS entry into the leaf that holds the
/// workspace's `Editor` sub-tab so the entry stacks alongside the
/// parent file rather than landing wherever the user was last
/// clicking (typically the VFS-tree leaf, since the click that
/// triggered the open came from there). The tree stays in its own
/// dedicated leaf as a tool panel.
#[cfg(not(target_arch = "wasm32"))]
fn push_workspace_entry(workspace: &mut crate::file::Workspace, file_id: FileId) {
    let entry = crate::file::WorkspaceTab::Entry(file_id);
    if let Some(editor_path) = workspace.dock.find_tab(&crate::file::WorkspaceTab::Editor)
        && let Ok(leaf) = workspace.dock.leaf_mut(editor_path.node_path())
    {
        leaf.append_tab(entry);
        return;
    }
    // Editor's gone (shouldn't happen during normal use). Fall back
    // to focused leaf so the file isn't lost.
    workspace.dock.push_to_focused_leaf(entry);
}

/// Tabs that belong in the right-hand "tool" panel: the plugin manager
/// and any live plugin VFS browser. Adding a new tool-class tab type
/// (e.g. a hypothetical `Tab::Workspace`) means listing it here so the
/// dock router knows to keep it out of the file editing area.
#[cfg(not(target_arch = "wasm32"))]
fn is_tool_tab(t: &Tab) -> bool {
    matches!(t, Tab::Plugins | Tab::PluginMount(_))
}

/// Tabs that hold the user's main editing surface -- File buffers and
/// the two static placeholders (Welcome, Settings) that share the same
/// leaf with them. Console / Inspector / SearchResults are *neither*
/// content nor tool: they have their own placement logic and never
/// receive routed File tabs.
#[cfg(not(target_arch = "wasm32"))]
fn is_content_tab(t: &Tab) -> bool {
    matches!(t, Tab::File(_) | Tab::Welcome | Tab::Settings)
}

/// First leaf in the dock whose tabs are all tool-class. Used as the
/// destination for plugin tab opens; if no such leaf exists, the
/// caller splits a new one off the right side.
#[cfg(not(target_arch = "wasm32"))]
fn find_tool_leaf(dock: &egui_dock::DockState<Tab>) -> Option<egui_dock::NodePath> {
    for (path, _tab) in dock.iter_all_tabs() {
        let node_path = path.node_path();
        let Ok(leaf) = dock.leaf(node_path) else { continue };
        if !leaf.tabs.is_empty() && leaf.tabs.iter().all(is_tool_tab) {
            return Some(node_path);
        }
    }
    None
}

/// First leaf whose tabs include any content-class entry. Used as the
/// fallback target for File opens originating from inside a tool
/// panel when `HxyApp::last_content_leaf` is stale or unset.
#[cfg(not(target_arch = "wasm32"))]
fn find_content_leaf(dock: &egui_dock::DockState<Tab>) -> Option<egui_dock::NodePath> {
    for (path, _tab) in dock.iter_all_tabs() {
        let node_path = path.node_path();
        let Ok(leaf) = dock.leaf(node_path) else { continue };
        if leaf.tabs.iter().any(is_content_tab) {
            return Some(node_path);
        }
    }
    None
}

/// Append `tab` to the dock's tool leaf, creating one with a right
/// split off the main surface root if none exists yet. Activates the
/// new tab in its leaf but does not move keyboard focus -- callers
/// that want focus follow this with `set_focused_node_and_surface`.
#[cfg(not(target_arch = "wasm32"))]
fn push_tool_tab(dock: &mut egui_dock::DockState<Tab>, tab: Tab) -> egui_dock::NodePath {
    if let Some(node_path) = find_tool_leaf(dock)
        && let Ok(leaf) = dock.leaf_mut(node_path)
    {
        leaf.append_tab(tab);
        return node_path;
    }
    dock.main_surface_mut().split_right(egui_dock::NodeIndex::root(), 0.72, vec![tab]);
    // After split_right the new leaf is the second one returned, but
    // we don't need the index here -- the caller can re-find it via
    // `dock.find_tab` if it needs the path.
    find_tool_leaf(dock).expect("split_right just created a tool leaf")
}

/// Snapshot the currently-focused leaf if it counts as a content
/// leaf. Called after each dock pass so file opens routed via
/// `last_content_leaf` land where the user was last editing.
#[cfg(not(target_arch = "wasm32"))]
fn track_content_leaf(app: &mut HxyApp) {
    let Some(node_path) = app.dock.focused_leaf() else { return };
    let Ok(leaf) = app.dock.leaf(node_path) else { return };
    if leaf.tabs.iter().any(is_content_tab) {
        app.last_content_leaf = Some(node_path);
    }
}

/// Move dock focus onto the saved `last_content_leaf`, falling back
/// to the first content-bearing leaf in the dock. Used right before
/// `app.open()` from a plugin VFS click so `push_to_focused_leaf`
/// inside `open` lands the new File tab in the editing area.
#[cfg(not(target_arch = "wasm32"))]
fn focus_content_leaf(app: &mut HxyApp) {
    if let Some(node_path) = app.last_content_leaf
        && app.dock.leaf(node_path).is_ok()
    {
        app.dock.set_focused_node_and_surface(node_path);
        return;
    }
    if let Some(node_path) = find_content_leaf(&app.dock) {
        app.last_content_leaf = Some(node_path);
        app.dock.set_focused_node_and_surface(node_path);
    }
}

fn remove_welcome_from_leaf(
    dock: &mut egui_dock::DockState<Tab>,
    surface: egui_dock::SurfaceIndex,
    node: egui_dock::NodeIndex,
) {
    let welcome_idx = match &dock[surface][node] {
        egui_dock::Node::Leaf(leaf) => leaf.tabs.iter().position(|t| matches!(t, Tab::Welcome)),
        _ => None,
    };
    if let Some(idx) = welcome_idx {
        let _ = dock.remove_tab(egui_dock::TabPath {
            surface,
            node,
            tab: egui_dock::TabIndex(idx),
        });
    }
}

/// Walk up the tree from `current` looking for the nearest ancestor
/// split oriented so `dir` steps across it; then descend the sibling
/// subtree to find a concrete leaf. Returns `None` when no such
/// neighbour exists (current is on the outer edge in `dir`).
fn find_neighbor_leaf(
    tree: &egui_dock::Tree<Tab>,
    current: egui_dock::NodeIndex,
    dir: crate::commands::DockDir,
) -> Option<egui_dock::NodeIndex> {
    use crate::commands::DockDir;
    use egui_dock::Node;
    let mut node = current;
    loop {
        let parent = node.parent()?;
        let was_left = node == parent.left();
        if parent.0 >= tree.len() {
            return None;
        }
        let takes_us_across = match (&tree[parent], dir) {
            (Node::Horizontal(_), DockDir::Right) if was_left => true,
            (Node::Horizontal(_), DockDir::Left) if !was_left => true,
            (Node::Vertical(_), DockDir::Down) if was_left => true,
            (Node::Vertical(_), DockDir::Up) if !was_left => true,
            _ => false,
        };
        if takes_us_across {
            let sibling = if was_left { parent.right() } else { parent.left() };
            return first_leaf_in(tree, sibling);
        }
        node = parent;
    }
}

fn first_leaf_in(tree: &egui_dock::Tree<Tab>, start: egui_dock::NodeIndex) -> Option<egui_dock::NodeIndex> {
    use egui_dock::Node;
    let mut cur = start;
    loop {
        if cur.0 >= tree.len() {
            return None;
        }
        match &tree[cur] {
            Node::Leaf(_) => return Some(cur),
            Node::Empty => return None,
            Node::Horizontal(_) | Node::Vertical(_) => cur = cur.left(),
        }
    }
}

/// Invoke the active tab's detected handler to mount its source into a
/// browsable VFS. On success, the tree panel picks it up automatically
/// because it reads `file.mount`.
#[cfg(not(target_arch = "wasm32"))]
fn run_template_dialog(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else { return };
    let Some(path) = rfd::FileDialog::new().pick_file() else { return };
    run_template_from_path(ctx, app, id, path);
}

#[cfg(not(target_arch = "wasm32"))]
fn run_template_from_path(ctx: &egui::Context, app: &mut HxyApp, id: FileId, path: std::path::PathBuf) {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_owned();

    let data_name = app.files.get(&id).map(|f| f.display_name.clone()).unwrap_or_else(|| format!("file-{}", id.get()));
    let tpl_name =
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    let console_ctx = format!("{data_name} / {tpl_name}");

    let Some(runtime) = app.template_runtime_for(&ext) else {
        let dir = user_template_plugins_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "$DATA/hxy/template-plugins".to_owned());
        let msg = format!(
            "No template runtime is registered for .{ext} files.\nInstall a matching runtime component (.wasm) into:\n{dir}"
        );
        app.console_log(ConsoleSeverity::Error, &console_ctx, &msg);
        if let Some(file) = app.files.get_mut(&id) {
            file.template = Some(crate::template_panel::error_state(msg));
        }
        return;
    };

    // Resolve `#include` textually before handing the source to the
    // runtime. Sandboxed to the user's templates directory so a
    // malicious template can't pull in arbitrary files via
    // `#include "../../..."`. Templates run directly from a path
    // outside the sandbox (e.g. in-tree fixtures) fall back to the
    // raw file with no expansion.
    let sandbox = user_templates_dir();
    let template_source = match sandbox.as_deref().and_then(|base| {
        let canonical_base = base.canonicalize().ok()?;
        let canonical_path = path.canonicalize().ok()?;
        canonical_path.starts_with(&canonical_base).then_some(canonical_base)
    }) {
        Some(base) => match crate::template_library::expand_includes(&path, &base) {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Failed to read template source {}: {e}", path.display());
                app.console_log(ConsoleSeverity::Error, &console_ctx, &msg);
                if let Some(file) = app.files.get_mut(&id) {
                    file.template = Some(crate::template_panel::error_state(msg));
                }
                return;
            }
        },
        None => match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Failed to read template source {}: {e}", path.display());
                app.console_log(ConsoleSeverity::Error, &console_ctx, &msg);
                if let Some(file) = app.files.get_mut(&id) {
                    file.template = Some(crate::template_panel::error_state(msg));
                }
                return;
            }
        },
    };

    // Spawn parse+execute on a worker. UI thread keeps rendering
    // while the template runs -- a big file can take seconds, which
    // would otherwise freeze pan / scroll / input. The `UiInbox`
    // triggers a repaint when the worker sends, so we don't poll.
    let Some(file) = app.files.get_mut(&id) else { return };
    let source = file.editor.source().clone();
    file.template = None;
    let (sender, inbox) = egui_inbox::UiInbox::channel_with_ctx(ctx);
    file.template_running =
        Some(crate::file::TemplateRun { inbox, template_name: tpl_name.clone(), started: jiff::Timestamp::now() });

    std::thread::spawn(move || {
        let outcome = match runtime.parse(source, &template_source) {
            Ok(parsed) => match parsed.execute(&[]) {
                Ok(tree) => crate::file::TemplateRunOutcome::Ok { parsed, tree },
                Err(e) => crate::file::TemplateRunOutcome::Err(format!("Execute failed: {e}")),
            },
            Err(e) => crate::file::TemplateRunOutcome::Err(format!("Parse failed: {e}")),
        };
        // Best-effort -- if the tab closed first the sender's inbox is
        // dropped and this returns Err, which is fine.
        let _ = sender.send(outcome);
    });

    app.console_log(ConsoleSeverity::Info, &console_ctx, format!("running template `{tpl_name}`..."));
}

/// Pop completed template-run results off each file's inbox and
/// swap them into the file's [`TemplateState`]. Called once per
/// frame; `UiInbox::read` is non-blocking and yields only items
/// that the worker has already sent.
#[cfg(not(target_arch = "wasm32"))]
fn drain_template_runs(ctx: &egui::Context, app: &mut HxyApp) {
    // Collect completed (file_id, outcome, tpl_name) first so we can
    // access `app.console_log` and mutate `files` sequentially
    // without borrow conflicts.
    let mut done: Vec<(FileId, crate::file::TemplateRunOutcome, String)> = Vec::new();
    for (id, file) in app.files.iter_mut() {
        let Some(run) = file.template_running.as_ref() else { continue };
        let outcomes: Vec<_> = run.inbox.read(ctx).collect();
        if outcomes.is_empty() {
            continue;
        }
        let tpl = run.template_name.clone();
        file.template_running = None;
        // A run only sends one outcome; if more land somehow, the
        // final one wins.
        for outcome in outcomes {
            done.push((*id, outcome, tpl.clone()));
        }
    }

    for (id, outcome, tpl) in done {
        let data_name =
            app.files.get(&id).map(|f| f.display_name.clone()).unwrap_or_else(|| format!("file-{}", id.get()));
        let console_ctx = format!("{data_name} / {tpl}");
        match outcome {
            crate::file::TemplateRunOutcome::Ok { parsed, tree } => {
                let diagnostics = tree.diagnostics.clone();
                let state = crate::template_panel::new_state_from(parsed, tree);
                if let Some(file) = app.files.get_mut(&id) {
                    file.template = Some(state);
                }
                for d in &diagnostics {
                    let severity = match d.severity {
                        hxy_plugin_host::template::Severity::Error => ConsoleSeverity::Error,
                        hxy_plugin_host::template::Severity::Warning => ConsoleSeverity::Warning,
                        hxy_plugin_host::template::Severity::Info => ConsoleSeverity::Info,
                    };
                    let loc = match d.file_offset {
                        Some(off) => format!(" @ {off:#x}"),
                        None => String::new(),
                    };
                    app.console_log(severity, &console_ctx, format!("{}{}", d.message, loc));
                }
                if diagnostics.is_empty() {
                    app.console_log(ConsoleSeverity::Info, &console_ctx, "template executed successfully");
                }
            }
            crate::file::TemplateRunOutcome::Err(msg) => {
                app.console_log(ConsoleSeverity::Error, &console_ctx, &msg);
                if let Some(file) = app.files.get_mut(&id) {
                    file.template = Some(crate::template_panel::error_state(msg));
                }
            }
        }
    }
    let _ = ctx;
}

/// Apply a frame's worth of events from the search bar to `file`.
/// The bar itself is render-only -- byte scans, selection moves, and
/// `matches` recomputation happen here.
#[cfg(not(target_arch = "wasm32"))]
fn apply_search_events(file: &mut OpenFile, events: Vec<crate::search_bar::SearchEvent>) {
    use crate::search::find_all;
    use crate::search::find_next;
    use crate::search::find_prev;
    use crate::search_bar::SearchEvent;

    let mut want_all = file.search.all_results;
    for ev in events {
        match ev {
            SearchEvent::Refresh => {
                file.search.refresh_pattern();
                if want_all && let Some(p) = file.search.pattern.clone() {
                    let m = find_all(file.editor.source().as_ref(), &p);
                    file.search.matches = m;
                    file.search.active_idx = nearest_match_idx(&file.search.matches, current_caret(file));
                }
            }
            SearchEvent::Next => {
                let Some(pattern) = file.search.pattern.clone() else { continue };
                let from = current_caret(file).saturating_add(1);
                if let Some(off) = find_next(file.editor.source().as_ref(), &pattern, from, true) {
                    apply_match_jump(file, off, &pattern);
                }
            }
            SearchEvent::Prev => {
                let Some(pattern) = file.search.pattern.clone() else { continue };
                let from = current_caret(file);
                if let Some(off) = find_prev(file.editor.source().as_ref(), &pattern, from, true) {
                    apply_match_jump(file, off, &pattern);
                }
            }
            SearchEvent::FindAll => {
                want_all = true;
                file.search.all_results = true;
                if let Some(p) = file.search.pattern.clone() {
                    let m = find_all(file.editor.source().as_ref(), &p);
                    file.search.matches = m;
                    file.search.active_idx = nearest_match_idx(&file.search.matches, current_caret(file));
                    if let Some(idx) = file.search.active_idx {
                        let off = file.search.matches[idx];
                        apply_match_jump(file, off, &p);
                    }
                }
            }
            SearchEvent::ClearAll => {
                want_all = false;
                file.search.all_results = false;
                file.search.matches.clear();
                file.search.active_idx = None;
            }
            SearchEvent::Close => {
                file.search.open = false;
            }
            SearchEvent::JumpTo(idx) => {
                let Some(pattern) = file.search.pattern.clone() else { continue };
                let Some(off) = file.search.matches.get(idx).copied() else { continue };
                file.search.active_idx = Some(idx);
                apply_match_jump(file, off, &pattern);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn current_caret(file: &OpenFile) -> u64 {
    file.editor.selection().map(|s| s.cursor.get()).unwrap_or(0)
}

/// Highlight the match at `off` and scroll it into view. Sets the
/// selection to `[off, off + pattern.len())` so the existing selection
/// rendering colors the match. Updates `active_idx` if the match
/// matches an entry in `matches`.
#[cfg(not(target_arch = "wasm32"))]
fn apply_match_jump(file: &mut OpenFile, off: u64, pattern: &[u8]) {
    let end_inclusive = off.saturating_add(pattern.len() as u64).saturating_sub(1);
    file.editor.set_selection(Some(hxy_core::Selection {
        anchor: hxy_core::ByteOffset::new(off),
        cursor: hxy_core::ByteOffset::new(end_inclusive),
    }));
    file.editor.set_scroll_to_byte(hxy_core::ByteOffset::new(off));
    if let Ok(idx) = file.search.matches.binary_search(&off) {
        file.search.active_idx = Some(idx);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn nearest_match_idx(matches: &[u64], caret: u64) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(matches.partition_point(|&m| m < caret).min(matches.len() - 1))
}

/// Apply a frame's worth of cross-file search events. `Run` rebuilds
/// the match list from scratch by scanning every open file's source;
/// `JumpTo` focuses the matched file's tab and selects the bytes.
#[cfg(not(target_arch = "wasm32"))]
fn apply_global_search_events(app: &mut HxyApp, events: Vec<crate::global_search::GlobalSearchEvent>) {
    use crate::global_search::GlobalMatch;
    use crate::global_search::GlobalSearchEvent;
    use crate::search::find_all;

    for ev in events {
        match ev {
            GlobalSearchEvent::Refresh => {
                app.global_search.query_state.refresh_pattern();
                app.global_search.matches.clear();
                app.global_search.active_idx = None;
            }
            GlobalSearchEvent::Run => {
                app.global_search.query_state.refresh_pattern();
                let Some(pattern) = app.global_search.query_state.pattern.clone() else {
                    app.global_search.matches.clear();
                    app.global_search.active_idx = None;
                    continue;
                };
                // Stable order: by file id then offset, so the result
                // list doesn't reshuffle on every rerun.
                let mut ids: Vec<FileId> = app.files.keys().copied().collect();
                ids.sort_by_key(|id| id.get());
                let mut all_matches: Vec<GlobalMatch> = Vec::new();
                for id in ids {
                    let Some(file) = app.files.get(&id) else { continue };
                    let src = file.editor.source().clone();
                    for off in find_all(src.as_ref(), &pattern) {
                        all_matches.push(GlobalMatch { file_id: id, offset: off });
                    }
                }
                app.global_search.matches = all_matches;
                app.global_search.active_idx = if app.global_search.matches.is_empty() { None } else { Some(0) };
            }
            GlobalSearchEvent::Close => {
                if let Some(path) = app.dock.find_tab(&Tab::SearchResults) {
                    let _ = app.dock.remove_tab(path);
                }
                app.global_search.open = false;
            }
            GlobalSearchEvent::JumpTo(idx) => {
                let Some(m) = app.global_search.matches.get(idx).cloned() else { continue };
                let Some(pattern) = app.global_search.query_state.pattern.clone() else { continue };
                app.global_search.active_idx = Some(idx);
                if let Some(file) = app.files.get_mut(&m.file_id) {
                    apply_match_jump(file, m.offset, &pattern);
                }
                app.focus_file_tab(m.file_id);
            }
        }
    }
}

/// "Browse VFS" entry point. Converts the active tab into a workspace
/// view, mounting the file's detected handler if one isn't already
/// mounted. Three resolutions:
/// * Active is `Tab::File(id)` with a detected handler -> mount, build
///   a `Workspace`, and swap the dock tab for `Tab::Workspace`. The
///   user's persisted state is updated to record `as_workspace = true`
///   so the next launch restores the same shape.
/// * Active is already `Tab::Workspace(id)` -> ensure `WorkspaceTab::VfsTree`
///   is present in the inner dock (re-add it if the user closed it).
/// * Anything else (no detected handler, no active file) -> no-op.
fn mount_active_file(app: &mut HxyApp) {
    if let Some(workspace_id) = active_workspace_id(app) {
        ensure_vfs_tree_visible(app, workspace_id);
        return;
    }
    let Some(file_id) = active_file_id(app) else { return };
    let Some(file) = app.files.get(&file_id) else { return };
    let Some(handler) = file.detected_handler.clone() else { return };
    let source = file.editor.source().clone();
    let mount = match handler.mount(source) {
        Ok(m) => Arc::new(m),
        Err(e) => {
            tracing::warn!(error = %e, handler = handler.name(), "mount vfs");
            return;
        }
    };
    let workspace_id = app.spawn_workspace(file_id, mount);

    // Swap the existing Tab::File(file_id) for Tab::Workspace(workspace_id)
    // in the same dock leaf, so the user's pane layout is preserved.
    if let Some(path) = app.dock.find_tab(&Tab::File(file_id)) {
        let _ = app.dock.remove_tab(path);
    }
    app.dock.push_to_focused_leaf(Tab::Workspace(workspace_id));
    if let Some(path) = app.dock.find_tab(&Tab::Workspace(workspace_id)) {
        let _ = app.dock.set_active_tab(path);
        app.dock.set_focused_node_and_surface(path.node_path());
    }

    // Persist the workspace flag against the file's source so restart
    // restores the same nested-dock shape.
    if let Some(source) = app.files.get(&file_id).and_then(|f| f.source_kind.clone()) {
        let mut g = app.state.write();
        if let Some(entry) = g.open_tabs.iter_mut().find(|t| t.source == source) {
            entry.as_workspace = true;
        }
    }
}

/// Best guess at "the workspace the user is in." Tries in order:
/// the outer-focused `Tab::Workspace`, the most recently focused
/// workspace (so clicking into Inspector / Console doesn't make
/// `Toggle VFS panel` and friends evaporate), and finally -- when
/// only one workspace is open -- that sole workspace. Returns
/// `None` only when no workspace exists.
fn active_workspace_id(app: &mut HxyApp) -> Option<crate::file::WorkspaceId> {
    if let Some((_, tab)) = app.dock.find_active_focused()
        && let Tab::Workspace(id) = *tab
    {
        app.last_active_workspace = Some(id);
        return Some(id);
    }
    if let Some(id) = app.last_active_workspace
        && app.workspaces.contains_key(&id)
    {
        return Some(id);
    }
    // Final fallback: pick any workspace if one exists. Matches the
    // single-workspace common case without forcing the user to
    // re-focus before invoking a workspace command.
    let id = app.workspaces.keys().next().copied();
    if let Some(id) = id {
        app.last_active_workspace = Some(id);
    }
    id
}

/// Re-add `WorkspaceTab::VfsTree` to the workspace's inner dock if the
/// user previously closed it. No-op when the tree is already present.
fn ensure_vfs_tree_visible(app: &mut HxyApp, workspace_id: crate::file::WorkspaceId) {
    let Some(workspace) = app.workspaces.get_mut(&workspace_id) else { return };
    let already_present =
        workspace.dock.iter_all_tabs().any(|(_, t)| matches!(t, crate::file::WorkspaceTab::VfsTree));
    if already_present {
        return;
    }
    workspace.dock.main_surface_mut().split_left(
        egui_dock::NodeIndex::root(),
        0.3,
        vec![crate::file::WorkspaceTab::VfsTree],
    );
}

/// Render a `Tab::PluginMount`. The whole tab body is the VFS tree
/// for the mount; clicking an entry queues a `PendingVfsOpen::Mount`
/// for the post-dock drain to turn into a regular `Tab::File`.
#[cfg(not(target_arch = "wasm32"))]
fn render_plugin_mount_tab(
    ui: &mut egui::Ui,
    mount_id: crate::file::MountId,
    mount: &Arc<hxy_vfs::MountedVfs>,
) {
    let scope = egui::Id::new(("hxy-plugin-mount-vfs", mount_id.get()));
    let events = crate::vfs_panel::show(ui, scope, &*mount.fs);
    let mut to_open: Vec<String> = Vec::new();
    for e in events {
        let crate::vfs_panel::VfsPanelEvent::OpenEntry(path) = e;
        to_open.push(path);
    }
    if !to_open.is_empty() {
        ui.ctx().data_mut(|d| {
            let queue: &mut Vec<PendingVfsOpen> = d.get_temp_mut_or_default(egui::Id::new(PENDING_VFS_OPEN_KEY));
            for p in to_open {
                queue.push(PendingVfsOpen::PluginMount { mount_id, entry_path: p });
            }
        });
    }
}

fn render_file_tab(
    ui: &mut egui::Ui,
    id: FileId,
    file: &mut OpenFile,
    state: &mut PersistedState,
    tab_focus: TabFocus,
) {
    let settings_base = state.app.offset_base;
    let mut new_base = settings_base;

    let tab_rect = ui.available_rect_before_wrap();
    let bg = ui.visuals().window_fill();
    ui.painter().rect_filled(tab_rect, 0.0, bg);

    let text_h = ui.text_style_height(&egui::TextStyle::Body);
    let status_h = text_h + 2.0;

    egui::Panel::bottom(egui::Id::new(("hxy-status-panel", id.get())))
        .resizable(false)
        .exact_size(status_h)
        .frame(egui::Frame::new().inner_margin(egui::Margin { left: 8, right: 8, top: 0, bottom: 0 }))
        .show_inside(ui, |ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                status_bar_ui(ui, file, settings_base, &mut new_base, tab_focus);
            });
        });

    #[cfg(not(target_arch = "wasm32"))]
    if file.search.open {
        egui::Panel::bottom(egui::Id::new(("hxy-search-panel", id.get())))
            .resizable(false)
            .show_inside(ui, |ui| {
                let events = crate::search_bar::show(ui, &mut file.search);
                apply_search_events(file, events);
            });
    }

    let body_rect = ui.available_rect_before_wrap();
    ui.painter().hline(
        tab_rect.x_range(),
        body_rect.bottom(),
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );

    #[cfg(not(target_arch = "wasm32"))]
    render_template_panel(ui, id, file);

    let copy_request = egui::CentralPanel::default()
        .frame(egui::Frame::new())
        .show_inside(ui, |ui| render_hex_body(ui, file, state))
        .inner;

    if let Some(kind) = copy_request {
        do_copy(ui.ctx(), file, kind);
    }

    if new_base != settings_base {
        state.app.offset_base = new_base;
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn render_template_panel(ui: &mut egui::Ui, id: FileId, file: &mut OpenFile) {
    let show_ready = file.template.as_ref().is_some_and(|t| t.show_panel);
    let show_running = file.template_running.is_some();
    if !show_ready && !show_running {
        return;
    }
    egui::Panel::bottom(egui::Id::new(("hxy-template-panel", id.get())))
        .resizable(true)
        .default_size(300.0)
        .min_size(160.0)
        .show_inside(ui, |ui| {
            if let Some(run) = file.template_running.as_ref() {
                render_template_running(ui, run);
                return;
            }
            let Some(state) = file.template.as_mut() else { return };
            let events = crate::template_panel::show(ui, id.get(), state);
            for e in events {
                match e {
                    crate::template_panel::TemplateEvent::Close => state.show_panel = false,
                    crate::template_panel::TemplateEvent::ExpandArray { array_id, count } => {
                        crate::template_panel::expand_array(state, array_id, count);
                    }
                    crate::template_panel::TemplateEvent::ToggleCollapse(idx) => {
                        crate::template_panel::toggle_collapse(state, idx);
                    }
                    crate::template_panel::TemplateEvent::Hover(idx) => {
                        state.hovered_node = idx;
                    }
                    crate::template_panel::TemplateEvent::Select(idx) => {
                        if let Some(node) = state.tree.nodes.get(idx.0 as usize) {
                            let offset = node.span.offset;
                            let length = node.span.length.max(1);
                            let end_inclusive = offset.saturating_add(length - 1);
                            file.editor.set_selection(Some(hxy_core::Selection {
                                anchor: hxy_core::ByteOffset::new(offset),
                                cursor: hxy_core::ByteOffset::new(end_inclusive),
                            }));
                            file.editor.set_scroll_to_byte(hxy_core::ByteOffset::new(offset));
                        }
                    }
                    crate::template_panel::TemplateEvent::Copy { idx, kind } => {
                        let ctx = ui.ctx().clone();
                        if kind.is_struct() {
                            if let Some(text) = format_template_struct(&state.tree.nodes, idx.0 as usize, kind) {
                                ctx.copy_text(text);
                            }
                        } else if let Some(node) = state.tree.nodes.get(idx.0 as usize).cloned() {
                            let source = file.editor.source().clone();
                            if let Some(text) = format_template_copy(&source, &node, kind) {
                                ctx.copy_text(text);
                            }
                        }
                    }
                    crate::template_panel::TemplateEvent::SaveBytes(idx) => {
                        if let Some(node) = state.tree.nodes.get(idx.0 as usize).cloned() {
                            save_template_bytes(file.editor.source(), &node);
                        }
                    }
                    crate::template_panel::TemplateEvent::ToggleColors(on) => {
                        state.show_colors = on;
                    }
                }
            }
        });
}

/// Read `node`'s byte span from `source` and format it according to
/// `kind`. Returns `None` when the bytes can't be read (out of
/// bounds, I/O error) -- the caller silently drops the copy.
#[cfg(not(target_arch = "wasm32"))]
fn format_template_copy(
    source: &std::sync::Arc<dyn hxy_core::HexSource>,
    node: &hxy_plugin_host::template::Node,
    kind: CopyKind,
) -> Option<String> {
    if kind.is_value() {
        let raw = scalar_value_u64(node.value.as_ref()?)?;
        return crate::copy_format::format_scalar(kind, raw);
    }
    let start = hxy_core::ByteOffset::new(node.span.offset);
    let end = hxy_core::ByteOffset::new(node.span.offset.saturating_add(node.span.length));
    let range = hxy_core::ByteRange::new(start, end).ok()?;
    let bytes = source.read(range).ok()?;
    let ty = hxy_plugin_host::node_type_label(&node.type_name);
    crate::copy_format::format_bytes(kind, &bytes, &node.name, &ty)
}

/// Walk a struct (or array-of-structs) node and produce a C99
/// designated-initialiser block or a Rust struct literal that
/// mirrors its children's field layout and values. Runs recursively
/// so nested structs and arrays render inline.
#[cfg(not(target_arch = "wasm32"))]
fn format_template_struct(
    nodes: &[hxy_plugin_host::template::Node],
    root_idx: usize,
    kind: CopyKind,
) -> Option<String> {
    let root = nodes.get(root_idx)?;
    let mut out = String::new();
    let ident = crate::copy_format::sanitize_ident(&root.name);
    let ty = hxy_plugin_host::node_type_label(&root.type_name);
    match kind {
        CopyKind::StructRust => {
            use std::fmt::Write;
            let _ = write!(out, "let {ident}: {ty} = ");
            write_struct_body(&mut out, nodes, root_idx, StructSyntax::Rust, 0)?;
            out.push(';');
        }
        CopyKind::StructC => {
            use std::fmt::Write;
            let _ = write!(out, "{ty} {ident} = ");
            write_struct_body(&mut out, nodes, root_idx, StructSyntax::C, 0)?;
            out.push(';');
        }
        _ => return None,
    }
    Some(out)
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy)]
enum StructSyntax {
    Rust,
    C,
}

/// Recursive body writer: emits `{ field: value, ... }` for a
/// struct, `[v0, v1, ...]` / `{ v0, v1, ... }` for an array, or a
/// literal for a scalar leaf. Returns `None` if the tree is
/// inconsistent (no children of a struct, e.g.).
#[cfg(not(target_arch = "wasm32"))]
fn write_struct_body(
    out: &mut String,
    nodes: &[hxy_plugin_host::template::Node],
    idx: usize,
    syntax: StructSyntax,
    depth: usize,
) -> Option<()> {
    use hxy_plugin_host::template::NodeType;
    use std::fmt::Write;

    let node = nodes.get(idx)?;
    let children: Vec<usize> =
        nodes.iter().enumerate().filter_map(|(i, n)| (n.parent == Some(idx as u32)).then_some(i)).collect();

    match &node.type_name {
        NodeType::StructType(name) | NodeType::StructArray((name, _)) => {
            // For arrays of structs, each child element IS a
            // struct node; we recurse into each so the output is
            // `[ Struct { .. }, Struct { .. } ]`.
            let is_array = matches!(node.type_name, NodeType::StructArray(_));
            if is_array {
                open_array(out, syntax);
                for (i, &cidx) in children.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_struct_body(out, nodes, cidx, syntax, depth + 1)?;
                }
                close_array(out, syntax);
            } else {
                match syntax {
                    StructSyntax::Rust => {
                        let _ = write!(out, "{name} {{");
                    }
                    StructSyntax::C => out.push('{'),
                }
                for &cidx in &children {
                    let child = &nodes[cidx];
                    out.push('\n');
                    for _ in 0..=depth {
                        out.push_str("    ");
                    }
                    match syntax {
                        StructSyntax::Rust => {
                            let _ = write!(out, "{}: ", crate::copy_format::sanitize_ident(&child.name));
                        }
                        StructSyntax::C => {
                            let _ = write!(out, ".{} = ", crate::copy_format::sanitize_ident(&child.name));
                        }
                    }
                    write_struct_body(out, nodes, cidx, syntax, depth + 1)?;
                    out.push(',');
                }
                out.push('\n');
                for _ in 0..depth {
                    out.push_str("    ");
                }
                out.push('}');
            }
        }
        NodeType::EnumType(_) | NodeType::EnumArray(_) => {
            // Enums and enum-arrays print their raw scalar value --
            // the named variant isn't tracked on the wire.
            write_scalar_or_array(out, node, &children, nodes, syntax, depth)?;
        }
        NodeType::Scalar(_) | NodeType::ScalarArray(_) | NodeType::Unknown(_) => {
            write_scalar_or_array(out, node, &children, nodes, syntax, depth)?;
        }
    }
    Some(())
}

#[cfg(not(target_arch = "wasm32"))]
fn write_scalar_or_array(
    out: &mut String,
    node: &hxy_plugin_host::template::Node,
    children: &[usize],
    nodes: &[hxy_plugin_host::template::Node],
    syntax: StructSyntax,
    depth: usize,
) -> Option<()> {
    use hxy_plugin_host::template::NodeType;
    let is_array = matches!(node.type_name, NodeType::ScalarArray(_) | NodeType::EnumArray(_));
    if is_array {
        open_array(out, syntax);
        // Scalar arrays may either have child element nodes (one
        // per entry) or a bare `value` of Bytes. Handle the nodes
        // case first; when empty, fall back to formatting the
        // raw value.
        if !children.is_empty() {
            for (i, &cidx) in children.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_struct_body(out, nodes, cidx, syntax, depth + 1)?;
            }
        } else if let Some(v) = node.value.as_ref() {
            out.push_str(&format_scalar_literal(v, syntax));
        }
        close_array(out, syntax);
    } else if let Some(v) = node.value.as_ref() {
        out.push_str(&format_scalar_literal(v, syntax));
    } else {
        out.push('0');
    }
    Some(())
}

#[cfg(not(target_arch = "wasm32"))]
fn open_array(out: &mut String, syntax: StructSyntax) {
    out.push_str(match syntax {
        StructSyntax::Rust => "[",
        StructSyntax::C => "{",
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn close_array(out: &mut String, syntax: StructSyntax) {
    out.push_str(match syntax {
        StructSyntax::Rust => "]",
        StructSyntax::C => "}",
    });
}

/// Literal rendering for a single scalar value. Mirrors the
/// inspector's conventions: integers hex-prefixed for Rust/C
/// (`0x...`), floats with trailing type suffix for Rust, booleans
/// lowercased. Falls back to a lossless debug form for values the
/// scalar formatters can't represent directly (strings, bytes,
/// enums).
#[cfg(not(target_arch = "wasm32"))]
fn format_scalar_literal(v: &hxy_plugin_host::template::Value, syntax: StructSyntax) -> String {
    use hxy_plugin_host::template::Value;
    match v {
        Value::U8Val(x) => format!("0x{x:02X}"),
        Value::U16Val(x) => format!("0x{x:04X}"),
        Value::U32Val(x) => format!("0x{x:08X}"),
        Value::U64Val(x) => format!("0x{x:016X}"),
        Value::S8Val(x) => format!("{x}"),
        Value::S16Val(x) => format!("{x}"),
        Value::S32Val(x) => format!("{x}"),
        Value::S64Val(x) => format!("{x}"),
        Value::F32Val(x) => match syntax {
            StructSyntax::Rust => format!("{x}f32"),
            StructSyntax::C => format!("{x}f"),
        },
        Value::F64Val(x) => match syntax {
            StructSyntax::Rust => format!("{x}f64"),
            StructSyntax::C => format!("{x}"),
        },
        Value::StringVal(s) => format!("{s:?}"),
        Value::BytesVal(bs) => {
            let mut out = String::new();
            out.push_str(match syntax {
                StructSyntax::Rust => "[",
                StructSyntax::C => "{",
            });
            for (i, b) in bs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("0x{b:02X}"));
            }
            out.push_str(match syntax {
                StructSyntax::Rust => "]",
                StructSyntax::C => "}",
            });
            out
        }
        Value::EnumVal((name, value)) => {
            // Print the numeric value (both syntaxes accept integer
            // literals here), with the variant name as a trailing
            // comment so it's still visible in the output.
            format!("{value} /* {name} */")
        }
    }
}

/// Extract a u64 bit pattern from a scalar [`hxy_plugin_host::template::Value`],
/// preserving signed-bit representation so hex displays match what the
/// user sees on the wire. Returns `None` for non-scalar values (Str /
/// Bool / Bytes / Enum).
#[cfg(not(target_arch = "wasm32"))]
fn scalar_value_u64(v: &hxy_plugin_host::template::Value) -> Option<u64> {
    use hxy_plugin_host::template::Value;
    Some(match v {
        Value::U8Val(x) => u64::from(*x),
        Value::U16Val(x) => u64::from(*x),
        Value::U32Val(x) => u64::from(*x),
        Value::U64Val(x) => *x,
        Value::S8Val(x) => *x as u8 as u64,
        Value::S16Val(x) => *x as u16 as u64,
        Value::S32Val(x) => *x as u32 as u64,
        Value::S64Val(x) => *x as u64,
        _ => return None,
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn save_template_bytes(source: &std::sync::Arc<dyn hxy_core::HexSource>, node: &hxy_plugin_host::template::Node) {
    let start = hxy_core::ByteOffset::new(node.span.offset);
    let end = hxy_core::ByteOffset::new(node.span.offset.saturating_add(node.span.length));
    let Ok(range) = hxy_core::ByteRange::new(start, end) else { return };
    let bytes = match source.read(range) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "read bytes for save");
            return;
        }
    };
    let default_name = format!("{}.bin", crate::copy_format::sanitize_ident(&node.name));
    let Some(path) = rfd::FileDialog::new().set_file_name(&default_name).save_file() else { return };
    if let Err(e) = std::fs::write(&path, &bytes) {
        tracing::warn!(error = %e, path = %path.display(), "write template bytes");
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn render_template_running(ui: &mut egui::Ui, run: &crate::file::TemplateRun) {
    ui.vertical_centered(|ui| {
        ui.add_space(24.0);
        ui.label(egui::RichText::new(format!("{} Template", egui_phosphor::regular::SCROLL)).strong());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!("Running `{}`...", run.template_name));
        });
        let elapsed_ms = jiff::Timestamp::now().duration_since(run.started).as_millis().max(0);
        ui.add_space(4.0);
        ui.weak(format!("{} ms", elapsed_ms));
    });
}

fn render_hex_body(ui: &mut egui::Ui, file: &mut OpenFile, state: &mut PersistedState) -> Option<CopyKind> {
    let template_palette_override = file.template.as_ref().and_then(|t| t.byte_palette_override.clone());
    // A plugin-supplied palette forces the highlight on (in Background
    // mode by default) so the user actually sees it; otherwise the
    // user's own setting wins.
    let (highlight, palette) = if let Some(table) = template_palette_override {
        (Some(state.app.byte_highlight_mode.as_view()), Some(hxy_view::HighlightPalette::Custom(table)))
    } else {
        let highlight = state.app.byte_value_highlight.then(|| state.app.byte_highlight_mode.as_view());
        (highlight, build_palette(ui.visuals().dark_mode, &state.app, highlight))
    };
    let has_sel = file.editor.selection().map(|s| !s.range().is_empty()).unwrap_or(false);
    // Pre-compute "is the selection a scalar integer width?" before
    // taking the mutable borrow HexView needs; the `ui.selection`
    // can't be read inside the context menu closure once HexView
    // holds it.
    let show_scalar_submenu = file.editor.selection().map(|s| matches!(s.range().len().get(), 1 | 2 | 4 | 8)).unwrap_or(false);

    let mut copy_request: Option<CopyKind> = None;
    let hover_span = file
        .template
        .as_ref()
        .and_then(|t| t.hovered_node)
        .and_then(|idx| file.template.as_ref().and_then(|t| t.tree.nodes.get(idx.0 as usize)))
        .and_then(|node| {
            let start = node.span.offset;
            let end = start.saturating_add(node.span.length);
            hxy_core::ByteRange::new(hxy_core::ByteOffset::new(start), hxy_core::ByteOffset::new(end)).ok()
        });

    let field_boundaries = file.template.as_ref().map(|t| t.leaf_boundaries.as_slice()).unwrap_or_default();
    let field_colors = file
        .template
        .as_ref()
        .filter(|t| t.show_colors && !t.leaf_boundaries.is_empty())
        .map(|t| (t.leaf_boundaries.as_slice(), t.leaf_colors.as_slice()));

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

    let address_separator =
        state.app.address_separator_enabled.then(|| {
            (hxy_view::address_hex_width(file.editor.source().len()), state.app.address_separator_char)
        });
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
    if let Some((base_chars, sep)) = address_separator {
        view = view
            .address_chars(hxy_view::address_chars_with_separator(base_chars, 4))
            .address_formatter(move |offset, _| hxy_view::format_address_grouped(offset, base_chars, sep, 4));
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
                if let Some(kind) = crate::copy_format::copy_as_menu(ui, show_scalar_submenu) {
                    copy_request = Some(kind);
                }
            });
        })
        .show(ui);
    file.editor.on_response(&response, columns);
    file.hovered = response.hovered_offset;
    sync_tab_state(state, file);

    // If a template is active and the user is hovering a byte it
    // covers, pop a breadcrumb tooltip next to the pointer showing
    // the full parent chain down to the containing field.
    if let Some(offset) = response.hovered_offset
        && let Some(template) = file.template.as_ref()
        && let Some(path) =
            crate::template_panel::breadcrumb_for_offset(&template.tree, file.editor.source().as_ref(), offset.get())
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
            for (i, line) in path.iter().enumerate() {
                let text = egui::RichText::new(line).monospace();
                if i + 1 == path.len() {
                    ui.label(text.strong());
                } else {
                    ui.label(text);
                }
            }
        });
    }

    copy_request
}

#[cfg(not(target_arch = "wasm32"))]
fn parent_missing(parent: &TabSource) -> crate::file::FileOpenError {
    crate::file::FileOpenError::Read {
        path: std::path::PathBuf::from(format!("{parent:?}")),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "parent tab / mount not available"),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn read_vfs_entry(fs: &dyn hxy_vfs::vfs::FileSystem, path: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = fs.open_file(path).map_err(|e| std::io::Error::other(format!("open {path}: {e}")))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(not(target_arch = "wasm32"))]
fn register_user_plugins(
    registry: &mut VfsRegistry,
    grants: &hxy_plugin_host::PluginGrants,
    state_store: Option<Arc<dyn hxy_plugin_host::StateStore>>,
) -> Vec<Arc<hxy_plugin_host::PluginHandler>> {
    let Some(dir) = user_plugins_dir() else { return Vec::new() };
    let mut out = Vec::new();
    match hxy_plugin_host::load_plugins_from_dir(&dir, grants, state_store) {
        Ok(handlers) => {
            for h in handlers {
                tracing::info!(name = h.name(), "loaded wasm plugin");
                let arc = Arc::new(h);
                registry.register(arc.clone());
                out.push(arc);
            }
        }
        Err(e) => tracing::warn!(error = %e, dir = %dir.display(), "load plugins"),
    }
    out
}

#[cfg(not(target_arch = "wasm32"))]
fn user_plugins_dir() -> Option<std::path::PathBuf> {
    // Plugins are installed artefacts (binaries + metadata), not user
    // settings -- they belong under the data dir, not the config dir.
    // On Linux this resolves to `$XDG_DATA_HOME/hxy/plugins` (i.e.
    // ~/.local/share/hxy/plugins); on macOS to `~/Library/Application
    // Support/hxy/plugins`.
    let base = dirs::data_dir()?;
    Some(base.join(APP_NAME).join("plugins"))
}

#[cfg(not(target_arch = "wasm32"))]
fn user_template_plugins_dir() -> Option<std::path::PathBuf> {
    let base = dirs::data_dir()?;
    Some(base.join(APP_NAME).join("template-plugins"))
}

/// Directory for user-authored template sources (`.bt` files). The
/// [`TemplateLibrary`] scans this for auto-detection; distinct from
/// `template-plugins/`, which holds compiled WASM runtimes.
#[cfg(not(target_arch = "wasm32"))]
fn user_templates_dir() -> Option<std::path::PathBuf> {
    let base = dirs::data_dir()?;
    Some(base.join(APP_NAME).join("templates"))
}

/// Per-tab unsaved-patch sidecars live here; one JSON file per
/// source path, named by BLAKE3 of the canonical path. Read on
/// open, written on quit, removed on successful save.
#[cfg(not(target_arch = "wasm32"))]
fn unsaved_edits_dir() -> Option<std::path::PathBuf> {
    let base = dirs::data_dir()?;
    Some(base.join(APP_NAME).join("edits"))
}

/// Per-install storage for anonymous / scratch tabs. One file per
/// tab named after the [`hxy_vfs::AnonymousId`], created on first
/// `New file` and removed when the tab is saved to a real path or
/// closed without saving.
#[cfg(not(target_arch = "wasm32"))]
fn anonymous_files_dir() -> Option<std::path::PathBuf> {
    let base = dirs::data_dir()?;
    Some(base.join(APP_NAME).join("anonymous"))
}

#[cfg(not(target_arch = "wasm32"))]
fn anonymous_file_path(id: hxy_vfs::AnonymousId) -> Option<std::path::PathBuf> {
    anonymous_files_dir().map(|d| d.join(format!("{:016x}.bin", id.get())))
}

/// Default byte count for a fresh anonymous tab. Writes are
/// length-preserving right now, so this also caps how much the
/// user can edit before saving-as. 256 bytes is 16 rows at the
/// default column count -- enough to experiment without looking
/// cavernous.
#[cfg(not(target_arch = "wasm32"))]
const ANONYMOUS_DEFAULT_SIZE: usize = 256;

#[cfg(not(target_arch = "wasm32"))]
fn load_user_template_plugins() -> Vec<Arc<dyn hxy_plugin_host::TemplateRuntime>> {
    let mut out: Vec<Arc<dyn hxy_plugin_host::TemplateRuntime>> = Vec::new();

    // Native builtin runtimes link as regular Rust -- no WASM wrap,
    // no separate rebuild cycle. A change to hxy-010-lang reaches
    // the user's next `cargo run` automatically.
    for rt in crate::builtin_runtimes::builtins() {
        tracing::info!(name = rt.name(), exts = ?rt.extensions(), builtin = true, "loaded template runtime");
        out.push(rt);
    }

    // User-installed WASM components can still override a builtin
    // for the same extension -- they get prepended so `find()` picks
    // them first.
    if let Some(dir) = user_template_plugins_dir() {
        match hxy_plugin_host::load_template_plugins_from_dir(&dir) {
            Ok(runtimes) => {
                for r in runtimes {
                    tracing::info!(name = r.name(), exts = ?r.extensions(), builtin = false, "loaded template runtime");
                    out.insert(0, Arc::new(r));
                }
            }
            Err(e) => tracing::warn!(error = %e, dir = %dir.display(), "load template runtimes"),
        }
    }

    out
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
}

#[cfg(target_os = "macos")]
fn drain_native_menu(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(menu) = app.menu.as_ref() else { return };
    let actions = menu.drain_actions();
    for action in actions {
        match action {
            crate::menu::MenuAction::NewFile => handle_new_file(app),
            crate::menu::MenuAction::OpenFile => handle_open_file(app),
            crate::menu::MenuAction::Save => save_active_file(app, false),
            crate::menu::MenuAction::SaveAs => save_active_file(app, true),
            crate::menu::MenuAction::CloseTab => request_close_active_tab(app),
            crate::menu::MenuAction::ToggleEditMode => toggle_active_edit_mode(app),
            crate::menu::MenuAction::Undo => undo_active_file(app),
            crate::menu::MenuAction::Redo => redo_active_file(app),
            crate::menu::MenuAction::Paste => paste_active_file(app, false),
            crate::menu::MenuAction::PasteAsHex => paste_active_file(app, true),
            crate::menu::MenuAction::CopyBytes => copy_active_file(ctx, app, CopyKind::BytesLossyUtf8),
            crate::menu::MenuAction::CopyHex => copy_active_file(ctx, app, CopyKind::BytesHexSpaced),
            crate::menu::MenuAction::CopyAs(kind) => copy_active_file(ctx, app, kind),
            crate::menu::MenuAction::ToggleConsole => app.toggle_console(),
            crate::menu::MenuAction::ToggleInspector => app.toggle_inspector(),
            crate::menu::MenuAction::TogglePlugins => app.toggle_plugins(),
        }
    }
}

#[cfg(target_os = "macos")]
fn sync_native_menu_state(app: &mut HxyApp) {
    let active = active_file_id(app);
    let has_file = active.is_some();
    let has_scalar = active
        .and_then(|id| app.files.get(&id))
        .and_then(|f| f.editor.selection())
        .map(|s| matches!(s.range().len().get(), 1 | 2 | 4 | 8))
        .unwrap_or(false);
    let can_save = active.and_then(|id| app.files.get(&id)).is_some_and(|f| f.editor.is_dirty() || f.root_path().is_some());
    let (can_undo, can_redo) = active
        .and_then(|id| app.files.get(&id))
        .map(|f| (f.editor.can_undo(), f.editor.can_redo()))
        .unwrap_or((false, false));
    let can_paste = active
        .and_then(|id| app.files.get(&id))
        .is_some_and(|f| f.editor.edit_mode() == crate::file::EditMode::Mutable);
    if let Some(menu) = app.menu.as_ref() {
        menu.set_file_open(has_file);
        menu.set_scalar_selection(has_scalar);
        menu.set_save_enabled(can_save);
        menu.set_edit_mode_enabled(has_file);
        menu.set_undo_enabled(can_undo);
        menu.set_redo_enabled(can_redo);
        menu.set_paste_enabled(can_paste);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn toggle_active_edit_mode(app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else { return };
    let Some(file) = app.files.get_mut(&id) else { return };
    // Hard read-only files refuse the toggle the same way the lock
    // icon does. The reason is already surfaced via the icon's
    // tooltip; silently no-op the keystroke rather than flicker the
    // edit mode and snap it back.
    if file.read_only_reason.is_some() {
        return;
    }
    let next = match file.editor.edit_mode() {
        crate::file::EditMode::Readonly => crate::file::EditMode::Mutable,
        crate::file::EditMode::Mutable => crate::file::EditMode::Readonly,
    };
    file.editor.set_edit_mode(next);
}

#[cfg(not(target_arch = "wasm32"))]
fn paste_active_file(app: &mut HxyApp, as_hex: bool) {
    let Some(id) = active_file_id(app) else { return };
    let edit_mode = app.files.get(&id).map(|f| f.editor.edit_mode());
    if edit_mode != Some(crate::file::EditMode::Mutable) {
        return;
    }
    let text = match crate::paste::read_text() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "read clipboard");
            return;
        }
    };
    let bytes = if as_hex {
        match crate::paste::parse_hex_clipboard(&text) {
            Ok(b) => b,
            Err(e) => {
                app.console_log(
                    ConsoleSeverity::Warning,
                    "Paste as hex",
                    format!("clipboard text is not valid hex: {e}"),
                );
                return;
            }
        }
    } else {
        text.into_bytes()
    };
    if bytes.is_empty() {
        return;
    }
    let Some(file) = app.files.get_mut(&id) else { return };
    paste_bytes_at_cursor(file, bytes);
}

#[cfg(not(target_arch = "wasm32"))]
fn undo_active_file(app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else { return };
    let Some(file) = app.files.get_mut(&id) else { return };
    if let Some(entry) = file.editor.undo() {
        jump_cursor_to(file, entry.offset);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn redo_active_file(app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else { return };
    let Some(file) = app.files.get_mut(&id) else { return };
    if let Some(entry) = file.editor.redo() {
        jump_cursor_to(file, entry.offset);
    }
}

/// Park the cursor at `offset` (clamped to the tab's source length)
/// after an undo or redo so the user can see where the change
/// landed. Also resets the nibble pointer and `last_cursor_offset`
/// so typing after the jump starts on the high nibble.
#[cfg(not(target_arch = "wasm32"))]
fn jump_cursor_to(file: &mut crate::file::OpenFile, offset: u64) {
    let len = file.editor.source().len().get();
    let clamped = offset.min(len.saturating_sub(1));
    file.editor.set_selection(Some(hxy_core::Selection::caret(hxy_core::ByteOffset::new(clamped))));    file.editor.reset_edit_nibble();
}

#[cfg(target_os = "macos")]
fn copy_active_file(ctx: &egui::Context, app: &mut HxyApp, kind: CopyKind) {
    let Some(id) = active_file_id(app) else { return };
    let Some(file) = app.files.get(&id) else { return };
    do_copy(ctx, file, kind);
}

#[cfg(not(target_os = "macos"))]
fn top_menu_bar(ui: &mut egui::Ui, app: &mut HxyApp) {
    egui::Panel::top("hxy_menu_bar").show_inside(ui, |ui| {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button(hxy_i18n::t("menu-file"), |ui| {
                let new_text = ui.ctx().format_shortcut(&NEW_FILE);
                if ui.add(egui::Button::new(hxy_i18n::t("menu-file-new")).shortcut_text(new_text)).clicked() {
                    ui.close();
                    handle_new_file(app);
                }
                if ui.button(hxy_i18n::t("menu-file-open")).clicked() {
                    ui.close();
                    handle_open_file(app);
                }
                let active = active_file_id(app);
                let can_save = active.and_then(|id| app.files.get(&id)).is_some_and(|f| f.editor.is_dirty() || f.root_path().is_some());
                let save_text = ui.ctx().format_shortcut(&SAVE_FILE);
                let save_as_text = ui.ctx().format_shortcut(&SAVE_FILE_AS);
                ui.add_enabled_ui(can_save, |ui| {
                    if ui.add(egui::Button::new(hxy_i18n::t("menu-file-save")).shortcut_text(save_text)).clicked() {
                        ui.close();
                        save_active_file(app, false);
                    }
                });
                ui.add_enabled_ui(active.is_some(), |ui| {
                    if ui
                        .add(egui::Button::new(hxy_i18n::t("menu-file-save-as")).shortcut_text(save_as_text))
                        .clicked()
                    {
                        ui.close();
                        save_active_file(app, true);
                    }
                });
                ui.separator();
                let close_text = ui.ctx().format_shortcut(&CLOSE_TAB);
                if ui.add(egui::Button::new(hxy_i18n::t("menu-file-close")).shortcut_text(close_text)).clicked() {
                    ui.close();
                    request_close_active_tab(app);
                }
                ui.separator();
                if ui.button(hxy_i18n::t("menu-file-quit")).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button(hxy_i18n::t("menu-edit"), |ui| {
                let copy_bytes_text = ui.ctx().format_shortcut(&COPY_BYTES);
                let copy_hex_text = ui.ctx().format_shortcut(&COPY_HEX);
                let toggle_text = ui.ctx().format_shortcut(&TOGGLE_EDIT_MODE);
                let undo_text = ui.ctx().format_shortcut(&UNDO);
                let redo_text = ui.ctx().format_shortcut(&REDO);
                let active_file = active_file_id(app);
                let (can_undo, can_redo) = active_file
                    .and_then(|id| app.files.get(&id))
                    .map(|f| (f.editor.can_undo(), f.editor.can_redo()))
                    .unwrap_or((false, false));
                ui.add_enabled_ui(can_undo, |ui| {
                    if ui.add(egui::Button::new(hxy_i18n::t("menu-edit-undo")).shortcut_text(undo_text)).clicked() {
                        ui.close();
                        undo_active_file(app);
                    }
                });
                ui.add_enabled_ui(can_redo, |ui| {
                    if ui.add(egui::Button::new(hxy_i18n::t("menu-edit-redo")).shortcut_text(redo_text)).clicked() {
                        ui.close();
                        redo_active_file(app);
                    }
                });
                ui.separator();
                let mode_label = active_file
                    .and_then(|id| app.files.get(&id))
                    .map(|f| match f.editor.edit_mode() {
                        crate::file::EditMode::Readonly => hxy_i18n::t("menu-edit-enter-edit-mode"),
                        crate::file::EditMode::Mutable => hxy_i18n::t("menu-edit-leave-edit-mode"),
                    })
                    .unwrap_or_else(|| hxy_i18n::t("menu-edit-enter-edit-mode"));
                ui.add_enabled_ui(active_file.is_some(), |ui| {
                    if ui.add(egui::Button::new(mode_label).shortcut_text(toggle_text)).clicked() {
                        ui.close();
                        toggle_active_edit_mode(app);
                    }
                });
                ui.separator();
                ui.add_enabled_ui(active_file.is_some(), |ui| {
                    // Keep the two most common targets as top-level
                    // items so the keyboard shortcuts have an
                    // obvious visual anchor.
                    if ui
                        .add(egui::Button::new(hxy_i18n::t("menu-edit-copy-bytes")).shortcut_text(copy_bytes_text))
                        .clicked()
                    {
                        if let Some(id) = active_file
                            && let Some(file) = app.files.get(&id)
                        {
                            do_copy(ui.ctx(), file, CopyKind::BytesLossyUtf8);
                        }
                        ui.close();
                    }
                    if ui
                        .add(egui::Button::new(hxy_i18n::t("menu-edit-copy-hex")).shortcut_text(copy_hex_text))
                        .clicked()
                    {
                        if let Some(id) = active_file
                            && let Some(file) = app.files.get(&id)
                        {
                            do_copy(ui.ctx(), file, CopyKind::BytesHexSpaced);
                        }
                        ui.close();
                    }
                    ui.separator();
                    let paste_text = ui.ctx().format_shortcut(&PASTE);
                    let paste_hex_text = ui.ctx().format_shortcut(&PASTE_AS_HEX);
                    let can_paste = active_file
                        .and_then(|id| app.files.get(&id))
                        .is_some_and(|f| f.editor.edit_mode() == crate::file::EditMode::Mutable);
                    ui.add_enabled_ui(can_paste, |ui| {
                        if ui.add(egui::Button::new(hxy_i18n::t("menu-edit-paste")).shortcut_text(paste_text)).clicked()
                        {
                            ui.close();
                            paste_active_file(app, false);
                        }
                        if ui
                            .add(egui::Button::new(hxy_i18n::t("menu-edit-paste-as-hex")).shortcut_text(paste_hex_text))
                            .clicked()
                        {
                            ui.close();
                            paste_active_file(app, true);
                        }
                    });
                    ui.separator();
                    // ...and the long tail in a submenu, same layout as
                    // the hex view's right-click and the template
                    // panel's row menu.
                    let show_scalar = active_file
                        .and_then(|id| app.files.get(&id))
                        .and_then(|f| f.editor.selection())
                        .map(|s| matches!(s.range().len().get(), 1 | 2 | 4 | 8))
                        .unwrap_or(false);
                    if let Some(kind) = crate::copy_format::copy_as_menu(ui, show_scalar)
                        && let Some(id) = active_file
                        && let Some(file) = app.files.get(&id)
                    {
                        do_copy(ui.ctx(), file, kind);
                        ui.close();
                    }
                });
            });
            ui.menu_button(hxy_i18n::t("menu-view"), |ui| {
                if ui.button(hxy_i18n::t("menu-view-console")).clicked() {
                    app.toggle_console();
                    ui.close();
                }
                if ui.button(hxy_i18n::t("menu-view-inspector")).clicked() {
                    app.toggle_inspector();
                    ui.close();
                }
                if ui.button(hxy_i18n::t("menu-view-plugins")).clicked() {
                    app.toggle_plugins();
                    ui.close();
                }
            });
            ui.menu_button(hxy_i18n::t("menu-help"), |ui| {
                ui.label(format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION")));
            });
        });
    });
}

/// Close a specific File tab by id, no questions asked. Removes
/// the tab from the dock, drops the `OpenFile` from `app.files`,
/// and clears the matching persisted `OpenTabState` so the tab
/// doesn't reappear on next launch. Callers responsible for
/// gating on dirtiness -- this helper is the unconditional path
/// the modal's "Don't Save" branch uses.
fn close_file_tab_by_id(app: &mut HxyApp, id: FileId) {
    // Top-level Tab::File case: the simple path -- remove the dock
    // tab and drop the file.
    if let Some(path) = app.dock.find_tab(&Tab::File(id)) {
        let _ = app.dock.remove_tab(path);
    }
    // Workspace-entry case: scan every workspace's inner dock for
    // a `WorkspaceTab::Entry(id)` and remove it. The workspace
    // editor itself never closes through this path -- it goes
    // through `close_workspace_by_id`.
    for workspace in app.workspaces.values_mut() {
        if let Some(path) = workspace.dock.find_tab(&crate::file::WorkspaceTab::Entry(id)) {
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
}

/// Cmd+W entry point. Closes the currently focused tab. For File
/// tabs the dirty-check is the same one `on_close` uses: when the
/// editor has uncommitted edits the modal is staged instead of
/// dropping. Non-File tabs (Console, Inspector, Plugins, ...)
/// close immediately -- they have no save state.
#[cfg(not(target_arch = "wasm32"))]
fn request_close_active_tab(app: &mut HxyApp) {
    let Some((_, tab)) = app.dock.find_active_focused() else { return };
    let tab = *tab;
    match tab {
        Tab::File(id) => {
            if let Some(file) = app.files.get(&id)
                && file.editor.is_dirty()
            {
                app.pending_close_tab =
                    Some(PendingCloseTab { file_id: id, display_name: file.display_name.clone() });
                return;
            }
            close_file_tab_by_id(app, id);
        }
        Tab::Welcome | Tab::Settings => {
            // These two are non-closeable in the TabViewer
            // (`closeable` returns false), so Cmd+W matches.
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
    }
}

/// Collapse a workspace whose inner dock has been emptied of
/// everything except the Editor sub-tab back to a plain `Tab::File`
/// in the outer dock. The workspace entry is dropped from
/// `app.workspaces` and the persisted `as_workspace` flag is cleared.
fn collapse_workspace_to_file(app: &mut HxyApp, workspace_id: crate::file::WorkspaceId) {
    let Some(workspace) = app.workspaces.remove(&workspace_id) else { return };
    if app.last_active_workspace == Some(workspace_id) {
        app.last_active_workspace = None;
    }
    let editor_id = workspace.editor_id;

    // Replace the outer Tab::Workspace with Tab::File(editor_id) in
    // the same leaf so the user's pane layout doesn't shift.
    if let Some(path) = app.dock.find_tab(&Tab::Workspace(workspace_id)) {
        let _ = app.dock.remove_tab(path);
    }
    app.dock.push_to_focused_leaf(Tab::File(editor_id));
    if let Some(path) = app.dock.find_tab(&Tab::File(editor_id)) {
        let _ = app.dock.set_active_tab(path);
    }

    // Clear the persisted as_workspace flag so the next launch
    // restores the file as a plain tab rather than spawning an
    // empty workspace.
    if let Some(source) = app.files.get(&editor_id).and_then(|f| f.source_kind.clone()) {
        let mut g = app.state.write();
        if let Some(entry) = g.open_tabs.iter_mut().find(|t| t.source == source) {
            entry.as_workspace = false;
        }
    }
}

/// Close the entire `Tab::Workspace(workspace_id)` -- the editor
/// itself plus any open VFS entries inside the inner dock. Each
/// closing file is dirty-checked against `pending_close_tab` the
/// same way single-file closes are; if any sub-file is dirty the
/// close stalls until the modal returns.
#[cfg(not(target_arch = "wasm32"))]
fn close_workspace_by_id(app: &mut HxyApp, workspace_id: crate::file::WorkspaceId) {
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

    // Remove the editor file + any entry files. Also drop their
    // persistence rows so the next launch doesn't try to restore them
    // into a vanished workspace.
    let mut to_drop: Vec<FileId> = vec![workspace.editor_id];
    for (_, t) in workspace.dock.iter_all_tabs() {
        if let crate::file::WorkspaceTab::Entry(file_id) = t {
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

/// Cmd+W shortcut dispatcher. Sits next to the other shortcut
/// dispatchers in [`HxyApp::ui`]; runs after the palette/picker so
/// they can claim the keypress first if either is open.
#[cfg(not(target_arch = "wasm32"))]
fn dispatch_close_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    if ctx.input_mut(|i| i.consume_shortcut(&CLOSE_TAB)) {
        request_close_active_tab(app);
    }
}

/// Cmd+K stages the visual pane-focus picker. No-op when a picker
/// session is already active so a double-press doesn't rebind state
/// mid-pick.
#[cfg(not(target_arch = "wasm32"))]
fn dispatch_focus_pane_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    if !ctx.input_mut(|i| i.consume_shortcut(&FOCUS_PANE)) {
        return;
    }
    if app.pending_pane_pick.is_some() {
        return;
    }
    start_pane_focus(app);
}

/// Cmd+F opens / closes the active file tab's search bar; Cmd+Shift+F
/// opens the cross-file search results tab. The shortcut runs after
/// the palette so a Cmd+F typed while the palette is open isn't
/// stolen for search.
#[cfg(not(target_arch = "wasm32"))]
fn dispatch_find_shortcut(ctx: &egui::Context, app: &mut HxyApp) {
    let global = ctx.input_mut(|i| i.consume_shortcut(&FIND_GLOBAL));
    let local = !global && ctx.input_mut(|i| i.consume_shortcut(&FIND_LOCAL));
    if global {
        toggle_global_search(app);
        return;
    }
    if local {
        toggle_local_search(app);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn toggle_local_search(app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else { return };
    let Some(file) = app.files.get_mut(&id) else { return };
    file.search.open = !file.search.open;
    if file.search.open {
        file.search.refresh_pattern();
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn toggle_global_search(app: &mut HxyApp) {
    if let Some(path) = app.dock.find_tab(&Tab::SearchResults) {
        let _ = app.dock.remove_tab(path);
        return;
    }
    app.dock.main_surface_mut().split_below(egui_dock::NodeIndex::root(), 0.65, vec![Tab::SearchResults]);
    app.global_search.open = true;
}

/// Ctrl+Tab / Ctrl+Shift+Tab cycle tabs in the surface implied by
/// `app.tab_focus`: the outer dock's focused leaf when focus is on
/// the outer dock, or the workspace's inner dock when focus is on
/// a workspace. Wraps at the ends of the leaf's tab list. Never
/// crosses dock leaves.
fn dispatch_tab_cycle(ctx: &egui::Context, app: &mut HxyApp) {
    let forward = ctx.input_mut(|i| i.consume_shortcut(&NEXT_TAB));
    let backward = ctx.input_mut(|i| i.consume_shortcut(&PREV_TAB));
    if !forward && !backward {
        return;
    }
    match app.tab_focus {
        TabFocus::Outer => cycle_outer_focused_leaf(app, forward),
        TabFocus::Workspace(workspace_id) => {
            // If the workspace was closed since we last focused, fall
            // back to the outer dock so the keystroke still does
            // something useful.
            if !app.workspaces.contains_key(&workspace_id) {
                app.tab_focus = TabFocus::Outer;
                cycle_outer_focused_leaf(app, forward);
                return;
            }
            cycle_workspace_focused_leaf(app, workspace_id, forward);
        }
    }
}

fn cycle_outer_focused_leaf(app: &mut HxyApp, forward: bool) {
    let Some(node_path) = app.dock.focused_leaf() else { return };
    let Ok(leaf) = app.dock.leaf(node_path) else { return };
    let count = leaf.tabs().len();
    if count < 2 {
        return;
    }
    let current = leaf.active.0.min(count - 1);
    let next = if forward { (current + 1) % count } else { (current + count - 1) % count };
    let tab_path = egui_dock::TabPath::from((node_path, egui_dock::TabIndex(next)));
    let _ = app.dock.set_active_tab(tab_path);
}

fn cycle_workspace_focused_leaf(app: &mut HxyApp, workspace_id: crate::file::WorkspaceId, forward: bool) {
    let Some(workspace) = app.workspaces.get_mut(&workspace_id) else { return };
    // The workspace's inner dock has its own focused-leaf concept;
    // when nothing is focused (e.g. immediately after restore) we
    // default to the main surface root so cycling still works.
    let node_path = workspace
        .dock
        .focused_leaf()
        .unwrap_or(egui_dock::NodePath { surface: egui_dock::SurfaceIndex::main(), node: egui_dock::NodeIndex::root() });
    let Ok(leaf) = workspace.dock.leaf(node_path) else { return };
    let count = leaf.tabs().len();
    if count < 2 {
        return;
    }
    let current = leaf.active.0.min(count - 1);
    let next = if forward { (current + 1) % count } else { (current + count - 1) % count };
    let tab_path = egui_dock::TabPath::from((node_path, egui_dock::TabIndex(next)));
    let _ = workspace.dock.set_active_tab(tab_path);
}

/// Alt+Tab toggles `tab_focus` between the outer dock and the
/// workspace currently active in the outer dock. If the active outer
/// tab isn't a workspace, the toggle is a no-op (there's nothing to
/// switch to).
fn dispatch_tab_focus_toggle(ctx: &egui::Context, app: &mut HxyApp) {
    if !ctx.input_mut(|i| i.consume_shortcut(&TOGGLE_TAB_FOCUS)) {
        return;
    }
    match app.tab_focus {
        TabFocus::Outer => {
            // Only switch into a workspace if the active outer tab
            // *is* one. Otherwise there's no inner dock to cycle.
            if let Some((_, tab)) = app.dock.find_active_focused()
                && let Tab::Workspace(workspace_id) = *tab
            {
                app.tab_focus = TabFocus::Workspace(workspace_id);
            }
        }
        TabFocus::Workspace(_) => {
            app.tab_focus = TabFocus::Outer;
        }
    }
}

/// Render the "Save before closing?" modal when a close request
/// is staged in `pending_close_tab`. Three terminal actions: Save
/// -> save then close (only if save actually wrote bytes; a
/// cancelled save dialog leaves the tab open and the staged
/// request is cleared so the user starts fresh next press),
/// Don't Save -> close immediately, Cancel -> do nothing.
#[cfg(not(target_arch = "wasm32"))]
fn render_close_tab_dialog(ctx: &egui::Context, app: &mut HxyApp) {
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
            if save_file_by_id(app, pending.file_id, false) {
                close_file_tab_by_id(app, pending.file_id);
            }
        }
        CloseTabAction::Discard => close_file_tab_by_id(app, pending.file_id),
        CloseTabAction::Cancel => {}
    }
}

#[cfg(not(target_arch = "wasm32"))]
enum CloseTabAction {
    Save,
    Discard,
    Cancel,
}

/// Stage a visual pane-pick session. Resolves the source leaf the
/// same way the directional commands do (focused leaf, falling back
/// to the active file's leaf), closes the palette so the overlay
/// owns the screen, and records the op for `handle_pane_pick` to
/// drive next frame. No-op when there's no resolvable source.
#[cfg(not(target_arch = "wasm32"))]
fn start_pane_pick(app: &mut HxyApp, op: crate::pane_pick::PaneOp) {
    let Some(source) = resolve_target_leaf(app) else { return };
    app.palette.close();
    app.pending_pane_pick = Some(crate::pane_pick::PendingPanePick { op, source: Some(source) });
}

/// Sourceless variant: stage a pane pick whose op doesn't need a
/// "from" leaf (currently just `Focus`). Every leaf in the dock
/// becomes a target. No-op when there's no dock (shouldn't happen).
#[cfg(not(target_arch = "wasm32"))]
fn start_pane_focus(app: &mut HxyApp) {
    app.palette.close();
    app.pending_pane_pick = Some(crate::pane_pick::PendingPanePick {
        op: crate::pane_pick::PaneOp::Focus,
        source: None,
    });
}

/// Drive one frame of the visual pane picker. Reads layout from the
/// dock (no mutation), then applies the chosen op via the same
/// helpers the directional commands use. Closes the palette as a
/// side effect of entering the pick (handled at command dispatch);
/// here we just consume input and execute when a target is hit.
#[cfg(not(target_arch = "wasm32"))]
fn handle_pane_pick(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(pending) = app.pending_pane_pick else { return };
    let outcome = crate::pane_pick::tick(ctx, &app.dock, pending, &mut app.pane_pick_letters);
    match outcome {
        crate::pane_pick::TickOutcome::Continue => {}
        crate::pane_pick::TickOutcome::Cancel => {
            app.pending_pane_pick = None;
        }
        crate::pane_pick::TickOutcome::Picked { source, target, op } => {
            app.pending_pane_pick = None;
            match op {
                crate::pane_pick::PaneOp::MoveTab => {
                    if let Some(source) = source {
                        dock_move_tab_to(app, source, target);
                    }
                }
                crate::pane_pick::PaneOp::Merge => {
                    if let Some(source) = source {
                        dock_merge_to(app, source, target);
                    }
                }
                crate::pane_pick::PaneOp::Focus => {
                    // Move keyboard focus + active tab into the
                    // picked leaf. Snap TabFocus back to Outer so the
                    // next Ctrl+Tab cycles top-level tabs in the
                    // newly focused pane (rather than continuing to
                    // cycle the previously-active workspace's inner
                    // dock).
                    app.dock.set_focused_node_and_surface(target);
                    app.tab_focus = TabFocus::Outer;
                }
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn handle_command_palette(ctx: &egui::Context, app: &mut HxyApp) {
    let toggle = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::P);
    if ctx.input_mut(|i| i.consume_shortcut(&toggle)) {
        if app.palette.is_open() {
            app.palette.close();
        } else {
            // The palette and the visual pane picker can't coexist:
            // both want full-screen keyboard ownership. Opening the
            // palette implicitly cancels any staged pick.
            app.pending_pane_pick = None;
            app.palette.open_at(crate::command_palette::Mode::Main);
        }
    }
    if !app.palette.is_open() {
        return;
    }
    let copy_ctx = copy_palette_context(app);
    let history_ctx = history_palette_context(app);
    let template_ctx = template_palette_context(app);
    let offset_ctx = offset_palette_context(app);
    let entries = build_palette_entries(ctx, app, copy_ctx, history_ctx, &template_ctx, &offset_ctx);
    let Some(outcome) = crate::command_palette::show(ctx, &mut app.palette, entries) else { return };
    match outcome {
        crate::command_palette::Outcome::Dismissed(reason) => dismiss_palette(app, reason),
        crate::command_palette::Outcome::Picked(action) => apply_palette_action(ctx, app, action),
    }
}

/// Decide what to do when the palette is dismissed without a pick.
/// Backdrop clicks always fully close. A dismiss key (Escape by
/// default) pops back to the parent cascade level when the user
/// has opted into that behaviour and we're in a sub-mode; otherwise
/// it closes outright.
#[cfg(not(target_arch = "wasm32"))]
fn dismiss_palette(app: &mut HxyApp, reason: crate::command_palette::DismissReason) {
    use crate::command_palette::DismissReason;
    match reason {
        DismissReason::Backdrop => app.palette.close(),
        DismissReason::Key(_) => {
            let pop = app.state.read().app.palette_escape_pops_to_parent;
            match (pop, app.palette.mode.parent()) {
                (true, Some(parent)) => app.palette.open_at(parent),
                _ => app.palette.close(),
            }
        }
    }
}

/// Snapshot of the active selection used by the palette to decide
/// which `Copy as...` entries to expose. `None` when no file is
/// focused or the selection is empty.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy)]
struct CopyPaletteContext {
    /// True when the selection width is a scalar integer width
    /// (1/2/4/8 bytes), meaning the `Copy value as...` options apply.
    scalar_width: bool,
}

#[cfg(not(target_arch = "wasm32"))]
fn history_palette_context(app: &mut HxyApp) -> HistoryPaletteContext {
    let Some(id) = active_file_id(app) else { return HistoryPaletteContext::default() };
    let Some(file) = app.files.get(&id) else { return HistoryPaletteContext::default() };
    HistoryPaletteContext {
        can_undo: file.editor.can_undo(),
        can_redo: file.editor.can_redo(),
        can_paste: file.editor.edit_mode() == crate::file::EditMode::Mutable,
        has_active_file: true,
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn copy_palette_context(app: &mut HxyApp) -> Option<CopyPaletteContext> {
    let id = active_file_id(app)?;
    let file = app.files.get(&id)?;
    let sel = file.editor.selection()?;
    let range = sel.range();
    if range.is_empty() {
        return None;
    }
    Some(CopyPaletteContext { scalar_width: matches!(range.len().get(), 1 | 2 | 4 | 8) })
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Default)]
struct HistoryPaletteContext {
    can_undo: bool,
    can_redo: bool,
    /// True when the active tab is mutable and would accept a paste.
    can_paste: bool,
    /// True when an active file tab exists, regardless of edit mode.
    /// Gates toggle-read-only and other tab-level actions.
    has_active_file: bool,
}

/// Snapshot of the active tab used for ranking `Run Template`
/// entries against its content. Empty when no file is active --
/// `rank_entries` falls through to the default ordering in that
/// case.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Default)]
struct TemplatePaletteContext {
    extension: Option<String>,
    head_bytes: Vec<u8>,
}

/// Snapshot of the active tab's caret + source length, used by the
/// Go-To / Select palette modes to resolve relative offsets and
/// bounds-check resulting ranges.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Default)]
struct OffsetPaletteContext {
    cursor: u64,
    source_len: u64,
    available: bool,
    /// `Some((start, end_exclusive))` when the active tab has a
    /// non-empty selection (including a single-byte caret). `None`
    /// means no selection exists -- caret-specific copy entries
    /// skip themselves in that case.
    selection: Option<(u64, u64)>,
}

#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug)]
enum OffsetCopy {
    Caret,
    SelectionRange,
    SelectionLength,
    FileLength,
}

/// Copy a formatted offset / length / range from the active tab to
/// the clipboard. Used by the palette's Copy-caret / Copy-selection
/// / Copy-file-length entries; formatting matches the status bar
/// (current `OffsetBase` setting).
#[cfg(not(target_arch = "wasm32"))]
fn copy_formatted_offset(ctx: &egui::Context, app: &mut HxyApp, kind: OffsetCopy) {
    let Some(id) = active_file_id(app) else { return };
    let base = app.state.read().app.offset_base;
    let Some(file) = app.files.get(&id) else { return };
    let source_len = file.editor.source().len().get();
    let sel = file.editor.selection();
    let text = match kind {
        OffsetCopy::Caret => {
            let Some(sel) = sel else { return };
            format_offset(sel.cursor.get(), base)
        }
        OffsetCopy::SelectionRange => {
            let Some(sel) = sel else { return };
            let range = sel.range();
            let last_inclusive = range.end().get().saturating_sub(1);
            format!(
                "{}-{} ({} bytes)",
                format_offset(range.start().get(), base),
                format_offset(last_inclusive, base),
                format_offset(range.len().get(), base),
            )
        }
        OffsetCopy::SelectionLength => {
            let Some(sel) = sel else { return };
            format_offset(sel.range().len().get(), base)
        }
        OffsetCopy::FileLength => format_offset(source_len, base),
    };
    ctx.copy_text(text);
}

#[cfg(not(target_arch = "wasm32"))]
fn offset_palette_context(app: &mut HxyApp) -> OffsetPaletteContext {
    let Some(id) = active_file_id(app) else { return OffsetPaletteContext::default() };
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

#[cfg(not(target_arch = "wasm32"))]
fn template_palette_context(app: &mut HxyApp) -> TemplatePaletteContext {
    let Some(id) = active_file_id(app) else { return TemplatePaletteContext::default() };
    let Some(file) = app.files.get(&id) else { return TemplatePaletteContext::default() };
    let extension = file
        .root_path()
        .and_then(|p| p.extension())
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    let source_len = file.editor.source().len().get();
    let window = source_len.min(crate::template_library::DETECTION_WINDOW as u64);
    let head_bytes = if window == 0 {
        Vec::new()
    } else if let Ok(range) = hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(window)) {
        file.editor.source().read(range).unwrap_or_default()
    } else {
        Vec::new()
    };
    TemplatePaletteContext { extension, head_bytes }
}

#[cfg(not(target_arch = "wasm32"))]
fn build_palette_entries(
    ctx: &egui::Context,
    app: &HxyApp,
    copy_ctx: Option<CopyPaletteContext>,
    history_ctx: HistoryPaletteContext,
    template_ctx: &TemplatePaletteContext,
    offset_ctx: &OffsetPaletteContext,
) -> Vec<egui_palette::Entry<crate::command_palette::Action>> {
    use crate::command_palette::Action;
    use crate::command_palette::Mode;
    use egui_phosphor::regular as icon;

    let fmt = |sc: &egui::KeyboardShortcut| ctx.format_shortcut(sc);
    let mut out: Vec<egui_palette::Entry<Action>> = Vec::new();
    match app.palette.mode {
        Mode::Main => {
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("menu-file-new"), Action::InvokeCommand(crate::command_palette::PaletteCommand::NewFile))
                    .with_icon(icon::FILE_PLUS)
                    .with_shortcut(fmt(&NEW_FILE)),
            );
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("toolbar-open-file"), Action::InvokeCommand(crate::command_palette::PaletteCommand::OpenFile))
                    .with_icon(icon::FOLDER_OPEN),
            );
            // "Open recent" cascades into a filtered list of recently
            // used files, omitting paths that are already open in the
            // current session.
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
                    Action::InvokeCommand(crate::command_palette::PaletteCommand::BrowseVfs),
                )
                .with_icon(icon::TREE_STRUCTURE),
            );
            // Toggle entries for the side panels. Subtitle flips
            // between "Show" / "Hide" so the user knows which
            // direction activation will take them without having
            // to peek at the dock.
            let panel_subtitle = |visible: bool| -> String {
                hxy_i18n::t(if visible { "palette-subtitle-hide" } else { "palette-subtitle-show" })
            };
            let console_visible = app.dock.find_tab(&Tab::Console).is_some();
            let inspector_visible = app.dock.find_tab(&Tab::Inspector).is_some();
            let plugins_visible = app.dock.find_tab(&Tab::Plugins).is_some();
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("menu-view-console"),
                    Action::InvokeCommand(crate::command_palette::PaletteCommand::ToggleConsole),
                )
                .with_icon(icon::TERMINAL)
                .with_subtitle(panel_subtitle(console_visible)),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("menu-view-inspector"),
                    Action::InvokeCommand(crate::command_palette::PaletteCommand::ToggleInspector),
                )
                .with_icon(icon::EYE)
                .with_subtitle(panel_subtitle(inspector_visible)),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("menu-view-plugins"),
                    Action::InvokeCommand(crate::command_palette::PaletteCommand::TogglePlugins),
                )
                .with_icon(icon::PUZZLE_PIECE)
                .with_subtitle(panel_subtitle(plugins_visible)),
            );

            // Toggle the workspace VFS tree (only meaningful when the
            // active tab is a workspace; the dispatcher no-ops
            // otherwise so we still surface the entry for keyboard
            // discoverability).
            let workspace_tree_visible = app
                .dock
                .focused_leaf()
                .and_then(|p| app.dock.leaf(p).ok())
                .and_then(|leaf| leaf.tabs().get(leaf.active.0))
                .and_then(|tab| match tab {
                    Tab::Workspace(workspace_id) => app
                        .workspaces
                        .get(workspace_id)
                        .map(|w| w.dock.find_tab(&crate::file::WorkspaceTab::VfsTree).is_some()),
                    _ => None,
                })
                .unwrap_or(false);
            out.push(
                egui_palette::Entry::new(
                    "Toggle VFS panel",
                    Action::InvokeCommand(crate::command_palette::PaletteCommand::ToggleWorkspaceVfs),
                )
                .with_icon(icon::TREE_STRUCTURE)
                .with_subtitle(panel_subtitle(workspace_tree_visible)),
            );

            // Toggle the right-hand tool panel as a unit (Plugins
            // manager + every plugin mount tab). Distinct from the
            // single-panel Plugins toggle above.
            let tool_panel_visible = app.hidden_tool_tabs.is_empty()
                && app.dock.iter_all_tabs().any(|(_, t)| is_tool_tab(t));
            out.push(
                egui_palette::Entry::new(
                    "Toggle tool panel",
                    Action::InvokeCommand(crate::command_palette::PaletteCommand::ToggleToolPanel),
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
                    egui_palette::Entry::new(hxy_i18n::t("menu-edit-undo"), Action::InvokeCommand(crate::command_palette::PaletteCommand::Undo))
                        .with_icon(icon::ARROW_COUNTER_CLOCKWISE)
                        .with_shortcut(fmt(&UNDO)),
                );
            }
            if history_ctx.can_redo {
                out.push(
                    egui_palette::Entry::new(hxy_i18n::t("menu-edit-redo"), Action::InvokeCommand(crate::command_palette::PaletteCommand::Redo))
                        .with_icon(icon::ARROW_CLOCKWISE)
                        .with_shortcut(fmt(&REDO)),
                );
            }
            if history_ctx.has_active_file {
                // Subtitle advertises the *resulting* state: when
                // currently mutable, invoking flips us to readonly,
                // and vice-versa. Same intent as the icon, spelled
                // out in words so the user doesn't have to interpret
                // padlock vs open-padlock semantics.
                let (result_key, toggle_icon) = if history_ctx.can_paste {
                    ("palette-toggle-readonly-result-readonly", icon::LOCK)
                } else {
                    ("palette-toggle-readonly-result-mutable", icon::LOCK_OPEN)
                };
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-toggle-readonly"),
                        Action::InvokeCommand(crate::command_palette::PaletteCommand::ToggleEditMode),
                    )
                    .with_subtitle(hxy_i18n::t(result_key))
                    .with_icon(toggle_icon)
                    .with_shortcut(fmt(&TOGGLE_EDIT_MODE)),
                );
            }
            if history_ctx.can_paste {
                out.push(
                    egui_palette::Entry::new(hxy_i18n::t("menu-edit-paste"), Action::InvokeCommand(crate::command_palette::PaletteCommand::Paste))
                        .with_icon(icon::CLIPBOARD_TEXT)
                        .with_shortcut(fmt(&PASTE)),
                );
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("menu-edit-paste-as-hex"),
                        Action::InvokeCommand(crate::command_palette::PaletteCommand::PasteAsHex),
                    )
                    .with_icon(icon::CLIPBOARD_TEXT)
                    .with_shortcut(fmt(&PASTE_AS_HEX)),
                );
            }
            if history_ctx.has_active_file {
                // Subtitles preview what would hit the clipboard so
                // the user doesn't have to guess between hex and
                // decimal or inclusive vs exclusive end.
                let base = app.state.read().app.offset_base;
                if let Some((start, end_exclusive)) = offset_ctx.selection {
                    let last_inclusive = end_exclusive.saturating_sub(1);
                    let len = end_exclusive.saturating_sub(start);
                    let caret_preview = format_offset(offset_ctx.cursor, base);
                    out.push(
                        egui_palette::Entry::new(
                            hxy_i18n::t("palette-copy-caret-offset"),
                            Action::InvokeCommand(crate::command_palette::PaletteCommand::CopyCaretOffset),
                        )
                        .with_icon(icon::COPY)
                        .with_subtitle(caret_preview),
                    );
                    if len > 1 {
                        let len_preview = format_offset(len, base);
                        let range_preview = format!(
                            "{}-{} ({} bytes)",
                            format_offset(start, base),
                            format_offset(last_inclusive, base),
                            len_preview,
                        );
                        out.push(
                            egui_palette::Entry::new(
                                hxy_i18n::t("palette-copy-selection-range"),
                                Action::InvokeCommand(crate::command_palette::PaletteCommand::CopySelectionRange),
                            )
                            .with_icon(icon::COPY)
                            .with_subtitle(range_preview),
                        );
                        out.push(
                            egui_palette::Entry::new(
                                hxy_i18n::t("palette-copy-selection-length"),
                                Action::InvokeCommand(crate::command_palette::PaletteCommand::CopySelectionLength),
                            )
                            .with_icon(icon::COPY)
                            .with_subtitle(len_preview),
                        );
                    }
                }
                out.push(
                    egui_palette::Entry::new(
                        hxy_i18n::t("palette-copy-file-length"),
                        Action::InvokeCommand(crate::command_palette::PaletteCommand::CopyFileLength),
                    )
                    .with_icon(icon::COPY)
                    .with_subtitle(format_offset(offset_ctx.source_len, base)),
                );
            }
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-split-right"), Action::InvokeCommand(crate::command_palette::PaletteCommand::SplitRight))
                    .with_icon(icon::ARROW_SQUARE_RIGHT),
            );
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-split-left"), Action::InvokeCommand(crate::command_palette::PaletteCommand::SplitLeft))
                    .with_icon(icon::ARROW_SQUARE_LEFT),
            );
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-split-down"), Action::InvokeCommand(crate::command_palette::PaletteCommand::SplitDown))
                    .with_icon(icon::ARROW_SQUARE_DOWN),
            );
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-split-up"), Action::InvokeCommand(crate::command_palette::PaletteCommand::SplitUp))
                    .with_icon(icon::ARROW_SQUARE_UP),
            );
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-merge-right"), Action::InvokeCommand(crate::command_palette::PaletteCommand::MergeRight))
                    .with_icon(icon::ARROW_LINE_RIGHT),
            );
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-merge-left"), Action::InvokeCommand(crate::command_palette::PaletteCommand::MergeLeft))
                    .with_icon(icon::ARROW_LINE_LEFT),
            );
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-merge-down"), Action::InvokeCommand(crate::command_palette::PaletteCommand::MergeDown))
                    .with_icon(icon::ARROW_LINE_DOWN),
            );
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-merge-up"), Action::InvokeCommand(crate::command_palette::PaletteCommand::MergeUp))
                    .with_icon(icon::ARROW_LINE_UP),
            );
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-move-tab-right"), Action::InvokeCommand(crate::command_palette::PaletteCommand::MoveTabRight))
                    .with_icon(icon::ARROW_FAT_RIGHT),
            );
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-move-tab-left"), Action::InvokeCommand(crate::command_palette::PaletteCommand::MoveTabLeft))
                    .with_icon(icon::ARROW_FAT_LEFT),
            );
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-move-tab-down"), Action::InvokeCommand(crate::command_palette::PaletteCommand::MoveTabDown))
                    .with_icon(icon::ARROW_FAT_DOWN),
            );
            out.push(
                egui_palette::Entry::new(hxy_i18n::t("palette-move-tab-up"), Action::InvokeCommand(crate::command_palette::PaletteCommand::MoveTabUp))
                    .with_icon(icon::ARROW_FAT_UP),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-move-tab-visual"),
                    Action::InvokeCommand(crate::command_palette::PaletteCommand::MoveTabVisual),
                )
                .with_icon(icon::CROSSHAIR_SIMPLE)
                .with_subtitle(hxy_i18n::t("palette-pane-pick-subtitle")),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-merge-visual"),
                    Action::InvokeCommand(crate::command_palette::PaletteCommand::MergeVisual),
                )
                .with_icon(icon::CROSSHAIR_SIMPLE)
                .with_subtitle(hxy_i18n::t("palette-pane-pick-subtitle")),
            );
            out.push(
                egui_palette::Entry::new(
                    hxy_i18n::t("palette-focus-pane"),
                    Action::InvokeCommand(crate::command_palette::PaletteCommand::FocusPane),
                )
                .with_icon(icon::CROSSHAIR_SIMPLE)
                .with_subtitle(hxy_i18n::t("palette-pane-pick-subtitle"))
                .with_shortcut(fmt(&FOCUS_PANE)),
            );
            if let Some(copy) = copy_ctx {
                for (label, kind) in crate::copy_format::BYTES_MENU {
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
                    for (label, kind) in crate::copy_format::VALUE_MENU {
                        out.push(
                            egui_palette::Entry::new(format!("Copy value: {label}"), Action::Copy(*kind))
                                .with_icon(icon::COPY),
                        );
                    }
                }
            }
            for (id, file) in &app.files {
                let mut entry =
                    egui_palette::Entry::new(file.display_name.clone(), Action::FocusFile(*id)).with_icon(icon::FILE);
                if let Some(parent) = file.root_path().and_then(|p| p.parent()) {
                    entry = entry.with_subtitle(parent.display().to_string());
                }
                out.push(entry);
            }
            // Plugin-contributed entries. Each loaded plugin is
            // asked for its current command list (gated host-side
            // by the `commands` permission); the host prefixes the
            // displayed label with the plugin's name so duplicates
            // across plugins stay disambiguated. The `puzzle-piece`
            // icon is the fallback when the plugin doesn't supply
            // one of its own.
            for plugin in &app.plugin_handlers {
                let plugin_name = plugin.name().to_owned();
                for cmd in plugin.list_commands() {
                    let mut entry = egui_palette::Entry::new(
                        format!("{plugin_name}: {}", cmd.label),
                        Action::InvokePluginCommand {
                            plugin_name: plugin_name.clone(),
                            command_id: cmd.id,
                        },
                    );
                    if let Some(s) = cmd.subtitle {
                        entry = entry.with_subtitle(s);
                    }
                    entry = entry.with_icon(cmd.icon.unwrap_or_else(|| icon::PUZZLE_PIECE.to_string()));
                    out.push(entry);
                }
            }
        }
        Mode::Templates => {
            let ranked =
                app.templates.rank_entries(template_ctx.extension.as_deref(), &template_ctx.head_bytes);
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
            if let Some(dir) = user_templates_dir() {
                for path in crate::template_library::list_installed_templates(&dir) {
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
            // Pull from both plugin directories so a single palette
            // mode covers handler plugins (`plugins/`) and template
            // runtimes (`template-plugins/`). The dispatcher handles
            // both the same way: delete the .wasm + sidecar, drop
            // grant, clear state, rescan.
            for dir in [user_plugins_dir(), user_template_plugins_dir()].into_iter().flatten() {
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
            // Already-open paths would just switch focus; FocusFile
            // from Main covers that case cleanly. Filter them out
            // here so "Open recent" only offers files that would
            // actually open a fresh tab.
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
        Mode::GoToOffset | Mode::SelectFromOffset | Mode::SelectRange => {
            // Argument-style modes -- the palette's query *is* the
            // input. Build a single dynamic entry that's actionable
            // only when the query parses; invalid queries show an
            // Invalid row that picks to a no-op (no dispatch arm).
            let query = app.palette.inner.query.trim();
            if !offset_ctx.available {
                // Surface the failure mode instead of returning an
                // empty list -- otherwise the user just sees the
                // generic "No matches." panel and has no idea why
                // their range didn't take.
                invalid_entry(&mut out, query, &hxy_i18n::t("palette-invalid-no-active-file"));
            } else {
                build_offset_entries(&mut out, app.palette.mode, query, offset_ctx);
            }
        }
        Mode::SetColumnsLocal | Mode::SetColumnsGlobal => {
            let query = app.palette.inner.query.trim();
            // Local needs an active tab to write into; Global writes
            // straight into the persisted settings so it's always
            // available.
            if matches!(app.palette.mode, Mode::SetColumnsLocal) && !offset_ctx.available {
                invalid_entry(&mut out, query, &hxy_i18n::t("palette-invalid-no-active-file"));
            } else {
                build_columns_entries(&mut out, app.palette.mode, query);
            }
        }
        Mode::PluginCascade => {
            // The plugin's `Cascade` outcome stashed both the
            // commands list and the plugin name on `palette.plugin_cascade`.
            // We render entries from that snapshot rather than
            // re-asking the plugin every frame -- otherwise the
            // user's selection would jitter as the list rebuilt.
            if let Some(cascade) = app.palette.plugin_cascade.as_ref() {
                let plugin_name = &cascade.plugin_name;
                for cmd in &cascade.commands {
                    let mut entry = egui_palette::Entry::new(
                        cmd.label.clone(),
                        Action::InvokePluginCommand {
                            plugin_name: plugin_name.clone(),
                            command_id: cmd.id.clone(),
                        },
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
            // Argument-style prompt: one dynamic entry whose label
            // is the user's current input, action sends the answer
            // back to the plugin. Mirrors the Go-To Offset / Select
            // Range modes -- bypass_filter is on for this mode so
            // the entry isn't fuzzy-filtered out as the user types.
            if let Some(prompt) = app.palette.plugin_prompt.as_ref() {
                let answer = app.palette.inner.query.clone();
                let label = if answer.is_empty() {
                    hxy_i18n::t("palette-plugin-prompt-empty")
                } else {
                    answer.clone()
                };
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

#[cfg(not(target_arch = "wasm32"))]
fn build_offset_entries(
    out: &mut Vec<egui_palette::Entry<crate::command_palette::Action>>,
    mode: crate::command_palette::Mode,
    query: &str,
    offset_ctx: &OffsetPaletteContext,
) {
    use crate::command_palette::Action;
    use crate::command_palette::Mode;
    use egui_phosphor::regular as icon;

    if query.is_empty() {
        return;
    }
    match mode {
        Mode::GoToOffset => match crate::goto::parse_number(query)
            .and_then(|n| n.resolve(offset_ctx.cursor, offset_ctx.source_len).ok_or(crate::goto::ParseError::OutOfRange))
        {
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
            Err(e) => invalid_entry(out, query, &e.to_string()),
        },
        Mode::SelectFromOffset => match crate::goto::parse_number(query) {
            // A byte count. Relative doesn't make sense here (what
            // is it relative to?), but treating `+N`/`-N` like `N`
            // /`abs(N)` would silently accept typos; require the
            // absolute form so the input matches the mental model.
            Ok(crate::goto::Number::Absolute(count)) if count > 0 => {
                let start = offset_ctx.cursor;
                let available = offset_ctx.source_len.saturating_sub(start);
                if available == 0 {
                    invalid_entry(out, query, "at EOF");
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
            Ok(crate::goto::Number::Absolute(_)) => invalid_entry(out, query, "count must be nonzero"),
            Ok(crate::goto::Number::Relative(_)) => {
                invalid_entry(out, query, "count must be absolute (no + / - prefix)")
            }
            Err(e) => invalid_entry(out, query, &e.to_string()),
        },
        Mode::SelectRange => match crate::goto::parse_range(query, offset_ctx.source_len) {
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
            Err(e) => invalid_entry(out, query, &e.to_string()),
        },
        _ => {}
    }
}

/// Match the Settings panel's slider cap so a user can't end up
/// with a hex view they can't comfortably read. The underlying
/// [`hxy_core::ColumnCount`] allows up to `u16::MAX`, but anything
/// above this overflows even ultrawide monitors at sane font sizes.
#[cfg(not(target_arch = "wasm32"))]
const PALETTE_MAX_COLUMNS: u16 = 64;

#[cfg(not(target_arch = "wasm32"))]
fn build_columns_entries(
    out: &mut Vec<egui_palette::Entry<crate::command_palette::Action>>,
    mode: crate::command_palette::Mode,
    query: &str,
) {
    use crate::command_palette::Action;
    use crate::command_palette::ColumnScope;
    use crate::command_palette::Mode;
    use egui_phosphor::regular as icon;

    if query.is_empty() {
        return;
    }
    let scope = match mode {
        Mode::SetColumnsLocal => ColumnScope::Local,
        Mode::SetColumnsGlobal => ColumnScope::Global,
        _ => return,
    };
    let parsed = match crate::goto::parse_number(query) {
        Ok(crate::goto::Number::Absolute(n)) => n,
        Ok(crate::goto::Number::Relative(_)) => {
            invalid_entry(out, query, "column count must be absolute (no + / - prefix)");
            return;
        }
        Err(e) => {
            invalid_entry(out, query, &e.to_string());
            return;
        }
    };
    let n_u16 = match u16::try_from(parsed) {
        Ok(n) if (1..=u64::from(PALETTE_MAX_COLUMNS)).contains(&parsed) => n,
        _ => {
            invalid_entry(
                out,
                query,
                &hxy_i18n::t_args("palette-invalid-columns-range", &[("max", &PALETTE_MAX_COLUMNS.to_string())]),
            );
            return;
        }
    };
    let count = match hxy_core::ColumnCount::new(n_u16) {
        Ok(c) => c,
        Err(e) => {
            invalid_entry(out, query, &e.to_string());
            return;
        }
    };
    let (key, scope_icon) = match scope {
        ColumnScope::Local => ("palette-set-columns-local-fmt", icon::COLUMNS),
        ColumnScope::Global => ("palette-set-columns-global-fmt", icon::COLUMNS_PLUS_RIGHT),
    };
    out.push(
        egui_palette::Entry::new(
            hxy_i18n::t_args(key, &[("count", &n_u16.to_string())]),
            Action::SetColumns { scope, count },
        )
        .with_icon(scope_icon),
    );
}

/// Push a non-actionable "Invalid: {reason}" row. Activating it
/// falls through to `apply_palette_action`'s existing Invalid arm
/// which just closes the palette -- keeps a visible indication
/// that the query isn't parseable without silently showing an
/// empty list.
#[cfg(not(target_arch = "wasm32"))]
fn invalid_entry(
    out: &mut Vec<egui_palette::Entry<crate::command_palette::Action>>,
    query: &str,
    reason: &str,
) {
    use crate::command_palette::Action;
    use egui_phosphor::regular as icon;

    out.push(
        egui_palette::Entry::new(hxy_i18n::t_args("palette-invalid-fmt", &[("reason", reason)]), Action::NoOp)
            .with_icon(icon::WARNING)
            .with_subtitle(query.to_owned()),
    );
}

#[cfg(not(target_arch = "wasm32"))]
fn apply_palette_action(ctx: &egui::Context, app: &mut HxyApp, action: crate::command_palette::Action) {
    use crate::commands::CommandEffect;
    match action {
        crate::command_palette::Action::InvokeCommand(id) => {
            app.palette.close();
            {
                use crate::command_palette::PaletteCommand;
                use crate::commands::DockDir;
                match id {
                    PaletteCommand::NewFile => handle_new_file(app),
                    PaletteCommand::OpenFile => apply_command_effect(ctx, app, CommandEffect::OpenFileDialog),
                    PaletteCommand::BrowseVfs => apply_command_effect(ctx, app, CommandEffect::MountActiveFile),
                    PaletteCommand::ToggleWorkspaceVfs => toggle_workspace_vfs(app),
                    PaletteCommand::ToggleToolPanel => toggle_tool_panel(app),
                    PaletteCommand::ToggleConsole => app.toggle_console(),
                    PaletteCommand::ToggleInspector => app.toggle_inspector(),
                    PaletteCommand::TogglePlugins => app.toggle_plugins(),
                    PaletteCommand::Undo => apply_command_effect(ctx, app, CommandEffect::UndoActiveFile),
                    PaletteCommand::Redo => apply_command_effect(ctx, app, CommandEffect::RedoActiveFile),
                    PaletteCommand::Paste => paste_active_file(app, false),
                    PaletteCommand::PasteAsHex => paste_active_file(app, true),
                    PaletteCommand::SplitRight => apply_command_effect(ctx, app, CommandEffect::DockSplit(DockDir::Right)),
                    PaletteCommand::SplitLeft => apply_command_effect(ctx, app, CommandEffect::DockSplit(DockDir::Left)),
                    PaletteCommand::SplitUp => apply_command_effect(ctx, app, CommandEffect::DockSplit(DockDir::Up)),
                    PaletteCommand::SplitDown => apply_command_effect(ctx, app, CommandEffect::DockSplit(DockDir::Down)),
                    PaletteCommand::MergeRight => apply_command_effect(ctx, app, CommandEffect::DockMerge(DockDir::Right)),
                    PaletteCommand::MergeLeft => apply_command_effect(ctx, app, CommandEffect::DockMerge(DockDir::Left)),
                    PaletteCommand::MergeUp => apply_command_effect(ctx, app, CommandEffect::DockMerge(DockDir::Up)),
                    PaletteCommand::MergeDown => apply_command_effect(ctx, app, CommandEffect::DockMerge(DockDir::Down)),
                    PaletteCommand::MoveTabRight => apply_command_effect(ctx, app, CommandEffect::DockMoveTab(DockDir::Right)),
                    PaletteCommand::MoveTabLeft => apply_command_effect(ctx, app, CommandEffect::DockMoveTab(DockDir::Left)),
                    PaletteCommand::MoveTabUp => apply_command_effect(ctx, app, CommandEffect::DockMoveTab(DockDir::Up)),
                    PaletteCommand::MoveTabDown => apply_command_effect(ctx, app, CommandEffect::DockMoveTab(DockDir::Down)),
                    PaletteCommand::MoveTabVisual => start_pane_pick(app, crate::pane_pick::PaneOp::MoveTab),
                    PaletteCommand::MergeVisual => start_pane_pick(app, crate::pane_pick::PaneOp::Merge),
                    PaletteCommand::FocusPane => start_pane_focus(app),
                    PaletteCommand::ToggleEditMode => toggle_active_edit_mode(app),
                    PaletteCommand::CopyCaretOffset => copy_formatted_offset(ctx, app, OffsetCopy::Caret),
                    PaletteCommand::CopySelectionRange => copy_formatted_offset(ctx, app, OffsetCopy::SelectionRange),
                    PaletteCommand::CopySelectionLength => copy_formatted_offset(ctx, app, OffsetCopy::SelectionLength),
                    PaletteCommand::CopyFileLength => copy_formatted_offset(ctx, app, OffsetCopy::FileLength),
                }
            }
        }
        crate::command_palette::Action::FocusFile(id) => {
            app.palette.close();
            app.focus_file_tab(id);
        }
        crate::command_palette::Action::RunTemplate(path) => {
            app.palette.close();
            if let Some(id) = active_file_id(app) {
                run_template_from_path(ctx, app, id, path);
            }
        }
        crate::command_palette::Action::SwitchMode(mode) => {
            app.palette.open_at(mode);
        }
        crate::command_palette::Action::InstallTemplate => {
            app.palette.close();
            install_template_from_dialog(app);
        }
        crate::command_palette::Action::UninstallTemplate(path) => {
            app.palette.close();
            uninstall_template(app, &path);
        }
        crate::command_palette::Action::UninstallPlugin(path) => {
            app.palette.close();
            uninstall_plugin(app, &path);
        }
        crate::command_palette::Action::Copy(kind) => {
            app.palette.close();
            if let Some(id) = active_file_id(app)
                && let Some(file) = app.files.get(&id)
            {
                do_copy(ctx, file, kind);
            }
        }
        crate::command_palette::Action::OpenRecent(path) => {
            app.palette.close();
            apply_command_effect(ctx, app, CommandEffect::OpenRecent(path));
        }
        crate::command_palette::Action::GoToOffset(target) => {
            app.palette.close();
            if let Some(id) = active_file_id(app)
                && let Some(file) = app.files.get_mut(&id)
            {
                let max = file.editor.source().len().get().saturating_sub(1);
                let clamped = hxy_core::ByteOffset::new(target.min(max));
                file.editor.set_selection(Some(hxy_core::Selection::caret(clamped)));
                // Only snap the viewport when the target isn't already
                // on screen; a nearby jump within the visible range
                // should leave scroll exactly where it is.
                if !file.editor.is_offset_visible(clamped) {
                    file.editor.set_scroll_to_byte(clamped);
                }
            }
        }
        crate::command_palette::Action::SetSelection { start, end_exclusive } => {
            app.palette.close();
            if let Some(id) = active_file_id(app)
                && let Some(file) = app.files.get_mut(&id)
            {
                let source_len = file.editor.source().len().get();
                if source_len == 0 || end_exclusive <= start {
                    return;
                }
                let last = end_exclusive.saturating_sub(1).min(source_len.saturating_sub(1));
                let anchor = hxy_core::ByteOffset::new(start.min(source_len.saturating_sub(1)));
                file.editor.set_selection(Some(hxy_core::Selection { anchor, cursor: hxy_core::ByteOffset::new(last) }));
                if !file.editor.is_offset_visible(anchor) {
                    file.editor.set_scroll_to_byte(anchor);
                }
            }
        }
        crate::command_palette::Action::SetColumns { scope, count } => {
            use crate::command_palette::ColumnScope;
            app.palette.close();
            match scope {
                ColumnScope::Local => {
                    if let Some(id) = active_file_id(app)
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
        crate::command_palette::Action::InvokePluginCommand { plugin_name, command_id } => {
            // Look up the plugin by its self-reported name. The
            // entry was built from the same source so a missing
            // hit means the plugin list reshuffled (rescan, hot-
            // reload) between palette open and activation -- log
            // and bail rather than guess.
            let Some(plugin) = app.plugin_handlers.iter().find(|p| p.name() == plugin_name).cloned() else {
                tracing::warn!(plugin = %plugin_name, command = %command_id, "plugin invoke target missing");
                app.palette.close();
                return;
            };
            // Close the palette immediately so the user can keep
            // typing; the worker thread runs in the background and
            // dispatches its outcome through `drain_pending_plugin_ops`.
            app.palette.close();
            let repaint = ctx.clone();
            let mut ops = std::mem::take(&mut app.pending_plugin_ops);
            crate::plugin_runner::spawn_invoke(&mut ops, app, repaint, plugin, plugin_name, command_id);
            app.pending_plugin_ops = ops;
        }
        crate::command_palette::Action::RespondToPlugin { plugin_name, command_id, answer } => {
            let Some(plugin) = app.plugin_handlers.iter().find(|p| p.name() == plugin_name).cloned() else {
                tracing::warn!(plugin = %plugin_name, command = %command_id, "plugin respond target missing");
                app.palette.close();
                return;
            };
            app.palette.close();
            let repaint = ctx.clone();
            let mut ops = std::mem::take(&mut app.pending_plugin_ops);
            crate::plugin_runner::spawn_respond(
                &mut ops,
                app,
                repaint,
                plugin,
                plugin_name,
                command_id,
                answer,
            );
            app.pending_plugin_ops = ops;
        }
        crate::command_palette::Action::NoOp => {
            // Placeholder rows (e.g. "Invalid: ..." in the Go-To
            // cascade) pick to this. Close the palette so repeated
            // Enter presses don't get the user stuck on an inert
            // row, but don't dispatch any other effect.
            app.palette.close();
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn install_template_from_dialog(app: &mut HxyApp) {
    let Some(picked) = rfd::FileDialog::new().add_filter("010 Editor binary template", &["bt"]).pick_file() else {
        return;
    };
    let Some(dir) = user_templates_dir() else {
        tracing::warn!("user templates dir could not be resolved");
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, "create templates dir");
        return;
    }
    let report = crate::template_library::install_template_with_deps(&picked, &dir);
    let ctx = format!("Install {}", picked.display());
    for copied in &report.copied {
        app.console_log(ConsoleSeverity::Info, &ctx, format!("installed {}", copied.display()));
    }
    for existing in &report.existing {
        app.console_log(ConsoleSeverity::Info, &ctx, format!("already present: {}", existing.display()));
    }
    for (src, target) in &report.missing {
        app.console_log(
            ConsoleSeverity::Warning,
            &ctx,
            format!("{} references `{target}` but it couldn't be resolved", src.display()),
        );
    }
    for (src, error) in &report.errors {
        app.console_log(ConsoleSeverity::Error, &ctx, format!("copy {} failed: {error}", src.display()));
    }
    app.reload_plugins();
}

#[cfg(not(target_arch = "wasm32"))]
fn uninstall_template(app: &mut HxyApp, path: &std::path::Path) {
    let ctx = format!("Uninstall {}", path.display());
    match std::fs::remove_file(path) {
        Ok(_) => {
            app.console_log(ConsoleSeverity::Info, &ctx, "deleted");
            app.reload_plugins();
        }
        Err(e) => {
            app.console_log(ConsoleSeverity::Error, &ctx, format!("delete failed: {e}"));
        }
    }
}

/// Full plugin uninstall: removes the `.wasm` + sidecar manifest,
/// drops the user's stored grant for the plugin's `PluginKey`, and
/// clears any persisted state blob the plugin owned. Each step
/// logs to the console; failures don't short-circuit the others
/// (a stale grant or leftover state shouldn't block the disk
/// cleanup that the user actually asked for).
#[cfg(not(target_arch = "wasm32"))]
fn uninstall_plugin(app: &mut HxyApp, wasm_path: &std::path::Path) {
    let ctx = format!("Uninstall {}", wasm_path.display());

    // Read the sidecar before deleting so we can scope the grant +
    // state cleanup to the plugin's actual identity. Falling back
    // to the file stem mirrors how the loader handles manifest-less
    // plugins (`PluginManifest::load_for` -> `Ok(None)`), so we
    // still clean up state keyed by the legacy name.
    let sidecar = hxy_plugin_host::PluginManifest::sidecar_path(wasm_path);
    let manifest = hxy_plugin_host::PluginManifest::load_for(wasm_path).ok().flatten();
    let plugin_name = manifest
        .as_ref()
        .map(|m| m.plugin.name.clone())
        .unwrap_or_else(|| {
            wasm_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });

    // Hash the on-disk bytes so we can drop the matching grant
    // entry. Failure to read just means the grant cleanup is
    // skipped -- the file deletion below still proceeds.
    let key = match std::fs::read(wasm_path) {
        Ok(bytes) => {
            let version = manifest
                .as_ref()
                .map(|m| m.plugin.version.clone())
                .unwrap_or_else(|| "0.0.0".to_string());
            Some(hxy_plugin_host::PluginKey::from_bytes(plugin_name.clone(), version, &bytes))
        }
        Err(e) => {
            app.console_log(
                ConsoleSeverity::Warning,
                &ctx,
                format!("read for grant cleanup: {e}"),
            );
            None
        }
    };

    match std::fs::remove_file(wasm_path) {
        Ok(_) => app.console_log(ConsoleSeverity::Info, &ctx, "removed component"),
        Err(e) => {
            app.console_log(ConsoleSeverity::Error, &ctx, format!("remove component: {e}"));
            return;
        }
    }
    if sidecar.exists() {
        match std::fs::remove_file(&sidecar) {
            Ok(_) => app.console_log(ConsoleSeverity::Info, &ctx, "removed manifest"),
            Err(e) => app.console_log(
                ConsoleSeverity::Warning,
                &ctx,
                format!("remove manifest {}: {e}", sidecar.display()),
            ),
        }
    }

    if let Some(key) = key {
        let mut grants_changed = false;
        {
            let mut state = app.state.write();
            if state.plugin_grants.forget(&key) {
                grants_changed = true;
            }
        }
        if grants_changed && let Some(sink) = app.sink.as_ref() {
            let snapshot = app.state.read().clone();
            if let Err(e) = sink.save(&snapshot) {
                app.console_log(
                    ConsoleSeverity::Warning,
                    &ctx,
                    format!("persist grants after uninstall: {e}"),
                );
            }
        }
    }

    if !plugin_name.is_empty()
        && let Some(store) = app.plugin_state_store.as_ref()
    {
        match store.clear(&plugin_name) {
            Ok(_) => app.console_log(ConsoleSeverity::Info, &ctx, "cleared persisted state"),
            Err(e) => app.console_log(
                ConsoleSeverity::Warning,
                &ctx,
                format!("clear persisted state: {e}"),
            ),
        }
    }

    app.reload_plugins();
}

/// Pick the FileId that should drive commands gated on the active
/// file when the user is focused on a `Tab::Workspace`. Prefers the
/// inner-dock active tab (Editor or Entry); falls back to the
/// workspace's editor when the focused inner tab is the VfsTree (no
/// file backs the tree itself).
fn inner_active_file(workspace: &crate::file::Workspace) -> FileId {
    if let Some((_rect, tab)) = workspace.dock.iter_all_tabs().find_map(|(path, tab)| {
        let leaf = workspace.dock.leaf(path.node_path()).ok()?;
        (leaf.active.0 == path.tab.0).then_some(((), tab))
    }) {
        match tab {
            crate::file::WorkspaceTab::Entry(file_id) => return *file_id,
            crate::file::WorkspaceTab::Editor => return workspace.editor_id,
            crate::file::WorkspaceTab::VfsTree => {}
        }
    }
    workspace.editor_id
}

/// Best guess at which file tab the user is "in" right now. Tries in
/// order: the egui_dock-focused tab (exact), the most recently
/// focused file (so clicking into the Inspector / Console doesn't
/// blank out a menu command), and finally -- when only one file is
/// open -- that sole file. Returning `None` means there's genuinely
/// no file to act on.
fn active_file_id(app: &mut HxyApp) -> Option<FileId> {
    if let Some((_, tab)) = app.dock.find_active_focused() {
        match *tab {
            Tab::File(id) => {
                app.last_active_file = Some(id);
                return Some(id);
            }
            Tab::Workspace(workspace_id) => {
                // The active "file" for a workspace is whatever sub-
                // tab is currently active in its inner dock: the
                // editor, an opened entry, or (when the user has
                // focused the tree) the editor as a fallback.
                if let Some(workspace) = app.workspaces.get(&workspace_id) {
                    let id = inner_active_file(workspace);
                    app.last_active_file = Some(id);
                    app.last_active_workspace = Some(workspace_id);
                    return Some(id);
                }
            }
            _ => {}
        }
    }
    if let Some(id) = app.last_active_file
        && app.files.contains_key(&id)
    {
        return Some(id);
    }
    // Final fallback: scan the dock for the first `Tab::File` in
    // iteration order. Covers the "files are open but nothing
    // file-shaped is currently focused" case (e.g. focus is on the
    // Inspector/Console, palette is opened from a fresh session
    // before `last_active_file` was ever populated). Without this,
    // every command gated on `has_active_file` -- Set columns,
    // Go-to offset, Select range -- silently disappears even
    // though a file is plainly visible.
    let fallback = app.dock.iter_all_tabs().find_map(|(_, t)| match t {
        Tab::File(id) => Some(*id),
        _ => None,
    });
    if let Some(id) = fallback {
        app.last_active_file = Some(id);
        return Some(id);
    }
    None
}

fn handle_open_file(app: &mut HxyApp) {
    #[cfg(not(target_arch = "wasm32"))]
    match pick_and_read_file() {
        Ok((name, path, bytes)) => {
            app.request_open_filesystem(name, path, bytes);
        }
        Err(crate::file::FileOpenError::Cancelled) => {}
        Err(e) => {
            tracing::warn!(error = %e, "open file");
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
    }
}

/// Create a fresh anonymous ("Untitled") tab with a small zero-filled
/// buffer. Picks the next free `AnonymousId` and a "Untitled N" title
/// that doesn't collide with any already-open or persisted tab.
#[cfg(not(target_arch = "wasm32"))]
/// Drive the side effect for whatever outcome a plugin returned
/// from `invoke` or `respond_to_prompt`. Centralized so both
/// initial command activation and prompt answers fan out through
/// the same switch (`Done` -> close, `Cascade` -> sub-palette,
/// `Mount` -> tab, `Prompt` -> argument-style sub-palette that
/// rounds back here on submit).
#[cfg(not(target_arch = "wasm32"))]
fn dispatch_plugin_outcome(
    ctx: &egui::Context,
    app: &mut HxyApp,
    plugin: Arc<hxy_plugin_host::PluginHandler>,
    plugin_name: &str,
    command_id: &str,
    outcome: Option<hxy_plugin_host::InvokeOutcome>,
) {
    match outcome {
        Some(hxy_plugin_host::InvokeOutcome::Done) => app.palette.close(),
        Some(hxy_plugin_host::InvokeOutcome::Cascade(commands)) => {
            app.palette.enter_plugin_cascade(plugin_name.to_owned(), commands);
        }
        Some(hxy_plugin_host::InvokeOutcome::Mount(req)) => {
            app.palette.close();
            // mount_by_token does the slow TCP-connect + banner read;
            // background-thread it so the UI stays responsive. The
            // tab opens once the worker finishes, via
            // `install_mount_tab` from `drain_pending_plugin_ops`.
            let plugin_name_owned = plugin.name().to_owned();
            let mut ops = std::mem::take(&mut app.pending_plugin_ops);
            crate::plugin_runner::spawn_mount_by_token(
                &mut ops,
                app,
                ctx.clone(),
                plugin,
                plugin_name_owned,
                req.token,
                req.title,
            );
            app.pending_plugin_ops = ops;
        }
        Some(hxy_plugin_host::InvokeOutcome::Prompt(req)) => {
            // Switch to argument-style prompt mode. The user's
            // typed answer rides back through `RespondToPlugin`
            // with the same command id, so chaining prompts is
            // just "the plugin returns another Prompt outcome".
            app.palette.enter_plugin_prompt(
                plugin_name.to_owned(),
                command_id.to_owned(),
                req.title,
                req.default_value,
            );
        }
        None => {
            // Plugin couldn't be invoked (commands grant off, or
            // an internal trap). PluginHandler already logged;
            // just close.
            app.palette.close();
        }
    }
}

/// Install an already-resolved `MountedVfs` as a new `Tab::PluginMount`.
/// The mount itself lives in `app.mounts`; the dock tab carries only the
/// `MountId`. The worker thread that ran `mount-by-token` ends here, and
/// session restoration funnels through here too. The mount entry is
/// recorded into `state.open_tabs` so restart re-invokes `mount_by_token`
/// with the saved token.
#[cfg(not(target_arch = "wasm32"))]
fn install_mount_tab(
    app: &mut HxyApp,
    plugin: Arc<hxy_plugin_host::PluginHandler>,
    mount: hxy_vfs::MountedVfs,
    token: String,
    title: String,
) {
    let mount_id = crate::file::MountId::new(app.next_mount_id);
    app.next_mount_id += 1;
    let plugin_name = plugin.name().to_owned();
    let entry = crate::file::MountedPlugin {
        display_name: title.clone(),
        plugin_name: plugin_name.clone(),
        token: token.clone(),
        mount: Arc::new(mount),
    };
    app.mounts.insert(mount_id, entry);

    let source = TabSource::PluginMount {
        plugin_name: plugin_name.clone(),
        token,
        title: title.clone(),
    };
    {
        let mut g = app.state.write();
        if !g.open_tabs.iter().any(|t| t.source == source) {
            g.open_tabs.push(crate::state::OpenTabState {
                source,
                selection: None,
                scroll_offset: 0.0,
                as_workspace: false,
            });
        }
    }

    let tool_leaf = push_tool_tab(&mut app.dock, Tab::PluginMount(mount_id));
    if let Some(path) = app.dock.find_tab(&Tab::PluginMount(mount_id)) {
        remove_welcome_from_leaf(&mut app.dock, path.surface, path.node);
        if let Some(fresh_path) = app.dock.find_tab(&Tab::PluginMount(mount_id)) {
            let _ = app.dock.set_active_tab(fresh_path);
        }
    }
    app.dock.set_focused_node_and_surface(tool_leaf);
    tracing::info!(plugin = %plugin_name, title = %title, id = %mount_id.get(), "mount tab installed");
}

fn handle_new_file(app: &mut HxyApp) {
    let mut used_ids: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for file in app.files.values() {
        if let Some(TabSource::Anonymous { id, .. }) = &file.source_kind {
            used_ids.insert(id.get());
        }
    }
    for tab in &app.state.read().open_tabs {
        if let TabSource::Anonymous { id, .. } = &tab.source {
            used_ids.insert(id.get());
        }
    }
    let next_id = (0u64..).find(|i| !used_ids.contains(i)).expect("u64 id space");

    let mut used_titles: std::collections::HashSet<String> = std::collections::HashSet::new();
    for file in app.files.values() {
        used_titles.insert(file.display_name.clone());
    }
    let title = (1u64..)
        .map(|n| if n == 1 { "Untitled".to_owned() } else { format!("Untitled {n}") })
        .find(|t| !used_titles.contains(t))
        .expect("nonzero range of titles");

    let id = hxy_vfs::AnonymousId(next_id);
    // Zero-length buffer: writes past EOF grow the file via
    // HexEditor::insert_at, so the user can just start typing.
    let bytes: Vec<u8> = Vec::new();
    // Touch the persistent sidecar so a crash before the next
    // save_if_dirty cycle still restores this tab (empty, but
    // present). Best-effort.
    if let Some(path) = anonymous_file_path(id) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, &bytes) {
            tracing::warn!(error = %e, path = %path.display(), "write anonymous tab seed");
        }
    }
    let source = TabSource::Anonymous { id, title: title.clone() };
    let initial_caret = Some(hxy_core::Selection::caret(hxy_core::ByteOffset::new(0)));
    let file_id = app.open(title, Some(source), bytes, initial_caret, None, false);
    app.focus_file_tab(file_id);
}

/// Save the active tab. `force_dialog` always asks for a destination
/// (Save As); otherwise the tab's existing filesystem path is used
/// when present, falling back to the dialog when there isn't one.
#[cfg(not(target_arch = "wasm32"))]
fn save_active_file(app: &mut HxyApp, force_dialog: bool) {
    let Some(id) = active_file_id(app) else { return };
    let _ = save_file_by_id(app, id, force_dialog);
}

/// Save a specific file tab by id. Returns `true` when the bytes
/// actually hit disk; `false` when the user dismissed the dialog or
/// the write itself failed (the latter is also surfaced via the
/// console log). Used by [`save_active_file`] for the Save / Save
/// As shortcut path and by the close-tab-with-unsaved-changes
/// modal, which conditions the tab close on the save succeeding.
#[cfg(not(target_arch = "wasm32"))]
/// In-place writeback for a VFS-backed tab. Walks the editor's
/// patch ops and pushes each in-place write through the mount's
/// `VfsWriter`. Pure inserts / deletes are rejected because the
/// only writeback target today is xbox-neighborhood's `/memory/`
/// + `/modules/` namespaces, neither of which can grow or shrink.
///
/// On success the patch is cleared (the mount is now the source
/// of truth) and the file's byte source is swapped to the post-
/// write contents so the editor doesn't keep showing the patch
/// overlay against stale base bytes.
#[cfg(not(target_arch = "wasm32"))]
fn save_vfs_entry_in_place(app: &mut HxyApp, id: FileId) -> bool {
    let Some(file) = app.files.get(&id) else { return false };
    let TabSource::VfsEntry { parent, entry_path } = file.source_kind.as_ref().expect("checked") else {
        return false;
    };
    let entry_path = entry_path.clone();
    let parent_source = (**parent).clone();
    let display = file.display_name.clone();
    let ctx = format!("Save {display}");

    // Resolve the parent's mount: file-rooted parents live in
    // `app.workspaces`, plugin parents in `app.mounts`. Both paths
    // are unified by `find_mount_for_source`.
    let mount = match app.find_mount_for_source(&parent_source) {
        Some(m) => m,
        None => {
            app.console_log(
                ConsoleSeverity::Error,
                &ctx,
                "parent VFS tab is gone -- close + reopen this tab",
            );
            return false;
        }
    };
    let writer = match mount.writer.clone() {
        Some(w) => w,
        None => {
            app.console_log(
                ConsoleSeverity::Error,
                &ctx,
                "this VFS handler doesn't support writeback",
            );
            return false;
        }
    };

    // Snapshot the patch ops + total byte length so we can rebuild
    // the post-write source after dispatching.
    let (patch_ops, total_len, post_write_bytes): (
        Vec<(u64, Vec<u8>)>,
        u64,
        Result<Vec<u8>, String>,
    ) = {
        let file = app.files.get(&id).expect("just looked up");
        let editor = &file.editor;
        let total_len = editor.source().len().get();
        let patch = editor.patch().read().unwrap();
        let mut ops: Vec<(u64, Vec<u8>)> = Vec::with_capacity(patch.ops().len());
        let mut bad_op: Option<(u64, u64, usize)> = None;
        for op in patch.ops() {
            if op.old_len != op.new_bytes.len() as u64 {
                bad_op = Some((op.offset, op.old_len, op.new_bytes.len()));
                break;
            }
            ops.push((op.offset, op.new_bytes.clone()));
        }
        drop(patch);
        if let Some((offset, old_len, new_len)) = bad_op {
            app.console_log(
                ConsoleSeverity::Error,
                &ctx,
                format!(
                    "can't poke insert/delete (offset={offset}, old_len={old_len}, new_len={new_len}); only in-place writes"
                ),
            );
            return false;
        }
        // Read the patched view (base + patch) so we can swap the
        // editor's source afterwards.
        let bytes = editor.source().read(
            hxy_core::ByteRange::new(
                hxy_core::ByteOffset::new(0),
                hxy_core::ByteOffset::new(total_len),
            )
            .expect("valid range"),
        );
        let bytes_result = bytes.map_err(|e| e.to_string());
        (ops, total_len, bytes_result)
    };

    if patch_ops.is_empty() {
        app.console_log(ConsoleSeverity::Info, &ctx, "no changes to save");
        return true;
    }

    let mut total_written: u64 = 0;
    let mut total_requested: u64 = 0;
    for (offset, bytes) in &patch_ops {
        let n = bytes.len() as u64;
        total_requested += n;
        match writer.write_range(&entry_path, *offset, bytes) {
            Ok(written) => {
                total_written += written;
                if written < n {
                    app.console_log(
                        ConsoleSeverity::Warning,
                        &ctx,
                        format!(
                            "partial write at offset {offset}: requested {n}, wrote {written}"
                        ),
                    );
                }
            }
            Err(e) => {
                app.console_log(
                    ConsoleSeverity::Error,
                    &ctx,
                    format!("write @ offset {offset}: {e}"),
                );
                return false;
            }
        }
    }

    // Swap the editor's source to the post-write bytes so the
    // patch overlay can be cleared without losing the user's
    // current view. If reading the patched view failed we leave
    // the patch in place (the user's changes are already on the
    // device but the local view would otherwise lie).
    match post_write_bytes {
        Ok(bytes) => {
            if let Some(file) = app.files.get_mut(&id) {
                let base: std::sync::Arc<dyn hxy_core::HexSource> =
                    std::sync::Arc::new(hxy_core::MemorySource::new(bytes));
                file.editor.swap_source(base);
            }
        }
        Err(e) => {
            app.console_log(
                ConsoleSeverity::Warning,
                &ctx,
                format!("post-write source rebuild: {e} (patch overlay still present)"),
            );
        }
    }

    let _ = total_len;
    app.console_log(
        ConsoleSeverity::Info,
        &ctx,
        format!("wrote {total_written}/{total_requested} bytes via plugin writer"),
    );
    true
}

fn save_file_by_id(app: &mut HxyApp, id: FileId, force_dialog: bool) -> bool {
    let Some(file) = app.files.get(&id) else { return false };
    // VFS-entry tabs (e.g. xbox-neighborhood `/memory/<addr>`)
    // have no filesystem path -- the save flow walks each patch
    // op back through the parent mount's `VfsWriter` instead.
    // `force_dialog` (Save As) still falls through to the
    // filesystem path so the user can spill VFS bytes to disk.
    if !force_dialog
        && let Some(TabSource::VfsEntry { .. }) = file.source_kind.as_ref()
    {
        return save_vfs_entry_in_place(app, id);
    }
    let display = file.display_name.clone();
    let target = if force_dialog {
        let mut dialog = rfd::FileDialog::new().set_file_name(&display);
        if let Some(parent) = file.root_path().and_then(|p| p.parent()) {
            dialog = dialog.set_directory(parent);
        }
        dialog.save_file()
    } else {
        match file.root_path().cloned() {
            Some(p) => Some(p),
            None => rfd::FileDialog::new().set_file_name(&display).save_file(),
        }
    };
    let Some(path) = target else { return false };

    let ctx = format!("Save {}", path.display());
    let len = file.editor.source().len().get();
    let bytes = match file.editor.source().read(
        hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len)).expect("valid range"),
    ) {
        Ok(b) => b,
        Err(e) => {
            app.console_log(ConsoleSeverity::Error, &ctx, format!("read patched bytes: {e}"));
            return false;
        }
    };
    if let Err(e) = write_atomic(&path, &bytes) {
        app.console_log(ConsoleSeverity::Error, &ctx, format!("write: {e}"));
        return false;
    }

    // Successful write -- swap the tab's byte source over to the
    // just-saved bytes so the view reflects on-disk state instead
    // of the stale pre-edit buffer. Reverting the patch alone would
    // leave `file.editor.source()` wrapping the original pre-edit bytes and
    // reads would show the wrong content.
    // Stash the previous source_kind before replacing so we can
    // clean up anonymous sidecars and persisted entries after the
    // tab re-anchors to Filesystem.
    let previous_source = app.files.get(&id).and_then(|f| f.source_kind.clone());
    if let Some(file) = app.files.get_mut(&id) {
        let base: std::sync::Arc<dyn hxy_core::HexSource> = std::sync::Arc::new(hxy_core::MemorySource::new(bytes));
        file.editor.swap_source(base);
        file.source_kind = Some(hxy_vfs::TabSource::Filesystem(path.clone()));
        if let Some(name) = path.file_name() {
            file.display_name = name.to_string_lossy().into_owned();
        }
    }
    // Drop any sidecar patch from a previous session; the file on
    // disk is now the source of truth and re-prompting on next
    // launch would be confusing.
    if let Some(dir) = unsaved_edits_dir() {
        let _ = crate::patch_persist::discard(&dir, &path);
    }
    // If we just saved an anonymous tab, remove its backing file
    // and swap the persisted entry from Anonymous to Filesystem.
    if let Some(TabSource::Anonymous { id: anon_id, .. }) = previous_source.as_ref() {
        if let Some(anon_path) = anonymous_file_path(*anon_id) {
            let _ = std::fs::remove_file(&anon_path);
        }
        let new_source = TabSource::Filesystem(path.clone());
        let mut state = app.state.write();
        state.open_tabs.retain(|t| !matches!(&t.source, TabSource::Anonymous { id, .. } if id == anon_id));
        if !state.open_tabs.iter().any(|t| t.source == new_source) {
            state.open_tabs.push(crate::state::OpenTabState {
                source: new_source,
                selection: None,
                scroll_offset: 0.0,
                as_workspace: false,
            });
        }
    }
    app.console_log(ConsoleSeverity::Info, &ctx, "saved");
    true
}

/// Write `bytes` to `path` atomically: stage in a sibling tempfile,
/// fsync, then rename. Avoids leaving a half-written file if the
/// process crashes mid-write.
#[cfg(not(target_arch = "wasm32"))]
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.as_file_mut().write_all(bytes)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn pick_and_read_file() -> Result<(String, std::path::PathBuf, Vec<u8>), crate::file::FileOpenError> {
    let Some(path) = rfd::FileDialog::new().pick_file() else {
        return Err(crate::file::FileOpenError::Cancelled);
    };
    let bytes =
        std::fs::read(&path).map_err(|source| crate::file::FileOpenError::Read { path: path.clone(), source })?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    Ok((name, path, bytes))
}

struct HxyTabViewer<'a> {
    files: &'a mut HashMap<FileId, OpenFile>,
    state: &'a mut PersistedState,
    console: &'a std::collections::VecDeque<ConsoleEntry>,
    /// Active plugin VFS mounts. Read-only here -- closing a mount tab
    /// only flags it via `pending_close_mount` and the app drops it
    /// from the map after the dock pass.
    #[cfg(not(target_arch = "wasm32"))]
    mounts: &'a std::collections::BTreeMap<crate::file::MountId, crate::file::MountedPlugin>,
    /// Slot for the dock's `on_close` handler when the user X-clicks a
    /// `Tab::PluginMount`. The app drains the mount entry from
    /// `app.mounts` after the dock pass.
    #[cfg(not(target_arch = "wasm32"))]
    pending_close_mount: &'a mut Option<crate::file::MountId>,
    /// Cross-file search state, rendered by `Tab::SearchResults`.
    #[cfg(not(target_arch = "wasm32"))]
    global_search: &'a mut crate::global_search::GlobalSearchState,
    /// Events emitted by the global search tab during render. Drained
    /// after the dock pass so we can mutate `files` to focus / jump.
    #[cfg(not(target_arch = "wasm32"))]
    pending_global_search_events: &'a mut Vec<crate::global_search::GlobalSearchEvent>,
    #[cfg(not(target_arch = "wasm32"))]
    inspector: &'a mut crate::inspector::InspectorState,
    #[cfg(not(target_arch = "wasm32"))]
    decoders: &'a [Arc<dyn crate::inspector::Decoder>],
    /// (caret offset, up to 16 bytes at caret) for the active file,
    /// snapshotted before dock render so the Inspector tab can read
    /// it without reborrowing `files`.
    #[cfg(not(target_arch = "wasm32"))]
    inspector_data: Option<(u64, Vec<u8>)>,
    /// Set to true when the Plugins tab mutated the plugin directories
    /// and needs the registry / template runtimes rebuilt. Drained at
    /// end of frame by [`HxyApp::ui`].
    #[cfg(not(target_arch = "wasm32"))]
    plugin_rescan: &'a mut bool,
    /// Read-only view of loaded plugin handlers so the Plugins tab
    /// can render their consent cards.
    #[cfg(not(target_arch = "wasm32"))]
    plugin_handlers: &'a [Arc<hxy_plugin_host::PluginHandler>],
    /// Sink for grant changes / state-wipe requests captured by the
    /// Plugins tab. Drained at end of frame by [`HxyApp::ui`].
    #[cfg(not(target_arch = "wasm32"))]
    pending_plugin_events: &'a mut Vec<crate::plugins_tab::PluginsEvent>,
    /// Slot the dock's `on_close` handler writes to when the user
    /// X-clicks a dirty File tab. The app drains this after the
    /// dock pass and renders the save-prompt modal next frame.
    pending_close_tab: &'a mut Option<PendingCloseTab>,
    /// Mutated whenever the user clicks an outer tab button so
    /// `Ctrl+Tab` knows to cycle the outer dock next, or hands off
    /// to a workspace inner dock when the user clicks into one.
    tab_focus: &'a mut TabFocus,
    /// File-mounted VFS workspaces. The viewer renders each
    /// `Tab::Workspace` by spinning up an inner `DockArea` against
    /// `workspace.dock`.
    workspaces: &'a mut std::collections::BTreeMap<crate::file::WorkspaceId, crate::file::Workspace>,
    /// Slot the inner workspace dock writes to when the user closes a
    /// `WorkspaceTab::Entry` whose file is dirty. Same shape as
    /// `pending_close_tab` (the modal handler treats them identically).
    pending_close_workspace_entry: &'a mut Option<PendingCloseTab>,
    /// `WorkspaceId`s the viewer drained to "no tabs left except the
    /// editor." The post-dock pass collapses these back to plain
    /// `Tab::File` entries in the outer dock.
    pending_collapse_workspace: &'a mut Vec<crate::file::WorkspaceId>,
}

/// Look up the caret offset and the bytes immediately after it for
/// the file the inspector should display. Uses the currently focused
/// file tab when one exists; otherwise falls back to the most
/// recently focused file (so clicking into the Inspector tab itself
/// doesn't make its content disappear).
#[cfg(not(target_arch = "wasm32"))]
fn snapshot_inspector_bytes(app: &mut HxyApp) -> Option<(u64, Vec<u8>)> {
    let id = active_file_id(app)?;
    let file = app.files.get(&id)?;
    let caret = file.editor.selection()?.cursor.get();
    let src_len = file.editor.source().len().get();
    if caret >= src_len {
        return Some((caret, Vec::new()));
    }
    let end = caret.saturating_add(16).min(src_len);
    let range = hxy_core::ByteRange::new(hxy_core::ByteOffset::new(caret), hxy_core::ByteOffset::new(end)).ok()?;
    let bytes = file.editor.source().read(range).ok()?;
    Some((caret, bytes))
}

impl TabViewer for HxyTabViewer<'_> {
    type Tab = Tab;

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        match tab {
            Tab::Welcome => hxy_i18n::t("tab-welcome").into(),
            Tab::Settings => hxy_i18n::t("tab-settings").into(),
            Tab::Console => hxy_i18n::t("tab-console").into(),
            Tab::Inspector => hxy_i18n::t("tab-inspector").into(),
            Tab::Plugins => "Plugins".into(),
            Tab::File(id) => match self.files.get(id) {
                Some(f) => {
                    // Both indicators sit to the left of the name:
                    // lock glyph first when the tab is read-only,
                    // then a bullet when there are unsaved edits,
                    // then the filename.
                    let mut prefix = String::new();
                    if matches!(f.editor.edit_mode(), crate::file::EditMode::Readonly) {
                        prefix.push_str(egui_phosphor::regular::LOCK);
                        prefix.push(' ');
                    }
                    if f.editor.is_dirty() {
                        prefix.push_str("\u{2022} ");
                    }
                    format!("{prefix}{}", f.display_name).into()
                }
                None => format!("file-{}", id.get()).into(),
            },
            #[cfg(not(target_arch = "wasm32"))]
            Tab::PluginMount(mount_id) => match self.mounts.get(mount_id) {
                Some(m) => format!("{} {}", egui_phosphor::regular::TREE_STRUCTURE, m.display_name).into(),
                None => format!("mount-{}", mount_id.get()).into(),
            },
            #[cfg(not(target_arch = "wasm32"))]
            Tab::SearchResults => {
                format!("{} Search", egui_phosphor::regular::MAGNIFYING_GLASS).into()
            }
            Tab::Workspace(workspace_id) => match self.workspaces.get(workspace_id) {
                Some(w) => match self.files.get(&w.editor_id) {
                    Some(f) => {
                        // Same dirty / readonly indicators as Tab::File,
                        // plus a tree-structure icon so the user can tell
                        // at a glance that this tab nests sub-tabs.
                        let mut prefix = String::from(egui_phosphor::regular::TREE_STRUCTURE);
                        prefix.push(' ');
                        if matches!(f.editor.edit_mode(), crate::file::EditMode::Readonly) {
                            prefix.push_str(egui_phosphor::regular::LOCK);
                            prefix.push(' ');
                        }
                        if f.editor.is_dirty() {
                            prefix.push_str("\u{2022} ");
                        }
                        format!("{prefix}{}", f.display_name).into()
                    }
                    None => format!("workspace-{}", workspace_id.get()).into(),
                },
                None => format!("workspace-{}", workspace_id.get()).into(),
            },
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            Tab::Welcome => welcome_ui(ui, self.state),
            Tab::Settings => settings_ui(ui, &mut self.state.app),
            Tab::Console => console_ui(ui, self.console),
            Tab::Inspector => {
                let (caret, bytes) = match &self.inspector_data {
                    Some((c, b)) => (Some(*c), b.as_slice()),
                    None => (None, &[] as &[u8]),
                };
                crate::inspector::show(ui, self.inspector, self.decoders, caret, bytes);
            }
            Tab::Plugins => {
                let handlers_dir = user_plugins_dir();
                let templates_dir = user_template_plugins_dir();
                let events = crate::plugins_tab::show(
                    ui,
                    handlers_dir.as_ref(),
                    templates_dir.as_ref(),
                    self.plugin_handlers,
                );
                for e in events {
                    match e {
                        crate::plugins_tab::PluginsEvent::Rescan => *self.plugin_rescan = true,
                        // Grant + wipe events apply to mutable state
                        // the viewer doesn't own; queue them for the
                        // app's post-dock drain.
                        other => self.pending_plugin_events.push(other),
                    }
                }
            }
            Tab::File(id) => match self.files.get_mut(id) {
                Some(file) => {
                    render_file_tab(ui, *id, file, self.state, *self.tab_focus);
                }
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing file {id:?}"));
                }
            },
            #[cfg(not(target_arch = "wasm32"))]
            Tab::PluginMount(mount_id) => match self.mounts.get(mount_id) {
                Some(m) => render_plugin_mount_tab(ui, *mount_id, &m.mount),
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing mount {mount_id:?}"));
                }
            },
            #[cfg(not(target_arch = "wasm32"))]
            Tab::SearchResults => {
                let names: std::collections::HashMap<FileId, String> =
                    self.files.iter().map(|(id, f)| (*id, f.display_name.clone())).collect();
                let events = crate::global_search::show(ui, self.global_search, &names);
                self.pending_global_search_events.extend(events);
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
                );
            }
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
        match tab {
            Tab::File(_) | Tab::Console | Tab::Inspector | Tab::Plugins | Tab::Workspace(_) => true,
            #[cfg(not(target_arch = "wasm32"))]
            Tab::PluginMount(_) | Tab::SearchResults => true,
            _ => false,
        }
    }

    fn scroll_bars(&self, tab: &Self::Tab) -> [bool; 2] {
        // File tabs and the console/inspector manage their own
        // scrolling; outer dock scrollbar off for those. Plugin mount
        // tabs render the VFS tree's own scroll area. Workspace tabs
        // host an inner DockArea that takes the full body.
        match tab {
            Tab::File(_) | Tab::Console | Tab::Inspector | Tab::Workspace(_) => [false, false],
            #[cfg(not(target_arch = "wasm32"))]
            Tab::PluginMount(_) | Tab::SearchResults => [false, false],
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
                    Some(PendingCloseTab { file_id: *id, display_name: file.display_name.clone() });
                return OnCloseResponse::Ignore;
            }
            if let Some(removed) = self.files.remove(id)
                && let Some(source) = removed.source_kind
            {
                self.state.open_tabs.retain(|t| t.source != source);
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Tab::PluginMount(mount_id) = tab {
            // Defer the actual removal -- the mounts map is borrowed
            // immutably here. The post-dock drain in `HxyApp::ui`
            // matches on this slot and drops the mount entry plus the
            // matching `state.open_tabs` record.
            *self.pending_close_mount = Some(*mount_id);
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
                    if let crate::file::WorkspaceTab::Entry(file_id) = t
                        && let Some(f) = self.files.get(file_id)
                        && f.editor.is_dirty()
                    {
                        dirty = Some((*file_id, f.display_name.clone()));
                        break;
                    }
                }
            }
            if let Some((file_id, display_name)) = dirty {
                *self.pending_close_tab = Some(PendingCloseTab { file_id, display_name });
                return OnCloseResponse::Ignore;
            }
            // Drain workspace contents from `app.files` + persistence;
            // the modal handler does the same on confirmed close.
            let workspace = self.workspaces.remove(workspace_id).expect("just looked up");
            let mut to_drop: Vec<FileId> = vec![workspace.editor_id];
            for (_, t) in workspace.dock.iter_all_tabs() {
                if let crate::file::WorkspaceTab::Entry(file_id) = t {
                    to_drop.push(*file_id);
                }
            }
            for file_id in &to_drop {
                if let Some(removed) = self.files.remove(file_id)
                    && let Some(source) = removed.source_kind
                {
                    self.state.open_tabs.retain(|t| t.source != source);
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
    workspace_id: crate::file::WorkspaceId,
    workspaces: &mut std::collections::BTreeMap<crate::file::WorkspaceId, crate::file::Workspace>,
    files: &mut HashMap<FileId, OpenFile>,
    state: &mut PersistedState,
    pending_close_workspace_entry: &mut Option<PendingCloseTab>,
    pending_collapse_workspace: &mut Vec<crate::file::WorkspaceId>,
    tab_focus: &mut TabFocus,
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
    };
    let style = egui_dock::Style::from_egui(ui.style());
    egui_dock::DockArea::new(inner_dock)
        .id(egui::Id::new(("hxy-workspace-dock", workspace_id.get())))
        .style(style)
        .show_leaf_collapse_buttons(false)
        .show_inside(ui, &mut viewer);

    // Collapse-back trigger: if the workspace is left with only its
    // Editor sub-tab (user closed the tree + every entry), schedule a
    // post-dock collapse to a plain `Tab::File`.
    let only_editor = workspace.dock.iter_all_tabs().count() == 1
        && workspace
            .dock
            .iter_all_tabs()
            .all(|(_, t)| matches!(t, crate::file::WorkspaceTab::Editor));
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
    files: &'a mut HashMap<FileId, OpenFile>,
    state: &'a mut PersistedState,
    editor_id: FileId,
    workspace_id: crate::file::WorkspaceId,
    mount: &'a Arc<MountedVfs>,
    pending_close_workspace_entry: &'a mut Option<PendingCloseTab>,
    /// Updated by `on_tab_button` when the user clicks an inner tab,
    /// so subsequent `Ctrl+Tab` cycles cycle this workspace's dock.
    tab_focus: &'a mut TabFocus,
}

impl egui_dock::TabViewer for WorkspaceTabViewer<'_> {
    type Tab = crate::file::WorkspaceTab;

    fn id(&mut self, tab: &mut Self::Tab) -> egui::Id {
        // Distinct ids per workspace so two open workspaces don't
        // share `WorkspaceTab::Editor` when egui_dock interns the tab.
        match tab {
            crate::file::WorkspaceTab::Editor => egui::Id::new(("ws-editor", self.workspace_id.get())),
            crate::file::WorkspaceTab::VfsTree => egui::Id::new(("ws-tree", self.workspace_id.get())),
            crate::file::WorkspaceTab::Entry(file_id) => {
                egui::Id::new(("ws-entry", self.workspace_id.get(), file_id.get()))
            }
        }
    }

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        match tab {
            crate::file::WorkspaceTab::Editor => match self.files.get(&self.editor_id) {
                Some(f) => {
                    // House icon marks the workspace's parent file --
                    // visually distinct from Entry sub-tabs (which are
                    // unprefixed), so the user can spot the root tab
                    // even when several entries are open beside it.
                    let mut prefix = String::from(egui_phosphor::regular::HOUSE);
                    prefix.push(' ');
                    if matches!(f.editor.edit_mode(), crate::file::EditMode::Readonly) {
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
            crate::file::WorkspaceTab::VfsTree => {
                format!("{} VFS", egui_phosphor::regular::TREE_STRUCTURE).into()
            }
            crate::file::WorkspaceTab::Entry(file_id) => match self.files.get(file_id) {
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
            crate::file::WorkspaceTab::Editor => match self.files.get_mut(&self.editor_id) {
                Some(file) => render_file_tab(ui, self.editor_id, file, self.state, *self.tab_focus),
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing editor {:?}", self.editor_id));
                }
            },
            crate::file::WorkspaceTab::VfsTree => {
                let scope = egui::Id::new(("hxy-workspace-vfs", self.workspace_id.get()));
                let events = crate::vfs_panel::show(ui, scope, &*self.mount.fs);
                let mut to_open: Vec<String> = Vec::new();
                for e in events {
                    let crate::vfs_panel::VfsPanelEvent::OpenEntry(path) = e;
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
            crate::file::WorkspaceTab::Entry(file_id) => match self.files.get_mut(file_id) {
                Some(file) => render_file_tab(ui, *file_id, file, self.state, *self.tab_focus),
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
        !matches!(tab, crate::file::WorkspaceTab::Editor)
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
        if let crate::file::WorkspaceTab::Entry(file_id) = tab {
            if let Some(f) = self.files.get(file_id)
                && f.editor.is_dirty()
            {
                *self.pending_close_workspace_entry =
                    Some(PendingCloseTab { file_id: *file_id, display_name: f.display_name.clone() });
                return OnCloseResponse::Ignore;
            }
            if let Some(removed) = self.files.remove(file_id)
                && let Some(source) = removed.source_kind
            {
                self.state.open_tabs.retain(|t| t.source != source);
            }
        }
        OnCloseResponse::Close
    }
}

/// Mirror the tab's in-memory selection + scroll into
/// [`PersistedState::open_tabs`] so the save-on-dirty path picks it up.
/// `as_workspace` is set elsewhere (when a workspace is created or
/// torn down) and not touched here.
fn sync_tab_state(state: &mut PersistedState, file: &OpenFile) {
    let Some(source) = &file.source_kind else { return };
    if let Some(entry) = state.open_tabs.iter_mut().find(|t| &t.source == source) {
        entry.selection = file.editor.selection();
        entry.scroll_offset = file.editor.scroll_offset();
    }
}

fn build_palette(
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

fn status_bar_ui(
    ui: &mut egui::Ui,
    file: &mut OpenFile,
    base: crate::settings::OffsetBase,
    new_base: &mut crate::settings::OffsetBase,
    tab_focus: TabFocus,
) {
    ui.horizontal(|ui| {
        // Tab-focus chip on the far left so Ctrl+Tab's effect is
        // legible at a glance. "Outer" = top-level tabs cycle; "VFS"
        // = the surrounding workspace's inner tabs cycle. Click a
        // tab in the other dock to switch, or press Alt+Tab.
        let (icon, label, tooltip) = match tab_focus {
            TabFocus::Outer => (
                egui_phosphor::regular::SQUARES_FOUR,
                "Tabs: Outer",
                "Ctrl+Tab cycles top-level tabs. Alt+Tab to switch into a workspace.",
            ),
            TabFocus::Workspace(_) => (
                egui_phosphor::regular::TREE_STRUCTURE,
                "Tabs: VFS",
                "Ctrl+Tab cycles workspace sub-tabs. Alt+Tab to switch back to outer tabs.",
            ),
        };
        ui.label(format!("{icon} {label}")).on_hover_text(tooltip);
        ui.separator();

        if let Some(hov) = file.hovered {
            let value = format_offset(hov.get(), base);
            copyable_status_label(
                ui,
                &format!("Hover: {value}"),
                &value,
                Some(format_offset(hov.get(), base.toggle())),
                new_base,
                base,
            );
        } else {
            ui.label("Hover: --");
        }
        ui.separator();
        if let Some(sel) = file.editor.selection() {
            let range = sel.range();
            let last_inclusive = range.end().get().saturating_sub(1);
            let (display, copy, tooltip) = if sel.is_caret() {
                let v = format_offset(range.start().get(), base);
                (format!("Caret: {v}"), v, format_offset(range.start().get(), base.toggle()))
            } else {
                let start = format_offset(range.start().get(), base);
                let end = format_offset(last_inclusive, base);
                let len = format_offset(range.len().get(), base);
                let copy_value = format!("{start}-{end} ({len} bytes)");
                let tooltip = format!(
                    "{}-{}",
                    format_offset(range.start().get(), base.toggle()),
                    format_offset(last_inclusive, base.toggle()),
                );
                (format!("Sel: {copy_value}"), copy_value, tooltip)
            };
            copyable_status_label(ui, &display, &copy, Some(tooltip), new_base, base);
        } else {
            ui.label("Sel: --");
        }

        let size = file.editor.source().len().get();
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Lock toggle sits next to the length readout. Clicking
            // flips EditMode; the tooltip describes what the click
            // will do, not what the icon currently shows. When the
            // file has a hard read-only constraint (`read_only_reason`)
            // the icon stays locked, the click is a no-op, and the
            // tooltip explains why mutability isn't available.
            let hard_readonly = file.read_only_reason;
            let (icon, tooltip): (&str, String) = match (hard_readonly, file.editor.edit_mode()) {
                (Some(reason), _) => (
                    egui_phosphor::regular::LOCK,
                    hxy_i18n::t_args(
                        "status-lock-readonly-locked-tooltip",
                        &[("reason", &hxy_i18n::t(reason.message_key()))],
                    ),
                ),
                (None, crate::file::EditMode::Readonly) => {
                    (egui_phosphor::regular::LOCK, hxy_i18n::t("status-lock-readonly-tooltip"))
                }
                (None, crate::file::EditMode::Mutable) => {
                    (egui_phosphor::regular::LOCK_OPEN, hxy_i18n::t("status-lock-mutable-tooltip"))
                }
            };
            let resp = ui
                .add(egui::Button::new(icon).frame(false).min_size(egui::vec2(18.0, 18.0)))
                .on_hover_text(tooltip);
            if resp.clicked() && hard_readonly.is_none() {
                let next = match file.editor.edit_mode() {
                    crate::file::EditMode::Readonly => crate::file::EditMode::Mutable,
                    crate::file::EditMode::Mutable => crate::file::EditMode::Readonly,
                };
                file.editor.set_edit_mode(next);
            }

            let value = format_offset(size, base);
            copyable_status_label(
                ui,
                &format!("Length: {value}"),
                &value,
                Some(format_offset(size, base.toggle())),
                new_base,
                base,
            );
        });
    });
}

/// Click to toggle offset base, hover for the alternate-base tooltip,
/// and -- while hovered -- consume Cmd/Ctrl+C to copy the label's text.
/// Consuming the shortcut keeps the hex-view selection copy handler
/// from also firing in the same frame.
fn copyable_status_label(
    ui: &mut egui::Ui,
    display: &str,
    copy: &str,
    tooltip: Option<String>,
    new_base: &mut crate::settings::OffsetBase,
    base: crate::settings::OffsetBase,
) {
    let r = ui.add(egui::Label::new(display).sense(egui::Sense::click()));
    if r.clicked() {
        *new_base = base.toggle();
    }
    // Direct pointer-in-rect check: `r.hovered()` and even
    // `ui.rect_contains_pointer` can read false when a tooltip or
    // neighbouring widget counts as covering the label. Reading the
    // pointer position and testing `r.rect.contains(p)` bypasses
    // egui's widget-layering bookkeeping entirely -- which is what
    // we want for a whole-cell-is-the-target hover.
    let over_label = ui.ctx().input(|i| i.pointer.latest_pos()).is_some_and(|p| r.rect.contains(p));
    let r = if let Some(tt) = tooltip { r.on_hover_text(tt) } else { r };
    let _ = r;
    if over_label && ui.ctx().input_mut(consume_copy_event) {
        ui.ctx().copy_text(copy.to_string());
    }
}

fn format_offset(value: u64, base: crate::settings::OffsetBase) -> String {
    match base {
        crate::settings::OffsetBase::Hex => format!("0x{value:X}"),
        crate::settings::OffsetBase::Decimal => format!("{value}"),
    }
}

use crate::copy_format::CopyKind;
use crate::shortcuts::CLOSE_TAB;
use crate::shortcuts::COPY_BYTES;
use crate::shortcuts::COPY_HEX;
use crate::shortcuts::FIND_GLOBAL;
use crate::shortcuts::FIND_LOCAL;
use crate::shortcuts::FOCUS_PANE;
use crate::shortcuts::NEW_FILE;
use crate::shortcuts::NEXT_TAB;
use crate::shortcuts::PASTE;
use crate::shortcuts::PASTE_AS_HEX;
use crate::shortcuts::PREV_TAB;
use crate::shortcuts::REDO;
use crate::shortcuts::SAVE_FILE;
use crate::shortcuts::SAVE_FILE_AS;
use crate::shortcuts::TOGGLE_EDIT_MODE;
use crate::shortcuts::TOGGLE_TAB_FOCUS;
use crate::shortcuts::UNDO;

/// Background tint for patched bytes when the user's highlight mode
/// paints glyphs. Saturated red stands out against the default cell
/// fill on both light and dark themes.
const MODIFIED_BYTE_BG: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x80, 0x10, 0x10, 0xB0);
/// Foreground tint for patched bytes when the base highlight already
/// owns the cell fill (background mode or highlighting disabled).
const MODIFIED_BYTE_FG: egui::Color32 = egui::Color32::from_rgb(0xFF, 0x5A, 0x4A);

/// Binary search a sorted, non-overlapping list of byte ranges for
/// `offset`. Used by the hex-view tinting closure -- O(log N) per
/// pixel-row instead of O(N).
fn range_contains(ranges: &[(u64, u64)], offset: u64) -> bool {
    let idx = ranges.partition_point(|(start, _)| *start <= offset);
    if idx == 0 {
        return false;
    }
    let (_start, end) = ranges[idx - 1];
    offset < end
}

/// Read the active selection's bytes from `file` and copy them to
/// the clipboard formatted per `kind`. Value-kind variants read the
/// first `selection.len()` bytes as a LE integer (0-8 bytes) -- the
/// hex view has no type context, so this is the best we can do
/// without a template supplying sign + endianness.
fn do_copy(ctx: &egui::Context, file: &OpenFile, kind: CopyKind) {
    let Some(selection) = file.editor.selection() else { return };
    let range = selection.range();
    if range.is_empty() {
        return;
    }
    let offset = range.start().get();
    let bytes = match file.editor.source().read(range) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "read selection for copy");
            return;
        }
    };

    let text = if kind.is_value() {
        if bytes.is_empty() || bytes.len() > 8 {
            return;
        }
        let mut arr = [0u8; 8];
        arr[..bytes.len()].copy_from_slice(&bytes);
        let raw = u64::from_le_bytes(arr);
        match crate::copy_format::format_scalar(kind, raw) {
            Some(s) => s,
            None => return,
        }
    } else {
        let ident = format!("data_{:X}", offset);
        let type_hint = format!("u8[{}]", bytes.len());
        match crate::copy_format::format_bytes(kind, &bytes, &ident, &type_hint) {
            Some(s) => s,
            None => return,
        }
    };
    ctx.copy_text(text);
}

const WELCOME_OPEN_RECENT: &str = "hxy_welcome_open_recent";

fn console_ui(ui: &mut egui::Ui, console: &std::collections::VecDeque<ConsoleEntry>) {
    if console.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.weak(hxy_i18n::t("console-empty"));
        });
        return;
    }

    // Newest entries at the bottom, matching the usual log UX.
    egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        egui::Grid::new("hxy_console_grid").num_columns(4).striped(true).show(ui, |ui| {
            for entry in console.iter() {
                let (icon, color) = match entry.severity {
                    ConsoleSeverity::Info => (egui_phosphor::regular::INFO, None),
                    ConsoleSeverity::Warning => (egui_phosphor::regular::WARNING, Some(egui::Color32::YELLOW)),
                    ConsoleSeverity::Error => (egui_phosphor::regular::X_CIRCLE, Some(egui::Color32::LIGHT_RED)),
                };
                let time = format_console_time(entry.timestamp);
                ui.label(egui::RichText::new(&time).monospace().weak());
                let mut icon_text = egui::RichText::new(icon);
                if let Some(c) = color {
                    icon_text = icon_text.color(c);
                }
                ui.label(icon_text);
                ui.label(egui::RichText::new(&entry.context).weak());
                ui.label(&entry.message);
                ui.end_row();
            }
        });
    });
}

fn format_console_time(ts: jiff::Timestamp) -> String {
    // Keep the display compact -- HH:MM:SS.mmm, user-local.
    let zoned = ts.in_tz("UTC").unwrap_or_else(|_| ts.to_zoned(jiff::tz::TimeZone::UTC));
    format!("{:02}:{:02}:{:02}", zoned.hour(), zoned.minute(), zoned.second())
}

fn welcome_ui(ui: &mut egui::Ui, state: &PersistedState) {
    ui.vertical_centered(|ui| {
        ui.add_space(32.0);
        ui.heading(hxy_i18n::t("app-name"));
        ui.label(hxy_i18n::t("app-tagline"));
    });
    ui.add_space(16.0);
    ui.separator();
    ui.add_space(8.0);
    ui.heading(hxy_i18n::t("welcome-recent"));
    if state.app.recent_files.is_empty() {
        ui.weak(hxy_i18n::t("welcome-recent-empty"));
        return;
    }
    egui::ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| {
        for entry in &state.app.recent_files {
            let label = entry.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let row = ui
                .add(egui::Button::new(label).wrap_mode(egui::TextWrapMode::Truncate))
                .on_hover_text(entry.path.display().to_string());
            if row.clicked() {
                ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new(WELCOME_OPEN_RECENT), entry.path.clone()));
            }
        }
    });
}

fn settings_ui(ui: &mut egui::Ui, settings: &mut crate::settings::AppSettings) {
    ui.heading(hxy_i18n::t("settings-general-header"));
    ui.separator();
    egui::Grid::new("hxy-general-settings").num_columns(2).striped(true).show(ui, |ui| {
        ui.label(hxy_i18n::t("settings-zoom"));
        ui.add(egui::Slider::new(&mut settings.zoom_factor, 0.5..=2.0).step_by(0.1));
        ui.end_row();

        ui.label(hxy_i18n::t("settings-columns"));
        let mut cols = settings.hex_columns.get();
        ui.add(egui::DragValue::new(&mut cols).range(1..=64));
        if let Ok(new_cols) = hxy_core::ColumnCount::new(cols) {
            settings.hex_columns = new_cols;
        }
        ui.end_row();

        ui.label(hxy_i18n::t("settings-check-updates"));
        ui.checkbox(&mut settings.check_for_updates, "");
        ui.end_row();

        ui.label(hxy_i18n::t("settings-byte-highlight"));
        ui.checkbox(&mut settings.byte_value_highlight, "");
        ui.end_row();

        ui.label(hxy_i18n::t("settings-byte-highlight-mode"));
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut settings.byte_highlight_mode,
                crate::settings::ByteHighlightMode::Background,
                hxy_i18n::t("settings-byte-highlight-background"),
            );
            ui.selectable_value(
                &mut settings.byte_highlight_mode,
                crate::settings::ByteHighlightMode::Text,
                hxy_i18n::t("settings-byte-highlight-text"),
            );
        });
        ui.end_row();

        ui.label(hxy_i18n::t("settings-byte-highlight-scheme"));
        ui.horizontal(|ui| {
            ui.selectable_value(
                &mut settings.byte_highlight_scheme,
                crate::settings::ByteHighlightScheme::Class,
                hxy_i18n::t("settings-byte-highlight-scheme-class"),
            );
            ui.selectable_value(
                &mut settings.byte_highlight_scheme,
                crate::settings::ByteHighlightScheme::Value,
                hxy_i18n::t("settings-byte-highlight-scheme-value"),
            );
        });
        ui.end_row();

        ui.label(hxy_i18n::t("settings-minimap"));
        ui.checkbox(&mut settings.show_minimap, "");
        ui.end_row();

        ui.label(hxy_i18n::t("settings-minimap-colored"));
        ui.add_enabled_ui(settings.show_minimap, |ui| {
            ui.checkbox(&mut settings.minimap_colored, "");
        });
        ui.end_row();

        ui.label(hxy_i18n::t("settings-offset-base"));
        egui::ComboBox::from_id_salt("hxy-offset-base")
            .selected_text(match settings.offset_base {
                crate::settings::OffsetBase::Hex => "Hex",
                crate::settings::OffsetBase::Decimal => "Decimal",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut settings.offset_base, crate::settings::OffsetBase::Hex, "Hex");
                ui.selectable_value(&mut settings.offset_base, crate::settings::OffsetBase::Decimal, "Decimal");
            });
        ui.end_row();

        ui.label(hxy_i18n::t("settings-address-separator"));
        ui.checkbox(&mut settings.address_separator_enabled, "");
        ui.end_row();

        ui.label(hxy_i18n::t("settings-address-separator-char"));
        ui.add_enabled_ui(settings.address_separator_enabled, |ui| {
            // Edit through a single-char string buffer; clamp on
            // commit so the user can type a multi-char paste and
            // still end up with a single character.
            let mut buf = settings.address_separator_char.to_string();
            if ui.add(egui::TextEdit::singleline(&mut buf).desired_width(28.0).char_limit(1)).changed()
                && let Some(ch) = buf.chars().next()
            {
                settings.address_separator_char = ch;
            }
        });
        ui.end_row();
    });
}
