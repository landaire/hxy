//! Desktop-target HxyApp impl: full feature set with plugin host
//! (wasmtime), filesystem watcher (notify), native macOS menu (muda),
//! IPC second-instance forwarding (interprocess), sync rfd dialogs,
//! sqlite snapshot persistence, and the larger eframe::App update
//! loop. Browser-portable subsets live in `app::wasm`; truly shared
//! types and helpers stay in `app::mod`.

use std::collections::HashMap;
use std::sync::Arc;

use egui_dock::DockArea;
use egui_dock::DockState;
use hxy_vfs::TabSource;
use hxy_vfs::VfsHandler;
use hxy_vfs::VfsRegistry;
use hxy_vfs::handlers::ZipHandler;

use super::ANONYMOUS_DEFAULT_SIZE;
use super::ConsoleEntry;
use super::ConsoleSeverity;
use super::HxyApp;
use super::MountedVfs;
use super::OpenTarget;
use super::PendingDuplicate;
use super::PendingOrphanEntry;
use super::PendingPatchRestore;
use super::ReloadDecision;
use super::TabFocus;
use super::apply_global_search_events;
use super::apply_zoom_change;
use super::byte_cache_limit_from_state;
use super::capture_window_on_drag_end;
use super::cascade_byte_change;
use super::compute_entropy_for;
use super::consume_dropped_files;
use super::consume_welcome_open_request;
use super::desktop_tab_viewer;
use super::drain_byte_change_cascade;
use super::drain_checksums_runs;
use super::drain_entropy_runs;
use super::drain_external_open_requests;
use super::drain_file_watch_events;
use super::drain_native_menu;
use super::drain_pending_vfs_opens;
use super::drain_strings_runs;
use super::drain_vfs_open_inbox;
use super::handle_command_palette;
use super::install_fonts;
use super::jump_to_strings_match;
use super::load_template_library_dirs;
use super::load_user_template_plugins;
use super::paint_drop_overlay;
use super::polling_prefs_from_settings;
use super::record_virtual_base_hint;
use super::register_user_plugins;
use super::spawn_checksums_with_panel_config;
use super::spawn_strings_with_panel_config;
use super::sync_native_menu_state;
use super::vfs_pref_key_for;
use crate::files::FileId;
use crate::files::OpenFile;
use crate::state::PersistedState;
use crate::state::SharedPersistedState;
use crate::tabs::Tab;
impl HxyApp {
    pub fn new(cc: &eframe::CreationContext<'_>, state: SharedPersistedState) -> Self {
        install_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        cc.egui_ctx.set_global_style(crate::style::hxy_style());
        // Spin up the shared CPU-bound worker pool eagerly so the
        // first template / diff / entropy job doesn't pay thread
        // creation latency on the UI hot path.
        crate::background::init();
        let (initial_zoom, initial_window, show_patterns_prompt, initial_polling) = {
            let s = state.read();
            // First-launch download dialog when the corpus isn't on
            // disk and the user hasn't actively declined. Snapped
            // here so we can move `state` into the struct below.
            let show_patterns_prompt =
                s.app.imhex_patterns.installed_hash.is_none() && !s.app.imhex_patterns.declined_prompt;
            let polling = polling_prefs_from_settings(&s.app);
            (s.app.zoom_factor, s.window, show_patterns_prompt, polling)
        };
        cc.egui_ctx.set_zoom_factor(initial_zoom);
        let mut registry = VfsRegistry::new();
        registry.register(Arc::new(ZipHandler::new()));
        // Plugin loading is deferred to [`Self::reload_plugins`], which
        // the host calls exactly once after [`Self::with_plugin_persistence`]
        // has wired the SQLite-backed grants and state store. Compiling
        // every WASM plugin twice (once with default grants here, then
        // again with real grants from `reload_plugins`) is wasted
        // wasmtime work -- a single failing plugin probe alone is tens
        // of MB of cranelift allocator churn. The wasm32 build never
        // calls `with_plugin_persistence`; it gets plugin loading via
        // the explicit `reload_plugins()` call right after construction.
        let plugin_handlers: Vec<Arc<hxy_plugin_host::PluginHandler>> = Vec::new();
        let template_plugins = load_user_template_plugins();
        Self {
            dock: DockState::new(vec![Tab::Welcome, Tab::Settings]),
            files: HashMap::new(),
            workspaces: std::collections::BTreeMap::new(),
            next_workspace_id: 1,
            mounts: std::collections::BTreeMap::new(),
            next_mount_id: 1,
            compares: std::collections::BTreeMap::new(),
            next_compare_id: 1,
            byte_cache: hxy_core::ByteCache::new(byte_cache_limit_from_state(&state)),
            state,
            next_file_id: 1,
            registry,
            template_plugins,
            plugin_handlers,
            plugin_state_store: None,
            sink: None,
            prev_window: None,
            last_saved_window: Some(initial_window),
            applied_zoom: initial_zoom,
            pending_duplicate: None,
            toasts: crate::toasts::ToastCenter::new(),
            pending_search_modal: None,
            compare_picker: None,
            pending_patch_restore: None,
            console: std::collections::VecDeque::new(),
            inspector: crate::panels::inspector::InspectorState::default(),
            decoders: crate::panels::inspector::default_decoders(),
            last_active_file: None,
            last_active_workspace: None,
            #[cfg(target_os = "macos")]
            menu: Some(crate::menu::MenuState::install()),
            plugin_rescan: false,
            pending_plugin_events: Vec::new(),
            pending_plugin_ops: Vec::new(),
            templates: load_template_library_dirs(),
            palette: crate::commands::palette::PaletteState::default(),
            pending_pane_pick: None,
            pane_pick_letters: std::collections::BTreeMap::new(),
            pending_close_tab: None,
            tab_focus: TabFocus::Outer,
            pending_close_workspace_entry: None,
            pending_collapse_workspace: Vec::new(),
            pending_close_mount: None,
            pane_pick_target_paths: None,
            global_search: crate::search::global::GlobalSearchState::default(),
            pending_global_search_events: Vec::new(),
            last_content_leaf: None,
            pending_cli_paths: Vec::new(),
            ipc_inbox: None,
            pattern_fetch: None,
            pattern_in_flight_bytes: None,
            pending_pattern_download_request: false,
            // Cached above before `state` was moved into the struct.
            pattern_first_run_prompt: show_patterns_prompt,
            pending_template_runs: Vec::new(),
            pending_byte_change_cascade: Vec::new(),
            pending_template_restore: false,
            file_watcher: match crate::files::watch::FileWatcher::with_prefs(&cc.egui_ctx, initial_polling) {
                Ok(w) => Some(w),
                Err(e) => {
                    tracing::warn!(error = %e, "filesystem watcher unavailable; external changes will go undetected");
                    None
                }
            },
            pending_reload_prompt: None,
            pending_virtual_base_prompt: None,
            pending_open_with_options: None,
            pending_orphan_entries: Vec::new(),
            pending_snapshot_dialog: None,
            closed_tabs: std::collections::VecDeque::with_capacity(crate::tabs::close::CLOSED_TABS_CAPACITY),
            vfs_open_inbox: egui_inbox::UiInbox::new_with_ctx(&cc.egui_ctx),
        }
    }

    /// Rebuild the VFS registry + template runtime list from the
    /// user's plugin directories. Called by the Plugins tab after the
    /// user installs or deletes a file.
    pub fn reload_plugins(&mut self) {
        let mut registry = VfsRegistry::new();
        registry.register(Arc::new(ZipHandler::new()));
        let grants = self.state.read().plugin_grants.clone();
        self.plugin_handlers = register_user_plugins(&mut registry, &grants, self.plugin_state_store.clone());
        self.registry = registry;
        self.template_plugins = load_user_template_plugins();
        self.templates = load_template_library_dirs();
    }

    /// Refresh the user-template library after a successful
    /// ImHex-Patterns download. Same shape as [`reload_plugins`]
    /// but only touches the templates list -- the plugin registry
    /// is unchanged.
    pub fn refresh_templates_after_pattern_install(&mut self) {
        self.templates = load_template_library_dirs();
    }

