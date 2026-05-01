//! Application type. ONE `HxyApp` struct backing both the native
//! desktop build and the browser wasm build -- field types differ
//! between targets via cfg gates, but the struct definition is
//! shared. Per-target `impl HxyApp` and `impl eframe::App for
//! HxyApp` blocks coexist in this file: the desktop impl handles
//! the file watcher / plugin host / IPC flows and renders the
//! full command-palette / native-menu UI; the wasm impl drops
//! those and adds rfd-async file open / drag-and-drop /
//! save-as-download.
//!
//! The wasm impl block lives at the bottom of the file under
//! `#[cfg(target_arch = "wasm32")]`. Future commits keep pushing
//! common rendering / event dispatch into shared helpers so the
//! two impls converge into one.

#[cfg(not(target_arch = "wasm32"))]
pub mod desktop;
#[cfg(not(target_arch = "wasm32"))]
pub mod desktop_tab_viewer;
#[cfg(not(target_arch = "wasm32"))]
pub mod dialogs;
pub mod shortcuts;
#[cfg(target_arch = "wasm32")]
pub mod wasm;

use std::collections::HashMap;
use std::sync::Arc;

use egui_dock::DockState;
#[cfg(not(target_arch = "wasm32"))]
use hxy_plugin_host::TemplateRuntime as _;
use hxy_vfs::TabSource;
#[cfg(not(target_arch = "wasm32"))]
use hxy_vfs::VfsHandler;
use hxy_vfs::VfsRegistry;

#[cfg(not(target_arch = "wasm32"))]
use crate::APP_NAME;
use crate::files::FileId;
use crate::files::OpenFile;
use crate::state::PersistedState;
use crate::state::SharedPersistedState;
use crate::tabs::Tab;
#[cfg(not(target_arch = "wasm32"))]
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
    pub(crate) compares: std::collections::BTreeMap<crate::compare::CompareId, crate::compare::CompareSession>,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(crate) next_compare_id: u64,
    pub(crate) state: SharedPersistedState,
    next_file_id: u64,
    /// Process-wide byte-range cache shared across hex views and
    /// template runs. The host wires every [`OpenFile`] through here
    /// so two tabs that read the same source share chunks; the
    /// debug panel attributes outstanding bytes back to the
    /// originating tab.
    pub(crate) byte_cache: Arc<hxy_core::ByteCache>,
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
    #[cfg(not(target_arch = "wasm32"))]
    prev_window: Option<WindowSettings>,
    #[cfg(not(target_arch = "wasm32"))]
    last_saved_window: Option<WindowSettings>,

    /// Zoom factor we last applied to the egui context. Used to push
    /// settings changes into the live context without re-running every
    /// frame.
    applied_zoom: f32,

    /// An open request that collided with an already-open tab. Held
    /// here while the modal asks the user whether to focus the
    /// existing tab or open a second copy. `None` outside that window.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_duplicate: Option<PendingDuplicate>,

    /// Toasts driven by `egui_toast`. Used for "search wrapped" /
    /// "replaced N matches" notifications and the open-file
    /// "Run X template?" prompts. Rendered once per frame at the
    /// top-right of the central panel; the wrapper exposes a
    /// `dismiss_group` helper for the file-open prompt flow that
    /// needs to clear sibling toasts when the user accepts one.
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
    pub(crate) pending_search_modal: Option<crate::search::modal::PendingSearchModal>,

    /// Set when an open hit a sidecar from a previous session that
    /// still matches the file on disk. The modal asks the user
    /// whether to restore the saved patch or discard it; rendering
    /// happens in `update()` next to the duplicate-open dialog.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_patch_restore: Option<PendingPatchRestore>,

    /// Bounded ring buffer of plugin / template log entries. Rendered
    /// by the Console dock tab when it's open; entries accumulate
    /// regardless so opening the tab later reveals back-scroll. The
    /// only log writers (plugin host, template runner, file watcher)
    /// are desktop-only -- wasm renders the empty placeholder
    /// directly without holding a buffer.
    #[cfg(not(target_arch = "wasm32"))]
    console: std::collections::VecDeque<ConsoleEntry>,

    /// Data-inspector dock tab state. Endianness + radix preferences
    /// and the `show_panel` flag that's only consulted when the
    /// Inspector tab is closed and re-opened.
    pub(crate) inspector: crate::panels::inspector::InspectorState,
    /// Registered decoders for the inspector. Defaults to the
    /// built-in set; user-registered decoders will be additive.
    pub(crate) decoders: Vec<Arc<dyn crate::panels::inspector::Decoder>>,
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
    /// Read by desktop-only focus tracking; on wasm it's set but
    /// never consulted.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
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
    /// explicitly when switching modes. The state struct is
    /// universal across targets; the wasm impl builds a slimmer
    /// entry list since plugin / template / file-watcher entries
    /// have nothing to dispatch into on the browser.
    pub(crate) palette: crate::commands::palette::PaletteState,
    /// Visual pane picker session. `Some` after the user activates
    /// the visual move/merge palette commands and before they
    /// either press a target letter (op fires) or Escape (cancel).
    /// Mutually exclusive with `palette` -- entering the picker
    /// closes the palette, opening the palette cancels the picker.
    pub(crate) pending_pane_pick: Option<crate::tabs::pane_pick::PendingPanePick>,
    /// Persistent letter assignments for the visual pane picker,
    /// keyed by a content hash of each leaf's tabs. Lets a leaf
    /// keep the same letter across pick sessions even when other
    /// leaves around it open / close. Stale entries (whose leaf
    /// no longer exists) are evicted by `pane_pick::tick` so the
    /// freed letter is available for the next new leaf.
    pub(crate) pane_pick_letters: std::collections::BTreeMap<u64, char>,
    /// Set when the user tries to close a tab that has unsaved
    /// edits -- via Cmd+W or by clicking the tab's X. The modal
    /// renders next frame and asks Save / Don't Save / Cancel;
    /// only `Save`-then-success or `Don't Save` actually close the
    /// tab, the third does nothing.
    #[cfg(not(target_arch = "wasm32"))]
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
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_close_workspace_entry: Option<crate::tabs::close::PendingCloseTab>,
    /// `WorkspaceId`s the inner dock drained to "no tabs left except
    /// the editor". Drained post-dock to collapse the workspace back
    /// to a plain `Tab::File` in the outer dock. Drained only on
    /// desktop today; wasm never populates it (no inner-dock close
    /// handler queues into it yet).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(crate) pending_collapse_workspace: Vec<crate::files::WorkspaceId>,
    /// Set when the user X-clicks a `Tab::PluginMount`; drained after
    /// the dock pass to remove the mount entry from `mounts` and any
    /// matching record from `state.open_tabs`.
    #[cfg(not(target_arch = "wasm32"))]
    pending_close_mount: Option<crate::files::MountId>,
    /// When set, the next pane-picker session ignores every
    /// leaf whose `NodePath` isn't in this list. Used by the
    /// `Close tool pane` action so the visual picker only
    /// highlights tool-class leaves rather than every leaf in
    /// the dock. Cleared automatically when the picker
    /// finishes (success or cancel).
    pub(crate) pane_pick_target_paths: Option<Vec<egui_dock::NodePath>>,
    /// Shared cross-file search state. Backs the `Tab::SearchResults`
    /// dock tab; lives on the app so query / matches survive the user
    /// closing and reopening the tab.
    pub(crate) global_search: crate::search::global::GlobalSearchState,
    /// Events the global search tab emitted this frame. Drained at the
    /// end of `ui()` so we can mutate `files` (focus / jump) after the
    /// dock has released its borrow.
    pending_global_search_events: Vec<crate::search::global::GlobalSearchEvent>,
    /// Most-recently-focused leaf that holds a content tab (File /
    /// Welcome / Settings). Used to route file opens that originate
    /// from inside a tool panel (e.g. clicking a VFS entry inside a
    /// `Tab::PluginMount`) back into the user's main editing area
    /// instead of the tool panel itself. Refreshed each frame.
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
    /// File ids whose bytes were just swapped from outside the
    /// per-frame update flow (`save_file_by_id` is the current
    /// caller). Drained at the top of every frame by
    /// [`drain_byte_change_cascade`], which re-runs the file's
    /// source-derived analyses (template, entropy, strings,
    /// checksums) gated by `AUTO_RUN_MAX_BYTES`. Reload's cascade
    /// happens directly inside [`HxyApp::apply_reload_decision`]
    /// because that path already has the egui context in scope.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_byte_change_cascade: Vec<FileId>,
    /// Set true at startup when [`Self::restore_open_tabs`] sees any
    /// persisted templates -- consumed once on the first `update()`
    /// frame (when the egui [`egui::Context`] is finally available)
    /// to spawn the auto-reruns. Builder-time can't do it: the
    /// template runner needs the context to wire its result inbox.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_template_restore: bool,
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
    /// Modal asking the user whether to apply a plugin-supplied
    /// virtual base address to a freshly-opened file. `Some` between
    /// the moment the file opens and the moment the user picks
    /// Accept / Decline; persisted choice rides on
    /// [`crate::state::OpenTabState::virtual_base_choice`] so the
    /// prompt fires only once per file across the lifetime of the
    /// tab entry.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_virtual_base_prompt: Option<PendingVirtualBasePrompt>,
    /// Open File with options... modal state. `Some` between the
    /// moment the user picked a file and the moment they confirm
    /// or cancel the virtual-base entry; the modal renders the
    /// path verbatim so the user can sanity-check what they're
    /// about to open.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) pending_open_with_options: Option<PendingOpenWithOptions>,
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
    /// LIFO buffer of recently-closed tab states. Cmd+Shift+T pops
    /// the most recent entry and replays it through the same
    /// `restore_one_tab` path the launch flow uses, so selection,
    /// scroll, templates, visualizer state, and virtual-base choice
    /// all come back as the user left them. Capped at
    /// [`crate::tabs::close::CLOSED_TABS_CAPACITY`] so a long session
    /// can't grow unbounded. Session-only -- not persisted; quitting
    /// drops the stack the same way every other unclosed buffer
    /// goes through the normal `state.open_tabs` restore path.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) closed_tabs: std::collections::VecDeque<crate::tabs::close::ClosedTabSnapshot>,
    /// Browser-side closed-tab buffer. Holds the actual bytes
    /// because there's no disk to re-read on Cmd+Shift+T --
    /// desktop's equivalent above stores a `TabSource` and
    /// re-opens the file from disk. Same field name, different
    /// type per target.
    #[cfg(target_arch = "wasm32")]
    pub(crate) closed_tabs: std::collections::VecDeque<wasm::ClosedTabWasm>,
    /// Inbox for VFS-entry opens deferred to a background thread so
    /// the UI stays responsive while the plugin's `metadata` /
    /// `open_file` round trips. Drained per-frame in `update()`;
    /// each delivered result either swaps the real source into the
    /// file's editor or stamps `LoadStatus::Failed` on the
    /// placeholder. Spawn calls grab a fresh sender clone via
    /// [`egui_inbox::UiInbox::sender`].
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) vfs_open_inbox: egui_inbox::UiInbox<crate::files::vfs_open::VfsOpenResult>,
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
#[cfg(not(target_arch = "wasm32"))]
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

