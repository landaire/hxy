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

pub struct HxyApp {
    dock: DockState<Tab>,
    files: HashMap<FileId, OpenFile>,
    state: SharedPersistedState,
    next_file_id: u64,
    registry: VfsRegistry,
    #[cfg(not(target_arch = "wasm32"))]
    template_plugins: Vec<Arc<dyn hxy_plugin_host::TemplateRuntime>>,
    commands: Vec<Box<dyn crate::commands::ToolbarCommand>>,

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
    /// Native macOS menu bar. `None` until the app is constructed on
    /// the main thread. Dropping it tears the NSMenu down.
    #[cfg(target_os = "macos")]
    menu: Option<crate::menu::MenuState>,
    /// Set by the Plugins tab when the user installs or deletes a
    /// file in the plugin directories. Drained at end of `ui()`.
    #[cfg(not(target_arch = "wasm32"))]
    plugin_rescan: bool,
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
        register_user_plugins(&mut registry);
        #[cfg(not(target_arch = "wasm32"))]
        let template_plugins = load_user_template_plugins();
        Self {
            dock: DockState::new(vec![Tab::Welcome, Tab::Settings]),
            files: HashMap::new(),
            state,
            next_file_id: 1,
            registry,
            #[cfg(not(target_arch = "wasm32"))]
            template_plugins,
            commands: crate::commands::default_commands(),
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
            #[cfg(target_os = "macos")]
            menu: Some(crate::menu::MenuState::install()),
            #[cfg(not(target_arch = "wasm32"))]
            plugin_rescan: false,
            #[cfg(not(target_arch = "wasm32"))]
            templates: crate::template_library::TemplateLibrary::load_from(user_templates_dir().as_deref()),
            #[cfg(not(target_arch = "wasm32"))]
            palette: crate::command_palette::PaletteState::default(),
        }
    }

    /// Rebuild the VFS registry + template runtime list from the
    /// user's plugin directories. Called by the Plugins tab after the
    /// user installs or deletes a file.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn reload_plugins(&mut self) {
        let mut registry = VfsRegistry::new();
        registry.register(Arc::new(ZipHandler::new()));
        register_user_plugins(&mut registry);
        self.registry = registry;
        self.template_plugins = load_user_template_plugins();
        self.templates = crate::template_library::TemplateLibrary::load_from(user_templates_dir().as_deref());
    }

    /// Show the Plugins tab. Focuses if already open; otherwise splits
    /// to the right of the main dock area like the other side panels.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn show_plugins(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Plugins) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        self.dock.main_surface_mut().split_right(egui_dock::NodeIndex::root(), 0.72, vec![Tab::Plugins]);
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
        let Some(path) = self.dock.find_tab(&Tab::File(file_id)) else { return };
        let node_path = path.node_path();
        // Ignore errors -- the worst case is the tab isn't focused.
        let _ = self.dock.set_active_tab(path);
        self.dock.set_focused_node_and_surface(node_path);
    }

    /// Open a new file tab with the given display name, persistent
    /// source identity, and byte contents. Runs format detection
    /// against the source's first bytes and caches the matching handler
    /// (if any) on the tab so the toolbar command can enable itself.
    /// When `restore_show_vfs_tree` is true and a handler matches, the
    /// source is mounted and the tree panel opens immediately -- used by
    /// restore-on-launch so children of mounted archives can find their
    /// parent mount.
    pub fn open(
        &mut self,
        display_name: impl Into<String>,
        source_kind: Option<TabSource>,
        bytes: Vec<u8>,
        restore_selection: Option<hxy_core::Selection>,
        restore_scroll: Option<f32>,
        restore_show_vfs_tree: bool,
    ) -> FileId {
        let id = self.fresh_file_id();
        let mut file = OpenFile::from_bytes(id, display_name, source_kind.clone(), bytes);
        file.editor.set_selection(restore_selection);        if let Some(s) = restore_scroll {
            file.editor.set_scroll_to(s);        }

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

        if restore_show_vfs_tree && let Some(handler) = file.detected_handler.clone() {
            match handler.mount(file.editor.source().clone()) {
                Ok(mount) => {
                    file.mount = Some(Arc::new(mount));
                    file.show_vfs_tree = true;
                }
                Err(e) => tracing::warn!(error = %e, handler = handler.name(), "restore mount"),
            }
        }

        self.files.insert(id, file);
        self.dock.push_to_focused_leaf(Tab::File(id));

        // Look for an unsaved-edits sidecar from a previous session
        // and offer it back to the user. The actual restore happens
        // after the modal returns; this just stages the prompt.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(TabSource::Filesystem(path)) = source_kind.as_ref()
            && let Some(dir) = unsaved_edits_dir()
        {
            match crate::patch_persist::load(&dir, path) {
                Ok(Some(sidecar)) => {
                    // Surface every sidecar; the modal reports the
                    // integrity status so the user can decide to
                    // restore, restore-anyway, or discard.
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
                    show_vfs_tree: restore_show_vfs_tree,
                });
            }
        }
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
        let show_tree = tab.show_vfs_tree || must_mount;
        match &tab.source {
            TabSource::Filesystem(path) => {
                let bytes = std::fs::read(path)
                    .map_err(|source| crate::file::FileOpenError::Read { path: path.clone(), source })?;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                self.open(name, Some(tab.source.clone()), bytes, tab.selection, Some(tab.scroll_offset), show_tree);
                Ok(())
            }
            TabSource::VfsEntry { parent, entry_path } => {
                // Parent must already exist as an open tab with a mount.
                let parent_file_id = self
                    .files
                    .iter()
                    .find_map(|(id, f)| (f.source_kind.as_ref() == Some(parent.as_ref())).then_some(*id))
                    .ok_or_else(|| parent_missing(parent.as_ref()))?;
                let parent_mount = self
                    .files
                    .get(&parent_file_id)
                    .and_then(|f| f.mount.clone())
                    .ok_or_else(|| parent_missing(parent.as_ref()))?;
                let bytes = read_vfs_entry(&*parent_mount.fs, entry_path)
                    .map_err(|e| crate::file::FileOpenError::Read { path: entry_path.into(), source: e })?;
                let name = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(entry_path).to_owned();
                self.open(name, Some(tab.source.clone()), bytes, tab.selection, Some(tab.scroll_offset), show_tree);
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

impl eframe::App for HxyApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snapshot_before = self.state.read().clone();

        #[cfg(target_os = "macos")]
        drain_native_menu(ui.ctx(), self);
        #[cfg(target_os = "macos")]
        sync_native_menu_state(self);

        #[cfg(not(target_os = "macos"))]
        top_menu_bar(ui, self);
        render_toolbar_and_apply(ui, self);

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
                inspector: &mut self.inspector,
                #[cfg(not(target_arch = "wasm32"))]
                decoders: &self.decoders,
                #[cfg(not(target_arch = "wasm32"))]
                inspector_data,
                #[cfg(not(target_arch = "wasm32"))]
                plugin_rescan: &mut self.plugin_rescan,
            };
            let style = Style::from_egui(ui.style());
            DockArea::new(&mut self.dock)
                .style(style)
                .show_leaf_collapse_buttons(false)
                .show_inside(ui, &mut viewer);
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
        drain_template_runs(ui.ctx(), self);
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
        dispatch_paste_shortcut(ui.ctx(), self);
        dispatch_hex_edit_keys(ui.ctx(), self);
        render_duplicate_open_dialog(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        render_patch_restore_dialog(ui.ctx(), self);

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
    let had_copy = input.events.iter().any(|e| matches!(e, egui::Event::Copy));
    if had_copy {
        input.events.retain(|e| !matches!(e, egui::Event::Copy));
        return true;
    }
    input.consume_shortcut(&COPY_BYTES)
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

#[cfg(not(target_arch = "wasm32"))]
fn drain_pending_vfs_opens(ctx: &egui::Context, app: &mut HxyApp) {
    let pending: Vec<(FileId, String)> = ctx
        .data_mut(|d| d.remove_temp::<Vec<(FileId, String)>>(egui::Id::new(PENDING_VFS_OPEN_KEY)))
        .unwrap_or_default();
    for (parent_id, entry_path) in pending {
        let Some(parent) = app.files.get(&parent_id) else { continue };
        let parent_source = parent.source_kind.clone();
        let Some(parent_source) = parent_source else { continue };
        let Some(mount) = parent.mount.clone() else { continue };
        let bytes = match read_vfs_entry(&*mount.fs, &entry_path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, entry = %entry_path, "open vfs entry");
                continue;
            }
        };
        let name = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&entry_path).to_owned();
        let source = TabSource::VfsEntry { parent: Box::new(parent_source), entry_path };
        app.open(name, Some(source), bytes, None, None, false);
    }
}