    /// Drain a batch of grant / wipe events captured by the
    /// Plugins tab. Mutates `PersistedState::plugin_grants` for
    /// any `SetGrant`, calls the state store for any `WipeState`,
    /// then triggers a single `reload_plugins` at the end so the
    /// linker reflects the new grant set.
    fn apply_plugin_events(&mut self, events: Vec<crate::panels::plugins::PluginsEvent>) {
        let mut grants_changed = false;
        for ev in events {
            match ev {
                crate::panels::plugins::PluginsEvent::Rescan => {
                    self.plugin_rescan = true;
                }
                crate::panels::plugins::PluginsEvent::SetGrant { key, grants: g } => {
                    self.state.write().plugin_grants.set(key, g);
                    grants_changed = true;
                }
                crate::panels::plugins::PluginsEvent::WipeState { plugin_name } => {
                    if let Some(store) = self.plugin_state_store.as_ref()
                        && let Err(e) = store.clear(&plugin_name)
                    {
                        tracing::warn!(error = %e, plugin = %plugin_name, "wipe plugin state");
                    }
                }
                crate::panels::plugins::PluginsEvent::RequestPatternsDownload => {
                    // Use the egui ctx the next frame already has; the
                    // worker only needs it to request a repaint when
                    // it posts a status update.
                    self.pending_pattern_download_request = true;
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
    pub fn show_plugins(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Plugins) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        let node_path = crate::tabs::dock_ops::push_tool_tab(&mut self.dock, Tab::Plugins);
        self.dock.set_focused_node_and_surface(node_path);
    }

    /// Close the Plugins tab if present; otherwise show it.
    pub fn toggle_plugins(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Plugins) {
            let _ = self.dock.remove_tab(path);
        } else {
            self.show_plugins();
        }
    }

    /// Open the data inspector. Routes through the shared tool
    /// leaf (the one that already holds Plugins / Memory /
    /// Visualizer / etc.) so opening the inspector when another
    /// tool is up adds it as a sibling tab instead of forcing a
    /// second right-split. Falls back to a fresh right split of
    /// the main dock area when no tool leaf exists yet, matching
    /// 010 Editor's layout. If already docked anywhere
    /// (including after the user drags it elsewhere), focus the
    /// existing tab instead of creating a second split.
    pub fn show_inspector(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Inspector) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        let node_path = crate::tabs::dock_ops::push_tool_tab(&mut self.dock, Tab::Inspector);
        self.dock.set_focused_node_and_surface(node_path);
    }

    /// Close the Inspector tab if present; otherwise show it.
    pub fn toggle_inspector(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Inspector) {
            let _ = self.dock.remove_tab(path);
        } else {
            self.show_inspector();
        }
    }