/// Modal prompting the user to apply a plugin-supplied virtual
/// base address to a freshly-opened file. Set when a VFS-entry
/// open captures a `virtual_base_hint` and the file's persisted
/// `virtual_base_choice` is still `None`. The dialog renders one
/// per frame and writes the user's choice into `OpenTabState`,
/// applying the base to the live `OpenFile` on Accept.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct PendingVirtualBasePrompt {
    pub(crate) file_id: FileId,
    pub(crate) display_name: String,
    pub(crate) hint: u64,
}

/// State carried between picking a file in "Open File with
/// options..." and the modal that prompts for the virtual base.
/// The modal owns the current text-input value so multi-frame
/// edits survive repaints.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct PendingOpenWithOptions {
    pub(crate) name: String,
    pub(crate) path: std::path::PathBuf,
    pub(crate) virtual_base_input: String,
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

/// Universal HxyApp methods that don't reach into desktop-only
/// state (plugin host, watcher, sync rfd, etc.). Lives outside the
/// big desktop-gated impl block so wasm can call into the workspace
/// machinery.
impl HxyApp {
    /// Attempt to wrap the freshly-created file `id` in a
    /// `Tab::Workspace` by mounting its detected handler. Returns
    /// `true` if the workspace was created and pushed; `false` falls
    /// back to the plain `Tab::File` path (no detected handler, or
    /// mount failed).
    pub(crate) fn try_push_as_workspace(&mut self, id: FileId) -> bool {
        let Some(file) = self.files.get(&id) else { return false };
        let Some(handler) = file.detected_handler.clone() else { return false };
        let source = file.editor.source().clone();
        let source_id = file.source_id;
        let mount = match handler.mount(source) {
            Ok(m) => Arc::new(m),
            Err(e) => {
                tracing::warn!(error = %e, handler = handler.name(), "mount as workspace");
                return false;
            }
        };
        // Handlers like ZipHandler eagerly slurp the whole source via
        // `load_all` to parse the central directory, then keep their
        // own `Arc<[u8]>` for VFS entry reads. Drop the cache chunks
        // so the archive isn't double-resident.
        self.byte_cache.drop_source(source_id);
        let workspace_id = self.spawn_workspace(id, mount);
        #[cfg(not(target_arch = "wasm32"))]
        if crate::tabs::dock_ops::focused_leaf_is_all_tool(self) {
            crate::tabs::dock_ops::focus_content_leaf(self);
        }
        self.dock.push_to_focused_leaf(Tab::Workspace(workspace_id));
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = self.dock.find_tab(&Tab::Workspace(workspace_id)) {
            crate::tabs::dock_ops::remove_welcome_from_leaf(&mut self.dock, path.surface, path.node);
        }
        #[cfg(target_arch = "wasm32")]
        if let Some(welcome) = self.dock.find_tab(&Tab::Welcome) {
            let _ = self.dock.remove_tab(welcome);
        }
        true
    }

