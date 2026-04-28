//! Main application type.

#[cfg(not(target_arch = "wasm32"))]
pub mod dialogs;
pub mod shortcuts;

use std::collections::HashMap;
use std::sync::Arc;

use egui_dock::DockArea;
use egui_dock::DockState;
use egui_dock::TabViewer;
use egui_dock::tab_viewer::OnCloseResponse;
use hxy_plugin_host::TemplateRuntime as _;
use hxy_vfs::TabSource;
use hxy_vfs::VfsHandler;
use hxy_vfs::VfsRegistry;
use hxy_vfs::handlers::ZipHandler;

use crate::APP_NAME;
use crate::files::FileId;
use crate::files::OpenFile;
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
    Workspace(crate::files::WorkspaceId),
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
    Workspace(crate::files::WorkspaceId),
}

pub struct HxyApp {
    pub(crate) dock: DockState<Tab>,
    pub(crate) files: HashMap<FileId, OpenFile>,
    /// File-mounted VFS workspaces, keyed by `WorkspaceId`. Each entry
    /// backs a `Tab::Workspace` and owns a nested `DockState` plus the
    /// `MountedVfs` that supplies child entries.
    pub(crate) workspaces: std::collections::BTreeMap<crate::files::WorkspaceId, crate::files::Workspace>,
    next_workspace_id: u64,
    /// Active plugin VFS mounts, keyed by `MountId`. Each entry backs a
    /// `Tab::PluginMount` and supplies the byte source for child VFS
    /// entry tabs the user opens from the tree.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) mounts: std::collections::BTreeMap<crate::files::MountId, crate::files::MountedPlugin>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) next_mount_id: u64,
    /// Live compare sessions, keyed by the same id their
    /// `Tab::Compare` payload carries.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) compares: std::collections::BTreeMap<crate::compare::CompareId, crate::compare::CompareSession>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) next_compare_id: u64,
    pub(crate) state: SharedPersistedState,
    next_file_id: u64,
    registry: VfsRegistry,
    #[cfg(not(target_arch = "wasm32"))]
    template_plugins: Vec<Arc<dyn hxy_plugin_host::TemplateRuntime>>,
    /// Loaded VFS plugin handlers, kept alongside the
    /// `VfsRegistry` so the palette can ask each one for its
    /// command contributions without going through the trait-
    /// object erasure the registry stores.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) plugin_handlers: Vec<Arc<hxy_plugin_host::PluginHandler>>,
    /// Shared per-plugin blob persistence. `None` means no SQLite
    /// pool was wired (e.g. db open failed at startup); plugins
    /// granted `persist` then see `denied` from the state interface.
    /// Grants themselves live in [`PersistedState::plugin_grants`].
    #[cfg(not(target_arch = "wasm32"))]
    plugin_state_store: Option<Arc<dyn hxy_plugin_host::StateStore>>,

    #[cfg(not(target_arch = "wasm32"))]
    sink: Option<crate::settings::persist::SaveSink>,

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
    pub(crate) pending_duplicate: Option<PendingDuplicate>,

    /// Toasts driven by `egui_toast`. Used for "search wrapped" /
    /// "replaced N matches" notifications and the open-file
    /// "Run X template?" prompts. Rendered once per frame at the
    /// top-right of the central panel; the wrapper exposes a
    /// `dismiss_group` helper for the file-open prompt flow that
    /// needs to clear sibling toasts when the user accepts one.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) toasts: crate::toasts::ToastCenter,

    /// Open compare-picker modal, if any. Holds the user's in-progress
    /// A / B selection while the dialog is up; cleared on confirm or
    /// cancel.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) compare_picker: Option<crate::compare::picker::ComparePickerState>,

    /// Pending search-modal request stashed by [`drain_search_effects`]
    /// and rendered next frame as either a length-mismatch
    /// confirmation or a Replace-All count confirmation. Carries the
    /// originating `FileId` so the resume path can re-issue the
    /// operation against the right buffer.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_search_modal: Option<crate::search::modal::PendingSearchModal>,

    /// Set when an open hit a sidecar from a previous session that
    /// still matches the file on disk. The modal asks the user
    /// whether to restore the saved patch or discard it; rendering
    /// happens in `update()` next to the duplicate-open dialog.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_patch_restore: Option<PendingPatchRestore>,

    /// Bounded ring buffer of plugin / template log entries. Rendered
    /// by the Console dock tab when it's open; entries accumulate
    /// regardless so opening the tab later reveals back-scroll.
    console: std::collections::VecDeque<ConsoleEntry>,

    /// Data-inspector dock tab state. Endianness + radix preferences
    /// and the `show_panel` flag that's only consulted when the
    /// Inspector tab is closed and re-opened.
    #[cfg(not(target_arch = "wasm32"))]
    inspector: crate::panels::inspector::InspectorState,
    /// Registered decoders for the inspector. Defaults to the
    /// built-in set; user-registered decoders will be additive.
    #[cfg(not(target_arch = "wasm32"))]
    decoders: Vec<Arc<dyn crate::panels::inspector::Decoder>>,
    /// The most recently focused File tab. Remembered across frames
    /// so panels like the Inspector (which take keyboard focus when
    /// clicked) keep showing data from the file the user was last
    /// reading, not from themselves.
    pub(crate) last_active_file: Option<FileId>,
    /// Same idea as `last_active_file` but for workspace context:
    /// remembers which workspace was most recently focused so
    /// "Toggle VFS panel" / "Browse VFS" don't silently no-op when
    /// the user happens to have clicked into the inspector or
    /// console. Cleared when the corresponding workspace closes.
    pub(crate) last_active_workspace: Option<crate::files::WorkspaceId>,
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
    pending_plugin_events: Vec<crate::panels::plugins::PluginsEvent>,
    /// Plugin operations (invoke / respond / mount-by-token) that
    /// were dispatched to a worker thread and are awaiting a result.
    /// Drained each frame; ready ops dispatch their outcome through
    /// the same paths the synchronous calls used to take.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_plugin_ops: Vec<crate::plugins::runner::PendingOp>,
    /// Auto-detected template library loaded from the user's
    /// `templates/` dir. Consulted when a file is opened so the
    /// toolbar can offer `Run ZIP.bt` directly.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) templates: crate::templates::library::TemplateLibrary,
    /// Cmd+P / Ctrl+P unified palette. Outlives individual opens so
    /// toggling off and back on feels continuous; the state is reset
    /// explicitly when switching modes.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) palette: crate::commands::palette::PaletteState,
    /// Visual pane picker session. `Some` after the user activates
    /// the visual move/merge palette commands and before they
    /// either press a target letter (op fires) or Escape (cancel).
    /// Mutually exclusive with `palette` -- entering the picker
    /// closes the palette, opening the palette cancels the picker.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_pane_pick: Option<crate::tabs::pane_pick::PendingPanePick>,
    /// Persistent letter assignments for the visual pane picker,
    /// keyed by a content hash of each leaf's tabs. Lets a leaf
    /// keep the same letter across pick sessions even when other
    /// leaves around it open / close. Stale entries (whose leaf
    /// no longer exists) are evicted by `pane_pick::tick` so the
    /// freed letter is available for the next new leaf.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pane_pick_letters: std::collections::BTreeMap<u64, char>,
    /// Set when the user tries to close a tab that has unsaved
    /// edits -- via Cmd+W or by clicking the tab's X. The modal
    /// renders next frame and asks Save / Don't Save / Cancel;
    /// only `Save`-then-success or `Don't Save` actually close the
    /// tab, the third does nothing.
    pub(crate) pending_close_tab: Option<crate::tabs::close::PendingCloseTab>,
    /// Tracks which dock the user's last tab-bar interaction was in,
    /// so `Ctrl+Tab` / `Ctrl+Shift+Tab` cycle the correct surface
    /// (outer dock vs a specific workspace's inner dock). Toggled
    /// directly by `Alt+Tab`. Updated on mouse click via the
    /// viewer's `on_tab_button` hook.
    pub(crate) tab_focus: TabFocus,
    /// Same shape as `pending_close_tab` but written from the inner
    /// workspace dock's `on_close`. Drained alongside the regular
    /// pending-close slot; the modal treats them identically.
    pub(crate) pending_close_workspace_entry: Option<crate::tabs::close::PendingCloseTab>,
    /// `WorkspaceId`s the inner dock drained to "no tabs left except
    /// the editor". Drained post-dock to collapse the workspace back
    /// to a plain `Tab::File` in the outer dock.
    pub(crate) pending_collapse_workspace: Vec<crate::files::WorkspaceId>,
    /// Set when the user X-clicks a `Tab::PluginMount`; drained after
    /// the dock pass to remove the mount entry from `mounts` and any
    /// matching record from `state.open_tabs`.
    #[cfg(not(target_arch = "wasm32"))]
    pending_close_mount: Option<crate::files::MountId>,
    /// Tool tabs the user has stashed via `toggle_tool_panel`. While
    /// non-empty, the right-hand tool panel is hidden -- the dock has
    /// no leaf for these tabs at all, so the surrounding panes get
    /// their horizontal space back. Toggling again recreates the
    /// right-split leaf and pushes these tabs into it.
    pub(crate) hidden_tool_tabs: Vec<Tab>,
    /// Shared cross-file search state. Backs the `Tab::SearchResults`
    /// dock tab; lives on the app so query / matches survive the user
    /// closing and reopening the tab.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) global_search: crate::search::global::GlobalSearchState,
    /// Events the global search tab emitted this frame. Drained at the
    /// end of `ui()` so we can mutate `files` (focus / jump) after the
    /// dock has released its borrow.
    #[cfg(not(target_arch = "wasm32"))]
    pending_global_search_events: Vec<crate::search::global::GlobalSearchEvent>,
    /// Most-recently-focused leaf that holds a content tab (File /
    /// Welcome / Settings). Used to route file opens that originate
    /// from inside a tool panel (e.g. clicking a VFS entry inside a
    /// `Tab::PluginMount`) back into the user's main editing area
    /// instead of the tool panel itself. Refreshed each frame.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) last_content_leaf: Option<egui_dock::NodePath>,
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
    /// Active patterns-download worker, if any. Held until the
    /// status reaches Success / Failed; the host then writes the
    /// resulting hash back to [`crate::settings::ImhexPatternsState`]
    /// and reloads the template library.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pattern_fetch: Option<crate::templates::patterns_fetch::FetchHandle>,
    /// Bytes downloaded so far on the active patterns fetch (mirrored
    /// from the worker's progress messages so the Plugins tab can
    /// render a progress label without re-pumping the inbox).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pattern_in_flight_bytes: Option<u64>,
    /// Set by the Plugins tab's "Download / update" button; drained
    /// in `update()` to spawn the worker. The flag indirection lets
    /// the click run inside the dock viewer where we don't have
    /// `&mut HxyApp`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_pattern_download_request: bool,
    /// Whether the first-launch "Download ImHex patterns?" modal
    /// should render this frame. Set on construction when the corpus
    /// isn't installed and the user hasn't declined; cleared once
    /// the user picks Download or Not Now.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pattern_first_run_prompt: bool,
    /// Template-prompt clicks the toast layer queued up this frame.
    /// Drained after `update()` and routed through the same path the
    /// command palette's `Run Template` action takes.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_template_runs: Vec<crate::toasts::PendingTemplateRun>,
    /// Filesystem watcher that emits per-frame events for any
    /// open path the user is editing. `None` only when the
    /// platform watcher couldn't be constructed at startup; in
    /// that case external changes go undetected and the user
    /// must reload manually.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) file_watcher: Option<crate::files::watch::FileWatcher>,
    /// One pending reload prompt at a time. The dialog renders
    /// next frame and the user's choice (Reload, Keep edits,
    /// Ignore) routes through `apply_reload_decision`. Set when
    /// the watcher reports a change for a tab whose effective
    /// auto-reload mode is `Ask`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_reload_prompt: Option<PendingReloadPrompt>,
    /// Workspace-entry tabs whose underlying VFS entry vanished
    /// after a reload. Each one prompts the user with "close the
    /// tab or keep its in-memory bytes?" -- the view may still
    /// be useful (the entry's contents are cached on the tab's
    /// editor) even though it can't be saved back through the
    /// mount any more.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_orphan_entries: Vec<PendingOrphanEntry>,
    /// Snapshot manager dialog state -- which file's snapshots
    /// are being inspected, plus the in-progress compare-pair
    /// picks. `None` hides the dialog.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_snapshot_dialog: Option<crate::files::snapshot_ui::SnapshotDialogState>,
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
/// the duplicate-open dialog. Just remembers the path -- with the
/// streaming open path, opening is cheap and we don't need to
/// stash the (potentially huge) bytes blob to avoid a re-read.
pub(crate) struct PendingDuplicate {
    pub(crate) display_name: String,
    pub(crate) path: std::path::PathBuf,
    pub(crate) existing: FileId,
}

/// A sidecar patch found on open that the user hasn't decided what
/// to do with yet. The modal renders next frame; either side resets
/// `pending_patch_restore` to `None`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct PendingPatchRestore {
    pub(crate) file_id: FileId,
    pub(crate) sidecar: crate::files::patch_persist::PatchSidecar,
    /// Classification captured at open time so the modal can reuse
    /// the reason string without re-stating the filesystem.
    pub(crate) integrity: crate::files::patch_persist::RestoreIntegrity,
}