    /// Show (or focus) the Memory debug panel. Routes through the
    /// shared tool leaf alongside the other debug panels.
    pub fn show_memory_panel(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Memory) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        let node_path = crate::tabs::dock_ops::push_tool_tab(&mut self.dock, Tab::Memory);
        self.dock.set_focused_node_and_surface(node_path);
    }

    /// Close the Memory tab if present; otherwise show it.
    pub fn toggle_memory_panel(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Memory) {
            let _ = self.dock.remove_tab(path);
        } else {
            self.show_memory_panel();
        }
    }

    /// Show (or focus) the Entropy panel for `file_id`. Each
    /// file gets its own tab so two panels can be docked
    /// side-by-side for visual comparison; opening entropy for
    /// the same file twice just focuses the existing tab.
    pub fn show_entropy_for(&mut self, file_id: FileId) {
        if let Some(path) = self.dock.find_tab(&Tab::Entropy(file_id)) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        let node_path = crate::tabs::dock_ops::push_tool_tab(&mut self.dock, Tab::Entropy(file_id));
        self.dock.set_focused_node_and_surface(node_path);
    }

    /// Show (or focus) the Strings panel for `file_id`. Modeled on
    /// [`Self::show_entropy_for`]: per-file dock tab, push to the
    /// shared tool leaf if not already present, focus otherwise.
    pub fn show_strings_for(&mut self, file_id: FileId) {
        if let Some(path) = self.dock.find_tab(&Tab::Strings(file_id)) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        let node_path = crate::tabs::dock_ops::push_tool_tab(&mut self.dock, Tab::Strings(file_id));
        self.dock.set_focused_node_and_surface(node_path);
    }

    /// Show (or focus) the Checksums panel for `file_id`.
    pub fn show_checksums_for(&mut self, file_id: FileId) {
        if let Some(path) = self.dock.find_tab(&Tab::Checksums(file_id)) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        let node_path = crate::tabs::dock_ops::push_tool_tab(&mut self.dock, Tab::Checksums(file_id));
        self.dock.set_focused_node_and_surface(node_path);
    }

    /// Show (or focus) the Visualizer panel for `file_id`. Used by
    /// the auto-open path after a template run produces visualizer
    /// attributes, and by the in-row visualizer icon. No-ops when
    /// the user has previously dismissed the panel for this file
    /// (so re-runs don't re-pop it). The user can still re-open
    /// manually via the View menu / palette.
    pub fn show_visualizer_for(&mut self, file_id: FileId) {
        if let Some(path) = self.dock.find_tab(&Tab::Visualizer(file_id)) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        let node_path = crate::tabs::dock_ops::push_tool_tab(&mut self.dock, Tab::Visualizer(file_id));
        self.dock.set_focused_node_and_surface(node_path);
    }

    /// After a template completes, restore the visualizer panel only
    /// when the user previously had it open for this file -- via
    /// either an explicit click this session or a persisted
    /// `OpenTabState::visualizer_open` from the prior session. Closed
    /// is the default; a freshly opened file with visualizer-bearing
    /// fields stays quiet until the user asks for the panel.
    pub fn auto_open_visualizer_for(&mut self, file_id: FileId) {
        let should_open = match self.files.get(&file_id) {
            Some(file) if file.visualizer_panel.open => !crate::visualizers::collect_targets(file).is_empty(),
            _ => false,
        };
        if should_open {
            self.show_visualizer_for(file_id);
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
        let mut still_pending: Vec<crate::plugins::runner::PendingOp> = Vec::new();
        for op in drained {
            let plugin_name = op.plugin_name.clone();
            let label = op.label.clone();
            let started = op.started;
            match op.try_take() {
                Err(unfinished) => still_pending.push(unfinished),
                Ok(crate::plugins::runner::DrainResult::Pending) => {}
                Ok(crate::plugins::runner::DrainResult::InvokeReady { plugin, command_id, outcome }) => {
                    self.log_plugin_completion(&plugin_name, &label, started, outcome.is_some());
                    crate::plugins::mount::dispatch_plugin_outcome(
                        ctx,
                        self,
                        plugin,
                        &plugin_name,
                        &command_id,
                        outcome,
                    );
                }
                Ok(crate::plugins::runner::DrainResult::RespondReady { plugin, command_id, outcome }) => {
                    self.log_plugin_completion(&plugin_name, &label, started, outcome.is_some());
                    crate::plugins::mount::dispatch_plugin_outcome(
                        ctx,
                        self,
                        plugin,
                        &plugin_name,
                        &command_id,
                        outcome,
                    );
                }
                Ok(crate::plugins::runner::DrainResult::MountReady { plugin, token, title, result }) => match result {
                    Ok(mount) => {
                        self.log_plugin_completion(&plugin_name, &label, started, true);
                        crate::plugins::mount::install_mount_tab(self, plugin, mount, token, title);
                    }
                    Err(e) => {
                        crate::plugins::runner::log_completion(
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

    fn log_plugin_completion(&mut self, plugin_name: &str, label: &str, started: std::time::Instant, ok: bool) {
        let (sev, detail) = if ok {
            (ConsoleSeverity::Info, "ok")
        } else {
            (ConsoleSeverity::Warning, "no outcome (call trapped or grant denied)")
        };
        crate::plugins::runner::log_completion(self, plugin_name, label, started, sev, detail);
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

    /// Focus the existing Settings tab if present; otherwise push a
    /// fresh one into the focused leaf. Settings is a content tab
    /// rather than a tool panel, so we land it next to whatever the
    /// user is currently looking at instead of routing through
    /// `push_tool_tab`.
    pub fn show_settings(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Settings) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        self.dock.push_to_focused_leaf(Tab::Settings);
    }

    /// Close the Settings tab if present; otherwise show it.
    pub fn toggle_settings(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Settings) {
            let _ = self.dock.remove_tab(path);
        } else {
            self.show_settings();
        }
    }
    pub fn template_runtime_for(&self, extension: &str) -> Option<Arc<dyn hxy_plugin_host::TemplateRuntime>> {
        self.template_plugins.iter().find(|r| r.extensions().iter().any(|e| e.eq_ignore_ascii_case(extension))).cloned()
    }

    pub fn registry(&self) -> &VfsRegistry {
        &self.registry
    }
    pub fn with_sink(mut self, sink: crate::settings::persist::SaveSink) -> Self {
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
    pub fn with_cli_paths(mut self, paths: Vec<std::path::PathBuf>) -> Self {
        self.pending_cli_paths = paths;
        self
    }

    /// Hand off the IPC listener's inbox so the running instance
    /// can pick up forwarded paths from later `hxy <file>...`
    /// invocations. `None` is fine: the GUI just won't accept
    /// forwarded opens.
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
        let source: Arc<dyn hxy_core::HexSource> = Arc::new(hxy_core::MemorySource::new(bytes));
        self.open(display_name, None, source, None, None, false)
    }

    /// Open a filesystem-backed tab from an already-constructed
    /// streaming source. Internal helper -- most callers use
    /// [`Self::open_filesystem_path`] which wires the source up
    /// for them.
    pub fn open_filesystem(
        &mut self,
        display_name: impl Into<String>,
        path: std::path::PathBuf,
        source: Arc<dyn hxy_core::HexSource>,
        restore_selection: Option<hxy_core::Selection>,
        restore_scroll: Option<f32>,
    ) -> FileId {
        self.open(display_name, Some(TabSource::Filesystem(path)), source, restore_selection, restore_scroll, false)
    }

    /// Open a filesystem path with a streaming `HexSource` -- no
    /// up-front full-file read. Returns the new tab id, or an
    /// error if the file can't be opened.
    pub fn open_filesystem_path(
        &mut self,
        display_name: impl Into<String>,
        path: std::path::PathBuf,
        restore_selection: Option<hxy_core::Selection>,
        restore_scroll: Option<f32>,
    ) -> Result<FileId, crate::files::FileOpenError> {
        let (source, _len) = crate::files::streaming::open_filesystem(&path)
            .map_err(|source| crate::files::FileOpenError::Read { path: path.clone(), source })?;
        Ok(self.open_filesystem(display_name, path, source, restore_selection, restore_scroll))
    }

    /// User-facing open: if the path is already in another tab, stash
    /// the request and show a "focus existing vs open duplicate"
    /// modal on the next frame. Otherwise opens straight away.
    ///
    /// Restore paths deliberately bypass this -- reopening a file
    /// across restarts shouldn't prompt.
    pub fn request_open_filesystem(&mut self, display_name: impl Into<String>, path: std::path::PathBuf) {
        let display_name = display_name.into();
        if let Some(existing) = self.existing_filesystem_tab(&path) {
            self.pending_duplicate = Some(PendingDuplicate { display_name, path, existing });
            return;
        }
        if let Err(e) = self.open_filesystem_path(display_name, path, None, None) {
            tracing::warn!(error = %e, "request_open_filesystem");
        }
    }

    fn existing_filesystem_tab(&self, path: &std::path::Path) -> Option<FileId> {
        self.files.iter().find_map(|(id, f)| match &f.source_kind {
            Some(TabSource::Filesystem(p)) if p == path => Some(*id),
            _ => None,
        })
    }

    /// Move dock focus to `tab`, if it lives in the outer dock. No-op
    /// when the tab is gone (closed since the caller picked it up).
    /// Use `focus_file_tab` instead for `Tab::File` / `Tab::Workspace`,
    /// which need the workspace-nesting fallback.
    pub(crate) fn focus_tab(&mut self, tab: Tab) {
        if let Some(path) = self.dock.find_tab(&tab) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
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
        source: Arc<dyn hxy_core::HexSource>,
        restore_selection: Option<hxy_core::Selection>,
        restore_scroll: Option<f32>,
        as_workspace: bool,
    ) -> FileId {
        let id = self.create_open_file(display_name, source_kind.clone(), source, restore_selection, restore_scroll);
        self.apply_readonly_for_source(id);

        let pushed_workspace = if as_workspace { self.try_push_as_workspace(id) } else { false };
        if !pushed_workspace {
            // Don't drop a fresh file tab into a leaf that's
            // entirely tool panels (Inspector, Console,
            // Entropy, Plugins, ...). Redirect focus to the
            // last known content leaf so push_to_focused_leaf
            // lands the file in the editing area instead.
            if crate::tabs::dock_ops::focused_leaf_is_all_tool(self) {
                crate::tabs::dock_ops::focus_content_leaf(self);
            }
            self.dock.push_to_focused_leaf(Tab::File(id));
            if let Some(path) = self.dock.find_tab(&Tab::File(id)) {
                crate::tabs::dock_ops::remove_welcome_from_leaf(&mut self.dock, path.surface, path.node);
            }
        }

        // Look for an unsaved-edits sidecar from a previous session
        // and offer it back to the user. The actual restore happens
        // after the modal returns; this just stages the prompt.
        if let Some(TabSource::Filesystem(path)) = source_kind.as_ref()
            && let Some(dir) = crate::files::save::unsaved_edits_dir()
        {
            match crate::files::patch_persist::load(&dir, path) {
                Ok(Some(sidecar)) => {
                    let integrity = sidecar.integrity();
                    self.pending_patch_restore = Some(PendingPatchRestore { file_id: id, sidecar, integrity });
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
                    templates: Vec::new(),
                    active_template_idx: None,
                    visualizer_open: false,
                    virtual_base_choice: None,
                });
            }
        }
        self.suggest_templates_for(id);
        self.watch_root_for_file(id);
        id
    }

    /// Register the watcher for whatever the just-opened
    /// `OpenFile` ultimately derives from -- a filesystem path
    /// for disk-backed tabs, or a sample-hash poller for VFS-
    /// entry tabs (xbox memory, plugin mounts, etc.). No-op for
    /// purely in-memory anonymous tabs, when the watcher failed
    /// to construct at startup, or when the per-file auto-reload
    /// pref is `Never` (which means "don't even watch").
    pub(crate) fn watch_root_for_file(&mut self, id: FileId) {
        let Some(file) = self.files.get(&id) else { return };
        // Skip enrolment entirely when the user marked this
        // file's effective auto-reload mode as Never -- there's
        // no point paying the kernel-watch / sample-hash cost
        // for a file the user has explicitly silenced.
        let watch_key = self.watch_key_for(id);
        if let Some(key) = watch_key.as_ref()
            && self.state.read().app.auto_reload_for(key) == crate::settings::AutoReloadMode::Never
        {
            return;
        }
        if let Some(path) = file.root_path().cloned()
            && let Some(watcher) = self.file_watcher.as_mut()
        {
            watcher.watch(path);
        }
        let needs_vfs_poll = matches!(file.source_kind.as_ref(), Some(TabSource::VfsEntry { parent, .. })
            if !matches!(parent.as_ref(), TabSource::Filesystem(_)));
        if needs_vfs_poll {
            let source = file.editor.source().clone();
            if let Some(watcher) = self.file_watcher.as_mut() {
                watcher.watch_vfs(id, source);
            }
        }
    }

    /// Resolve the per-file pref key the auto-reload table is
    /// indexed by for `id` -- a real filesystem path for disk-
    /// backed tabs, or a synthesised `vfs://...` key for VFS-
    /// entry tabs. `None` for purely in-memory anonymous tabs
    /// where there's nothing to remember across restarts.
    pub(crate) fn watch_key_for(&self, id: FileId) -> Option<std::path::PathBuf> {
        let file = self.files.get(&id)?;
        if let Some(p) = file.root_path() {
            return Some(p.clone());
        }
        let source = file.source_kind.as_ref()?;
        Some(vfs_pref_key_for(source))
    }

    /// Re-evaluate the watcher enrolment for `id` after the
    /// per-file auto-reload pref or the source identity
    /// changed. Idempotent: re-watching is a no-op for paths /
    /// entries already watched.
    pub(crate) fn refresh_watch_for_file(&mut self, id: FileId) {
        let Some(file) = self.files.get(&id) else { return };
        let watch_key = self.watch_key_for(id);
        let mode = match watch_key.as_ref() {
            Some(k) => self.state.read().app.auto_reload_for(k),
            None => crate::settings::AutoReloadMode::default(),
        };
        let path = file.root_path().cloned();
        match mode {
            crate::settings::AutoReloadMode::Never => {
                if let Some(p) = path {
                    self.unwatch_path_if_unused(&p);
                }
                self.unwatch_vfs_for_file(id);
            }
            crate::settings::AutoReloadMode::Always | crate::settings::AutoReloadMode::Ask => {
                self.watch_root_for_file(id);
            }
        }
    }

    /// Set the per-file auto-reload pref for `id` and re-aim
    /// the watcher. Used by the palette and the reload prompt's
    /// "remember for this file" checkbox.
    pub fn set_file_watch_pref(&mut self, id: FileId, mode: crate::settings::AutoReloadMode) {
        let Some(key) = self.watch_key_for(id) else { return };
        let global = self.state.read().app.auto_reload;
        {
            let mut g = self.state.write();
            // Clearing the override (passing the same mode as
            // the global default) makes the file fall back to
            // the global -- prevents accumulating redundant
            // entries in file_watch_prefs.
            let pref = if mode == global { None } else { Some(mode) };
            g.app.set_auto_reload_for(key, pref);
        }
        self.refresh_watch_for_file(id);
    }

    /// Unregister the watcher for `path` if no remaining open file
    /// or workspace still references it.
    pub(crate) fn unwatch_path_if_unused(&mut self, path: &std::path::Path) {
        let Some(watcher) = self.file_watcher.as_mut() else { return };
        let still_used = self.files.values().any(|f| f.root_path().map(|p| p.as_path()) == Some(path));
        if still_used {
            return;
        }
        watcher.unwatch(path);
    }

    /// Drop the VFS sample-hash poller for `id`. Called from
    /// the close path so the worker stops re-reading bytes
    /// through a source the user already torn down.
    pub(crate) fn unwatch_vfs_for_file(&mut self, id: FileId) {
        if let Some(watcher) = self.file_watcher.as_mut() {
            watcher.unwatch_vfs(id);
        }
    }

    /// Reload `id` from its filesystem-backed root path. The
    /// `decision` arm controls whether the user's patch + undo /
    /// redo survive the swap. Returns `false` when the file isn't
    /// reloadable (in-memory tab, vanished path, read failure);
    /// the caller is expected to surface the diagnostic via the
    /// console.
    ///
    /// On success the workspace mount (if any) is re-built so the
    /// VFS tree reflects the new bytes, every workspace-entry tab
    /// derived from it re-reads its bytes (or stages an orphan
    /// prompt when the entry no longer exists in the new mount),
    /// and any template that previously ran against the old bytes
    /// is re-fired against the new ones.
    pub fn apply_reload_decision(&mut self, ctx: &egui::Context, id: FileId, decision: ReloadDecision) -> bool {
        if matches!(decision, ReloadDecision::Ignore) {
            return true;
        }
        let Some(file) = self.files.get(&id) else { return false };
        let Some(path) = file.root_path().cloned() else { return false };
        let display = file.display_name.clone();
        let ctx_label = format!("Reload {}", path.display());
        let (stream, len) = match crate::files::streaming::open_filesystem(&path) {
            Ok(s) => s,
            Err(e) => {
                self.console_log(ConsoleSeverity::Error, &ctx_label, format!("re-open disk source: {e}"));
                return false;
            }
        };
        if let Some(file) = self.files.get_mut(&id) {
            // Drop any chunks the cache held for this source: the
            // disk content has changed under us, so a fresh read
            // population must not return stale bytes.
            file.byte_cache.drop_source(file.source_id);
            let cached = file.rewrap_for_view(stream);
            match decision {
                ReloadDecision::DiscardEdits => file.editor.swap_source(cached),
                ReloadDecision::KeepEdits => file.editor.swap_source_keep_patch(cached),
                ReloadDecision::Ignore => unreachable!("handled above"),
            }
        }
        if let Some(watcher) = self.file_watcher.as_mut() {
            watcher.mark_synced(&path);
        }
        self.refresh_workspace_for_file(id);
        let kept = matches!(decision, ReloadDecision::KeepEdits);
        let summary = if kept {
            format!("reloaded {len} byte(s); kept local edits on top of new base ({display})")
        } else {
            format!("reloaded {len} byte(s); local edits discarded ({display})")
        };
        self.console_log(ConsoleSeverity::Info, &ctx_label, summary);
        // Re-run every source-derived analysis (template,
        // visualizer-via-template, strings, checksums, entropy)
        // against the freshly-swapped bytes. Templates always rerun;
        // the others gate on `AUTO_RUN_MAX_BYTES` plus prior use to
        // keep a reload from chewing tens of seconds of CPU on a
        // multi-GiB dump.
        cascade_byte_change(ctx, self, id);
        true
    }

    /// Re-mount the workspace whose editor is `file_id` (if any)
    /// against the file's freshly-swapped byte source. Walks the
    /// workspace's inner dock for `WorkspaceTab::Entry(_)` tabs;
    /// each surviving entry's bytes get re-read, each vanished
    /// entry stages an orphan-tab prompt the host renders next
    /// frame. No-op when the file isn't the editor of any
    /// workspace.
    fn refresh_workspace_for_file(&mut self, file_id: FileId) {
        let Some(workspace_id) = self.workspaces.values().find(|w| w.editor_id == file_id).map(|w| w.id) else {
            return;
        };
        let handler = match self.files.get(&file_id).and_then(|f| f.detected_handler.clone()) {
            Some(h) => h,
            None => {
                tracing::debug!(file_id = file_id.get(), "workspace reload: no detected handler; skipping re-mount");
                return;
            }
        };
        let new_source = match self.files.get(&file_id) {
            Some(f) => f.editor.source().clone(),
            None => return,
        };
        let new_mount = match handler.mount(new_source) {
            Ok(m) => Arc::new(m),
            Err(e) => {
                self.console_log(
                    ConsoleSeverity::Warning,
                    "Reload",
                    format!("re-mount {} after reload failed: {e}", handler.name()),
                );
                return;
            }
        };
        // Replace the workspace's mount; entry tabs still hold
        // their own byte source, so we re-fetch each one against
        // the new mount below.
        if let Some(workspace) = self.workspaces.get_mut(&workspace_id) {
            workspace.mount = new_mount.clone();
        }

        // Snapshot every Entry tab inside the workspace so we
        // don't hold a borrow into self.workspaces while mutating
        // self.files / self.pending_orphan_entries below.
        let entry_specs: Vec<(FileId, String)> = {
            let workspace = self.workspaces.get(&workspace_id).expect("just refreshed");
            workspace
                .dock
                .iter_all_tabs()
                .filter_map(|(_, t)| match t {
                    crate::files::WorkspaceTab::Entry(entry_id) => {
                        let file = self.files.get(entry_id)?;
                        let entry_path = match file.source_kind.as_ref()? {
                            TabSource::VfsEntry { entry_path, .. } => entry_path.clone(),
                            _ => return None,
                        };
                        Some((*entry_id, entry_path))
                    }
                    _ => None,
                })
                .collect()
        };
        for (entry_id, entry_path) in entry_specs {
            match crate::files::streaming::open_vfs(new_mount.clone(), entry_path.clone()) {
                Ok((stream, _len)) => {
                    let stream_for_watch = stream.clone();
                    if let Some(file) = self.files.get_mut(&entry_id) {
                        file.byte_cache.drop_source(file.source_id);
                        let cached = file.rewrap_for_view(stream);
                        file.editor.swap_source(cached);
                    }
                    // Refresh the sample-hash fingerprint so
                    // the next poll tick measures against the
                    // post-remount bytes rather than the stale
                    // pre-remount snapshot.
                    if let Some(watcher) = self.file_watcher.as_mut() {
                        watcher.mark_vfs_synced(entry_id, stream_for_watch);
                    }
                }
                Err(e) => {
                    let display = self.files.get(&entry_id).map(|f| f.display_name.clone()).unwrap_or_default();
                    tracing::debug!(error = %e, entry = %entry_path, "vfs entry vanished after reload");
                    self.pending_orphan_entries.push(PendingOrphanEntry {
                        file_id: entry_id,
                        display_name: display,
                        entry_path,
                    });
                }
            }
        }
    }

    /// Re-fire every completed template against `file_id` so the
    /// parsed trees stay in sync with the new bytes. Skips the file
    /// when nothing has completed yet, or when any run is still in
    /// flight (the worker hasn't seen the old bytes yet either, so
    /// rerunning would just duplicate work).
    pub(crate) fn rerun_template_for_file(&mut self, ctx: &egui::Context, file_id: FileId) {
        let Some(file) = self.files.get(&file_id) else { return };
        if file.templates.is_empty() || !file.templates_running.is_empty() {
            return;
        }
        // Snapshot path+range+overrides+fingerprint first because
        // run_template_from_path borrows `self` mutably and pushes
        // new `templates_running` entries which, on completion, would
        // replace the existing instances under the same id. Re-running
        // a stale instance means dropping it and starting fresh;
        // collect identities first, then drain. Carrying the previous
        // fingerprint + overrides through means a data-only reload
        // (template source unchanged) preserves the user's color picks.
        let to_rerun: Vec<(std::path::PathBuf, hxy_core::ByteRange, crate::templates::runner::RestoreContext)> = file
            .templates
            .iter()
            .map(|t| {
                (
                    t.source_path.clone(),
                    t.range,
                    crate::templates::runner::RestoreContext {
                        expected_fingerprint: t.source_fingerprint,
                        overrides: t.state.node_color_overrides.clone(),
                    },
                )
            })
            .collect();
        if let Some(file) = self.files.get_mut(&file_id) {
            file.templates.clear();
            file.active_template = None;
        }
        for (path, range, restore) in to_rerun {
            crate::templates::runner::run_template_from_path(ctx, self, file_id, path, Some(range), restore);
        }
    }

    /// Look at the just-opened file's extension + first bytes and
    /// raise a single template-prompt panel listing every plausible
    /// match. Multiple candidates render as rows in one anchored
    /// window; accepting any row dispatches that template and closes
    /// the panel.
    pub(super) fn suggest_templates_for(&mut self, id: FileId) {
        let Some(file) = self.files.get(&id) else { return };
        let extension = file.source_kind.as_ref().and_then(|s| s.leaf_extension());
        let source_len = file.editor.source().len().get();
        let window = source_len.min(crate::templates::library::DETECTION_WINDOW as u64);
        let head_bytes: Vec<u8> = if window == 0 {
            Vec::new()
        } else if let Ok(range) =
            hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(window))
        {
            file.editor.source().read(range).unwrap_or_default()
        } else {
            Vec::new()
        };
        // Pull every magic / extension hit (rank_entries puts hits
        // first, then trailing alphabetical filler -- we keep only
        // the prefix that actually matches).
        let candidates: Vec<crate::templates::library::TemplateEntry> = self
            .templates
            .rank_entries(extension.as_deref(), &head_bytes)
            .into_iter()
            .take_while(|entry| {
                let ext_match = extension.as_ref().is_some_and(|e| entry.extensions.iter().any(|x| x == e));
                let magic_match = !entry.magic.is_empty() && entry.magic.iter().any(|m| head_bytes.starts_with(m));
                ext_match || magic_match
            })
            .cloned()
            .collect();
        // Cap at three rows so the panel stays scannable on a
        // popular extension. The palette still surfaces the full
        // list for power users.
        let group = id.get();
        let entries: Vec<crate::toasts::TemplatePromptEntry> = candidates
            .into_iter()
            .take(3)
            .map(|entry| crate::toasts::TemplatePromptEntry {
                template_path: entry.path,
                name: entry.name,
                description: entry.description,
            })
            .collect();
        self.toasts.set_template_prompt(group, id, entries);
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
        source: Arc<dyn hxy_core::HexSource>,
        restore_selection: Option<hxy_core::Selection>,
        restore_scroll: Option<f32>,
    ) -> FileId {
        let id = self.fresh_file_id();
        let mut file = OpenFile::from_source(id, display_name, source_kind, source, &self.byte_cache);
        file.editor.set_selection(restore_selection);
        if let Some(s) = restore_scroll {
            file.editor.set_scroll_to(s);
        }
        // Pick up the user's chosen input style at construction.
        // Switching the global setting later walks every open file
        // and updates each editor; this just seeds new tabs.
        file.editor.set_input_mode(self.state.read().app.input_mode);

        // Detect a matching VFS handler against the first ~4 KiB.
        if let Ok(range) = hxy_core::ByteRange::new(
            hxy_core::ByteOffset::new(0),
            hxy_core::ByteOffset::new(file.editor.source().len().get().min(4096)),
        ) && let Ok(head) = file.editor.source().read(range)
        {
            file.detected_handler = self.registry.detect(&head);
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
                    crate::files::SuggestedTemplate { path: entry.path.clone(), display_name: entry.name.clone() }
                });
            }
        }

        self.files.insert(id, file);
        id
    }

    /// Try to open each saved tab. Filesystem tabs are read directly
    /// from disk; VFS-entry tabs require their parent tab to be open
    /// with a materialised mount. We sort tabs by `TabSource` depth so
    /// parents are restored before their children. Failures (file
    /// missing, parent failed to mount, entry path gone) drop the tab
    /// from the persisted list.
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
        // After every tab has been remounted to a live FileId /
        // WorkspaceId / MountId, replay the saved dock layout on top
        // so splits / sizes / focus / window state survive.
        self.apply_persisted_dock_layout();
        // Defer template auto-rerun to the first `update()` frame --
        // the runner needs an `egui::Context` for its result inbox,
        // which the builder can't supply. The flag is no-op when
        // there's nothing to rerun.
        self.pending_template_restore = self.state.read().open_tabs.iter().any(|t| !t.templates.is_empty());
    }

    /// Replay the previous session's running templates on this frame.
    /// Idempotent via `pending_template_restore`; the per-template
    /// fingerprint check inside [`crate::templates::runner::run_template_from_path`]
    /// drops persisted color overrides when the template source has
    /// changed on disk since the last save.
    fn restore_persisted_templates(&mut self, ctx: &egui::Context) {
        // Snapshot the work list so the per-template loop doesn't have
        // to keep re-acquiring `self.state` against the runner's own
        // writes.
        let sources: Vec<TabSource> =
            self.state.read().open_tabs.iter().filter(|t| !t.templates.is_empty()).map(|t| t.source.clone()).collect();
        for source in sources {
            self.restore_persisted_templates_for_source(ctx, &source);
        }
    }

    /// Replay one source's persisted templates (and visualizer flag)
    /// out of `state.open_tabs`. Shared by the launch-time restore
    /// loop (which calls it for every entry with non-empty templates)
    /// and the `Reopen Last Closed` path (which only wants to
    /// re-fire the just-restored tab, not every other open file's
    /// templates).
    pub(crate) fn restore_persisted_templates_for_source(&mut self, ctx: &egui::Context, source: &TabSource) {
        let (templates, active_idx, visualizer_open) = {
            let g = self.state.read();
            let Some(entry) = g.open_tabs.iter().find(|t| &t.source == source) else { return };
            (entry.templates.clone(), entry.active_template_idx, entry.visualizer_open)
        };
        if templates.is_empty() {
            return;
        }
        let Some(file_id) = self.files.iter().find(|(_, f)| f.source_kind.as_ref() == Some(source)).map(|(&id, _)| id)
        else {
            return;
        };
        // Skip files whose byte source is still being fetched in the
        // background -- running templates against the zero-byte
        // placeholder produces diagnostics-only instances. The
        // VFS-open inbox drain re-fires this helper for the
        // matching source once the real bytes land.
        if let Some(file) = self.files.get(&file_id)
            && matches!(file.load_status, crate::files::LoadStatus::Loading)
        {
            return;
        }
        // Restore the visualizer panel's open flag *before* the
        // template auto-rerun fires. Once the worker completes,
        // `auto_open_visualizer_for` consults this flag; without
        // the restore, the panel would always come up closed and
        // the user would have to reopen it every relaunch even
        // when they had it open last session.
        if let Some(file) = self.files.get_mut(&file_id) {
            file.visualizer_panel.open = visualizer_open;
        }
        // The tab's first-frame open already enqueued a "would
        // you like to run X.bt?" prompt via `suggest_templates_for`.
        // The user already picked a template last session (we're
        // about to auto-rerun it); nagging them again is wrong,
        // so dismiss the prompt before it gets a paint.
        self.toasts.dismiss_for_file(file_id);
        for t in &templates {
            let restore = crate::templates::runner::RestoreContext {
                expected_fingerprint: t.source_fingerprint,
                overrides: t.node_color_overrides.iter().map(|(&k, &v)| (k, v)).collect(),
            };
            crate::templates::runner::run_template_from_path(
                ctx,
                self,
                file_id,
                t.source_path.clone(),
                Some(t.range),
                restore,
            );
        }
        // The runner sets `active_template` to the most recently
        // queued instance; override it with the persisted choice
        // so the panel comes back focused on the same tab the
        // user closed it on.
        if let Some(idx) = active_idx
            && let Some(file) = self.files.get_mut(&file_id)
            && let Some(running) = file.templates_running.get(idx)
        {
            file.active_template = Some(running.id);
        }
    }
    pub(crate) fn restore_one_tab(
        &mut self,
        tab: &crate::state::OpenTabState,
        must_mount: bool,
    ) -> Result<(), crate::files::FileOpenError> {
        // A parent of any persisted VfsEntry must restore as a
        // workspace so the children can find a mount; user-saved
        // workspace state forces the same path.
        let as_workspace = tab.as_workspace || must_mount;
        match &tab.source {
            TabSource::Filesystem(path) => {
                let (source, _len) = crate::files::streaming::open_filesystem(path)
                    .map_err(|source| crate::files::FileOpenError::Read { path: path.clone(), source })?;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                self.open(name, Some(tab.source.clone()), source, tab.selection, Some(tab.scroll_offset), as_workspace);
                Ok(())
            }
            TabSource::VfsEntry { parent, entry_path } => {
                let Some(parent_mount) = self.find_mount_for_source(parent.as_ref()) else {
                    // Parent mount currently unavailable. If it's a
                    // plugin mount that landed in `Failed` state, the
                    // tab is preserved in `open_tabs` so it survives
                    // restart and a successful retry will fan out to
                    // open it; otherwise propagate the standard
                    // "parent missing" error so callers can drop it.
                    return if self.parent_mount_pending(parent.as_ref()) {
                        Ok(())
                    } else {
                        Err(crate::files::open::parent_missing(parent.as_ref()))
                    };
                };
                // Parent mount is Ready, but the entry-specific
                // metadata / open call can still fail or be slow --
                // xbox-neighborhood lazy-loads its module +
                // memory-region tables on first metadata for a
                // synthetic path, and a transient session hiccup
                // (kit was just powered on, network round trip
                // timing out, region list churned since last
                // session) bubbles up as an io::Error there.
                //
                // Push a zero-byte placeholder tab into the dock
                // immediately and spawn a worker that opens the
                // entry through the plugin mount on its own
                // thread. The result lands on `vfs_open_inbox`,
                // which the per-frame drain swaps into the
                // editor (success) or stamps as Failed (error).
                // Worker errors don't propagate up the restore
                // loop -- the tab stays put with its load
                // status surfaced in the tab strip + hex view.
                let name = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(entry_path).to_owned();
                let target = self
                    .workspace_for_source(parent.as_ref())
                    .map(OpenTarget::Workspace)
                    .unwrap_or(OpenTarget::Toplevel);
                let virtual_base_hint = parent_mount.virtual_base.as_ref().and_then(|q| q.virtual_base(entry_path));
                let placeholder: Arc<dyn hxy_core::HexSource> = Arc::new(hxy_core::MemorySource::new(Vec::new()));
                let opened_id = self.open_with_target(
                    name,
                    Some(tab.source.clone()),
                    placeholder,
                    tab.selection,
                    Some(tab.scroll_offset),
                    target,
                );
                record_virtual_base_hint(self, opened_id, virtual_base_hint);
                if let Some(file) = self.files.get_mut(&opened_id) {
                    file.load_status = crate::files::LoadStatus::Loading;
                }
                let sender = self.vfs_open_inbox.sender();
                crate::files::vfs_open::spawn(sender, opened_id, parent_mount, entry_path.clone());
                Ok(())
            }
            TabSource::Anonymous { id, title } => {
                let path =
                    crate::files::new::anonymous_file_path(*id).ok_or_else(|| crate::files::FileOpenError::Read {
                        path: std::path::PathBuf::from(format!("anonymous/{}", id.get())),
                        source: std::io::Error::other("no data dir"),
                    })?;
                let source: Arc<dyn hxy_core::HexSource> = match crate::files::streaming::open_filesystem(&path) {
                    Ok((s, _)) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // Sidecar gone; fall back to a fresh zero buffer
                        // so the tab still opens rather than dropping the
                        // entry silently.
                        Arc::new(hxy_core::MemorySource::new(vec![0u8; ANONYMOUS_DEFAULT_SIZE]))
                    }
                    Err(e) => {
                        return Err(crate::files::FileOpenError::Read { path, source: e });
                    }
                };
                self.open(
                    title.clone(),
                    Some(tab.source.clone()),
                    source,
                    tab.selection,
                    Some(tab.scroll_offset),
                    false,
                );
                Ok(())
            }
            TabSource::PluginMount { plugin_name, token, title } => {
                let plugin =
                    self.plugin_handlers.iter().find(|p| p.name() == plugin_name).cloned().ok_or_else(|| {
                        crate::files::FileOpenError::PluginMount {
                            plugin_name: plugin_name.clone(),
                            token: token.clone(),
                            reason: "plugin no longer installed".to_owned(),
                        }
                    })?;
                // Failures here are expected at restore (xbox offline,
                // network blocked, ...) -- preserve the tab as a
                // placeholder rather than dropping it. The user can
                // click the plugin-supplied retry button to re-invoke
                // `mount_by_token` later.
                let status = match plugin.mount_by_token(token) {
                    Ok(mount) => crate::files::MountStatus::Ready(Arc::new(mount)),
                    Err(e) => crate::files::MountStatus::Failed { message: e.message, retry_label: e.retry_label },
                };
                let mount_id = crate::files::MountId::new(self.next_mount_id);
                self.next_mount_id += 1;
                self.mounts.insert(
                    mount_id,
                    crate::files::MountedPlugin {
                        display_name: title.clone(),
                        plugin_name: plugin_name.clone(),
                        token: token.clone(),
                        status,
                    },
                );
                let _ = as_workspace; // plugin mount tabs always show the tree
                let _ = crate::tabs::dock_ops::push_tool_tab(&mut self.dock, Tab::PluginMount(mount_id));
                if let Some(path) = self.dock.find_tab(&Tab::PluginMount(mount_id)) {
                    crate::tabs::dock_ops::remove_welcome_from_leaf(&mut self.dock, path.surface, path.node);
                }
                Ok(())
            }
        }
    }

    /// Whether `source` references a plugin mount that currently
    /// exists in `self.mounts` but is in a [`crate::files::MountStatus::Failed`]
    /// state. Used by `restore_one_tab` to decide whether a missing
    /// VfsEntry parent is "deferred until retry succeeds" (preserve
    /// the tab) or "genuinely gone" (drop it).
    fn parent_mount_pending(&self, source: &TabSource) -> bool {
        let TabSource::PluginMount { plugin_name, token, .. } = source else { return false };
        self.mounts.values().any(|m| m.plugin_name == *plugin_name && m.token == *token && m.status.live().is_none())
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
            file.read_only_reason = Some(crate::files::ReadOnlyReason::VfsNoWriter);
            file.editor.set_edit_mode(crate::files::EditMode::Readonly);
        }
    }

    /// Locate the `MountedVfs` for the given source, regardless of
    /// where the mount lives -- workspace (file-rooted) or plugin
    /// (rootless). Returns `None` if no live mount currently provides
    /// that source. Plugin mounts only exist on desktop (the
    /// wasm-side runtime can't host wasmtime), but workspaces work
    /// everywhere -- so the function itself is universal and the
    /// `PluginMount` arm is the only desktop-only piece.
    pub(crate) fn find_mount_for_source(&self, source: &TabSource) -> Option<Arc<MountedVfs>> {
        match source {
            TabSource::PluginMount { plugin_name, token, .. } => self
                .mounts
                .values()
                .find(|m| m.plugin_name == *plugin_name && m.token == *token)
                .and_then(|m| m.status.live().cloned()),
            other => {
                let editor_id =
                    self.files.iter().find_map(|(id, f)| (f.source_kind.as_ref() == Some(other)).then_some(*id))?;
                self.workspaces.values().find(|w| w.editor_id == editor_id).map(|w| w.mount.clone())
            }
        }
    }

    /// Find the `WorkspaceId` whose editor file has the given source,
    /// if any. Used by VfsEntry restore to graft the entry into the
    /// parent's workspace's inner dock instead of opening it as a
    /// top-level tab.
    fn workspace_for_source(&self, source: &TabSource) -> Option<crate::files::WorkspaceId> {
        let editor_id =
            self.files.iter().find_map(|(id, f)| (f.source_kind.as_ref() == Some(source)).then_some(*id))?;
        self.workspaces.values().find(|w| w.editor_id == editor_id).map(|w| w.id)
    }

    /// `app.open` plus an explicit target: top-level dock leaf or a
    /// specific workspace's inner dock. Used by VfsEntry restore +
    /// runtime VFS-tree clicks to push entries inside their parent
    /// workspace rather than fragmenting them out as siblings.
    pub fn open_with_target(
        &mut self,
        display_name: impl Into<String>,
        source_kind: Option<TabSource>,
        source: Arc<dyn hxy_core::HexSource>,
        restore_selection: Option<hxy_core::Selection>,
        restore_scroll: Option<f32>,
        target: OpenTarget,
    ) -> FileId {
        match target {
            OpenTarget::Toplevel => {
                self.open(display_name, source_kind, source, restore_selection, restore_scroll, false)
            }
            OpenTarget::Workspace(workspace_id) => {
                let id =
                    self.create_open_file(display_name, source_kind.clone(), source, restore_selection, restore_scroll);
                self.apply_readonly_for_source(id);
                if let Some(workspace) = self.workspaces.get_mut(&workspace_id) {
                    crate::tabs::dock_ops::push_workspace_entry(workspace, id);
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
                            templates: Vec::new(),
                            active_template_idx: None,
                            visualizer_open: false,
                            virtual_base_choice: None,
                        });
                    }
                }
                self.suggest_templates_for(id);
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
        if let Some(sink) = &self.sink {
            if let Err(e) = sink.save(&after) {
                tracing::warn!(error = %e, "save persisted state");
            } else {
                self.last_saved_window = Some(after.window);
            }
        }
    }

    /// Snapshot the live outer dock + every workspace's inner dock
    /// into [`crate::state::PersistedState::dock_layout_json`].
    /// Compares against the previous JSON before writing so the
    /// per-frame [`Self::save_if_dirty`] check correctly elides a
    /// disk write when nothing actually changed.
    fn snapshot_dock_layout(&mut self) {
        let snapshot = crate::tabs::persisted_dock::live_to_persisted(
            &self.dock,
            &self.workspaces,
            &self.files,
            &self.mounts,
            &self.compares,
        );
        let json = match serde_json::to_string(&snapshot) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "serialize dock layout -- skipping snapshot");
                return;
            }
        };
        let mut g = self.state.write();
        if g.dock_layout_json.as_deref() != Some(json.as_str()) {
            g.dock_layout_json = Some(json);
        }
    }

    /// Translate the most recently loaded
    /// [`crate::state::PersistedState::dock_layout_json`] back into
    /// live dock state and replace the freshly-restored default
    /// layout. No-op if the blob is absent, malformed, or carries
    /// an unknown schema version -- in any of those cases the host
    /// keeps the layout that [`Self::restore_open_tabs`] just built
    /// from `open_tabs` alone.
    fn apply_persisted_dock_layout(&mut self) {
        let json = match self.state.read().dock_layout_json.clone() {
            Some(j) => j,
            None => return,
        };
        let snapshot: crate::tabs::persisted_dock::PersistedDock = match serde_json::from_str(&json) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "decode dock layout -- keeping default layout");
                return;
            }
        };
        if snapshot.schema_version != crate::tabs::persisted_dock::SCHEMA_VERSION {
            tracing::info!(
                version = snapshot.schema_version,
                expected = crate::tabs::persisted_dock::SCHEMA_VERSION,
                "dock layout schema mismatch -- keeping default layout"
            );
            return;
        }
        let files_by_source: HashMap<TabSource, FileId> =
            self.files.iter().filter_map(|(id, f)| f.source_kind.clone().map(|s| (s, *id))).collect();
        let workspaces_by_parent: HashMap<TabSource, crate::files::WorkspaceId> = self
            .workspaces
            .iter()
            .filter_map(|(id, ws)| {
                let parent = self.files.get(&ws.editor_id)?.source_kind.clone()?;
                Some((parent, *id))
            })
            .collect();
        let mounts_by_token: HashMap<(String, String), crate::files::MountId> =
            self.mounts.iter().map(|(id, m)| ((m.plugin_name.clone(), m.token.clone()), *id)).collect();
        // Re-spawn every compare session referenced by the saved
        // dock before translating the layout, so the translation
        // can resolve `PersistedTab::Compare` to a live id. Compare
        // tabs whose source bytes can't be read this launch (file
        // deleted, parent VFS gone) drop out -- the layout's
        // surrounding splits / sizes survive without them.
        let compares_by_sources = self.respawn_persisted_compares(&snapshot);
        let maps = crate::tabs::persisted_dock::RestoreMaps {
            files_by_source: &files_by_source,
            workspaces_by_parent: &workspaces_by_parent,
            mounts_by_token: &mounts_by_token,
            compares_by_sources: &compares_by_sources,
        };
        let (outer, inner_by_id) = crate::tabs::persisted_dock::persisted_to_live(&snapshot, &maps);
        self.dock = outer;
        for (ws_id, inner_dock) in inner_by_id {
            if let Some(ws) = self.workspaces.get_mut(&ws_id) {
                ws.dock = inner_dock;
            }
        }
    }

    /// Walk `snapshot` for [`PersistedTab::Compare`] entries, read
    /// fresh bytes for each side, and register a live
    /// [`crate::compare::CompareSession`]. Returns a lookup map
    /// keyed by the `(a, b)` source pair so the dock translation
    /// can resolve persisted compare tabs to live ids.
    fn respawn_persisted_compares(
        &mut self,
        snapshot: &crate::tabs::persisted_dock::PersistedDock,
    ) -> HashMap<(TabSource, TabSource), crate::compare::CompareId> {
        let mut out = HashMap::new();
        for (_, tab) in snapshot.outer.iter_all_tabs() {
            let crate::tabs::persisted_dock::PersistedTab::Compare { a, b } = tab else { continue };
            let key = (a.clone(), b.clone());
            if out.contains_key(&key) {
                continue;
            }
            match self.spawn_compare_from_sources(a.clone(), b.clone()) {
                Ok(id) => {
                    out.insert(key, id);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "restore compare tab -- dropping from layout");
                }
            }
        }
        out
    }

    /// Read bytes for both sides of a persisted compare and spawn
    /// a fresh [`crate::compare::CompareSession`]. Filesystem
    /// sources are read directly; VFS-entry sources read through
    /// the parent mount (which `restore_open_tabs` has already
    /// remounted).
    pub(crate) fn spawn_compare_from_sources(
        &mut self,
        a: TabSource,
        b: TabSource,
    ) -> Result<crate::compare::CompareId, crate::compare::picker::CompareSpawnError> {
        let a_picked = self.read_tab_source_bytes(&a)?;
        let b_picked = self.read_tab_source_bytes(&b)?;
        let id = crate::compare::CompareId::new(self.next_compare_id);
        self.next_compare_id += 1;
        let session = crate::compare::CompareSession::new(
            id,
            crate::compare::ComparePane::from_bytes(a_picked.name, Some(a), a_picked.bytes),
            crate::compare::ComparePane::from_bytes(b_picked.name, Some(b), b_picked.bytes),
        );
        // Initial diff fires async via the per-frame debounce path
        // when the tab next renders -- no ctx is available here
        // (we're inside the restore pass that runs before the
        // first frame).
        self.compares.insert(id, session);
        Ok(id)
    }

    /// Read whatever a [`TabSource`] resolves to as a byte buffer
    /// for compare's purposes. Filesystem reads from disk, VFS
    /// entries route through the parent mount.
    fn read_tab_source_bytes(
        &self,
        source: &TabSource,
    ) -> Result<RestoredCompareSide, crate::compare::picker::CompareSpawnError> {
        match source {
            TabSource::Filesystem(path) => {
                let bytes = std::fs::read(path).map_err(|e| crate::compare::picker::CompareSpawnError::ReadFile {
                    path: path.clone(),
                    source: e,
                })?;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                Ok(RestoredCompareSide { name, bytes })
            }
            TabSource::VfsEntry { parent, entry_path } => {
                let mount = self.find_mount_for_source(parent.as_ref()).ok_or_else(|| {
                    crate::compare::picker::CompareSpawnError::ReadOpenFile("parent VFS mount missing".to_owned())
                })?;
                let bytes = crate::files::open::read_vfs_entry(&*mount.fs, entry_path)
                    .map_err(|e| crate::compare::picker::CompareSpawnError::ReadOpenFile(e.to_string()))?;
                let name = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(entry_path).to_owned();
                Ok(RestoredCompareSide { name, bytes })
            }
            TabSource::Anonymous { .. } | TabSource::PluginMount { .. } => {
                Err(crate::compare::picker::CompareSpawnError::ReadOpenFile(format!(
                    "unsupported compare source: {source:?}"
                )))
            }
        }
    }
}