    /// Move dock focus to the tab backing `file_id`, if found.
    pub(crate) fn focus_file_tab(&mut self, file_id: FileId) {
        if let Some(path) = self.dock.find_tab(&Tab::File(file_id)) {
            let node_path = path.node_path();
            let _ = self.dock.set_active_tab(path);
            self.dock.set_focused_node_and_surface(node_path);
            return;
        }
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

    /// Allocate a `WorkspaceId`, build a `Workspace`, and register
    /// it. Does not push a tab -- the caller decides whether the
    /// workspace is fresh or replacing an existing `Tab::File`.
    pub(crate) fn spawn_workspace(&mut self, editor_id: FileId, mount: Arc<MountedVfs>) -> crate::files::WorkspaceId {
        let id = crate::files::WorkspaceId::new(self.next_workspace_id);
        self.next_workspace_id += 1;
        let workspace = crate::files::Workspace::new(id, editor_id, mount);
        self.workspaces.insert(id, workspace);
        id
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn consume_welcome_open_request(ctx: &egui::Context, app: &mut HxyApp) {
    let req = ctx.data_mut(|d| d.remove_temp::<std::path::PathBuf>(egui::Id::new(WELCOME_OPEN_RECENT)));
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(path) = req {
        let name =
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        app.request_open_filesystem(name, path);
    }
    #[cfg(target_arch = "wasm32")]
    let _ = (req, app);
}

#[cfg(not(target_arch = "wasm32"))]
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
    app.pending_reload_prompt =
        Some(PendingReloadPrompt { file_id: id, display_name, path, kind: ExternalChangeKind::Modified, has_unsaved });
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

/// Close one tool pane. Auto-picks the only candidate when
/// just one tool-only leaf exists; opens the visual pane
/// picker (filtered to tool-only leaves) when several do; is
/// a no-op when there are zero. The picker callback in
/// `handle_pane_pick` calls back into `close_tool_leaf` once
/// the user presses a target letter.
pub fn close_tool_pane(app: &mut HxyApp) {
    let leaves = crate::tabs::dock_ops::tool_only_leaves(&app.dock);
    match leaves.len() {
        0 => {}
        1 => crate::tabs::dock_ops::close_tool_leaf(app, leaves[0]),
        _ => {
            app.pane_pick_target_paths = Some(leaves);
            app.pending_pane_pick = Some(crate::tabs::pane_pick::PendingPanePick {
                op: crate::tabs::pane_pick::PaneOp::CloseToolLeaf,
                source: None,
            });
        }
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
/// file's bytes and open a per-file entropy panel. No-op when
/// there's no active file or the buffer is empty. Routed to
/// from the command-palette's "Compute entropy" entry.
#[cfg(not(target_arch = "wasm32"))]
pub fn compute_entropy_active_file(ctx: &egui::Context, app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else {
        app.console_log(ConsoleSeverity::Warning, "Entropy", hxy_i18n::t("palette-reload-no-active-file"));
        return;
    };
    compute_entropy_for(ctx, app, id);
}

/// Recompute the entropy for whichever file the panel button
/// was clicked from. The panel passes its pinned `FileId` --
/// distinct from `active_file_id` so a recompute clicked on a
/// docked-but-unfocused entropy tab still targets that tab's
/// file.
#[cfg(not(target_arch = "wasm32"))]
pub fn compute_entropy_for(ctx: &egui::Context, app: &mut HxyApp, id: FileId) {
    app.show_entropy_for(id);
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

/// Range scope for the strings palette commands.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringsScope {
    /// Run the strings extractor against every byte in the file.
    WholeFile,
    /// Run against the active file's current non-empty selection.
    /// Falls back to the whole file when no selection exists.
    Selection,
}

/// Above this size, the strings + checksum palette commands open
/// the tool tab but skip the auto-run -- the user has to press Run
/// in the panel to actually do the work. Stops a casual palette
/// invocation against a 4 GiB memory dump from chewing tens of
/// seconds of CPU before the user realizes what happened.
#[cfg(not(target_arch = "wasm32"))]
pub const AUTO_RUN_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Resolve a `StringsScope` to a concrete byte range against the
/// file's current source. Pulled out so the strings + checksum
/// palette helpers can share scope logic and gate auto-run by
/// the actual range length.
#[cfg(not(target_arch = "wasm32"))]
fn resolve_scope_range(file: &OpenFile, scope: StringsScope) -> Result<hxy_core::ByteRange, hxy_core::Error> {
    let source_len = file.editor.source().len().get();
    match scope {
        StringsScope::WholeFile => whole_file_range(source_len),
        StringsScope::Selection => match file.editor.selection() {
            Some(sel) if !sel.range().is_empty() => Ok(sel.range()),
            _ => whole_file_range(source_len),
        },
    }
}

/// Open the Strings panel for the active file. When `auto_run` is
/// true, also kick off a fresh extraction with the current panel
/// config -- but only if the resolved range fits inside
/// [`AUTO_RUN_MAX_BYTES`]; over that, the tab opens with the range
/// pre-filled and the user has to press Run explicitly. When
/// `auto_run` is false (the "with options" palette entry), the tab
/// always opens without spawning so the user can adjust settings.
#[cfg(not(target_arch = "wasm32"))]
pub fn run_strings_for_active(ctx: &egui::Context, app: &mut HxyApp, scope: StringsScope, auto_run: bool) {
    let Some(id) = active_file_id(app) else {
        app.console_log(ConsoleSeverity::Warning, "Strings", hxy_i18n::t("palette-reload-no-active-file"));
        return;
    };
    app.show_strings_for(id);
    let Some(range_len) = apply_strings_scope(app, id, scope) else { return };
    if !auto_run {
        return;
    }
    if range_len > AUTO_RUN_MAX_BYTES {
        let display = app.files.get(&id).map(|f| f.display_name.clone()).unwrap_or_default();
        app.console_log(
            ConsoleSeverity::Info,
            format!("Strings {display}"),
            format!(
                "{} byte(s) selected -- press Run in the panel to scan (auto-run is gated above {} MiB)",
                range_len,
                AUTO_RUN_MAX_BYTES / (1024 * 1024),
            ),
        );
        return;
    }
    spawn_strings_with_panel_config(ctx, app, id);
}

/// Resolve `scope` against `id` and write the resulting range onto
/// the file's strings panel config. Returns the range length on
/// success or `None` when the file vanished, the source is empty,
/// or the range is invalid (each case console-logs its own
/// diagnostic).
#[cfg(not(target_arch = "wasm32"))]
fn apply_strings_scope(app: &mut HxyApp, id: FileId, scope: StringsScope) -> Option<u64> {
    let file = app.files.get(&id)?;
    if file.editor.source().len().get() == 0 {
        app.console_log(ConsoleSeverity::Info, "Strings", "buffer is empty");
        return None;
    }
    let range = match resolve_scope_range(file, scope) {
        Ok(r) => r,
        Err(e) => {
            app.console_log(ConsoleSeverity::Error, "Strings", format!("invalid file range: {e}"));
            return None;
        }
    };
    let file = app.files.get_mut(&id)?;
    file.strings_panel.config.range = range;
    Some(range.len().get())
}

/// Recompute strings for `id` using the panel's existing config
/// (range, encoding, min length). Used by the panel's own "Run"
/// button, which doesn't want to redrive the range from a palette
/// scope.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_strings_with_panel_config(ctx: &egui::Context, app: &mut HxyApp, id: FileId) {
    let Some(file) = app.files.get_mut(&id) else { return };
    let config = file.strings_panel.config.clone();
    if config.range.is_empty() {
        app.console_log(ConsoleSeverity::Info, "Strings", "configured range is empty");
        return;
    }
    let source = file.editor.source().clone();
    let display = file.display_name.clone();
    file.strings_panel.last_result = None;
    file.strings_panel.running = Some(crate::panels::strings::spawn_compute(ctx, id, source, config.clone()));
    app.console_log(
        ConsoleSeverity::Info,
        format!("Strings {display}"),
        format!(
            "scanning {} bytes ({}, min length {})...",
            config.range.len().get(),
            config.encoding.label(),
            config.min_length,
        ),
    );
}

#[cfg(not(target_arch = "wasm32"))]
fn whole_file_range(source_len: u64) -> Result<hxy_core::ByteRange, hxy_core::Error> {
    hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(source_len))
}

/// Move the file's hex view selection onto a strings match and
/// scroll it into view. Pinned to `file_id` (rather than the
/// currently-focused tab) so a click in one file's strings panel
/// routes to that file's hex view regardless of focus.
#[cfg(not(target_arch = "wasm32"))]
pub fn jump_to_strings_match(app: &mut HxyApp, file_id: FileId, offset: u64, end: u64) {
    // Focus the file tab first so the selection + scroll lands
    // somewhere visible.
    app.focus_file_tab(file_id);
    let Some(file) = app.files.get_mut(&file_id) else { return };
    if end <= offset {
        return;
    }
    // Selection range is inclusive on both ends; cursor lands on
    // the last byte of the run.
    let anchor = hxy_core::ByteOffset::new(offset);
    let cursor = hxy_core::ByteOffset::new(end - 1);
    file.editor.set_selection(Some(hxy_core::Selection { anchor, cursor }));
    file.editor.set_scroll_to_byte(anchor);
}

/// Open the Checksums panel for the active file and kick off a
/// fresh compute against `scope` when the range fits inside
/// [`AUTO_RUN_MAX_BYTES`]; for larger ranges the panel opens with
/// the range pre-filled and the user presses Run explicitly.
#[cfg(not(target_arch = "wasm32"))]
pub fn run_checksums_for_active(ctx: &egui::Context, app: &mut HxyApp, scope: StringsScope) {
    let Some(id) = active_file_id(app) else {
        app.console_log(ConsoleSeverity::Warning, "Checksums", hxy_i18n::t("palette-reload-no-active-file"));
        return;
    };
    app.show_checksums_for(id);
    let Some(range_len) = apply_checksums_scope(app, id, scope) else { return };
    if range_len > AUTO_RUN_MAX_BYTES {
        let display = app.files.get(&id).map(|f| f.display_name.clone()).unwrap_or_default();
        app.console_log(
            ConsoleSeverity::Info,
            format!("Checksums {display}"),
            format!(
                "{} byte(s) selected -- press Run in the panel to compute (auto-run is gated above {} MiB)",
                range_len,
                AUTO_RUN_MAX_BYTES / (1024 * 1024),
            ),
        );
        return;
    }
    spawn_checksums_with_panel_config(ctx, app, id);
}

/// Resolve `scope` against `id` and write the resulting range onto
/// the file's checksums panel config. Returns the range length on
/// success or `None` when the file vanished, the source is empty,
/// or the range is invalid.
#[cfg(not(target_arch = "wasm32"))]
fn apply_checksums_scope(app: &mut HxyApp, id: FileId, scope: StringsScope) -> Option<u64> {
    let file = app.files.get(&id)?;
    if file.editor.source().len().get() == 0 {
        app.console_log(ConsoleSeverity::Info, "Checksums", "buffer is empty");
        return None;
    }
    let range = match resolve_scope_range(file, scope) {
        Ok(r) => r,
        Err(e) => {
            app.console_log(ConsoleSeverity::Error, "Checksums", format!("invalid file range: {e}"));
            return None;
        }
    };
    let file = app.files.get_mut(&id)?;
    file.checksums_panel.config.range = range;
    Some(range.len().get())
}

/// Recompute checksums for `id` using the panel's existing config.
/// Used by the panel's own "Run" button.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_checksums_with_panel_config(ctx: &egui::Context, app: &mut HxyApp, id: FileId) {
    let Some(file) = app.files.get_mut(&id) else { return };
    let config = file.checksums_panel.config.clone();
    if config.algorithms.is_empty() {
        app.console_log(ConsoleSeverity::Warning, "Checksums", "no algorithms selected");
        return;
    }
    if config.range.is_empty() {
        app.console_log(ConsoleSeverity::Info, "Checksums", "configured range is empty");
        return;
    }
    let source = file.editor.source().clone();
    let display = file.display_name.clone();
    file.checksums_panel.last_result = None;
    file.checksums_panel.running = Some(crate::panels::checksums::spawn_compute(ctx, id, source, config.clone()));
    let alg_list = config.algorithms.iter().map(|a| a.label()).collect::<Vec<_>>().join(", ");
    app.console_log(
        ConsoleSeverity::Info,
        format!("Checksums {display}"),
        format!("computing [{alg_list}] over {} byte(s)...", config.range.len().get()),
    );
}

/// Drain any completed checksum computations into the file's
/// `checksums_panel.last_result` slot.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn drain_checksums_runs(ctx: &egui::Context, app: &mut HxyApp) {
    let mut done: Vec<(FileId, crate::panels::checksums::ChecksumOutcome, std::time::Duration)> = Vec::new();
    for (id, file) in app.files.iter_mut() {
        let Some(run) = file.checksums_panel.running.as_ref() else { continue };
        let outcomes: Vec<_> = run.inbox.read(ctx).collect();
        if outcomes.is_empty() {
            continue;
        }
        let elapsed = run.started.elapsed();
        file.checksums_panel.running = None;
        for outcome in outcomes {
            done.push((*id, outcome, elapsed));
        }
    }
    for (id, outcome, elapsed) in done {
        let display = app.files.get(&id).map(|f| f.display_name.clone()).unwrap_or_default();
        let ctx_label = format!("Checksums {display}");
        match outcome {
            crate::panels::checksums::ChecksumOutcome::Ok(result) => {
                let summary = format!(
                    "computed {} checksum(s) in {:.0} ms",
                    result.values.len(),
                    elapsed.as_secs_f64() * 1000.0,
                );
                if let Some(file) = app.files.get_mut(&id) {
                    file.checksums_panel.last_result = Some(result);
                }
                app.console_log(ConsoleSeverity::Info, &ctx_label, summary);
            }
            crate::panels::checksums::ChecksumOutcome::Err(msg) => {
                app.console_log(ConsoleSeverity::Error, &ctx_label, msg);
            }
        }
    }
}

/// Drain any completed strings extractions into the file's
/// `strings_panel.last_result` slot.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn drain_strings_runs(ctx: &egui::Context, app: &mut HxyApp) {
    let mut done: Vec<(FileId, crate::panels::strings::StringsOutcome, std::time::Duration)> = Vec::new();
    for (id, file) in app.files.iter_mut() {
        let Some(run) = file.strings_panel.running.as_ref() else { continue };
        let outcomes: Vec<_> = run.inbox.read(ctx).collect();
        if outcomes.is_empty() {
            continue;
        }
        let elapsed = run.started.elapsed();
        file.strings_panel.running = None;
        for outcome in outcomes {
            done.push((*id, outcome, elapsed));
        }
    }
    for (id, outcome, elapsed) in done {
        let display = app.files.get(&id).map(|f| f.display_name.clone()).unwrap_or_default();
        let ctx_label = format!("Strings {display}");
        match outcome {
            crate::panels::strings::StringsOutcome::Ok(result) => {
                let summary = format!(
                    "found {} string(s) in {:.0} ms{}",
                    result.entries.len(),
                    elapsed.as_secs_f64() * 1000.0,
                    if result.truncated {
                        format!(" (truncated to {} hits)", crate::panels::strings::MAX_RESULTS)
                    } else {
                        String::new()
                    },
                );
                if let Some(file) = app.files.get_mut(&id) {
                    file.strings_panel.last_result = Some(result);
                }
                app.console_log(ConsoleSeverity::Info, &ctx_label, summary);
            }
            crate::panels::strings::StringsOutcome::Err(msg) => {
                app.console_log(ConsoleSeverity::Error, &ctx_label, msg);
            }
        }
    }
}