/// Why the watcher fired. The reload prompt shows different
/// wording for a content change vs. an outright removal so the
/// user understands the choice they're making.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExternalChangeKind {
    Modified,
    Removed,
}

/// One pending reload prompt the per-frame dialog renderer is
/// going to surface. Only one of these is queued at a time; if a
/// second event lands for a different file before the user
/// dismisses, it is dropped (the file is still being watched, so
/// the next change re-fires).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct PendingReloadPrompt {
    pub(crate) file_id: FileId,
    pub(crate) display_name: String,
    pub(crate) path: std::path::PathBuf,
    pub(crate) kind: ExternalChangeKind,
    /// Whether the file has uncommitted edits at prompt time.
    /// Drives the wording of the "discard local edits" warning
    /// inside the dialog so the user knows what's at stake.
    pub(crate) has_unsaved: bool,
}

/// One choice from the reload-prompt dialog. Routed back into
/// `apply_reload_decision` after the user picks.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadDecision {
    /// Re-read disk bytes; drop the current patch + undo / redo.
    DiscardEdits,
    /// Re-read disk bytes; keep the patch on top of the new
    /// base. Undo / redo are dropped because their `old_bytes`
    /// references no longer match the new base.
    KeepEdits,
    /// Do nothing. The file's in-memory state stays as it was.
    Ignore,
}

/// One workspace-entry tab whose underlying VFS path no longer
/// resolves after a reload. The orphan-tab dialog renders these
/// one at a time, asking the user to either close the tab or
/// keep it open (the editor still holds the entry's last-known
/// bytes; only writeback through the mount is broken).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct PendingOrphanEntry {
    pub(crate) file_id: FileId,
    pub(crate) display_name: String,
    pub(crate) entry_path: String,
}

impl HxyApp {
    pub fn new(cc: &eframe::CreationContext<'_>, state: SharedPersistedState) -> Self {
        install_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        cc.egui_ctx.set_global_style(crate::style::hxy_style());
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
        // Plugins load with empty grants + no state store at
        // construction time. The runtime-owned `with_plugin_persistence`
        // builder reloads them once the SQLite-backed grants and
        // state store are available; without that call (e.g. db open
        // failed at startup) every requested permission stays denied.
        #[cfg(not(target_arch = "wasm32"))]
        let plugin_handlers = register_user_plugins(&mut registry, &hxy_plugin_host::PluginGrants::default(), None);
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
            #[cfg(not(target_arch = "wasm32"))]
            compares: std::collections::BTreeMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            next_compare_id: 1,
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
            toasts: crate::toasts::ToastCenter::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_search_modal: None,
            #[cfg(not(target_arch = "wasm32"))]
            compare_picker: None,
            #[cfg(not(target_arch = "wasm32"))]
            pending_patch_restore: None,
            console: std::collections::VecDeque::new(),
            #[cfg(not(target_arch = "wasm32"))]
            inspector: crate::panels::inspector::InspectorState::default(),
            #[cfg(not(target_arch = "wasm32"))]
            decoders: crate::panels::inspector::default_decoders(),
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
            templates: load_template_library_dirs(),
            #[cfg(not(target_arch = "wasm32"))]
            palette: crate::commands::palette::PaletteState::default(),
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
            global_search: crate::search::global::GlobalSearchState::default(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_global_search_events: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            last_content_leaf: None,
            #[cfg(not(target_arch = "wasm32"))]
            pending_cli_paths: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            ipc_inbox: None,
            #[cfg(not(target_arch = "wasm32"))]
            pattern_fetch: None,
            #[cfg(not(target_arch = "wasm32"))]
            pattern_in_flight_bytes: None,
            #[cfg(not(target_arch = "wasm32"))]
            pending_pattern_download_request: false,
            #[cfg(not(target_arch = "wasm32"))]
            // Cached above before `state` was moved into the struct.
            pattern_first_run_prompt: show_patterns_prompt,
            #[cfg(not(target_arch = "wasm32"))]
            pending_template_runs: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            file_watcher: match crate::files::watch::FileWatcher::with_prefs(&cc.egui_ctx, initial_polling) {
                Ok(w) => Some(w),
                Err(e) => {
                    tracing::warn!(error = %e, "filesystem watcher unavailable; external changes will go undetected");
                    None
                }
            },
            #[cfg(not(target_arch = "wasm32"))]
            pending_reload_prompt: None,
            #[cfg(not(target_arch = "wasm32"))]
            pending_orphan_entries: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_snapshot_dialog: None,
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
        self.plugin_handlers = register_user_plugins(&mut registry, &grants, self.plugin_state_store.clone());
        self.registry = registry;
        self.template_plugins = load_user_template_plugins();
        self.templates = load_template_library_dirs();
    }

    /// Refresh the user-template library after a successful
    /// ImHex-Patterns download. Same shape as [`reload_plugins`]
    /// but only touches the templates list -- the plugin registry
    /// is unchanged.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn refresh_templates_after_pattern_install(&mut self) {
        self.templates = load_template_library_dirs();
    }

    /// Drain a batch of grant / wipe events captured by the
    /// Plugins tab. Mutates `PersistedState::plugin_grants` for
    /// any `SetGrant`, calls the state store for any `WipeState`,
    /// then triggers a single `reload_plugins` at the end so the
    /// linker reflects the new grant set.
    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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