/// Bytes + display name produced by [`HxyApp::read_tab_source_bytes`].
struct RestoredCompareSide {
    name: String,
    bytes: Vec<u8>,
}
impl crate::plugins::runner::Logger for HxyApp {
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
        self.drain_pending_plugin_ops(ui.ctx());

        // Push the user's polling preferences into the watcher
        // so any settings-tab nudge takes effect on the very
        // next tick. Idempotent when nothing changed.
        if let Some(watcher) = self.file_watcher.as_mut() {
            let prefs = polling_prefs_from_settings(&self.state.read().app);
            watcher.set_polling(prefs);
        }

        // Pull queued filesystem-change notifications off the
        // notify watcher + polling worker and route each one
        // through the reload prompt / auto-reload paths.
        drain_file_watch_events(ui.ctx(), self);

        // First-frame auto-rerun of every persisted template the
        // previous session left running. Cleared after one shot;
        // see `restore_persisted_templates` for the semantics.
        if self.pending_template_restore {
            self.pending_template_restore = false;
            self.restore_persisted_templates(ui.ctx());
        }

        #[cfg(target_os = "macos")]
        drain_native_menu(ui.ctx(), self);
        #[cfg(target_os = "macos")]
        sync_native_menu_state(self);

        #[cfg(not(target_os = "macos"))]
        top_menu_bar(ui, self);