/// Open File with options... entry point. Pops the native file
/// picker first (so the user can't be confused by a typed path),
/// then queues the modal that asks for a virtual base. Cancelling
/// the picker is a no-op; cancelling the modal still leaves the
/// file unopened.
#[cfg(not(target_arch = "wasm32"))]
pub fn start_open_file_with_options(app: &mut HxyApp) {
    let (name, path) = match crate::files::open::pick_file() {
        Ok(pair) => pair,
        Err(crate::files::FileOpenError::Cancelled) => return,
        Err(e) => {
            tracing::warn!(error = %e, "open file with options: pick");
            return;
        }
    };
    app.pending_open_with_options = Some(PendingOpenWithOptions { name, path, virtual_base_input: String::new() });
}

/// Apply an Open-with-options decision. Opens the file via the
/// standard filesystem path, then -- when `virtual_base` is
/// provided -- sets `OpenFile.virtual_base` and persists the
/// choice into `OpenTabState` so the user doesn't get re-prompted
/// on a future plain Open.
#[cfg(not(target_arch = "wasm32"))]
pub fn finish_open_file_with_options(
    app: &mut HxyApp,
    name: String,
    path: std::path::PathBuf,
    virtual_base: Option<u64>,
) {
    app.request_open_filesystem(name, path.clone());
    let Some(vbase) = virtual_base else { return };
    // request_open_filesystem queues the open; the open landed
    // synchronously above, so the file is in `app.files`. Find the
    // newest tab with a matching path and apply.
    let target_source = hxy_vfs::TabSource::Filesystem(path.clone());
    let opened_id =
        app.files.iter().find_map(|(&id, f)| (f.source_kind.as_ref() == Some(&target_source)).then_some(id));
    let Some(id) = opened_id else { return };
    if let Some(file) = app.files.get_mut(&id) {
        file.virtual_base = Some(vbase);
        file.virtual_base_hint = Some(vbase);
    }
    let mut g = app.state.write();
    if let Some(entry) = g.open_tabs.iter_mut().find(|t| t.source == target_source) {
        entry.virtual_base_choice = Some(crate::state::VirtualBaseChoice::Accepted(vbase));
    }
}