    /// Show the Entropy panel as a tool tab. Mirrors how the
    /// Inspector / Plugins tabs route -- adds it to the shared
    /// tool leaf if no other tool tab exists, otherwise focuses
    /// the existing one.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn show_entropy(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Entropy) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        let node_path = crate::tabs::dock_ops::push_tool_tab(&mut self.dock, Tab::Entropy);
        self.dock.set_focused_node_and_surface(node_path);
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn toggle_entropy(&mut self) {
        if let Some(path) = self.dock.find_tab(&Tab::Entropy) {
            let _ = self.dock.remove_tab(path);
        } else {
            self.show_entropy();
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

    #[cfg(not(target_arch = "wasm32"))]
    pub fn template_runtime_for(&self, extension: &str) -> Option<Arc<dyn hxy_plugin_host::TemplateRuntime>> {
        self.template_plugins.iter().find(|r| r.extensions().iter().any(|e| e.eq_ignore_ascii_case(extension))).cloned()
    }

    pub fn registry(&self) -> &VfsRegistry {
        &self.registry
    }

    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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

    /// Move dock focus to the tab backing `file_id`, if found.
    pub(crate) fn focus_file_tab(&mut self, file_id: FileId) {
        if let Some(path) = self.dock.find_tab(&Tab::File(file_id)) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
        // The file might live inside a workspace either as the
        // editor or as an opened entry. Focus the workspace tab in
        // the outer dock and the matching sub-tab in the inner dock.
        let workspace_target: Option<(crate::files::WorkspaceId, crate::files::WorkspaceTab)> =
            self.workspaces.values().find_map(|w| {
                if w.editor_id == file_id {
                    Some((w.id, crate::files::WorkspaceTab::Editor))
                } else if w.dock.find_tab(&crate::files::WorkspaceTab::Entry(file_id)).is_some() {
                    Some((w.id, crate::files::WorkspaceTab::Entry(file_id)))
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
        source: Arc<dyn hxy_core::HexSource>,
        restore_selection: Option<hxy_core::Selection>,
        restore_scroll: Option<f32>,
        as_workspace: bool,
    ) -> FileId {
        let id = self.create_open_file(display_name, source_kind.clone(), source, restore_selection, restore_scroll);
        self.apply_readonly_for_source(id);

        let pushed_workspace = if as_workspace { self.try_push_as_workspace(id) } else { false };
        if !pushed_workspace {
            self.dock.push_to_focused_leaf(Tab::File(id));
            if let Some(path) = self.dock.find_tab(&Tab::File(id)) {
                crate::tabs::dock_ops::remove_welcome_from_leaf(&mut self.dock, path.surface, path.node);
            }
        }

        // Look for an unsaved-edits sidecar from a previous session
        // and offer it back to the user. The actual restore happens
        // after the modal returns; this just stages the prompt.
        #[cfg(not(target_arch = "wasm32"))]
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
                });
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.suggest_templates_for(id);
        #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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
        if let Some(path) = file.root_path().cloned() {
            if let Some(watcher) = self.file_watcher.as_mut() {
                watcher.watch(path);
            }
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
    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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
            match decision {
                ReloadDecision::DiscardEdits => file.editor.swap_source(stream),
                ReloadDecision::KeepEdits => file.editor.swap_source_keep_patch(stream),
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
        // Re-run the template, if any. Done last so the post-
        // reload tree reflects the new bytes -- the runner
        // takes a fresh source clone.
        self.rerun_template_for_file(ctx, id);
        true
    }

    /// Re-mount the workspace whose editor is `file_id` (if any)
    /// against the file's freshly-swapped byte source. Walks the
    /// workspace's inner dock for `WorkspaceTab::Entry(_)` tabs;
    /// each surviving entry's bytes get re-read, each vanished
    /// entry stages an orphan-tab prompt the host renders next
    /// frame. No-op when the file isn't the editor of any
    /// workspace.
    #[cfg(not(target_arch = "wasm32"))]
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
                        file.editor.swap_source(stream);
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

    /// Re-fire the template that was last run against `file_id`,
    /// if any. Called from the reload path so the parsed tree
    /// stays in sync with the new bytes; if no template was ever
    /// run, this is a no-op.
    #[cfg(not(target_arch = "wasm32"))]
    fn rerun_template_for_file(&mut self, ctx: &egui::Context, file_id: FileId) {
        // Only re-run when the previous run actually completed.
        // Pending suggestions or in-flight runs aren't worth
        // displacing -- the user hasn't committed to a template
        // yet, or the worker is still computing the original.
        let Some(file) = self.files.get(&file_id) else { return };
        if file.template.is_none() || file.template_running.is_some() {
            return;
        }
        let Some(path) = file.last_template_path.clone() else { return };
        crate::templates::runner::run_template_from_path(ctx, self, file_id, path);
    }

    /// Look at the just-opened file's extension + first bytes and
    /// raise a template-prompt toast for every plausible match. The
    /// toast layer collapses sibling prompts when the user accepts
    /// one, so opening a `.zip` with both a `.bt` and a `.hexpat`
    /// match shows both options but cleans up after the choice.
    #[cfg(not(target_arch = "wasm32"))]
    fn suggest_templates_for(&mut self, id: FileId) {
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
        // Cap at three so the corner doesn't fill with toasts on a
        // popular extension. The palette still surfaces the full
        // list for power users.
        let group = id.get();
        for entry in candidates.into_iter().take(3) {
            let label = hxy_i18n::t_args("toast-template-suggestion", &[("name", &entry.name)]);
            self.toasts.push_template_prompt(group, id, entry.path.clone(), label);
        }
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
        let mut file = OpenFile::from_source(id, display_name, source_kind, source);
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
                    crate::files::SuggestedTemplate { path: entry.path.clone(), display_name: entry.name.clone() }
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
            crate::tabs::dock_ops::remove_welcome_from_leaf(&mut self.dock, path.surface, path.node);
        }
        true
    }

    /// Allocate a `WorkspaceId`, build a `Workspace`, and register it.
    /// Does not push a tab -- the caller decides whether the workspace
    /// is fresh (push `Tab::Workspace`) or replacing an existing
    /// `Tab::File` for the same `editor_id` (swap the dock tab).
    pub(crate) fn spawn_workspace(&mut self, editor_id: FileId, mount: Arc<MountedVfs>) -> crate::files::WorkspaceId {
        let id = crate::files::WorkspaceId::new(self.next_workspace_id);
        self.next_workspace_id += 1;
        let workspace = crate::files::Workspace::new(id, editor_id, mount);
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
        // After every tab has been remounted to a live FileId /
        // WorkspaceId / MountId, replay the saved dock layout on top
        // so splits / sizes / focus / window state survive.
        self.apply_persisted_dock_layout();
    }

    #[cfg(not(target_arch = "wasm32"))]
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
                let (source, _len) =
                    crate::files::streaming::open_vfs(parent_mount.clone(), entry_path.clone()).map_err(|e| {
                        crate::files::FileOpenError::Read { path: entry_path.into(), source: e }
                    })?;
                let name = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(entry_path).to_owned();
                let target = self
                    .workspace_for_source(parent.as_ref())
                    .map(OpenTarget::Workspace)
                    .unwrap_or(OpenTarget::Toplevel);
                self.open_with_target(
                    name,
                    Some(tab.source.clone()),
                    source,
                    tab.selection,
                    Some(tab.scroll_offset),
                    target,
                );
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
    #[cfg(not(target_arch = "wasm32"))]
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
            #[cfg(not(target_arch = "wasm32"))]
            TabSource::PluginMount { plugin_name, token, .. } => self
                .mounts
                .values()
                .find(|m| m.plugin_name == *plugin_name && m.token == *token)
                .and_then(|m| m.status.live().cloned()),
            #[cfg(target_arch = "wasm32")]
            TabSource::PluginMount { .. } => None,
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
                        });
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
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
        #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
struct RestoredCompareSide {
    name: String,
    bytes: Vec<u8>,
}

#[cfg(not(target_arch = "wasm32"))]
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
        #[cfg(not(target_arch = "wasm32"))]
        self.drain_pending_plugin_ops(ui.ctx());

        // Push the user's polling preferences into the watcher
        // so any settings-tab nudge takes effect on the very
        // next tick. Idempotent when nothing changed.
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(watcher) = self.file_watcher.as_mut() {
            let prefs = polling_prefs_from_settings(&self.state.read().app);
            watcher.set_polling(prefs);
        }

        // Pull queued filesystem-change notifications off the
        // notify watcher + polling worker and route each one
        // through the reload prompt / auto-reload paths.
        #[cfg(not(target_arch = "wasm32"))]
        drain_file_watch_events(ui.ctx(), self);

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
        // Snapshot which file the Entropy panel should render
        // before the dock pass, mirroring the Inspector. Drives
        // the panel's heading + plot data without requiring the
        // viewer to walk back through `app.dock` mid-render.
        #[cfg(not(target_arch = "wasm32"))]
        let entropy_active_file = active_file_id(self);
        #[cfg(not(target_arch = "wasm32"))]
        let mut entropy_recompute = false;

        {
            // Snapshot fields that the viewer needs but that live on
            // `self.state` BEFORE taking the write guard -- otherwise
            // `self.state.read()` inside the struct literal deadlocks
            // against the outer write guard (parking_lot RwLock is not
            // reentrant).
            #[cfg(not(target_arch = "wasm32"))]
            let patterns_installed_hash_snapshot = self.state.read().app.imhex_patterns.installed_hash.clone();
            let mut state_guard = self.state.write();
            let mut viewer = HxyTabViewer {
                files: &mut self.files,
                state: &mut state_guard,
                #[cfg(not(target_arch = "wasm32"))]
                compares: &mut self.compares,
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
                #[cfg(not(target_arch = "wasm32"))]
                patterns_installed_hash: patterns_installed_hash_snapshot,
                #[cfg(not(target_arch = "wasm32"))]
                patterns_in_flight_bytes: self.pattern_in_flight_bytes,
                pending_close_tab: &mut self.pending_close_tab,
                tab_focus: &mut self.tab_focus,
                workspaces: &mut self.workspaces,
                pending_close_workspace_entry: &mut self.pending_close_workspace_entry,
                pending_collapse_workspace: &mut self.pending_collapse_workspace,
                #[cfg(not(target_arch = "wasm32"))]
                toasts: &mut self.toasts,
                #[cfg(not(target_arch = "wasm32"))]
                pending_template_runs: &mut self.pending_template_runs,
                #[cfg(not(target_arch = "wasm32"))]
                entropy_active_file,
                #[cfg(not(target_arch = "wasm32"))]
                entropy_recompute: &mut entropy_recompute,
            };
            let style = crate::style::hxy_dock_style(ui.style());
            DockArea::new(&mut self.dock).style(style).show_leaf_collapse_buttons(false).show_inside(ui, &mut viewer);
        }

        // Drain the panel's recompute click. Done after the dock
        // borrow releases so we can mutate `app.files` freely.
        #[cfg(not(target_arch = "wasm32"))]
        if entropy_recompute {
            compute_entropy_active_file(ui.ctx(), self);
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let events = std::mem::take(&mut self.pending_plugin_events);
            if !events.is_empty() {
                self.apply_plugin_events(events);
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        crate::tabs::dock_ops::track_content_leaf(self);
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
            crate::tabs::close::collapse_workspace_to_file(self, workspace_id);
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
        crate::plugins::mount::drain_pending_mount_retries(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        drain_external_open_requests(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        crate::templates::runner::drain_template_runs(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        drain_entropy_runs(ui.ctx(), self);
        // Visual pane picker takes priority over the palette and
        // any other keyboard consumer: while a pick is staged it
        // owns Escape (cancel) and a..z (target letters). It runs
        // after the dock has rendered so leaf rects are this
        // frame's, not last frame's.
        #[cfg(not(target_arch = "wasm32"))]
        crate::tabs::focus::handle_pane_pick(ui.ctx(), self);
        // Palette runs first so it gets first crack at keyboard
        // events. egui clears focus on plain Escape during its own
        // event preprocessing, so egui_wants_keyboard_input() reads
        // false by the time dispatch_hex_edit_keys runs -- if the
        // hex editor ran first it would drain Escape for its own
        // clear-selection handler before the palette could use it
        // to dismiss.
        #[cfg(not(target_arch = "wasm32"))]
        handle_command_palette(ui.ctx(), self);
        crate::app::shortcuts::dispatch_copy_shortcut(ui.ctx(), self);
        crate::app::shortcuts::dispatch_save_shortcut(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        crate::tabs::close::dispatch_close_shortcut(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        crate::app::shortcuts::dispatch_paste_shortcut(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        crate::app::shortcuts::dispatch_find_shortcut(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        crate::app::shortcuts::dispatch_jump_field_shortcut(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        crate::tabs::focus::dispatch_focus_pane_shortcut(ui.ctx(), self);
        crate::tabs::focus::dispatch_tab_focus_toggle(ui.ctx(), self);
        crate::tabs::focus::dispatch_tab_cycle(ui.ctx(), self);
        crate::app::shortcuts::dispatch_hex_edit_keys(ui.ctx(), self);
        crate::app::dialogs::render_duplicate_open_dialog(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        crate::app::dialogs::render_patch_restore_dialog(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        crate::app::dialogs::render_reload_prompt_dialog(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        crate::app::dialogs::render_orphaned_entry_dialog(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        crate::files::snapshot_ui::render_snapshot_dialog(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        crate::tabs::close::render_close_tab_dialog(ui.ctx(), self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            crate::search::modal::drain_search_effects(self);
            crate::search::modal::render_search_modal(ui.ctx(), self);
            crate::compare::picker::render_compare_picker(ui.ctx(), self);
            crate::app::dialogs::render_imhex_patterns_first_run(ui.ctx(), self);
            crate::app::dialogs::pump_pattern_fetch(ui.ctx(), self);
            self.toasts.show_toasts(ui.ctx());
            crate::templates::runner::drain_pending_template_runs(ui.ctx(), self);
        }

        #[cfg(not(target_arch = "wasm32"))]
        self.snapshot_dock_layout();
        self.save_if_dirty(&snapshot_before);
    }

    #[cfg(not(target_arch = "wasm32"))]
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

fn consume_welcome_open_request(ctx: &egui::Context, app: &mut HxyApp) {
    let req = ctx.data_mut(|d| d.remove_temp::<std::path::PathBuf>(egui::Id::new(WELCOME_OPEN_RECENT)));
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(path) = req {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        app.request_open_filesystem(name, path);
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
/// Stage a reload prompt for whatever file is currently active,
/// or surface a console hint when the active tab has no
/// filesystem source the host can re-read. Routed to from the
/// command palette's "Reload file..." entry.
#[cfg(not(target_arch = "wasm32"))]
pub fn request_reload_active_file(app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else {
        app.console_log(ConsoleSeverity::Warning, "Reload", hxy_i18n::t("palette-reload-no-active-file"));
        return;
    };
    let Some(file) = app.files.get(&id) else { return };
    let Some(path) = file.root_path().cloned() else {
        app.console_log(ConsoleSeverity::Warning, "Reload", hxy_i18n::t("palette-reload-no-disk-source"));
        return;
    };
    let display_name = file.display_name.clone();
    let has_unsaved = file.editor.is_dirty();
    app.pending_reload_prompt = Some(PendingReloadPrompt {
        file_id: id,
        display_name,
        path,
        kind: ExternalChangeKind::Modified,
        has_unsaved,
    });
}

/// Capture a snapshot of the active file's current bytes with
/// an auto-generated name. Console-logs when the active tab
/// can't snapshot (no stable identity / read failure).
#[cfg(not(target_arch = "wasm32"))]
pub fn take_snapshot_active_file(app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else {
        app.console_log(ConsoleSeverity::Warning, "Snapshot", hxy_i18n::t("palette-reload-no-active-file"));
        return;
    };
    if app.files.get(&id).and_then(|f| f.snapshots.as_ref()).is_none() {
        app.console_log(ConsoleSeverity::Warning, "Snapshot", hxy_i18n::t("snapshot-no-store"));
        return;
    }
    if let Some(new_id) = crate::files::snapshot_ui::capture_snapshot(app, id, String::new()) {
        let display = app.files.get(&id).map(|f| f.display_name.clone()).unwrap_or_default();
        app.console_log(
            ConsoleSeverity::Info,
            format!("Snapshot {display}"),
            hxy_i18n::t_args("snapshot-capture-toast", &[("id", &new_id.get().to_string())]),
        );
    }
}

/// Open the snapshot manager dialog for the active file.
#[cfg(not(target_arch = "wasm32"))]
pub fn open_snapshots_active_file(app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else {
        app.console_log(ConsoleSeverity::Warning, "Snapshot", hxy_i18n::t("palette-reload-no-active-file"));
        return;
    };
    crate::files::snapshot_ui::open_for(app, id);
}

/// Kick off (or re-fire) an entropy compute for the active
/// file's bytes and open the Entropy panel so the result is
/// visible the moment the worker finishes. No-op when there's
/// no active file or the buffer is empty.
#[cfg(not(target_arch = "wasm32"))]
pub fn compute_entropy_active_file(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else {
        app.console_log(ConsoleSeverity::Warning, "Entropy", hxy_i18n::t("palette-reload-no-active-file"));
        return;
    };
    app.show_entropy();
    let Some(file) = app.files.get_mut(&id) else { return };
    let len = file.editor.source().len().get();
    if len == 0 {
        app.console_log(ConsoleSeverity::Info, "Entropy", "buffer is empty");
        return;
    }
    let window = crate::panels::entropy::pick_window_size(len);
    let source = file.editor.source().clone();
    let display = file.display_name.clone();
    file.entropy = None;
    file.entropy_running = Some(crate::panels::entropy::spawn_compute(ctx, id, source, window));
    app.console_log(
        ConsoleSeverity::Info,
        format!("Entropy {display}"),
        format!("computing entropy with {window}-byte windows over {len} byte(s)..."),
    );
}

/// Drain any completed entropy computations into the file's
/// `entropy` slot. Mirrors `drain_template_runs` -- runs once
/// per frame, non-blocking inbox read.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn drain_entropy_runs(ctx: &egui::Context, app: &mut HxyApp) {
    let mut done: Vec<(FileId, crate::panels::entropy::EntropyOutcome, std::time::Duration)> = Vec::new();
    for (id, file) in app.files.iter_mut() {
        let Some(run) = file.entropy_running.as_ref() else { continue };
        let outcomes: Vec<_> = run.inbox.read(ctx).collect();
        if outcomes.is_empty() {
            continue;
        }
        let elapsed = run.started.elapsed();
        file.entropy_running = None;
        for outcome in outcomes {
            done.push((*id, outcome, elapsed));
        }
    }
    for (id, outcome, elapsed) in done {
        let display = app.files.get(&id).map(|f| f.display_name.clone()).unwrap_or_default();
        let ctx_label = format!("Entropy {display}");
        match outcome {
            crate::panels::entropy::EntropyOutcome::Ok(state) => {
                let summary = format!(
                    "computed {} entropy point(s) in {:.0} ms (mean {:.2} bits/byte)",
                    state.points.len(),
                    elapsed.as_secs_f64() * 1000.0,
                    state.mean(),
                );
                if let Some(file) = app.files.get_mut(&id) {
                    file.entropy = Some(state);
                }
                app.console_log(ConsoleSeverity::Info, &ctx_label, summary);
            }
            crate::panels::entropy::EntropyOutcome::Err(msg) => {
                app.console_log(ConsoleSeverity::Error, &ctx_label, msg);
            }
        }
    }
}

/// Set the per-file auto-reload pref for the active tab and
/// re-aim the watcher accordingly. `Never` unwatches the file
/// so neither notify nor the polling worker spends any cycles
/// on it; the other modes (re-)enrol it.
#[cfg(not(target_arch = "wasm32"))]
pub fn set_active_file_watch_pref(app: &mut HxyApp, mode: crate::settings::AutoReloadMode) {
    let Some(id) = active_file_id(app) else {
        app.console_log(ConsoleSeverity::Warning, "Watch", hxy_i18n::t("palette-reload-no-active-file"));
        return;
    };
    let display = app.files.get(&id).map(|f| f.display_name.clone()).unwrap_or_default();
    app.set_file_watch_pref(id, mode);
    app.console_log(
        ConsoleSeverity::Info,
        format!("Watch {display}"),
        hxy_i18n::t_args("watch-pref-applied", &[("mode", &hxy_i18n::t(mode.label_key()))]),
    );
}

/// Translate persisted settings into the live polling prefs the
/// watcher worker thread expects. Used both at startup and every
/// time the settings UI nudges the cadence / poll-all flag.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn polling_prefs_from_settings(s: &crate::settings::AppSettings) -> crate::files::watch::PollingPrefs {
    let interval = if s.file_poll_interval_ms == 0 {
        None
    } else {
        let dur = std::time::Duration::from_millis(s.file_poll_interval_ms as u64);
        Some(dur.clamp(crate::files::watch::PollingPrefs::MIN_INTERVAL, crate::files::watch::PollingPrefs::MAX_INTERVAL))
    };
    crate::files::watch::PollingPrefs { interval, poll_all: s.file_poll_all }
}

/// Pull every event the filesystem watcher has buffered since the
/// previous frame and react. Auto-reload paths swap their source
/// in-place; ask-mode paths stage a reload prompt; never-mode
/// paths are dropped silently.
#[cfg(not(target_arch = "wasm32"))]
fn drain_file_watch_events(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(watcher) = app.file_watcher.as_mut() else { return };
    let events = watcher.drain();
    if events.is_empty() {
        return;
    }
    for event in events {
        match event {
            crate::files::watch::WatchEvent::Modified(target) => {
                handle_external_change(ctx, app, target, ExternalChangeKind::Modified);
            }
            crate::files::watch::WatchEvent::Removed(target) => {
                handle_external_change(ctx, app, target, ExternalChangeKind::Removed);
            }
            crate::files::watch::WatchEvent::Renamed { from, to } => {
                tracing::debug!(from = %from.display(), to = %to.display(), "watched file renamed externally");
                handle_external_change(
                    ctx,
                    app,
                    crate::files::watch::WatchTarget::Filesystem(from),
                    ExternalChangeKind::Removed,
                );
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn handle_external_change(
    ctx: &egui::Context,
    app: &mut HxyApp,
    target: crate::files::watch::WatchTarget,
    kind: ExternalChangeKind,
) {
    use crate::files::watch::WatchTarget;
    let (affected_ids, label_path, pref_key): (Vec<FileId>, std::path::PathBuf, std::path::PathBuf) = match &target
    {
        WatchTarget::Filesystem(path) => {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            let ids: Vec<FileId> = app
                .files
                .iter()
                .filter_map(|(id, f)| {
                    let root = f.root_path()?;
                    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
                    (root_canonical == canonical || root.as_path() == path.as_path()).then_some(*id)
                })
                .collect();
            (ids, path.clone(), path.clone())
        }
        WatchTarget::Vfs(file_id) => {
            // VFS keys identify a single tab directly. The
            // pref-key path is synthesised from the tab's
            // source so per-file auto-reload remembers VFS
            // entries the same way it remembers disk paths.
            let key = match app.files.get(file_id).and_then(|f| f.source_kind.as_ref()).map(vfs_pref_key_for) {
                Some(k) => k,
                None => return,
            };
            (vec![*file_id], key.clone(), key)
        }
    };
    if affected_ids.is_empty() {
        return;
    }
    let mode_for_path = app.state.read().app.auto_reload_for(&pref_key);
    for file_id in affected_ids {
        let (display_name, has_unsaved) = match app.files.get(&file_id) {
            Some(f) => (f.display_name.clone(), f.editor.is_dirty()),
            None => continue,
        };
        if matches!(kind, ExternalChangeKind::Removed) {
            app.console_log(
                ConsoleSeverity::Warning,
                format!("{display_name}"),
                format!("source removed externally ({})", label_path.display()),
            );
            continue;
        }
        match mode_for_path {
            crate::settings::AutoReloadMode::Always => {
                if !app.apply_reload_decision(ctx, file_id, ReloadDecision::DiscardEdits) {
                    continue;
                }
            }
            crate::settings::AutoReloadMode::Never => {
                tracing::debug!(target = %label_path.display(), "auto-reload set to Never; ignoring change");
            }
            crate::settings::AutoReloadMode::Ask => {
                if app.pending_reload_prompt.is_some() {
                    continue;
                }
                app.pending_reload_prompt = Some(PendingReloadPrompt {
                    file_id,
                    display_name,
                    path: label_path.clone(),
                    kind,
                    has_unsaved,
                });
            }
        }
    }
}

/// Stable per-file key used by the auto-reload preference list
/// for VFS-entry tabs. We don't have a real path so we
/// synthesise one from the source's parent + entry path. Two
/// tabs of the same VFS entry share the same key.
#[cfg(not(target_arch = "wasm32"))]
fn vfs_pref_key_for(source: &TabSource) -> std::path::PathBuf {
    match source {
        TabSource::VfsEntry { parent, entry_path } => {
            let parent_label = match parent.as_ref() {
                TabSource::Filesystem(p) => p.display().to_string(),
                TabSource::PluginMount { plugin_name, token, .. } => format!("plugin:{plugin_name}/{token}"),
                TabSource::VfsEntry { entry_path, .. } => format!("vfs:{entry_path}"),
                TabSource::Anonymous { id, .. } => format!("anon:{}", id.get()),
            };
            std::path::PathBuf::from(format!("vfs://{parent_label}{entry_path}"))
        }
        TabSource::Filesystem(p) => p.clone(),
        TabSource::PluginMount { plugin_name, token, .. } => {
            std::path::PathBuf::from(format!("plugin:{plugin_name}/{token}"))
        }
        TabSource::Anonymous { id, .. } => std::path::PathBuf::from(format!("anon:{}", id.get())),
    }
}

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
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        app.request_open_filesystem(name, path);
    }
}

fn consume_dropped_files(ctx: &egui::Context, app: &mut HxyApp) {
    let dropped = ctx.input(|i| i.raw.dropped_files.clone());
    for file in dropped {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = file.path {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            app.request_open_filesystem(name, path);
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
pub(crate) const PENDING_VFS_OPEN_KEY: &str = "hxy_pending_vfs_open";

/// One pending "open this entry as a new tab" request, queued from a
/// VFS panel during render. `Workspace` carries a `WorkspaceId` (the
/// file-rooted workspaces like zip / minidump); `PluginMount` carries
/// a `MountId` (plugin VFS tabs whose mount lives in `app.mounts`,
/// not in any file).
#[derive(Clone, Debug)]
pub enum PendingVfsOpen {
    Workspace {
        workspace_id: crate::files::WorkspaceId,
        entry_path: String,
    },
    #[cfg(not(target_arch = "wasm32"))]
    PluginMount {
        mount_id: crate::files::MountId,
        entry_path: String,
    },
}

#[cfg(not(target_arch = "wasm32"))]
fn drain_pending_vfs_opens(ctx: &egui::Context, app: &mut HxyApp) {
    let pending: Vec<PendingVfsOpen> =
        ctx.data_mut(|d| d.remove_temp::<Vec<PendingVfsOpen>>(egui::Id::new(PENDING_VFS_OPEN_KEY))).unwrap_or_default();
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
                let (stream, _len) = match crate::files::streaming::open_vfs(mount.clone(), entry_path.clone()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, entry = %entry_path, "open vfs entry");
                        continue;
                    }
                };
                let name = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&entry_path).to_owned();
                let source = TabSource::VfsEntry { parent: Box::new(parent_source), entry_path };
                app.open_with_target(name, Some(source), stream, None, None, OpenTarget::Workspace(workspace_id));
            }
            PendingVfsOpen::PluginMount { mount_id, entry_path } => {
                let Some(entry) = app.mounts.get(&mount_id) else { continue };
                let parent_source = TabSource::PluginMount {
                    plugin_name: entry.plugin_name.clone(),
                    token: entry.token.clone(),
                    title: entry.display_name.clone(),
                };
                let Some(mount) = entry.status.live().cloned() else {
                    tracing::warn!(entry = %entry_path, "plugin mount not ready -- ignoring entry open");
                    continue;
                };
                let (stream, _len) = match crate::files::streaming::open_vfs(mount.clone(), entry_path.clone()) {
                    Ok(s) => s,
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
                crate::tabs::dock_ops::focus_content_leaf(app);
                app.open(name, Some(source), stream, None, None, false);
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn drain_pending_vfs_opens(_ctx: &egui::Context, _app: &mut HxyApp) {}

pub(crate) fn apply_command_effect(ctx: &egui::Context, app: &mut HxyApp, effect: crate::commands::CommandEffect) {
    use crate::commands::CommandEffect;
    match effect {
        CommandEffect::OpenFileDialog => crate::files::open::handle_open_file(app),
        CommandEffect::MountActiveFile => crate::plugins::mount::mount_active_file(app),
        CommandEffect::RunTemplateDialog => {
            #[cfg(not(target_arch = "wasm32"))]
            crate::templates::runner::run_template_dialog(ctx, app);
        }
        CommandEffect::RunTemplateDirect(path) => {
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(id) = active_file_id(app) {
                crate::templates::runner::run_template_from_path(ctx, app, id, path);
            }
            #[cfg(target_arch = "wasm32")]
            let _ = path;
        }
        CommandEffect::OpenRecent(path) => {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                app.request_open_filesystem(name, path);
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
        CommandEffect::DockSplit(dir) => crate::tabs::dock_ops::dock_split_focused(app, dir),
        CommandEffect::DockMerge(dir) => crate::tabs::dock_ops::dock_merge_focused(app, dir),
        CommandEffect::DockMoveTab(dir) => crate::tabs::dock_ops::dock_move_focused_tab(app, dir),
    }
}

/// Apply a frame's worth of events from the search bar to `file`.
/// The bar itself is render-only -- byte scans, selection moves, and
/// `matches` recomputation happen here.
#[cfg(not(target_arch = "wasm32"))]
fn apply_search_events(file: &mut OpenFile, events: Vec<crate::search::bar::SearchEvent>) {
    use crate::search::SearchSideEffect;
    use crate::search::bar::SearchEvent;
    use crate::search::find_all;
    use crate::search::find_next;
    use crate::search::find_prev;

    let mut want_all = file.search.all_results;
    for ev in events {
        let bounds = file.search.scope.bounds(file.editor.source().len().get());
        match ev {
            SearchEvent::Refresh => {
                file.search.refresh_pattern();
                if want_all && let Some(p) = file.search.pattern.clone() {
                    let m = find_all(file.editor.source().as_ref(), &p, bounds);
                    file.search.matches = m;
                    file.search.active_idx = nearest_match_idx(&file.search.matches, current_caret(file));
                }
            }
            SearchEvent::Next => {
                let Some(pattern) = file.search.pattern.clone() else { continue };
                let from = current_caret(file).saturating_add(1);
                if let Some(hit) = find_next(file.editor.source().as_ref(), &pattern, from, true, bounds) {
                    apply_match_jump(file, hit.offset, &pattern);
                    if hit.wrapped {
                        file.search.pending_effects.push(SearchSideEffect::WrappedForward);
                    }
                }
            }
            SearchEvent::Prev => {
                let Some(pattern) = file.search.pattern.clone() else { continue };
                let from = current_caret(file);
                if let Some(hit) = find_prev(file.editor.source().as_ref(), &pattern, from, true, bounds) {
                    apply_match_jump(file, hit.offset, &pattern);
                    if hit.wrapped {
                        file.search.pending_effects.push(SearchSideEffect::WrappedBackward);
                    }
                }
            }
            SearchEvent::FindAll => {
                want_all = true;
                file.search.all_results = true;
                if let Some(p) = file.search.pattern.clone() {
                    let m = find_all(file.editor.source().as_ref(), &p, bounds);
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
            SearchEvent::ToggleReplace => {
                file.search.replace_open = !file.search.replace_open;
            }
            SearchEvent::RefreshReplace => {
                file.search.refresh_replace_pattern();
            }
            SearchEvent::SetScope(scope) => {
                file.search.scope = scope;
                file.search.matches.clear();
                file.search.active_idx = None;
            }
            SearchEvent::ReplaceCurrent => {
                crate::search::replace::queue_replace_current(file);
            }
            SearchEvent::ReplaceAll => {
                crate::search::replace::queue_replace_all(file, bounds);
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
fn apply_global_search_events(app: &mut HxyApp, events: Vec<crate::search::global::GlobalSearchEvent>) {
    use crate::search::find_all;
    use crate::search::global::GlobalMatch;
    use crate::search::global::GlobalSearchEvent;

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
                    let bounds = hxy_core::ByteRange::new(
                        hxy_core::ByteOffset::new(0),
                        hxy_core::ByteOffset::new(src.len().get()),
                    )
                    .expect("0 <= len");
                    for off in find_all(src.as_ref(), &pattern, bounds) {
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

/// Best guess at "the workspace the user is in." Tries in order:
/// the outer-focused `Tab::Workspace`, the most recently focused
/// workspace (so clicking into Inspector / Console doesn't make
/// `Toggle VFS panel` and friends evaporate), and finally -- when
/// only one workspace is open -- that sole workspace. Returns
/// `None` only when no workspace exists.
pub(crate) fn active_workspace_id(app: &mut HxyApp) -> Option<crate::files::WorkspaceId> {
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

/// Find-or-insert helper for the persisted VFS expansion list. The
/// list is a [`Vec`] of `(parent_source, expanded_paths)` pairs
/// because [`hxy_vfs::TabSource`] isn't a JSON-string-friendly key
/// (see [`crate::state::PersistedState::vfs_tree_expanded`]); this
/// helper hides the linear scan from call sites that just want a
/// `&mut Vec<String>` they can hand to [`crate::panels::vfs::show`].
fn vfs_expanded_for<'a>(list: &'a mut Vec<(TabSource, Vec<String>)>, key: &TabSource) -> &'a mut Vec<String> {
    if let Some(idx) = list.iter().position(|(k, _)| k == key) {
        return &mut list[idx].1;
    }
    list.push((key.clone(), Vec::new()));
    &mut list.last_mut().expect("just pushed").1
}

fn render_file_tab(
    ui: &mut egui::Ui,
    id: FileId,
    file: &mut OpenFile,
    state: &mut PersistedState,
    tab_focus: TabFocus,
    #[cfg(not(target_arch = "wasm32"))] toasts: &mut crate::toasts::ToastCenter,
    #[cfg(not(target_arch = "wasm32"))] pending_template_runs: &mut Vec<crate::toasts::PendingTemplateRun>,
) {
    let settings_base = state.app.offset_base;
    let mut new_base = settings_base;

    let tab_rect = ui.available_rect_before_wrap();
    let bg = ui.visuals().window_fill();
    ui.painter().rect_filled(tab_rect, 0.0, bg);

    let text_h = ui.text_style_height(&egui::TextStyle::Body);
    let status_h = text_h + 2.0;

    #[cfg(not(target_arch = "wasm32"))]
    let watch_chip = compute_watch_chip(file, &state.app);
    #[cfg(target_arch = "wasm32")]
    let watch_chip: Option<WatchStatusChip> = None;
    egui::Panel::bottom(egui::Id::new(("hxy-status-panel", id.get())))
        .resizable(false)
        .exact_size(status_h)
        .frame(egui::Frame::new().inner_margin(egui::Margin { left: 8, right: 8, top: 0, bottom: 0 }))
        .show_inside(ui, |ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                status_bar_ui(ui, file, settings_base, &mut new_base, tab_focus, watch_chip.as_ref());
            });
        });

    #[cfg(not(target_arch = "wasm32"))]
    if file.search.open {
        egui::Panel::bottom(egui::Id::new(("hxy-search-panel", id.get()))).resizable(false).show_inside(ui, |ui| {
            let events = crate::search::bar::show(ui, &mut file.search);
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
        .show_inside(ui, |ui| crate::view::hex_body::render_hex_body(ui, file, state))
        .inner;

    if let Some(kind) = copy_request {
        do_copy(ui.ctx(), file, kind);
    }

    // Render template-prompt toasts scoped to this tab. The full
    // tab rect (captured before the panels carved into it) is the
    // anchor target so the prompts ride along with the tab when the
    // dock layout changes.
    #[cfg(not(target_arch = "wasm32"))]
    toasts.show_template_prompts_for(ui.ctx(), tab_rect, id, pending_template_runs);

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
            let events = crate::panels::template::show(ui, id.get(), state);
            for e in events {
                match e {
                    crate::panels::template::TemplateEvent::Close => state.show_panel = false,
                    crate::panels::template::TemplateEvent::ExpandArray { array_id, count } => {
                        crate::panels::template::expand_array(state, array_id, count);
                    }
                    crate::panels::template::TemplateEvent::ToggleCollapse(idx) => {
                        crate::panels::template::toggle_collapse(state, idx);
                    }
                    crate::panels::template::TemplateEvent::Hover(idx) => {
                        state.hovered_node = idx;
                    }
                    crate::panels::template::TemplateEvent::Select(idx) => {
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
                    crate::panels::template::TemplateEvent::Copy { idx, kind } => {
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
                    crate::panels::template::TemplateEvent::SaveBytes(idx) => {
                        if let Some(node) = state.tree.nodes.get(idx.0 as usize).cloned() {
                            save_template_bytes(file.editor.source(), &node);
                        }
                    }
                    crate::panels::template::TemplateEvent::ToggleColors(on) => {
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
        return crate::files::copy::format_scalar(kind, raw);
    }
    let start = hxy_core::ByteOffset::new(node.span.offset);
    let end = hxy_core::ByteOffset::new(node.span.offset.saturating_add(node.span.length));
    let range = hxy_core::ByteRange::new(start, end).ok()?;
    let bytes = source.read(range).ok()?;
    let ty = hxy_plugin_host::node_type_label(&node.type_name);
    crate::files::copy::format_bytes(kind, &bytes, &node.name, &ty)
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
    let ident = crate::files::copy::sanitize_ident(&root.name);
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
                            let _ = write!(out, "{}: ", crate::files::copy::sanitize_ident(&child.name));
                        }
                        StructSyntax::C => {
                            let _ = write!(out, ".{} = ", crate::files::copy::sanitize_ident(&child.name));
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
        Value::BoolVal(b) => match syntax {
            StructSyntax::Rust | StructSyntax::C => format!("{b}"),
        },
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
    let default_name = format!("{}.bin", crate::files::copy::sanitize_ident(&node.name));
    let Some(path) = rfd::FileDialog::new().set_file_name(&default_name).save_file() else { return };
    if let Err(e) = std::fs::write(&path, &bytes) {
        tracing::warn!(error = %e, path = %path.display(), "write template bytes");
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn render_template_running(ui: &mut egui::Ui, run: &crate::files::TemplateRun) {
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
pub(crate) fn user_plugins_dir() -> Option<std::path::PathBuf> {
    // Plugins are installed artefacts (binaries + metadata), not user
    // settings -- they belong under the data dir, not the config dir.
    // On Linux this resolves to `$XDG_DATA_HOME/hxy/plugins` (i.e.
    // ~/.local/share/hxy/plugins); on macOS to `~/Library/Application
    // Support/hxy/plugins`.
    let base = dirs::data_dir()?;
    Some(base.join(APP_NAME).join("plugins"))
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn user_template_plugins_dir() -> Option<std::path::PathBuf> {
    let base = dirs::data_dir()?;
    Some(base.join(APP_NAME).join("template-plugins"))
}

/// Directory for user-authored template sources (`.bt` files). The
/// [`TemplateLibrary`] scans this for auto-detection; distinct from
/// `template-plugins/`, which holds compiled WASM runtimes.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn user_templates_dir() -> Option<std::path::PathBuf> {
    let base = dirs::data_dir()?;
    Some(base.join(APP_NAME).join("templates"))
}

/// Build the global template library from every relevant on-disk
/// source: the user's hand-curated `templates/` directory plus the
/// auto-installed ImHex-Patterns corpus. Either path may be missing
/// (first launch, never installed, etc.); the loader skips empty
/// dirs gracefully.
#[cfg(not(target_arch = "wasm32"))]
fn load_template_library_dirs() -> crate::templates::library::TemplateLibrary {
    let user = user_templates_dir();
    let patterns = crate::templates::patterns_fetch::install_dir();
    let dirs: Vec<&std::path::Path> = [user.as_deref(), patterns.as_deref()].into_iter().flatten().collect();
    crate::templates::library::TemplateLibrary::load_from_dirs(dirs)
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
    for rt in crate::templates::builtin::builtins() {
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
            crate::menu::MenuAction::NewFile => crate::files::new::handle_new_file(app),
            crate::menu::MenuAction::OpenFile => crate::files::open::handle_open_file(app),
            crate::menu::MenuAction::Save => crate::files::save::save_active_file(app, false),
            crate::menu::MenuAction::SaveAs => crate::files::save::save_active_file(app, true),
            crate::menu::MenuAction::CloseTab => crate::tabs::close::request_close_active_tab(app),
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
    let can_save =
        active.and_then(|id| app.files.get(&id)).is_some_and(|f| f.editor.is_dirty() || f.root_path().is_some());
    let (can_undo, can_redo) = active
        .and_then(|id| app.files.get(&id))
        .map(|f| (f.editor.can_undo(), f.editor.can_redo()))
        .unwrap_or((false, false));
    let can_paste = active
        .and_then(|id| app.files.get(&id))
        .is_some_and(|f| f.editor.edit_mode() == crate::files::EditMode::Mutable);
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
pub(crate) fn toggle_active_edit_mode(app: &mut HxyApp) {
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
        crate::files::EditMode::Readonly => crate::files::EditMode::Mutable,
        crate::files::EditMode::Mutable => crate::files::EditMode::Readonly,
    };
    file.editor.set_edit_mode(next);
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn paste_active_file(app: &mut HxyApp, as_hex: bool) {
    let Some(id) = active_file_id(app) else { return };
    let edit_mode = app.files.get(&id).map(|f| f.editor.edit_mode());
    if edit_mode != Some(crate::files::EditMode::Mutable) {
        return;
    }
    let text = match crate::files::paste::read_text() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "read clipboard");
            return;
        }
    };
    let bytes = if as_hex {
        match crate::files::paste::parse_hex_clipboard(&text) {
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
    crate::app::shortcuts::paste_bytes_at_cursor(file, bytes);
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn undo_active_file(app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else { return };
    let Some(file) = app.files.get_mut(&id) else { return };
    if let Some(entry) = file.editor.undo() {
        jump_cursor_to(file, entry.offset);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn redo_active_file(app: &mut HxyApp) {
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
fn jump_cursor_to(file: &mut crate::files::OpenFile, offset: u64) {
    let len = file.editor.source().len().get();
    let clamped = offset.min(len.saturating_sub(1));
    file.editor.set_selection(Some(hxy_core::Selection::caret(hxy_core::ByteOffset::new(clamped))));
    file.editor.reset_edit_nibble();
}

/// Move the active file's caret to the next / previous template
/// field's start offset relative to the current cursor. No-op when
/// the active file has no template run or no fields lie in the
/// requested direction. Scrolls the new caret into view if it isn't
/// already on screen.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn jump_to_template_field(app: &mut HxyApp, forward: bool) {
    let Some(id) = active_file_id(app) else { return };
    let Some(file) = app.files.get_mut(&id) else { return };
    let Some(template) = file.template.as_ref() else { return };
    let cursor = file.editor.selection().map(|s| s.cursor.get()).unwrap_or(0);
    // Boundaries are sorted ascending by offset; partition_point
    // pivots the slice at the first entry whose start is > cursor
    // (forward) or >= cursor (backward).
    let target = if forward {
        let idx = template.leaf_boundaries.partition_point(|(o, _)| o.get() <= cursor);
        template.leaf_boundaries.get(idx).map(|(o, _)| o.get())
    } else {
        let idx = template.leaf_boundaries.partition_point(|(o, _)| o.get() < cursor);
        if idx == 0 { None } else { template.leaf_boundaries.get(idx - 1).map(|(o, _)| o.get()) }
    };
    let Some(target) = target else { return };
    jump_cursor_to(file, target);
    let target_off = hxy_core::ByteOffset::new(target);
    if !file.editor.is_offset_visible(target_off) {
        file.editor.set_scroll_to_byte(target_off);
    }
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
                    crate::files::new::handle_new_file(app);
                }
                if ui.button(hxy_i18n::t("menu-file-open")).clicked() {
                    ui.close();
                    crate::files::open::handle_open_file(app);
                }
                let active = active_file_id(app);
                let can_save = active
                    .and_then(|id| app.files.get(&id))
                    .is_some_and(|f| f.editor.is_dirty() || f.root_path().is_some());
                let save_text = ui.ctx().format_shortcut(&SAVE_FILE);
                let save_as_text = ui.ctx().format_shortcut(&SAVE_FILE_AS);
                ui.add_enabled_ui(can_save, |ui| {
                    if ui.add(egui::Button::new(hxy_i18n::t("menu-file-save")).shortcut_text(save_text)).clicked() {
                        ui.close();
                        crate::files::save::save_active_file(app, false);
                    }
                });
                ui.add_enabled_ui(active.is_some(), |ui| {
                    if ui.add(egui::Button::new(hxy_i18n::t("menu-file-save-as")).shortcut_text(save_as_text)).clicked()
                    {
                        ui.close();
                        crate::files::save::save_active_file(app, true);
                    }
                });
                ui.separator();
                let close_text = ui.ctx().format_shortcut(&CLOSE_TAB);
                if ui.add(egui::Button::new(hxy_i18n::t("menu-file-close")).shortcut_text(close_text)).clicked() {
                    ui.close();
                    crate::tabs::close::request_close_active_tab(app);
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
                        crate::files::EditMode::Readonly => hxy_i18n::t("menu-edit-enter-edit-mode"),
                        crate::files::EditMode::Mutable => hxy_i18n::t("menu-edit-leave-edit-mode"),
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
                        .is_some_and(|f| f.editor.edit_mode() == crate::files::EditMode::Mutable);
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
                    if let Some(kind) = crate::files::copy::copy_as_menu(ui, show_scalar)
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

/// Stage a visual pane-pick session. Resolves the source leaf the
/// same way the directional commands do (focused leaf, falling back
/// to the active file's leaf), closes the palette so the overlay
/// owns the screen, and records the op for `handle_pane_pick` to
/// drive next frame. No-op when there's no resolvable source.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn start_pane_pick(app: &mut HxyApp, op: crate::tabs::pane_pick::PaneOp) {
    let Some(source) = crate::tabs::dock_ops::resolve_target_leaf(app) else { return };
    app.palette.close();
    app.pending_pane_pick = Some(crate::tabs::pane_pick::PendingPanePick { op, source: Some(source) });
}

/// Flip Vim mode: rotates the saved setting between `Default` and
/// `Vim`, then walks every open file's editor and applies the new
/// mode so the change takes effect immediately rather than waiting
/// for the next file to open.
pub(crate) fn toggle_vim_mode(app: &mut HxyApp) {
    let next = match app.state.read().app.input_mode {
        hxy_view::InputMode::Default => hxy_view::InputMode::Vim,
        hxy_view::InputMode::Vim => hxy_view::InputMode::Default,
    };
    app.state.write().app.input_mode = next;
    for file in app.files.values_mut() {
        file.editor.set_input_mode(next);
    }
}

/// Sourceless variant: stage a pane pick whose op doesn't need a
/// "from" leaf (currently just `Focus`). Every leaf in the dock
/// becomes a target. No-op when there's no dock (shouldn't happen).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn start_pane_focus(app: &mut HxyApp) {
    app.palette.close();
    app.pending_pane_pick =
        Some(crate::tabs::pane_pick::PendingPanePick { op: crate::tabs::pane_pick::PaneOp::Focus, source: None });
}

/// Drive one frame of the visual pane picker. Reads layout from the
/// dock (no mutation), then applies the chosen op via the same
/// helpers the directional commands use. Closes the palette as a
/// side effect of entering the pick (handled at command dispatch);
/// here we just consume input and execute when a target is hit.
#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(target_arch = "wasm32"))]
fn handle_command_palette(ctx: &egui::Context, app: &mut HxyApp) {
    // Cmd+P opens / closes the file switcher; Cmd+Shift+P opens
    // the full command palette. Match VS Code's split: filename
    // muscle memory goes to plain Cmd+P, the busier "search
    // everything" list takes the shift.
    //
    // egui's `consume_shortcut` ignores extra Shift/Alt modifiers
    // (per its own docstring on `InputState::consume_shortcut`), so
    // Cmd+Shift+P matches both COMMAND_PALETTE *and* QUICK_OPEN.
    // We have to check the more-specific shortcut first, otherwise
    // Cmd+Shift+P silently routes through QUICK_OPEN.
    let full = ctx.input_mut(|i| i.consume_shortcut(&crate::commands::shortcuts::COMMAND_PALETTE));
    let quick = !full && ctx.input_mut(|i| i.consume_shortcut(&crate::commands::shortcuts::QUICK_OPEN));
    if quick || full {
        let target_mode =
            if quick { crate::commands::palette::Mode::QuickOpen } else { crate::commands::palette::Mode::Main };
        if app.palette.is_open() && app.palette.mode == target_mode {
            // Same shortcut twice == close, matching the existing
            // "Cmd+P toggles the palette" expectation.
            app.palette.close();
        } else {
            // The palette and the visual pane picker can't coexist:
            // both want full-screen keyboard ownership. Opening the
            // palette implicitly cancels any staged pick.
            app.pending_pane_pick = None;
            app.palette.open_at(target_mode);
        }
    }
    if !app.palette.is_open() {
        return;
    }
    let copy_ctx = crate::commands::palette::entries::copy_palette_context(app);
    let history_ctx = crate::commands::palette::entries::history_palette_context(app);
    let template_ctx = crate::commands::palette::entries::template_palette_context(app);
    let offset_ctx = crate::commands::palette::offset::offset_palette_context(app);
    let entries = crate::commands::palette::entries::build_palette_entries(
        ctx,
        app,
        copy_ctx,
        history_ctx,
        &template_ctx,
        &offset_ctx,
    );
    let Some(outcome) = crate::commands::palette::show(ctx, &mut app.palette, entries) else { return };
    match outcome {
        crate::commands::palette::Outcome::Dismissed(reason) => dismiss_palette(app, reason),
        crate::commands::palette::Outcome::Picked(action) => {
            crate::commands::palette::apply::apply_palette_action(ctx, app, action)
        }
    }
}

/// Decide what to do when the palette is dismissed without a pick.
/// Backdrop clicks always fully close. A dismiss key (Escape by
/// default) pops back to the parent cascade level when the user
/// has opted into that behaviour and we're in a sub-mode; otherwise
/// it closes outright.
#[cfg(not(target_arch = "wasm32"))]
fn dismiss_palette(app: &mut HxyApp, reason: crate::commands::palette::DismissReason) {
    use crate::commands::palette::DismissReason;
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

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn install_template_from_dialog(app: &mut HxyApp) {
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
    let report = crate::templates::library::install_template_with_deps(&picked, &dir);
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
pub(crate) fn uninstall_template(app: &mut HxyApp, path: &std::path::Path) {
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
pub(crate) fn uninstall_plugin(app: &mut HxyApp, wasm_path: &std::path::Path) {
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
        .unwrap_or_else(|| wasm_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());

    // Hash the on-disk bytes so we can drop the matching grant
    // entry. Failure to read just means the grant cleanup is
    // skipped -- the file deletion below still proceeds.
    let key = match std::fs::read(wasm_path) {
        Ok(bytes) => {
            let version = manifest.as_ref().map(|m| m.plugin.version.clone()).unwrap_or_else(|| "0.0.0".to_string());
            Some(hxy_plugin_host::PluginKey::from_bytes(plugin_name.clone(), version, &bytes))
        }
        Err(e) => {
            app.console_log(ConsoleSeverity::Warning, &ctx, format!("read for grant cleanup: {e}"));
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
            Err(e) => {
                app.console_log(ConsoleSeverity::Warning, &ctx, format!("remove manifest {}: {e}", sidecar.display()))
            }
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
                app.console_log(ConsoleSeverity::Warning, &ctx, format!("persist grants after uninstall: {e}"));
            }
        }
    }

    if !plugin_name.is_empty()
        && let Some(store) = app.plugin_state_store.as_ref()
    {
        match store.clear(&plugin_name) {
            Ok(_) => app.console_log(ConsoleSeverity::Info, &ctx, "cleared persisted state"),
            Err(e) => app.console_log(ConsoleSeverity::Warning, &ctx, format!("clear persisted state: {e}")),
        }
    }

    app.reload_plugins();
}

/// Pick the FileId that should drive commands gated on the active
/// file when the user is focused on a `Tab::Workspace`. Uses the
/// inner dock's *focused* leaf -- not just any leaf with an active
/// tab -- so when the workspace is split (Editor in one leaf, an
/// Entry in another) keystrokes route to whichever the user
/// actually clicked into. Falls back to the workspace's editor when
/// the focused inner tab is the VfsTree (no file backs the tree
/// itself) or when nothing has focus yet.
fn inner_active_file(workspace: &mut crate::files::Workspace) -> FileId {
    if let Some((_, tab)) = workspace.dock.find_active_focused() {
        match *tab {
            crate::files::WorkspaceTab::Entry(file_id) => return file_id,
            crate::files::WorkspaceTab::Editor => return workspace.editor_id,
            crate::files::WorkspaceTab::VfsTree => {}
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
pub(crate) fn active_file_id(app: &mut HxyApp) -> Option<FileId> {
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
                if let Some(workspace) = app.workspaces.get_mut(&workspace_id) {
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

struct HxyTabViewer<'a> {
    files: &'a mut HashMap<FileId, OpenFile>,
    state: &'a mut PersistedState,
    #[cfg(not(target_arch = "wasm32"))]
    compares: &'a mut std::collections::BTreeMap<crate::compare::CompareId, crate::compare::CompareSession>,
    console: &'a std::collections::VecDeque<ConsoleEntry>,
    /// Active plugin VFS mounts. Read-only here -- closing a mount tab
    /// only flags it via `pending_close_mount` and the app drops it
    /// from the map after the dock pass.
    #[cfg(not(target_arch = "wasm32"))]
    mounts: &'a std::collections::BTreeMap<crate::files::MountId, crate::files::MountedPlugin>,
    /// Slot for the dock's `on_close` handler when the user X-clicks a
    /// `Tab::PluginMount`. The app drains the mount entry from
    /// `app.mounts` after the dock pass.
    #[cfg(not(target_arch = "wasm32"))]
    pending_close_mount: &'a mut Option<crate::files::MountId>,
    /// Cross-file search state, rendered by `Tab::SearchResults`.
    #[cfg(not(target_arch = "wasm32"))]
    global_search: &'a mut crate::search::global::GlobalSearchState,
    /// Events emitted by the global search tab during render. Drained
    /// after the dock pass so we can mutate `files` to focus / jump.
    #[cfg(not(target_arch = "wasm32"))]
    pending_global_search_events: &'a mut Vec<crate::search::global::GlobalSearchEvent>,
    #[cfg(not(target_arch = "wasm32"))]
    inspector: &'a mut crate::panels::inspector::InspectorState,
    #[cfg(not(target_arch = "wasm32"))]
    decoders: &'a [Arc<dyn crate::panels::inspector::Decoder>],
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
    pending_plugin_events: &'a mut Vec<crate::panels::plugins::PluginsEvent>,
    /// Snapshot of the persisted ImHex-Patterns hash, captured before
    /// the dock pass so the Plugins tab can render its status
    /// without re-borrowing `state`.
    #[cfg(not(target_arch = "wasm32"))]
    patterns_installed_hash: Option<String>,
    /// Bytes received so far on an in-flight pattern download, or
    /// None when no fetch is running. Mirrors
    /// [`HxyApp::pattern_in_flight_bytes`] for the dock viewer.
    #[cfg(not(target_arch = "wasm32"))]
    patterns_in_flight_bytes: Option<u64>,
    /// Slot the dock's `on_close` handler writes to when the user
    /// X-clicks a dirty File tab. The app drains this after the
    /// dock pass and renders the save-prompt modal next frame.
    pending_close_tab: &'a mut Option<crate::tabs::close::PendingCloseTab>,
    /// Mutated whenever the user clicks an outer tab button so
    /// `Ctrl+Tab` knows to cycle the outer dock next, or hands off
    /// to a workspace inner dock when the user clicks into one.
    tab_focus: &'a mut TabFocus,
    /// File-mounted VFS workspaces. The viewer renders each
    /// `Tab::Workspace` by spinning up an inner `DockArea` against
    /// `workspace.dock`.
    workspaces: &'a mut std::collections::BTreeMap<crate::files::WorkspaceId, crate::files::Workspace>,
    /// Slot the inner workspace dock writes to when the user closes a
    /// `WorkspaceTab::Entry` whose file is dirty. Same shape as
    /// `pending_close_tab` (the modal handler treats them identically).
    pending_close_workspace_entry: &'a mut Option<crate::tabs::close::PendingCloseTab>,
    /// `WorkspaceId`s the viewer drained to "no tabs left except the
    /// editor." The post-dock pass collapses these back to plain
    /// `Tab::File` entries in the outer dock.
    pending_collapse_workspace: &'a mut Vec<crate::files::WorkspaceId>,
    /// Toast / template-prompt center, plumbed in so `render_file_tab`
    /// can render its prompts scoped to the tab's content rect rather
    /// than the app-global corner.
    #[cfg(not(target_arch = "wasm32"))]
    toasts: &'a mut crate::toasts::ToastCenter,
    /// Sink for "Run X.bt" toast accepts. Drained by the host loop
    /// after the dock pass.
    #[cfg(not(target_arch = "wasm32"))]
    pending_template_runs: &'a mut Vec<crate::toasts::PendingTemplateRun>,
    /// `FileId` whose entropy result the Entropy tab should
    /// render. Captured before the dock pass so the panel
    /// renders even when keyboard focus has drifted to the
    /// panel itself (the active-file resolver returns `None`
    /// when the focused tab is the Entropy tab itself).
    #[cfg(not(target_arch = "wasm32"))]
    entropy_active_file: Option<FileId>,
    /// Set to `true` when the user clicks the panel's
    /// "Compute" / "Recompute" button. Drained by the host
    /// after the dock pass and routed through
    /// [`compute_entropy_active_file`].
    #[cfg(not(target_arch = "wasm32"))]
    entropy_recompute: &'a mut bool,
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
            Tab::Entropy => hxy_i18n::t("tab-entropy").into(),
            Tab::File(id) => match self.files.get(id) {
                Some(f) => {
                    // Both indicators sit to the left of the name:
                    // lock glyph first when the tab is read-only,
                    // then a bullet when there are unsaved edits,
                    // then the filename.
                    let mut prefix = String::new();
                    if matches!(f.editor.edit_mode(), crate::files::EditMode::Readonly) {
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
            Tab::SearchResults => format!("{} Search", egui_phosphor::regular::MAGNIFYING_GLASS).into(),
            Tab::Workspace(workspace_id) => match self.workspaces.get(workspace_id) {
                Some(w) => match self.files.get(&w.editor_id) {
                    Some(f) => {
                        // Same dirty / readonly indicators as Tab::File,
                        // plus a tree-structure icon so the user can tell
                        // at a glance that this tab nests sub-tabs.
                        let mut prefix = String::from(egui_phosphor::regular::TREE_STRUCTURE);
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
                    None => format!("workspace-{}", workspace_id.get()).into(),
                },
                None => format!("workspace-{}", workspace_id.get()).into(),
            },
            #[cfg(not(target_arch = "wasm32"))]
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
            Tab::Settings => settings_ui(ui, &mut self.state.app, self.files),
            Tab::Console => console_ui(ui, self.console),
            Tab::Inspector => {
                let (caret, bytes) = match &self.inspector_data {
                    Some((c, b)) => (Some(*c), b.as_slice()),
                    None => (None, &[] as &[u8]),
                };
                crate::panels::inspector::show(ui, self.inspector, self.decoders, caret, bytes);
            }
            Tab::Entropy => {
                let (label, state, running) = match self.entropy_active_file.and_then(|id| self.files.get(&id)) {
                    Some(f) => (Some(f.display_name.as_str()), f.entropy.as_ref(), f.entropy_running.is_some()),
                    None => (None, None, false),
                };
                crate::panels::entropy::show(ui, label, state, running, self.entropy_recompute);
            }
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
                Some(file) => {
                    render_file_tab(
                        ui,
                        *id,
                        file,
                        self.state,
                        *self.tab_focus,
                        #[cfg(not(target_arch = "wasm32"))]
                        self.toasts,
                        #[cfg(not(target_arch = "wasm32"))]
                        self.pending_template_runs,
                    );
                }
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing file {id:?}"));
                }
            },
            #[cfg(not(target_arch = "wasm32"))]
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
            #[cfg(not(target_arch = "wasm32"))]
            Tab::SearchResults => {
                let names: std::collections::HashMap<FileId, String> =
                    self.files.iter().map(|(id, f)| (*id, f.display_name.clone())).collect();
                let events = crate::search::global::show(ui, self.global_search, &names);
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
                    self.toasts,
                    self.pending_template_runs,
                );
            }
            #[cfg(not(target_arch = "wasm32"))]
            Tab::Compare(compare_id) => match self.compares.get_mut(compare_id) {
                Some(session) => crate::compare::tab::render_compare_tab(ui, session, self.state),
                None => {
                    ui.colored_label(egui::Color32::RED, format!("missing compare {compare_id:?}"));
                }
            },
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
            #[cfg(not(target_arch = "wasm32"))]
            Tab::Entropy => true,
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
            #[cfg(not(target_arch = "wasm32"))]
            Tab::Entropy => [false, false],
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
    files: &'a mut HashMap<FileId, OpenFile>,
    state: &'a mut PersistedState,
    editor_id: FileId,
    workspace_id: crate::files::WorkspaceId,
    mount: &'a Arc<MountedVfs>,
    pending_close_workspace_entry: &'a mut Option<crate::tabs::close::PendingCloseTab>,
    /// Updated by `on_tab_button` when the user clicks an inner tab,
    /// so subsequent `Ctrl+Tab` cycles cycle this workspace's dock.
    tab_focus: &'a mut TabFocus,
    /// Plumbed through so the workspace's inner File-tabs can render
    /// their template prompts scoped to the tab body.
    toasts: &'a mut crate::toasts::ToastCenter,
    pending_template_runs: &'a mut Vec<crate::toasts::PendingTemplateRun>,
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
            if let Some(removed) = self.files.remove(file_id)
                && let Some(source) = removed.source_kind
            {
                self.state.open_tabs.retain(|t| t.source != source);
            }
        }
        OnCloseResponse::Close
    }
}

/// Snapshot the host computes per-frame for the file's watch
/// state and hands to the status bar. Empty for tabs the
/// watcher can't track (anonymous in-memory buffers); when
/// present the renderer paints an eye / eye-slash icon and
/// uses `tooltip` as the hover text.
pub(crate) struct WatchStatusChip {
    pub watching: bool,
    pub tooltip: String,
}

/// Build the per-frame watch-status chip for `file` given the
/// current `settings`. Returns `None` for purely in-memory
/// tabs (anonymous scratch buffers without a stable identity)
/// since the watcher has nothing to track for those.
#[cfg(not(target_arch = "wasm32"))]
fn compute_watch_chip(file: &OpenFile, settings: &crate::settings::AppSettings) -> Option<WatchStatusChip> {
    let source = file.source_kind.as_ref()?;
    if matches!(source, TabSource::Anonymous { .. }) {
        return Some(WatchStatusChip {
            watching: false,
            tooltip: format!(
                "{}: {}",
                hxy_i18n::t("status-watch-tooltip-prefix"),
                hxy_i18n::t("status-watch-tooltip-anonymous"),
            ),
        });
    }
    let key = match file.root_path() {
        Some(p) => p.clone(),
        None => vfs_pref_key_for(source),
    };
    let mode = settings.auto_reload_for(&key);
    let interval_ms = settings.file_poll_interval_ms;
    let watching = !matches!(mode, crate::settings::AutoReloadMode::Never);

    let mode_label = hxy_i18n::t(mode.label_key());
    let mode_line = hxy_i18n::t_args("status-watch-mode", &[("mode", &mode_label)]);

    let cadence_line = if !watching {
        String::new()
    } else if file.root_path().is_some() {
        // Filesystem-backed: kernel events with optional poll.
        if interval_ms == 0 {
            hxy_i18n::t("status-watch-cadence-fs-notify-only")
        } else if settings.file_poll_all {
            hxy_i18n::t_args("status-watch-cadence-fs-poll", &[("ms", &interval_ms.to_string())])
        } else {
            hxy_i18n::t_args("status-watch-cadence-fs-notify", &[("ms", &interval_ms.to_string())])
        }
    } else {
        // VFS-only: the only signal we have is sample-hash polling.
        if interval_ms == 0 {
            hxy_i18n::t("status-watch-cadence-off")
        } else {
            hxy_i18n::t_args("status-watch-cadence-vfs-poll", &[("ms", &interval_ms.to_string())])
        }
    };

    let header = hxy_i18n::t("status-watch-tooltip-prefix");
    let body = if watching { hxy_i18n::t("status-watch-watching") } else { hxy_i18n::t("status-watch-not-watching") };
    let mut tooltip = format!("{header}\n{body}\n{mode_line}");
    if !cadence_line.is_empty() {
        tooltip.push('\n');
        tooltip.push_str(&cadence_line);
    }
    Some(WatchStatusChip { watching, tooltip })
}

fn status_bar_ui(
    ui: &mut egui::Ui,
    file: &mut OpenFile,
    base: crate::settings::OffsetBase,
    new_base: &mut crate::settings::OffsetBase,
    tab_focus: TabFocus,
    watch_chip: Option<&WatchStatusChip>,
) {
    ui.horizontal(|ui| {
        // Vim-mode chip first so the modal state is the most
        // prominent thing on the status bar when the user has it
        // on. Hidden entirely in Default mode so non-vim users
        // don't see noise.
        if !matches!(file.editor.input_mode(), hxy_view::InputMode::Default) {
            let (label, tooltip) = match file.editor.vim_state().mode {
                hxy_view::VimMode::Normal => ("NORMAL", "Vim Normal mode -- motions, operators"),
                hxy_view::VimMode::Visual => ("VISUAL", "Vim Visual mode -- motions extend selection"),
                hxy_view::VimMode::VisualLine => ("V-LINE", "Vim Visual-line mode -- selection snaps to whole rows"),
                hxy_view::VimMode::Insert => ("INSERT", "Vim Insert mode -- typing splices new bytes; Esc to return"),
                hxy_view::VimMode::Replace => {
                    ("REPLACE", "Vim Replace mode -- typing overwrites; extends past EOF; Esc to return")
                }
            };
            ui.label(format!("[{label}]")).on_hover_text(tooltip);
            ui.separator();
        }
        // Tab-focus chip: "Outer" = top-level tabs cycle; "VFS" =
        // the surrounding workspace's inner tabs cycle.
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

        if let Some(chip) = watch_chip {
            let icon = if chip.watching { egui_phosphor::regular::EYE } else { egui_phosphor::regular::EYE_SLASH };
            // Dim the icon when watching is off so the user has
            // a passive at-a-glance signal rather than a flat
            // foreground tone for both states.
            let response = if chip.watching {
                ui.label(icon)
            } else {
                ui.label(egui::RichText::new(icon).color(ui.visuals().weak_text_color()))
            };
            response.on_hover_text(&chip.tooltip);
            ui.separator();
        }

        if let Some(hov) = file.hovered {
            let value = crate::view::format::format_offset(hov.get(), base);
            crate::view::format::copyable_status_label(
                ui,
                &format!("Hover: {value}"),
                &value,
                Some(crate::view::format::format_offset(hov.get(), base.toggle())),
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
                let v = crate::view::format::format_offset(range.start().get(), base);
                (format!("Caret: {v}"), v, crate::view::format::format_offset(range.start().get(), base.toggle()))
            } else {
                let start = crate::view::format::format_offset(range.start().get(), base);
                let end = crate::view::format::format_offset(last_inclusive, base);
                let len = crate::view::format::format_offset(range.len().get(), base);
                let copy_value = format!("{start}-{end} ({len} bytes)");
                let tooltip = format!(
                    "{}-{}",
                    crate::view::format::format_offset(range.start().get(), base.toggle()),
                    crate::view::format::format_offset(last_inclusive, base.toggle()),
                );
                (format!("Sel: {copy_value}"), copy_value, tooltip)
            };
            crate::view::format::copyable_status_label(ui, &display, &copy, Some(tooltip), new_base, base);
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
                (None, crate::files::EditMode::Readonly) => {
                    (egui_phosphor::regular::LOCK, hxy_i18n::t("status-lock-readonly-tooltip"))
                }
                (None, crate::files::EditMode::Mutable) => {
                    (egui_phosphor::regular::LOCK_OPEN, hxy_i18n::t("status-lock-mutable-tooltip"))
                }
            };
            let resp =
                ui.add(egui::Button::new(icon).frame(false).min_size(egui::vec2(18.0, 18.0))).on_hover_text(tooltip);
            if resp.clicked() && hard_readonly.is_none() {
                let next = match file.editor.edit_mode() {
                    crate::files::EditMode::Readonly => crate::files::EditMode::Mutable,
                    crate::files::EditMode::Mutable => crate::files::EditMode::Readonly,
                };
                file.editor.set_edit_mode(next);
            }

            let value = crate::view::format::format_offset(size, base);
            crate::view::format::copyable_status_label(
                ui,
                &format!("Length: {value}"),
                &value,
                Some(crate::view::format::format_offset(size, base.toggle())),
                new_base,
                base,
            );
        });
    });
}

use crate::files::copy::CopyKind;

/// Read the active selection's bytes from `file` and copy them to
/// the clipboard formatted per `kind`. Value-kind variants read the
/// first `selection.len()` bytes as a LE integer (0-8 bytes) -- the
/// hex view has no type context, so this is the best we can do
/// without a template supplying sign + endianness.
pub(crate) fn do_copy(ctx: &egui::Context, file: &OpenFile, kind: CopyKind) {
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
        match crate::files::copy::format_scalar(kind, raw) {
            Some(s) => s,
            None => return,
        }
    } else {
        let ident = format!("data_{:X}", offset);
        let type_hint = format!("u8[{}]", bytes.len());
        match crate::files::copy::format_bytes(kind, &bytes, &ident, &type_hint) {
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

fn settings_ui(ui: &mut egui::Ui, settings: &mut crate::settings::AppSettings, files: &mut HashMap<FileId, OpenFile>) {
    ui.heading(hxy_i18n::t("settings-general-header"));
    ui.separator();
    egui::Grid::new("hxy-general-settings").num_columns(2).striped(true).show(ui, |ui| {
        ui.label(hxy_i18n::t("settings-zoom"));
        ui.add(egui::Slider::new(&mut settings.zoom_factor, 0.5..=2.0).step_by(0.1));
        ui.end_row();

        ui.label(hxy_i18n::t("settings-input-mode"));
        let prev_mode = settings.input_mode;
        egui::ComboBox::from_id_salt("hxy-input-mode")
            .selected_text(match settings.input_mode {
                hxy_view::InputMode::Default => hxy_i18n::t("settings-input-mode-default"),
                hxy_view::InputMode::Vim => hxy_i18n::t("settings-input-mode-vim"),
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut settings.input_mode,
                    hxy_view::InputMode::Default,
                    hxy_i18n::t("settings-input-mode-default"),
                );
                ui.selectable_value(
                    &mut settings.input_mode,
                    hxy_view::InputMode::Vim,
                    hxy_i18n::t("settings-input-mode-vim"),
                );
            });
        if settings.input_mode != prev_mode {
            for file in files.values_mut() {
                file.editor.set_input_mode(settings.input_mode);
            }
        }
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

        ui.label(hxy_i18n::t("settings-compare-deadline"));
        let mut ms = settings.compare_recompute_deadline.as_ms();
        let response = ui.add(
            egui::DragValue::new(&mut ms)
                .range(crate::settings::RecomputeDeadline::MIN_MS..=crate::settings::RecomputeDeadline::MAX_MS)
                .speed(50.0)
                .suffix(" ms"),
        );
        response.on_hover_text(hxy_i18n::t("settings-compare-deadline-tooltip"));
        if ms != settings.compare_recompute_deadline.as_ms() {
            settings.compare_recompute_deadline = crate::settings::RecomputeDeadline::from_ms(ms);
        }
        ui.end_row();
    });

    ui.add_space(12.0);
    ui.heading(hxy_i18n::t("settings-watch-header"));
    ui.separator();
    egui::Grid::new("hxy-watch-settings").num_columns(2).striped(true).show(ui, |ui| {
        ui.label(hxy_i18n::t("settings-auto-reload"));
        egui::ComboBox::from_id_salt("hxy-auto-reload")
            .selected_text(hxy_i18n::t(settings.auto_reload.label_key()))
            .show_ui(ui, |ui| {
                for mode in crate::settings::AutoReloadMode::ALL {
                    ui.selectable_value(&mut settings.auto_reload, mode, hxy_i18n::t(mode.label_key()));
                }
            });
        ui.end_row();

        ui.label(hxy_i18n::t("settings-poll-interval"));
        let mut ms = settings.file_poll_interval_ms;
        let response = ui.add(egui::DragValue::new(&mut ms).range(0..=600_000u32).speed(50.0).suffix(" ms"));
        response.on_hover_text(hxy_i18n::t("settings-poll-interval-tooltip"));
        if ms != settings.file_poll_interval_ms {
            settings.file_poll_interval_ms = ms;
        }
        ui.end_row();

        ui.label(hxy_i18n::t("settings-poll-all"));
        let response = ui.checkbox(&mut settings.file_poll_all, "");
        response.on_hover_text(hxy_i18n::t("settings-poll-all-tooltip"));
        ui.end_row();
    });
}