        // Capture the active file id before the dock pass so the
        // Inspector tab arm can render against `self.files` (it does
        // its own caret-window read at render time, with disjoint
        // borrows on the viewer struct).
        let active_file_id = super::active_file_id(self);
        // Recompute clicks fired by entropy panels during this
        // frame's dock pass land here. Each panel pushes its
        // pinned FileId; we drain the list after the dock
        // borrow releases.
        let mut entropy_recompute: Vec<FileId> = Vec::new();
        // Visualizer-panel close clicks land here for the same
        // reason; drained after the dock pass to remove the
        // matching dock tab + record the sticky-dismiss flag.
        let mut pending_visualizer_dismiss: Vec<FileId> = Vec::new();
        // Strings panel "Run" clicks queue here (re-runs the
        // extractor against the panel's current config), and offset-
        // link clicks queue (FileId, offset, end) tuples for the
        // hex-view jump dispatch.
        let mut pending_strings_run: Vec<FileId> = Vec::new();
        let mut pending_strings_jump: Vec<(FileId, u64, u64)> = Vec::new();
        // Checksum panel "Run" clicks + clipboard requests.
        let mut pending_checksums_run: Vec<FileId> = Vec::new();
        let mut pending_checksums_copy: Vec<String> = Vec::new();

        {
            // Snapshot fields that the viewer needs but that live on
            // `self.state` BEFORE taking the write guard -- otherwise
            // `self.state.read()` inside the struct literal deadlocks
            // against the outer write guard (parking_lot RwLock is not
            // reentrant).
            let patterns_installed_hash_snapshot = self.state.read().app.imhex_patterns.installed_hash.clone();
            let mut state_guard = self.state.write();
            let mut viewer = desktop_tab_viewer::HxyTabViewer {
                files: &mut self.files,
                state: &mut state_guard,
                compares: &mut self.compares,
                console: &self.console,
                mounts: &self.mounts,
                pending_close_mount: &mut self.pending_close_mount,
                global_search: &mut self.global_search,
                pending_global_search_events: &mut self.pending_global_search_events,
                inspector: &mut self.inspector,
                decoders: &self.decoders,
                active_file_id,
                plugin_rescan: &mut self.plugin_rescan,
                plugin_handlers: &self.plugin_handlers,
                pending_plugin_events: &mut self.pending_plugin_events,
                patterns_installed_hash: patterns_installed_hash_snapshot,
                patterns_in_flight_bytes: self.pattern_in_flight_bytes,
                pending_close_tab: &mut self.pending_close_tab,
                tab_focus: &mut self.tab_focus,
                workspaces: &mut self.workspaces,
                pending_close_workspace_entry: &mut self.pending_close_workspace_entry,
                pending_collapse_workspace: &mut self.pending_collapse_workspace,
                toasts: &mut self.toasts,
                pending_template_runs: &mut self.pending_template_runs,
                entropy_recompute: &mut entropy_recompute,
                pending_visualizer_dismiss: &mut pending_visualizer_dismiss,
                pending_strings_run: &mut pending_strings_run,
                pending_strings_jump: &mut pending_strings_jump,
                pending_checksums_run: &mut pending_checksums_run,
                pending_checksums_copy: &mut pending_checksums_copy,
                byte_cache: &self.byte_cache,
            };
            let style = crate::style::hxy_dock_style(ui.style());
            DockArea::new(&mut self.dock).style(style).show_leaf_collapse_buttons(false).show_inside(ui, &mut viewer);
        }