/// Stash a plugin-supplied virtual base hint on `file_id` and either
/// apply the user's previously-recorded choice (Accepted / Declined
/// from `OpenTabState`) or queue the first-time prompt. Called from
/// every VFS-entry open path -- restore-from-disk, live click into a
/// workspace, live click into a plugin mount -- so the prompt fires
/// exactly once per (file, plugin) lifetime.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn record_virtual_base_hint(app: &mut HxyApp, file_id: FileId, hint: Option<u64>) {
    let Some(file) = app.files.get_mut(&file_id) else { return };
    file.virtual_base_hint = hint;
    let Some(hint) = hint else { return };
    let display_name = file.display_name.clone();
    // Look up persisted choice. The OpenTabState entry is keyed by
    // the file's source; if no record exists yet (a freshly-opened
    // tab the host hasn't pushed into open_tabs) treat that as
    // "never asked" too.
    let source = file.source_kind.clone();
    let prior = source.as_ref().and_then(|src| app.state.read().open_tabs.iter().find(|t| &t.source == src).cloned());
    match prior.and_then(|t| t.virtual_base_choice) {
        Some(crate::state::VirtualBaseChoice::Accepted(base)) => {
            // User already said yes; restore the applied base
            // without re-prompting. We trust the persisted value
            // even if the plugin's hint shifted -- letting the
            // plugin override would erase the user's control.
            if let Some(file) = app.files.get_mut(&file_id) {
                file.virtual_base = Some(base);
            }
        }
        Some(crate::state::VirtualBaseChoice::Declined) => {
            // User already said no; respect that.
        }
        None => {
            // First time we're seeing this file with a hint; queue
            // the modal. Only one prompt is in flight at a time --
            // if another is already queued, the second hint is
            // dropped and the user can re-trigger by reopening.
            if app.pending_virtual_base_prompt.is_none() {
                app.pending_virtual_base_prompt = Some(PendingVirtualBasePrompt { file_id, display_name, hint });
            }
        }
    }
}

/// Re-run the file's source-derived analyses after the bytes
/// were swapped out from under us (reload from disk, save flushing
/// edits + reopening). Templates always re-fire; entropy / strings
/// / checksums refresh only when they had a prior result this
/// session and the file fits inside [`AUTO_RUN_MAX_BYTES`], so a
/// reload of a 4 GiB dump doesn't pin three background workers
/// for thirty seconds. The visualizer is derived from the template
/// tree, so re-running templates is enough to pick up new bytes
/// there.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn cascade_byte_change(ctx: &egui::Context, app: &mut HxyApp, id: FileId) {
    // Templates first: this preserves the long-standing reload
    // behaviour and is the natural source of truth for the
    // visualizer panel. Un-gated by size to match the existing
    // contract -- callers that worry about template cost can
    // gate at the call site.
    app.rerun_template_for_file(ctx, id);

    let Some(file) = app.files.get(&id) else { return };
    let len = file.editor.source().len().get();
    if len == 0 || len > AUTO_RUN_MAX_BYTES {
        return;
    }
    // Skip tools the user hasn't actually used this session: an
    // entropy panel that's never been computed against this file
    // doesn't need a fresh result, and silently kicking one off
    // would waste cycles for an output the user can't see.
    let has_entropy = file.entropy.is_some() || file.entropy_running.is_some();
    let has_strings = file.strings_panel.last_result.is_some() || file.strings_panel.running.is_some();
    let has_checksums = file.checksums_panel.last_result.is_some() || file.checksums_panel.running.is_some();
    if has_entropy {
        compute_entropy_for(ctx, app, id);
    }
    if has_strings {
        spawn_strings_with_panel_config(ctx, app, id);
    }
    if has_checksums {
        spawn_checksums_with_panel_config(ctx, app, id);
    }
}

/// Drain any pending byte-change cascades scheduled outside the
/// per-frame update path (`save_file_by_id` is the current
/// producer). Empty most frames; runs cheaply when populated.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn drain_byte_change_cascade(ctx: &egui::Context, app: &mut HxyApp) {
    let pending = std::mem::take(&mut app.pending_byte_change_cascade);
    for id in pending {
        cascade_byte_change(ctx, app, id);
    }
}

/// Drain background-VFS-open results into the matching files. For
/// each completed open, swap the real source into the editor and
/// flip the file's `LoadStatus` to `Ready`; failures stamp
/// `Failed(message)` so the placeholder hex view can show the
/// reason instead of an empty grid. Successful swaps also kick the
/// template-suggestion + watcher enrolment that the placeholder
/// open skipped, plus an auto-rerun of any persisted templates the
/// session restore captured for this tab (deferred until now
/// because firing them against zero-byte placeholder bytes would
/// yield diagnostics-only template instances).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn drain_vfs_open_inbox(ctx: &egui::Context, app: &mut HxyApp) {
    let results: Vec<crate::files::vfs_open::VfsOpenResult> = app.vfs_open_inbox.read(ctx).collect();
    for crate::files::vfs_open::VfsOpenResult { file_id, outcome } in results {
        let Some(file) = app.files.get_mut(&file_id) else { continue };
        match outcome {
            Ok(source) => {
                let display = file.display_name.clone();
                let source_kind = file.source_kind.clone();
                file.byte_cache.drop_source(file.source_id);
                let cached = file.rewrap_for_view(source);
                file.editor.swap_source(cached);
                file.load_status = crate::files::LoadStatus::Ready;
                app.suggest_templates_for(file_id);
                app.watch_root_for_file(file_id);
                if let Some(source) = source_kind {
                    app.restore_persisted_templates_for_source(ctx, &source);
                }
                app.console_log(ConsoleSeverity::Info, "vfs", format!("loaded {display}"));
            }
            Err(msg) => {
                file.load_status = crate::files::LoadStatus::Failed(msg.clone());
                let display = file.display_name.clone();
                app.console_log(ConsoleSeverity::Warning, "vfs", format!("load {display} failed: {msg}"));
            }
        }
    }
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

/// Materialise the persisted byte-cache budget into the typed
/// [`hxy_core::CacheLimit`] the cache itself accepts. Reads under
/// the persisted-state lock so the host can pass the result without
/// holding the lock across the cache constructor.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn byte_cache_limit_from_state(state: &SharedPersistedState) -> hxy_core::CacheLimit {
    hxy_core::CacheLimit::from_mib(state.read().app.byte_cache_limit_mib)
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
        Some(
            dur.clamp(crate::files::watch::PollingPrefs::MIN_INTERVAL, crate::files::watch::PollingPrefs::MAX_INTERVAL),
        )
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
    let (affected_ids, label_path, pref_key): (Vec<FileId>, std::path::PathBuf, std::path::PathBuf) = match &target {
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
                display_name.to_string(),
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
                app.pending_reload_prompt =
                    Some(PendingReloadPrompt { file_id, display_name, path: label_path.clone(), kind, has_unsaved });
            }
        }
    }
}

/// Stable per-file key used by the auto-reload preference list
/// for VFS-entry tabs. We don't have a real path so we
/// synthesise one from the source's parent + entry path. Two
/// tabs of the same VFS entry share the same key.
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
        let name =
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        app.request_open_filesystem(name, path);
    }
}

#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
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
                let virtual_base_hint = mount.virtual_base.as_ref().and_then(|q| q.virtual_base(&entry_path));
                let name = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&entry_path).to_owned();
                let source = TabSource::VfsEntry { parent: Box::new(parent_source), entry_path };
                let opened_id =
                    app.open_with_target(name, Some(source), stream, None, None, OpenTarget::Workspace(workspace_id));
                record_virtual_base_hint(app, opened_id, virtual_base_hint);
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
                let virtual_base_hint = mount.virtual_base.as_ref().and_then(|q| q.virtual_base(&entry_path));
                let name = entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&entry_path).to_owned();
                let source = TabSource::VfsEntry { parent: Box::new(parent_source), entry_path };
                // The click happened in the tool panel, so focus is
                // there too. Move focus back to the editing area
                // before `open` -- it routes via push_to_focused_leaf.
                crate::tabs::dock_ops::focus_content_leaf(app);
                let opened_id = app.open(name, Some(source), stream, None, None, false);
                record_virtual_base_hint(app, opened_id, virtual_base_hint);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
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
                crate::templates::runner::run_template_from_path(
                    ctx,
                    app,
                    id,
                    path,
                    None,
                    crate::templates::runner::RestoreContext::default(),
                );
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
            SearchEvent::ReplaceCurrent => crate::search::replace::queue_replace_current(file),
            SearchEvent::ReplaceAll => crate::search::replace::queue_replace_all(file, bounds),
        }
    }
}

fn current_caret(file: &OpenFile) -> u64 {
    file.editor.selection().map(|s| s.cursor.get()).unwrap_or(0)
}

/// Highlight the match at `off` and scroll it into view. Sets the
/// selection to `[off, off + pattern.len())` so the existing selection
/// rendering colors the match. Updates `active_idx` if the match
/// matches an entry in `matches`.
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

fn nearest_match_idx(matches: &[u64], caret: u64) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(matches.partition_point(|&m| m < caret).min(matches.len() - 1))
}