#[cfg(target_arch = "wasm32")]
fn drain_pending_vfs_opens(_ctx: &egui::Context, _app: &mut HxyApp) {}

fn render_toolbar_and_apply(ui: &mut egui::Ui, app: &mut HxyApp) {
    use crate::commands::CommandEffect;
    use crate::commands::ToolbarCtx;

    let active_file_id = active_file_id(app);
    // Resolve styles + icon font once outside the borrow so the command
    // trait objects can read them via the context below.
    let mut effects: Vec<CommandEffect> = Vec::new();

    // Snapshot the command list off `app` so we can borrow other fields
    // of `app` through `ToolbarCtx`. Commands are `Send + Sync` and are
    // owned trait objects -- moving them out and back is cheap (they're
    // zero-size types in practice).
    let commands = std::mem::take(&mut app.commands);

    egui::Panel::top("hxy_toolbar")
        .resizable(false)
        .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(6, 4)))
        .show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                let mut state_guard = app.state.write();
                let active_file = active_file_id.and_then(|id| app.files.get_mut(&id));
                let ctx_handle = ui.ctx().clone();
                let mut cx = ToolbarCtx {
                    ctx: &ctx_handle,
                    state: &mut state_guard,
                    active_file,
                    active_file_id,
                    effects: &mut effects,
                };
                for cmd in &commands {
                    let enabled = cmd.enabled(&cx);
                    let label = cmd.label(&cx);
                    let icon = cmd.icon();
                    let btn = egui::Button::new(egui::RichText::new(icon).size(16.0)).frame(false);
                    let r = ui.add_enabled(enabled, btn).on_hover_text(&label);
                    if r.clicked() {
                        cmd.invoke(&mut cx);
                    }
                }
            });
        });

    app.commands = commands;

    for effect in effects {
        apply_command_effect(ui.ctx(), app, effect);
    }
}

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