        // Drain panel-level recompute clicks. Done after the
        // dock borrow releases so we can mutate `app.files`
        // freely. Multiple entropy panels can fire in the same
        // frame; each one targets its own pinned FileId.
        for file_id in std::mem::take(&mut entropy_recompute) {
            compute_entropy_for(ui.ctx(), self, file_id);
        }
        // Strings panel "Run" + offset-link clicks. The Run path
        // re-fires the extractor against the panel's current config
        // (range, encoding, min length the user just edited inline);
        // the Jump path drives the file's hex-view selection so the
        // matched bytes are visible and selected.
        for file_id in std::mem::take(&mut pending_strings_run) {
            spawn_strings_with_panel_config(ui.ctx(), self, file_id);
        }
        for (file_id, offset, end) in std::mem::take(&mut pending_strings_jump) {
            jump_to_strings_match(self, file_id, offset, end);
        }
        // Checksum panel "Run" + Copy. Run uses the panel's current
        // config (algorithm set + range) and re-fires the worker;
        // Copy puts the formatted hex on the clipboard.
        for file_id in std::mem::take(&mut pending_checksums_run) {
            spawn_checksums_with_panel_config(ui.ctx(), self, file_id);
        }
        for text in std::mem::take(&mut pending_checksums_copy) {
            ui.ctx().copy_text(text);
        }