/// Apply a frame's worth of cross-file search events. `Run` rebuilds
/// the match list from scratch by scanning every open file's source;
/// `JumpTo` focuses the matched file's tab and selects the bytes.
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

/// Centered "Loading {name}..." overlay shown in place of the hex
/// grid while a VFS-entry tab's bytes are being fetched on a
/// background thread. Spins the phosphor circle-notch glyph by
/// rotating it on the UI's elapsed time so the user has a clear
/// "still working" signal without a separate animation system.
fn render_loading_placeholder(ui: &mut egui::Ui, display_name: &str) {
    let rect = ui.available_rect_before_wrap();
    let bg = ui.visuals().window_fill();
    ui.painter().rect_filled(rect, 0.0, bg);
    let center = rect.center();
    let time = ui.input(|i| i.time);
    // Rotate twice per second; phosphor glyph rendered through the
    // egui font system so it inherits the user's text color. The
    // angle drives a manual layout to spin the glyph in place.
    let angle = (time as f32) * std::f32::consts::TAU * 0.75;
    let glyph = egui_phosphor::regular::CIRCLE_NOTCH;
    let font = egui::FontId::proportional(28.0);
    let color = ui.visuals().text_color();
    let galley = ui.painter().layout_no_wrap(glyph.to_string(), font, color);
    let half = galley.size() * 0.5;
    let mut shape = egui::epaint::TextShape::new(center - half, galley, color);
    shape.angle = angle;
    // Pivot rotation around the glyph center, not the layout origin.
    shape.override_text_color = Some(color);
    ui.painter().add(shape);
    let label_y = center.y + 32.0;
    let label = hxy_i18n::t_args("vfs-loading-fmt", &[("name", display_name)]);
    ui.painter().text(
        egui::pos2(center.x, label_y),
        egui::Align2::CENTER_TOP,
        label,
        egui::FontId::proportional(13.0),
        ui.visuals().weak_text_color(),
    );
    // Keep repainting until the inbox drain swaps in the real
    // source. Without this the spinner freezes between input
    // events.
    ui.ctx().request_repaint();
}

/// Static "could not load" overlay shown when the background VFS
/// open returned an error. Surfaces the plugin / IO message
/// alongside the file name so the user knows what to retry from
/// the console / palette without flipping to logs.
fn render_failed_placeholder(ui: &mut egui::Ui, display_name: &str, message: &str) {
    let rect = ui.available_rect_before_wrap();
    let bg = ui.visuals().window_fill();
    ui.painter().rect_filled(rect, 0.0, bg);
    let center = rect.center();
    let glyph = egui_phosphor::regular::WARNING;
    let color = ui.visuals().warn_fg_color;
    ui.painter().text(
        center - egui::vec2(0.0, 24.0),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(28.0),
        color,
    );
    let title = hxy_i18n::t_args("vfs-failed-fmt", &[("name", display_name)]);
    ui.painter().text(
        egui::pos2(center.x, center.y + 16.0),
        egui::Align2::CENTER_TOP,
        title,
        egui::FontId::proportional(13.0),
        ui.visuals().text_color(),
    );
    ui.painter().text(
        egui::pos2(center.x, center.y + 36.0),
        egui::Align2::CENTER_TOP,
        message,
        egui::FontId::monospace(11.0),
        ui.visuals().weak_text_color(),
    );
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
    // text_h already covers ascent + descent, but with only ~2px of
    // slack the centered label gets pixel-rounded against the panel
    // edge and clips the bottom of descenders ("g", "p", "y"). Add
    // a few px so a descender always has a clear pixel below it.
    let status_h = text_h + 6.0;

    let watch_chip = compute_watch_chip(file, &state.app);
    egui::Panel::bottom(egui::Id::new(("hxy-status-panel", id.get())))
        .resizable(false)
        .exact_size(status_h)
        .frame(egui::Frame::new().inner_margin(egui::Margin { left: 8, right: 8, top: 0, bottom: 0 }))
        .show_inside(ui, |ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                status_bar_ui(ui, file, settings_base, &mut new_base, tab_focus, watch_chip.as_ref());
            });
        });

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
    render_template_panel(ui, id, file, state.app.numeric_format, &state.app.template_value_formats);

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
fn render_template_panel(
    ui: &mut egui::Ui,
    id: FileId,
    file: &mut OpenFile,
    numeric_format: crate::settings::NumericFormat,
    template_value_formats: &crate::settings::TemplateValueFormats,
) {
    let has_any = !file.templates.is_empty() || !file.templates_running.is_empty();
    if !has_any || !file.template_panel_visible {
        return;
    }
    let whole_file_len = file.editor.source().len().get();
    egui::Panel::bottom(egui::Id::new(("hxy-template-panel", id.get())))
        .resizable(true)
        .default_size(300.0)
        .min_size(160.0)
        .show_inside(ui, |ui| {
            let events =
                crate::panels::template::show(ui, file, whole_file_len, numeric_format, template_value_formats);
            for e in events {
                apply_template_event(ui, file, e);
            }
        });
}

/// Dispatch one event from the template panel. Pulled out so the
/// per-frame loop above doesn't have to keep ten arms inline; events
/// that target "the active instance" look it up here so the panel
/// renderer can stay borrow-clean.
#[cfg(not(target_arch = "wasm32"))]
fn apply_template_event(ui: &mut egui::Ui, file: &mut OpenFile, event: crate::panels::template::TemplateEvent) {
    use crate::panels::template::TemplateEvent;
    match event {
        TemplateEvent::HidePanel => {
            file.template_panel_visible = false;
        }
        TemplateEvent::SetActive(id) => {
            file.active_template = Some(id);
        }
        TemplateEvent::RemoveInstance(id) => {
            file.templates.retain(|t| t.id != id);
            file.templates_running.retain(|r| r.id != id);
            if file.active_template == Some(id) {
                file.active_template =
                    file.templates.first().map(|t| t.id).or_else(|| file.templates_running.first().map(|r| r.id));
            }
        }
        TemplateEvent::ExpandArray { array_id, count } => {
            if let Some(state) = file.active_template_mut().map(|t| &mut t.state) {
                crate::panels::template::expand_array(state, array_id, count);
            }
        }
        TemplateEvent::ToggleCollapse(idx) => {
            if let Some(state) = file.active_template_mut().map(|t| &mut t.state) {
                crate::panels::template::toggle_collapse(state, idx);
            }
        }
        TemplateEvent::Hover(idx) => {
            if let Some(state) = file.active_template_mut().map(|t| &mut t.state) {
                state.hovered_node = idx;
            }
        }
        TemplateEvent::Select(idx) => {
            select_template_node(file, idx);
        }
        TemplateEvent::Copy { idx, kind } => {
            let Some(state) = file.active_template().map(|t| &t.state) else { return };
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
        TemplateEvent::SaveBytes(idx) => {
            let Some(state) = file.active_template().map(|t| &t.state) else { return };
            let Some(node) = state.tree.nodes.get(idx.0 as usize).cloned() else { return };
            save_template_bytes(file.editor.source(), &node);
        }
        TemplateEvent::ToggleColors(on) => {
            if let Some(state) = file.active_template_mut().map(|t| &mut t.state) {
                state.show_colors = on;
            }
        }
        TemplateEvent::SetColor { idx, color } => {
            if let Some(state) = file.active_template_mut().map(|t| &mut t.state) {
                state.node_color_overrides.insert(idx.0, color);
                crate::panels::template::recompute_leaf_colors(state);
            }
        }
        TemplateEvent::ResetColor(idx) => {
            if let Some(state) = file.active_template_mut().map(|t| &mut t.state) {
                state.node_color_overrides.remove(&idx.0);
                crate::panels::template::recompute_leaf_colors(state);
            }
        }
        TemplateEvent::MoveSelection(delta) => {
            move_template_selection(file, delta);
        }
        TemplateEvent::CollapseSelected => {
            let Some(state) = file.active_template_mut().map(|t| &mut t.state) else { return };
            if let Some(idx) = state.selected_node {
                state.collapsed.insert(idx);
            }
        }
        TemplateEvent::ExpandSelected => {
            let Some(state) = file.active_template_mut().map(|t| &mut t.state) else { return };
            if let Some(idx) = state.selected_node {
                state.collapsed.remove(&idx);
            }
        }
        TemplateEvent::OpenVisualizer(node_idx) => {
            // Set the active visualizer key + flag the panel so the
            // post-dock-pass drain pops the dock tab. Mark the panel
            // as user-opened so the choice rides through template
            // re-runs and (via `sync_tab_state` mirroring the flag
            // into `OpenTabState`) survives a restart.
            let Some(active_id) = file.active_template else { return };
            file.visualizer_panel.active =
                Some(crate::visualizers::VisualizerKey { instance: active_id, node: node_idx });
            file.visualizer_panel.open = true;
            file.visualizer_panel.pending_show = true;
        }
    }
}

/// Set the active template's selected row to `idx` and re-fire the
/// usual byte-selection / scroll side effects so the hex view jumps
/// to the new field. Shared between mouse Select clicks and arrow-
/// key MoveSelection.
#[cfg(not(target_arch = "wasm32"))]
fn select_template_node(file: &mut OpenFile, idx: crate::files::TemplateNodeIdx) {
    let Some(state) = file.active_template_mut().map(|t| &mut t.state) else { return };
    state.selected_node = Some(idx);
    let Some(node) = state.tree.nodes.get(idx.0 as usize) else { return };
    let offset = node.span.offset;
    let length = node.span.length.max(1);
    let end_inclusive = offset.saturating_add(length - 1);
    file.editor.set_selection(Some(hxy_core::Selection {
        anchor: hxy_core::ByteOffset::new(offset),
        cursor: hxy_core::ByteOffset::new(end_inclusive),
    }));
    file.editor.set_scroll_to_byte(hxy_core::ByteOffset::new(offset));
}

/// Move the active template's selection by `delta` positions in the
/// flattened visible row list, skipping non-Node rows (synthesized
/// array elements have no tree-node identity). Wraps to the first /
/// last node row at the ends.
#[cfg(not(target_arch = "wasm32"))]
fn move_template_selection(file: &mut OpenFile, delta: i32) {
    let Some(template) = file.active_template() else { return };
    let state = &template.state;
    let Some(current) = state.selected_node else { return };

    // Recompute the visible list the same way the panel does. It
    // depends on collapsed state and on which deferred arrays are
    // expanded, so we need a fresh walk; the panel's view is
    // ephemeral and not stored on TemplateState.
    let visible = crate::panels::template::visible_node_indices(state);
    if visible.is_empty() {
        return;
    }
    let pos = visible.iter().position(|i| *i == current).unwrap_or_default();
    let next = (pos as i32 + delta).clamp(0, visible.len() as i32 - 1) as usize;
    let next_idx = visible[next];
    select_template_node(file, next_idx);
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
            crate::menu::MenuAction::ReopenClosedTab => {
                crate::tabs::close::reopen_last_closed_tab(ctx, app);
            }
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
            crate::menu::MenuAction::ToggleSettings => app.toggle_settings(),
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
        menu.set_reopen_enabled(!app.closed_tabs.is_empty());
    }
}

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