/// Split the target leaf in `dir`, duplicating the focused tab into
/// the new pane. The new leaf becomes focused so follow-up commands
/// (navigation, another split) target it.
fn dock_split_focused(app: &mut HxyApp, dir: crate::commands::DockDir) {
    use crate::commands::DockDir;
    let Some(path) = resolve_target_leaf(app) else { return };
    let tab = match &app.dock[path.surface][path.node] {
        egui_dock::Node::Leaf(leaf) => leaf.tabs.first().cloned(),
        _ => None,
    };
    let Some(tab) = tab else { return };
    let tree = &mut app.dock[path.surface];
    let [_, new_node] = match dir {
        DockDir::Right => tree.split_right(path.node, 0.5, vec![tab]),
        DockDir::Left => tree.split_left(path.node, 0.5, vec![tab]),
        DockDir::Up => tree.split_above(path.node, 0.5, vec![tab]),
        DockDir::Down => tree.split_below(path.node, 0.5, vec![tab]),
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
    let Some(target) = find_neighbor_leaf(tree, path.node, dir) else { return };
    if target == path.node {
        return;
    }
    let tree = &mut app.dock[path.surface];
    let tabs: Vec<_> = match &mut tree[path.node] {
        egui_dock::Node::Leaf(leaf) => std::mem::take(&mut leaf.tabs),
        _ => return,
    };
    if tabs.is_empty() {
        return;
    }
    // Stash one of the tabs we're about to move so we can find
    // the destination leaf again after remove_leaf -- it rewires
    // node indices, so `target` is not safe to index into the tree
    // after the remove. Looking it up by tab is robust.
    let refocus_tab = tabs[0];
    for tab in tabs {
        tree[target].append_tab(tab);
    }
    tree.remove_leaf(path.node);
    if let Some(found) = app.dock.find_tab(&refocus_tab) {
        app.dock.set_focused_node_and_surface(egui_dock::NodePath { surface: found.surface, node: found.node });
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

fn mount_active_file(app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else { return };
    let Some(file) = app.files.get_mut(&id) else { return };
    if file.mount.is_some() {
        file.show_vfs_tree = true;
        return;
    }
    let Some(handler) = file.detected_handler.clone() else { return };
    match handler.mount(file.editor.source().clone()) {
        Ok(mount) => {
            file.mount = Some(Arc::new(mount));
            file.show_vfs_tree = true;
        }
        Err(e) => tracing::warn!(error = %e, handler = handler.name(), "mount vfs"),
    }
}

fn render_file_tab(ui: &mut egui::Ui, id: FileId, file: &mut OpenFile, state: &mut PersistedState) {
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
                status_bar_ui(ui, file, settings_base, &mut new_base);
            });
        });

    let body_rect = ui.available_rect_before_wrap();
    ui.painter().hline(
        tab_rect.x_range(),
        body_rect.bottom(),
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );

    let mut open_entries: Vec<String> = Vec::new();
    if file.mount.is_some() && file.show_vfs_tree {
        egui::Panel::left(egui::Id::new(("hxy-vfs-panel", id.get())))
            .resizable(true)
            .default_size(220.0)
            .min_size(140.0)
            .show_inside(ui, |ui| {
                render_tree_panel_header(ui, &mut file.show_vfs_tree);
                ui.separator();
                if let Some(mount) = file.mount.clone() {
                    let events = crate::vfs_panel::show(ui, id.get(), &*mount.fs);
                    for e in events {
                        let crate::vfs_panel::VfsPanelEvent::OpenEntry(path) = e;
                        open_entries.push(path);
                    }
                }
            });
    }

    #[cfg(not(target_arch = "wasm32"))]
    render_template_panel(ui, id, file);

    let copy_request = egui::CentralPanel::default()
        .frame(egui::Frame::new())
        .show_inside(ui, |ui| render_hex_body(ui, file, state))
        .inner;

    if let Some(kind) = copy_request {
        do_copy(ui.ctx(), file, kind);
    }
    for entry_path in open_entries {
        ui.ctx().data_mut(|d| {
            let queue: &mut Vec<(FileId, String)> = d.get_temp_mut_or_default(egui::Id::new(PENDING_VFS_OPEN_KEY));
            queue.push((id, entry_path));
        });
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
                        if let Some(node) = state.tree.nodes.get(idx.0 as usize).cloned() {
                            let source = file.editor.source().clone();
                            let ctx = ui.ctx().clone();
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

fn render_tree_panel_header(ui: &mut egui::Ui, show: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{} Tree", egui_phosphor::regular::TREE_STRUCTURE)).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let close = egui::Button::new(egui_phosphor::regular::X).frame(false);
            if ui.add(close).on_hover_text("Hide tree").clicked() {
                *show = false;
            }
        });
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
    let columns = state.app.hex_columns;
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
fn register_user_plugins(registry: &mut VfsRegistry) {
    let Some(dir) = user_plugins_dir() else { return };
    match hxy_plugin_host::load_plugins_from_dir(&dir) {
        Ok(handlers) => {
            for h in handlers {
                tracing::info!(name = h.name(), "loaded wasm plugin");
                registry.register(Arc::new(h));
            }
        }
        Err(e) => tracing::warn!(error = %e, dir = %dir.display(), "load plugins"),
    }
}

#[cfg(target_arch = "wasm32")]
fn register_user_plugins(_registry: &mut VfsRegistry) {}

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

#[cfg(not(target_arch = "wasm32"))]
fn handle_command_palette(ctx: &egui::Context, app: &mut HxyApp) {
    let toggle = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::P);
    if ctx.input_mut(|i| i.consume_shortcut(&toggle)) {
        if app.palette.is_open() {
            app.palette.close();
        } else {
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
        crate::command_palette::Outcome::Closed => app.palette.close(),
        crate::command_palette::Outcome::Picked(action) => apply_palette_action(ctx, app, action),
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
}

#[cfg(not(target_arch = "wasm32"))]
fn offset_palette_context(app: &mut HxyApp) -> OffsetPaletteContext {
    let Some(id) = active_file_id(app) else { return OffsetPaletteContext::default() };
    let Some(file) = app.files.get(&id) else { return OffsetPaletteContext::default() };
    let source_len = file.editor.source().len().get();
    let cursor = file.editor.selection().map(|s| s.cursor.get()).unwrap_or(0);
    OffsetPaletteContext { cursor, source_len, available: true }
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
                    hxy_i18n::t("toolbar-browse-archive"),
                    Action::InvokeCommand(crate::command_palette::PaletteCommand::BrowseArchive),
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
            }
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
                let (label_key, toggle_icon) = if history_ctx.can_paste {
                    ("palette-toggle-edit-mode-leave", icon::LOCK)
                } else {
                    ("palette-toggle-edit-mode-enter", icon::LOCK_OPEN)
                };
                out.push(
                    egui_palette::Entry::new(hxy_i18n::t(label_key), Action::InvokeCommand(crate::command_palette::PaletteCommand::ToggleEditMode))
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
            if !offset_ctx.available {
                return out;
            }
            let query = app.palette.inner.query.trim();
            build_offset_entries(&mut out, app.palette.mode, query, offset_ctx);
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
                    PaletteCommand::BrowseArchive => apply_command_effect(ctx, app, CommandEffect::MountActiveFile),
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
                    PaletteCommand::ToggleEditMode => toggle_active_edit_mode(app),
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
                let clamped = target.min(max);
                file.editor.set_selection(Some(hxy_core::Selection::caret(hxy_core::ByteOffset::new(clamped))));
                file.editor.set_scroll_to_byte(hxy_core::ByteOffset::new(clamped));
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
                let anchor = start.min(source_len.saturating_sub(1));
                file.editor.set_selection(Some(hxy_core::Selection {
                    anchor: hxy_core::ByteOffset::new(anchor),
                    cursor: hxy_core::ByteOffset::new(last),
                }));
                file.editor.set_scroll_to_byte(hxy_core::ByteOffset::new(anchor));
            }
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

/// Best guess at which file tab the user is "in" right now. Tries in
/// order: the egui_dock-focused tab (exact), the most recently
/// focused file (so clicking into the Inspector / Console doesn't
/// blank out a menu command), and finally -- when only one file is
/// open -- that sole file. Returning `None` means there's genuinely
/// no file to act on.
fn active_file_id(app: &mut HxyApp) -> Option<FileId> {
    if let Some((_, tab)) = app.dock.find_active_focused()
        && let Tab::File(id) = *tab
    {
        app.last_active_file = Some(id);
        return Some(id);
    }
    if let Some(id) = app.last_active_file
        && app.files.contains_key(&id)
    {
        return Some(id);
    }
    if app.files.len() == 1 {
        return app.files.keys().copied().next();
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
    let Some(file) = app.files.get(&id) else { return };
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
    let Some(path) = target else { return };

    let ctx = format!("Save {}", path.display());
    let len = file.editor.source().len().get();
    let bytes = match file.editor.source().read(
        hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len)).expect("valid range"),
    ) {
        Ok(b) => b,
        Err(e) => {
            app.console_log(ConsoleSeverity::Error, &ctx, format!("read patched bytes: {e}"));
            return;
        }
    };
    if let Err(e) = write_atomic(&path, &bytes) {
        app.console_log(ConsoleSeverity::Error, &ctx, format!("write: {e}"));
        return;
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
                show_vfs_tree: false,
            });
        }
    }
    app.console_log(ConsoleSeverity::Info, &ctx, "saved");
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
                let events = crate::plugins_tab::show(ui, handlers_dir.as_ref(), templates_dir.as_ref());
                for e in events {
                    match e {
                        crate::plugins_tab::PluginsEvent::Rescan => *self.plugin_rescan = true,
                    }
                }
            }
            Tab::File(id) => match self.files.get_mut(id) {
                Some(file) => {
                    render_file_tab(ui, *id, file, self.state);
                }
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing file {id:?}"));
                }
            },
        }
    }

    fn closeable(&mut self, tab: &mut Self::Tab) -> bool {
        matches!(tab, Tab::File(_) | Tab::Console | Tab::Inspector | Tab::Plugins)
    }

    fn scroll_bars(&self, tab: &Self::Tab) -> [bool; 2] {
        // File tabs and the console/inspector manage their own
        // scrolling; outer dock scrollbar off for those.
        if matches!(tab, Tab::File(_) | Tab::Console | Tab::Inspector) { [false, false] } else { [true, true] }
    }

    fn on_close(&mut self, tab: &mut Self::Tab) -> OnCloseResponse {
        if let Tab::File(id) = tab
            && let Some(removed) = self.files.remove(id)
            && let Some(source) = removed.source_kind
        {
            self.state.open_tabs.retain(|t| t.source != source);
        }
        OnCloseResponse::Close
    }
}