        // Visualizer panel header X-clicks: remove the dock tab
        // and clear the user's "open" flag so a re-run on the same
        // file doesn't pop the panel back. Persisted so the closure
        // also survives a restart.
        for file_id in std::mem::take(&mut pending_visualizer_dismiss) {
            if let Some(path) = self.dock.find_tab(&Tab::Visualizer(file_id)) {
                let _ = self.dock.remove_tab(path);
            }
            crate::tabs::close::set_visualizer_open(self, file_id, false);
        }
        // In-row visualizer-icon clicks: pop or focus the panel
        // for each file whose template-panel handler set the
        // pending_show flag. The handler also wrote the active
        // node into `panel.active`, so the next render lands on
        // the right sub-tab.
        {
            let to_show: Vec<FileId> = self
                .files
                .iter_mut()
                .filter_map(
                    |(id, file)| {
                        if std::mem::take(&mut file.visualizer_panel.pending_show) { Some(*id) } else { None }
                    },
                )
                .collect();
            for id in to_show {
                self.show_visualizer_for(id);
            }
        }
        {
            let events = std::mem::take(&mut self.pending_plugin_events);
            if !events.is_empty() {
                self.apply_plugin_events(events);
            }
        }
        crate::tabs::dock_ops::track_content_leaf(self);
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
            crate::tabs::close::collapse_workspace_to_file(self, workspace_id);
        }

        // Empty dock = blank-canvas frame next render. Push Welcome
        // back so the user always has *something* to look at, both to
        // give them a starting point for the next action and so they
        // don't think the app froze.
        if self.dock.iter_all_tabs().next().is_none() {
            self.dock.push_to_focused_leaf(Tab::Welcome);
        }
        {
            let events = std::mem::take(&mut self.pending_global_search_events);
            if !events.is_empty() {
                apply_global_search_events(self, events);
            }
        }
        if std::mem::take(&mut self.plugin_rescan) {
            self.reload_plugins();
        }

        apply_zoom_change(ui.ctx(), &self.state, &mut self.applied_zoom);

        capture_window_on_drag_end(ui.ctx(), &self.state, &mut self.prev_window, &self.last_saved_window);

        paint_drop_overlay(ui.ctx());
        consume_dropped_files(ui.ctx(), self);
        consume_welcome_open_request(ui.ctx(), self);
        drain_pending_vfs_opens(ui.ctx(), self);
        crate::plugins::mount::drain_pending_mount_retries(ui.ctx(), self);
        drain_external_open_requests(ui.ctx(), self);
        crate::templates::runner::drain_template_runs(ui.ctx(), self);
        drain_entropy_runs(ui.ctx(), self);
        drain_strings_runs(ui.ctx(), self);
        drain_checksums_runs(ui.ctx(), self);
        drain_byte_change_cascade(ui.ctx(), self);
        drain_vfs_open_inbox(ui.ctx(), self);
        // Visual pane picker takes priority over the palette and
        // any other keyboard consumer: while a pick is staged it
        // owns Escape (cancel) and a..z (target letters). It runs
        // after the dock has rendered so leaf rects are this
        // frame's, not last frame's.
        crate::tabs::focus::handle_pane_pick(ui.ctx(), self);
        // Palette runs first so it gets first crack at keyboard
        // events. egui clears focus on plain Escape during its own
        // event preprocessing, so egui_wants_keyboard_input() reads
        // false by the time dispatch_hex_edit_keys runs -- if the
        // hex editor ran first it would drain Escape for its own
        // clear-selection handler before the palette could use it
        // to dismiss.
        handle_command_palette(ui.ctx(), self);
        crate::app::shortcuts::dispatch_copy_shortcut(ui.ctx(), self);
        crate::app::shortcuts::dispatch_save_shortcut(ui.ctx(), self);
        crate::tabs::close::dispatch_close_shortcut(ui.ctx(), self);
        crate::app::shortcuts::dispatch_paste_shortcut(ui.ctx(), self);
        crate::app::shortcuts::dispatch_find_shortcut(ui.ctx(), self);
        crate::app::shortcuts::dispatch_jump_field_shortcut(ui.ctx(), self);
        crate::tabs::focus::dispatch_focus_pane_shortcut(ui.ctx(), self);
        crate::tabs::focus::dispatch_tab_focus_toggle(ui.ctx(), self);
        crate::tabs::focus::dispatch_tab_cycle(ui.ctx(), self);
        crate::app::shortcuts::dispatch_hex_edit_keys(ui.ctx(), self);
        crate::app::dialogs::render_duplicate_open_dialog(ui.ctx(), self);
        crate::app::dialogs::render_patch_restore_dialog(ui.ctx(), self);
        crate::app::dialogs::render_reload_prompt_dialog(ui.ctx(), self);
        crate::app::dialogs::render_virtual_base_prompt_dialog(ui.ctx(), self);
        crate::app::dialogs::render_open_with_options_dialog(ui.ctx(), self);
        crate::app::dialogs::render_orphaned_entry_dialog(ui.ctx(), self);
        crate::files::snapshot_ui::render_snapshot_dialog(ui.ctx(), self);
        crate::tabs::close::render_close_tab_dialog(ui.ctx(), self);
        {
            crate::search::modal::drain_search_effects(self);
            crate::search::modal::render_search_modal(ui.ctx(), self);
            crate::compare::picker::render_compare_picker(ui.ctx(), self);
            crate::app::dialogs::render_imhex_patterns_first_run(ui.ctx(), self);
            crate::app::dialogs::pump_pattern_fetch(ui.ctx(), self);
            self.toasts.show_toasts(ui.ctx());
            crate::templates::runner::drain_pending_template_runs(ui.ctx(), self);
        }
        self.snapshot_dock_layout();
        self.save_if_dirty(&snapshot_before);
    }

    fn on_exit(&mut self) {
        // Persist every dirty tab's patch to a sidecar so restart
        // can offer to restore it. Best-effort: errors only log.
        if let Some(dir) = crate::files::save::unsaved_edits_dir() {
            for file in self.files.values() {
                let Some(path) = file.root_path().cloned() else { continue };
                if !file.editor.is_dirty() {
                    // Clear any lingering sidecar from a previous session
                    // -- the in-memory state for this file is clean now.
                    let _ = crate::files::patch_persist::discard(&dir, &path);
                    continue;
                }
                let patch = file.editor.patch().read().expect("patch lock poisoned").clone();
                let Some(sidecar) = crate::files::patch_persist::snapshot(
                    path.clone(),
                    file.editor.source().as_ref(),
                    patch,
                    file.editor.undo_stack().to_vec(),
                    file.editor.redo_stack().to_vec(),
                ) else {
                    continue;
                };
                if let Err(e) = crate::files::patch_persist::store(&dir, &sidecar) {
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
            let Some(path) = crate::files::new::anonymous_file_path(*id) else { continue };
            let len = file.editor.source().len().get();
            let bytes = if len == 0 {
                Vec::new()
            } else {
                let range = match hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len))
                {
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