/// Paste from the OS clipboard at the active tab's cursor. The
/// desktop reads via arboard; on wasm the only entry point is the
/// keyboard shortcut handler which threads `egui::Event::Paste`
/// through `dispatch_paste_shortcut`, so the explicit-palette
/// dispatch on wasm has no clipboard text to source and just
/// no-ops (the user picks the entry from the palette but no
/// platform clipboard text is reachable from here).
pub(crate) fn paste_active_file(app: &mut HxyApp, as_hex: bool) {
    let Some(id) = active_file_id(app) else { return };
    let edit_mode = app.files.get(&id).map(|f| f.editor.edit_mode());
    if edit_mode != Some(crate::files::EditMode::Mutable) {
        return;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
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
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (as_hex, id);
    }
}

pub(crate) fn undo_active_file(app: &mut HxyApp) {
    let Some(id) = active_file_id(app) else { return };
    let Some(file) = app.files.get_mut(&id) else { return };
    if let Some(entry) = file.editor.undo() {
        jump_cursor_to(file, entry.offset);
    }
}

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
/// already on screen. Uses the active template instance's
/// boundaries; switching tabs in the template panel changes which
/// fields the jump traverses.
pub(crate) fn jump_to_template_field(app: &mut HxyApp, forward: bool) {
    let Some(id) = active_file_id(app) else { return };
    let Some(file) = app.files.get_mut(&id) else { return };
    // Templates are desktop-only (the runner needs wasmtime), so on
    // wasm there are no field boundaries to traverse and the jump
    // is a no-op.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Some(template) = file.active_template() else { return };
        let cursor = file.editor.selection().map(|s| s.cursor.get()).unwrap_or(0);
        let target = if forward {
            let idx = template.state.leaf_boundaries.partition_point(|(o, _)| o.get() <= cursor);
            template.state.leaf_boundaries.get(idx).map(|(o, _)| o.get())
        } else {
            let idx = template.state.leaf_boundaries.partition_point(|(o, _)| o.get() < cursor);
            if idx == 0 { None } else { template.state.leaf_boundaries.get(idx - 1).map(|(o, _)| o.get()) }
        };
        let Some(target) = target else { return };
        jump_cursor_to(file, target);
        let target_off = hxy_core::ByteOffset::new(target);
        if !file.editor.is_offset_visible(target_off) {
            file.editor.set_scroll_to_byte(target_off);
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (file, forward);
    }
}

#[cfg(target_os = "macos")]
fn copy_active_file(ctx: &egui::Context, app: &mut HxyApp, kind: CopyKind) {
    let Some(id) = active_file_id(app) else { return };
    let Some(file) = app.files.get(&id) else { return };
    do_copy(ctx, file, kind);
}