/// Mirror the tab's in-memory selection + scroll into
/// [`PersistedState::open_tabs`] so the save-on-dirty path picks it up.
fn sync_tab_state(state: &mut PersistedState, file: &OpenFile) {
    let Some(source) = &file.source_kind else { return };
    if let Some(entry) = state.open_tabs.iter_mut().find(|t| &t.source == source) {
        entry.selection = file.editor.selection();
        entry.scroll_offset = file.editor.scroll_offset();
        entry.show_vfs_tree = file.show_vfs_tree;
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
) {
    ui.horizontal(|ui| {
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
                let len = range.len().get();
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
            // will do, not what the icon currently shows.
            let (icon, tooltip_key) = match file.editor.edit_mode() {
                crate::file::EditMode::Readonly => (egui_phosphor::regular::LOCK, "status-lock-readonly-tooltip"),
                crate::file::EditMode::Mutable => (egui_phosphor::regular::LOCK_OPEN, "status-lock-mutable-tooltip"),
            };
            let resp = ui
                .add(egui::Button::new(icon).frame(false).min_size(egui::vec2(18.0, 18.0)))
                .on_hover_text(hxy_i18n::t(tooltip_key));
            if resp.clicked() {
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
    // Raw pointer-in-rect; `r.hovered()` can read false when a
    // tooltip overlay or neighbouring widget counts as covering the
    // label, which meant the Cmd+C consume here never fired and
    // the hex-view's selection copy handler got the event instead.
    let over_label = ui.rect_contains_pointer(r.rect);
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

const COPY_BYTES: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::C);
const COPY_HEX: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT), egui::Key::C);
const NEW_FILE: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::N);
const SAVE_FILE: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::S);
const SAVE_FILE_AS: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT), egui::Key::S);
const TOGGLE_EDIT_MODE: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::E);
const UNDO: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Z);
const REDO: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT), egui::Key::Z);
const PASTE: egui::KeyboardShortcut = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::V);
const PASTE_AS_HEX: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT), egui::Key::V);

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
    });
}