#[cfg(not(target_os = "macos"))]
#[cfg(not(target_arch = "wasm32"))]
fn top_menu_bar(ui: &mut egui::Ui, app: &mut HxyApp) {
    use crate::commands::shortcuts::CLOSE_TAB;
    use crate::commands::shortcuts::COPY_BYTES;
    use crate::commands::shortcuts::COPY_HEX;
    use crate::commands::shortcuts::NEW_FILE;
    use crate::commands::shortcuts::PASTE;
    use crate::commands::shortcuts::PASTE_AS_HEX;
    use crate::commands::shortcuts::REDO;
    use crate::commands::shortcuts::REOPEN_CLOSED_TAB;
    use crate::commands::shortcuts::SAVE_FILE;
    use crate::commands::shortcuts::SAVE_FILE_AS;
    use crate::commands::shortcuts::TOGGLE_EDIT_MODE;
    use crate::commands::shortcuts::UNDO;

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
                let reopen_text = ui.ctx().format_shortcut(&REOPEN_CLOSED_TAB);
                let can_reopen = !app.closed_tabs.is_empty();
                ui.add_enabled_ui(can_reopen, |ui| {
                    if ui
                        .add(egui::Button::new(hxy_i18n::t("menu-file-reopen-closed")).shortcut_text(reopen_text))
                        .clicked()
                    {
                        ui.close();
                        crate::tabs::close::reopen_last_closed_tab(ui.ctx(), app);
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
    // Stage inline ghost-completion for the calculator query
    // before handing off to egui_palette. Computed against the
    // active file's templates so `png.<seg>` knows what fields
    // the parsed PNG actually has.
    {
        let templates: &[crate::files::TemplateInstance] =
            app.last_active_file.and_then(|id| app.files.get(&id)).map(|f| f.templates.as_slice()).unwrap_or(&[]);
        let resolver = crate::commands::palette::calculator::TemplateFieldResolver::new(templates);
        app.palette.inner.completion_suggestion =
            crate::commands::palette::completion::compute_suggestion(&app.palette.inner.query, &resolver);
    }
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

        // When the file has an accepted virtual base, the status
        // bar's caret / hover / selection labels render virtual
        // addresses. The tooltip still shows the alternate base of
        // whatever's primary, mirroring the no-vaddr behaviour.
        let format_value = |value: u64, base: crate::settings::OffsetBase| -> String {
            match file.virtual_base {
                Some(v) => crate::view::format::format_offset_with_vaddr(value, base, v),
                None => crate::view::format::format_offset(value, base),
            }
        };
        if let Some(hov) = file.hovered {
            let value = format_value(hov.get(), base);
            crate::view::format::copyable_status_label(
                ui,
                &format!("Hover: {value}"),
                &value,
                Some(format_value(hov.get(), base.toggle())),
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
                let v = format_value(range.start().get(), base);
                (format!("Caret: {v}"), v, format_value(range.start().get(), base.toggle()))
            } else {
                let start = format_value(range.start().get(), base);
                let end = format_value(last_inclusive, base);
                let len = crate::view::format::format_offset(range.len().get(), base);
                let copy_value = format!("{start}-{end} ({len} bytes)");
                let tooltip = format!(
                    "{}-{}",
                    format_value(range.start().get(), base.toggle()),
                    format_value(last_inclusive, base.toggle()),
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

/// Build the tab-bar title for a `Tab::File`, including the
/// load-status / read-only / dirty indicators desktop shows.
/// Universal so the wasm tab viewer renders identical titles.
pub(crate) fn format_file_tab_title(file: &OpenFile) -> String {
    let mut prefix = String::new();
    match &file.load_status {
        crate::files::LoadStatus::Ready => {}
        crate::files::LoadStatus::Loading => {
            prefix.push_str(egui_phosphor::regular::CIRCLE_NOTCH);
            prefix.push(' ');
        }
        crate::files::LoadStatus::Failed(_) => {
            prefix.push_str(egui_phosphor::regular::WARNING);
            prefix.push(' ');
        }
    }
    if matches!(file.editor.edit_mode(), crate::files::EditMode::Readonly) {
        prefix.push_str(egui_phosphor::regular::LOCK);
        prefix.push(' ');
    }
    if file.editor.is_dirty() {
        prefix.push_str("\u{2022} ");
    }
    format!("{prefix}{}", file.display_name)
}

/// Build the tab-bar title for a `Tab::Workspace` -- adds the
/// tree-structure icon to the standard file title.
pub(crate) fn format_workspace_tab_title(file: &OpenFile) -> String {
    let mut prefix = String::from(egui_phosphor::regular::TREE_STRUCTURE);
    prefix.push(' ');
    if matches!(file.editor.edit_mode(), crate::files::EditMode::Readonly) {
        prefix.push_str(egui_phosphor::regular::LOCK);
        prefix.push(' ');
    }
    if file.editor.is_dirty() {
        prefix.push_str("\u{2022} ");
    }
    format!("{prefix}{}", file.display_name)
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

/// Mode dropdown + per-base sub-controls for editing a
/// [`crate::settings::NumericFormat`] in-place. Renders inside a
/// single grid row: a primary mode selector ("Always" / "Switch
/// at threshold") followed by either one base picker or three
/// (small / large / threshold value) for the threshold form.
/// `id_prefix` salts the inner combo boxes so multiple
/// independent NumericFormat editors on the same settings tab
/// don't collide on egui ids.
fn numeric_format_row(ui: &mut egui::Ui, fmt: &mut crate::settings::NumericFormat, id_prefix: &str) {
    use crate::settings::NumericBase;
    use crate::settings::NumericFormat;

    /// Bare mode tag, used so the user-facing dropdown can
    /// preserve the bases / threshold the user already picked
    /// when toggling between modes (egui's `selectable_value`
    /// would replace the whole enum and lose them otherwise).
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Always,
        Threshold,
    }
    let mut mode = match *fmt {
        NumericFormat::Always(_) => Mode::Always,
        NumericFormat::Threshold { .. } => Mode::Threshold,
    };
    let prev_mode = mode;

    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt(format!("{id_prefix}-mode"))
            .selected_text(match mode {
                Mode::Always => hxy_i18n::t("settings-numeric-format-always"),
                Mode::Threshold => hxy_i18n::t("settings-numeric-format-threshold"),
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut mode, Mode::Always, hxy_i18n::t("settings-numeric-format-always"));
                ui.selectable_value(&mut mode, Mode::Threshold, hxy_i18n::t("settings-numeric-format-threshold"));
            });

        if mode != prev_mode {
            // Mode flip: derive sensible defaults from whatever
            // bases / threshold the user had selected, so the
            // pickers don't snap back to an arbitrary default.
            *fmt = match (mode, *fmt) {
                (Mode::Always, NumericFormat::Threshold { large, .. }) => NumericFormat::Always(large),
                (Mode::Threshold, NumericFormat::Always(b)) => {
                    NumericFormat::Threshold { small: NumericBase::Decimal, large: b, threshold: 256 }
                }
                (_, current) => current,
            };
        }

        match fmt {
            NumericFormat::Always(base) => {
                base_combo(ui, format!("{id_prefix}-always-base"), base);
            }
            NumericFormat::Threshold { small, large, threshold } => {
                ui.label(hxy_i18n::t("settings-numeric-format-small-label"));
                base_combo(ui, format!("{id_prefix}-small"), small);
                ui.label(hxy_i18n::t("settings-numeric-format-large-label"));
                base_combo(ui, format!("{id_prefix}-large"), large);
                ui.label(hxy_i18n::t("settings-numeric-format-threshold-label"));
                ui.add(egui::DragValue::new(threshold).range(1..=u64::MAX));
            }
        }
    });
}

/// Per-integer-type editor for [`crate::settings::TemplateValueFormats`].
/// Renders inline as a collapsing header so the eight rows
/// don't dominate the General settings tab; expanding it
/// reveals one [`numeric_format_row`] per integer slot
/// (u8 / u16 / u32 / u64 / s8 / s16 / s32 / s64).
fn template_value_formats_row(ui: &mut egui::Ui, fmts: &mut crate::settings::TemplateValueFormats) {
    use crate::settings::IntValueType;

    egui::CollapsingHeader::new(hxy_i18n::t("settings-template-value-format-collapsed-label"))
        .id_salt("hxy-template-value-formats")
        .default_open(false)
        .show(ui, |ui| {
            egui::Grid::new("hxy-template-value-formats-grid").num_columns(2).striped(true).show(ui, |ui| {
                for ty in IntValueType::all() {
                    ui.monospace(ty.label());
                    let id_prefix = format!("hxy-template-value-format-{}", ty.label());
                    numeric_format_row(ui, fmts.slot_mut(*ty), &id_prefix);
                    ui.end_row();
                }
            });
        });
}

fn base_combo(ui: &mut egui::Ui, id: String, base: &mut crate::settings::NumericBase) {
    use crate::settings::NumericBase;
    egui::ComboBox::from_id_salt(id)
        .selected_text(match base {
            NumericBase::Hex => "Hex",
            NumericBase::Decimal => "Decimal",
        })
        .show_ui(ui, |ui| {
            ui.selectable_value(base, NumericBase::Hex, "Hex");
            ui.selectable_value(base, NumericBase::Decimal, "Decimal");
        });
}

fn settings_ui(
    ui: &mut egui::Ui,
    settings: &mut crate::settings::AppSettings,
    files: &mut HashMap<FileId, OpenFile>,
    byte_cache: &Arc<hxy_core::ByteCache>,
) {
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

        ui.label(hxy_i18n::t("settings-numeric-format"));
        numeric_format_row(ui, &mut settings.numeric_format, "hxy-numeric-format");
        ui.end_row();

        ui.label(hxy_i18n::t("settings-template-value-format"));
        template_value_formats_row(ui, &mut settings.template_value_formats);
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

    ui.add_space(12.0);
    ui.heading(hxy_i18n::t("settings-memory-header"));
    ui.separator();
    egui::Grid::new("hxy-memory-settings").num_columns(2).striped(true).show(ui, |ui| {
        ui.label(hxy_i18n::t("settings-byte-cache-limit"));
        let mut mib = settings.byte_cache_limit_mib.max(hxy_core::CacheLimit::MIN_MIB);
        let response = ui.add(
            egui::DragValue::new(&mut mib).range(hxy_core::CacheLimit::MIN_MIB..=u32::MAX).speed(8.0).suffix(" MiB"),
        );
        response.on_hover_text(hxy_i18n::t("settings-byte-cache-limit-tooltip"));
        if mib != settings.byte_cache_limit_mib {
            settings.byte_cache_limit_mib = mib;
            byte_cache.set_limit(hxy_core::CacheLimit::from_mib(mib));
        }
        ui.end_row();
    });
    let _ = files;
}

// =====================================================================
