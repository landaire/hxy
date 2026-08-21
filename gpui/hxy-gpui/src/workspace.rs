//! The GPUI workbench root view: owns a [`DockArea`] whose center tabs
//! hold one [`FilePanel`] per open file (or a [`WelcomePanel`] when
//! empty), plus the bottom status bar reflecting the active tab. Drives
//! file-open (CLI + `cmd-o`), the `cmd-alt-v` vim toggle, live
//! system-theme sync, the window title, and debounced layout
//! persistence.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::App;
use gpui::Axis;
use gpui::Context;
use gpui::Entity;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::PathPromptOptions;
use gpui::Render;
use gpui::SharedString;
use gpui::Styled;
use gpui::Subscription;
use gpui::Task;
use gpui::WeakEntity;
use gpui::Window;
use gpui::actions;
use gpui::div;
use gpui::prelude::*;
use gpui::px;
use gpui_component::ActiveTheme;
use gpui_component::WindowExt;
use gpui_component::button::Button;
use gpui_component::dock::DockArea;
use gpui_component::dock::DockEvent;
use gpui_component::dock::DockItem;
use gpui_component::dock::DockPlacement;
use gpui_component::dock::PanelInfo;
use gpui_component::dock::PanelRegistry;
use gpui_component::dock::PanelState;
use gpui_component::dock::PanelView;
use gpui_component::h_flex;
use gpui_component::label::Label;
use gpui_component::notification::Notification;
use gpui_component::v_flex;
use gpui_dock_picker::DockPicker;
use gpui_dock_picker::PickTarget;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;
use hxy_editor::EditMode;
use hxy_editor::InputMode;
use hxy_view_gpui::HexPane;

use crate::menu::CloseTab;
use crate::menu::CopyBytes;
use crate::menu::CopyHex;
use crate::menu::Redo;
use crate::menu::ShowAbout;
use crate::menu::ToggleEditMode;
use crate::menu::Undo;
use crate::palette::Palette;
use crate::palette::apply;
use crate::palette::modes::CompareSide;
use crate::palette::modes::CopyFormat;
use crate::palette::modes::PaletteAction;
use crate::palette::modes::PaletteContext;
use crate::panels::CHECKSUMS_PANEL_NAME;
use crate::panels::COMPARE_PANEL_NAME;
use crate::panels::ChecksumsPanel;
use crate::panels::ComparePanel;
use crate::panels::ENTROPY_PANEL_NAME;
use crate::panels::EntropyPanel;
use crate::panels::FILE_PANEL_NAME;
use crate::panels::FilePanel;
use crate::panels::GLOBAL_SEARCH_PANEL_NAME;
use crate::panels::GlobalSearchPanel;
use crate::panels::INSPECTOR_PANEL_NAME;
use crate::panels::InspectorPanel;
use crate::panels::OpenRecentRequested;
use crate::panels::OpenVisualizerRequested;
use crate::panels::SETTINGS_PANEL_NAME;
use crate::panels::STRINGS_PANEL_NAME;
use crate::panels::SettingsPanel;
use crate::panels::StringsPanel;
use crate::panels::VISUALIZER_PANEL_NAME;
use crate::panels::VisualizerPanel;
use crate::panels::WELCOME_PANEL_NAME;
use crate::panels::WORKSPACE_HOST_PANEL_NAME;
use crate::panels::WelcomePanel;
use crate::panels::WorkspaceHostPanel;
use crate::panels::compare::CompareSideInit;
use crate::panels::compare::leaf_name;
use crate::panels::global_search::GlobalSearchJumped;
use crate::panels::inspector::ActiveHexPane;
use crate::panels::strings::OpenFilePanels;
use crate::panels::strings::StringsJumped;
use crate::persist;
use crate::settings::AppSettings;
use crate::settings::SettingsGlobal;
use crate::settings::update_settings;
use crate::status::dirty_marker;
use crate::status::status_file_name_text;
use crate::status::status_offset_text;
use crate::status::status_open_error_text;
use crate::status::status_vim_mode_text;
use crate::status::window_title_text;
use crate::templates::FieldJump;
use crate::templates::RestoreContext;
use crate::templates::run_template;
use crate::watch::ReloadDecision;

actions!(
    hxy_gpui,
    [
        OpenFile,
        Save,
        SaveAs,
        ReopenClosedTab,
        ToggleVim,
        ToggleInspector,
        ToggleSearch,
        CloseSearch,
        ToggleGlobalSearch,
        OpenPalette,
        PickPane,
        OpenStrings,
        OpenEntropy,
        OpenChecksums,
        OpenSettings,
        TakeSnapshot,
        OpenSnapshots
    ]
);

/// LIFO ring capacity for closed-tab reopen (`cmd-shift-t`). Mirrors the
/// egui app's `CLOSED_TABS_CAPACITY` (`crates/hxy/src/tabs/close.rs:21`).
const CLOSED_TABS_CAPACITY: usize = 32;

/// One reopenable closed file tab: its on-disk path plus the view state
/// worth restoring. In-memory only (dropped on quit), mirroring egui's
/// `closed_tabs` ring -- distinct from the on-quit unsaved-patch sidecars
/// (a separate mechanism). Untitled buffers (no path) never enter the
/// ring: there is nothing to reopen them from.
struct ClosedTab {
    path: PathBuf,
    /// Caret offset at close time, re-parked on reopen.
    selection: Option<u64>,
    /// Hex column count at close time.
    columns: hxy_core::ColumnCount,
}

/// The user's answer to the save-before-closing prompt. Dialog dismissal
/// (Escape, overlay click, close icon) resolves as `Cancel` -- the tab
/// stays open (mirrors egui's close dialog treating window-chrome close
/// as Cancel, `crates/hxy/src/tabs/close.rs:439-441`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseDecision {
    Save,
    DontSave,
    Cancel,
}

/// Whether a save targets the tab's existing path when it has one
/// (`Save`) or always prompts for a destination (`SaveAs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveKind {
    Save,
    SaveAs,
}

/// One row rendered in the snapshots dialog: the snapshot id and a
/// pre-formatted "name (N B)" label.
struct SnapshotRow {
    id: hxy_panels::files::snapshot::SnapshotId,
    label: String,
}

/// A staged unsaved-edits restore prompt: the freshly opened file, the
/// sidecar recovered for its path, and how confident we are the on-disk
/// bytes still match what the patch was generated against (drives the
/// prompt's wording). Mirrors egui's `PendingPatchRestore`.
struct PendingRestore {
    file: Entity<FilePanel>,
    sidecar: hxy_panels::files::patch_persist::PatchSidecar,
    integrity: hxy_panels::files::patch_persist::RestoreIntegrity,
}

/// The user's answer to the restore-unsaved-edits prompt. A footer
/// button resolves `Restore` (apply the patch, then drop the sidecar) or
/// `Discard` (drop the sidecar without applying). Dismissing the dialog
/// (Escape / overlay / close icon) resolves neither: the prompt clears
/// but the sidecar stays on disk, so the next open re-offers it --
/// mirroring egui's dialog dropping the pending restore without
/// discarding the sidecar (`crates/hxy/src/app/dialogs.rs:640-643`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestoreDecision {
    Restore,
    Discard,
}

/// Debounce window for coalescing the frequent `LayoutChanged` events
/// into a single layout save.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Default width of the inspector dock when no persisted layout has
/// sized it yet.
const INSPECTOR_DOCK_WIDTH: gpui::Pixels = px(280.0);

/// Register the shell's keybindings. Called once at startup before any
/// window opens.
pub fn init_keybindings(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("cmd-o", OpenFile, None),
        gpui::KeyBinding::new("cmd-s", Save, None),
        gpui::KeyBinding::new("cmd-shift-s", SaveAs, None),
        gpui::KeyBinding::new("cmd-shift-t", ReopenClosedTab, None),
        gpui::KeyBinding::new("cmd-alt-v", ToggleVim, None),
        gpui::KeyBinding::new("cmd-i", ToggleInspector, None),
        gpui::KeyBinding::new("cmd-f", ToggleSearch, None),
        // Mirror the egui app's `FIND_GLOBAL` chord (Cmd+Shift+F).
        gpui::KeyBinding::new("cmd-shift-f", ToggleGlobalSearch, None),
        // Mirror the egui app's `COMMAND_PALETTE` chord (Cmd+Shift+P).
        gpui::KeyBinding::new("cmd-shift-p", OpenPalette, None),
        // Mirror the egui app's Toggle Settings accelerator (Cmd+Comma).
        gpui::KeyBinding::new("cmd-,", OpenSettings, None),
        // Mirror the egui app's `FOCUS_PANE` chord (Cmd+K).
        gpui::KeyBinding::new("cmd-k", PickPane, None),
        // Palette navigation, scoped to the overlay's own key context so
        // these keys are inert everywhere else; the palette's text input
        // is a descendant, so with it focused these still dispatch here.
        gpui::KeyBinding::new("up", crate::palette::PaletteUp, Some("Palette")),
        gpui::KeyBinding::new("down", crate::palette::PaletteDown, Some("Palette")),
        gpui::KeyBinding::new("escape", crate::palette::PaletteDismiss, Some("Palette")),
        // Scoped to the search bar's own key context (set on its
        // render root) so plain Escape elsewhere is left alone; the
        // bar's `InputState`s propagate Escape up to this binding when
        // they don't handle it themselves (not `clean_on_escape`).
        gpui::KeyBinding::new("escape", CloseSearch, Some("SearchBar")),
    ]);
    crate::panels::template_view::init_keybindings(cx);
}

pub struct Workspace {
    dock: Entity<DockArea>,
    /// The welcome placeholder while it occupies the center; `None`
    /// whenever any file tab is open. Owned so it can be removed by
    /// identity when the first file opens.
    welcome: Option<Entity<WelcomePanel>>,
    /// [`OpenRecentRequested`] subscription for the live welcome
    /// panel; dropped alongside it.
    welcome_sub: Option<Subscription>,
    /// The file panel backing the active center tab; drives `cmd-w` /
    /// `focus_existing_tab`'s "already active" fast path, and the
    /// close-tab dispatch. `None` whenever the active tab is not a
    /// `FilePanel` (welcome, or a `StringsPanel`) -- deliberately never
    /// widened to a fallback (see `active_file_panel`'s doc for why
    /// that was tried and reverted). `reference_active_file` is the
    /// fallback-aware read used for display purposes.
    active_file: Option<Entity<FilePanel>>,
    /// The most recent non-`None` value of `active_file`. Powers
    /// `reference_active_file`'s fallback so the inspector / title /
    /// status bar keep reflecting a file while a `StringsPanel` tab
    /// (which has no `active_file` representation) is focused, until
    /// that file actually closes (`reference_active_file` filters
    /// against `open_files` membership, so this can go stale and be
    /// ignored rather than needing to be cleared eagerly).
    last_active_file: Option<Entity<FilePanel>>,
    /// Repaints the workspace (hence the status bar) when the
    /// reference file's pane's editor changes. Re-established whenever
    /// `reference_active_file`'s result changes.
    active_pane_observe: Option<Subscription>,
    _dock_subscription: Subscription,
    last_title: Option<String>,
    /// Tracked on the root div so `cmd-o` / `cmd-alt-v` stay reachable
    /// even with no pane focused (fresh launch showing Welcome): gpui
    /// dispatches actions from the focused element up to the window
    /// root. The dock's panels are descendants of this div, so their
    /// focus keeps the shortcuts reachable while letting keystrokes
    /// reach the active pane directly.
    focus_handle: FocusHandle,
    /// Consumed on the next `render` (which has the `&mut Window` needed
    /// to move focus): focuses the active file's pane if one exists,
    /// else the workspace handle so `cmd-o` stays reachable with no
    /// file open. Only needed at boot; runtime opens focus via the
    /// dock's own active-tab focusing.
    focus_pending: bool,
    /// Set when a `LayoutChanged` event arrives; consumed by `render`,
    /// which defers reconciliation (welcome presence + active tracking)
    /// to just after the frame, where a `&mut Window` is available.
    needs_reconcile: bool,
    /// `FilePanel` entities the workspace has opened, looked up by path
    /// so a center-cache resync can reinstall the SAME panels (preserving
    /// editor state) instead of rebuilding them from disk. Deduped per
    /// path on open and reset to the live set after each resync rebuild;
    /// closed entities may linger between resyncs (a bounded, distinct-
    /// files-per-session cost, not a correctness issue -- a dead path is
    /// never looked up).
    open_files: Vec<Entity<FilePanel>>,
    /// `StringsPanel` entities the workspace has opened, mirroring
    /// `open_files`'s registry shape (dedup by owning path, refreshed
    /// after a center-cache rebuild). Needed because gpui-component's
    /// `DockItem::Tabs.items` cache is not authoritative for
    /// incremental adds (see `resolve_leaf`'s doc) -- there is no other
    /// reliable way to get a live `Entity<StringsPanel>` handle back
    /// out of the dock for open-or-focus dedup / close-cascade.
    strings_panels: Vec<Entity<StringsPanel>>,
    /// One [`StringsJumped`](crate::panels::strings::StringsJumped)
    /// subscription per entry in `strings_panels`, kept alive so
    /// `on_strings_panel_jumped` fires for row clicks -- see
    /// `track_strings_panel`. Never pruned individually (a dropped
    /// source entity just makes its subscription inert), only replaced
    /// wholesale alongside `strings_panels` on a center-cache rebuild.
    strings_panel_subs: Vec<Subscription>,
    /// `EntropyPanel` entities the workspace has opened. Same registry
    /// shape and staleness caveat as `strings_panels` (needed for the
    /// same reason: `DockItem::Tabs.items` isn't authoritative for
    /// incremental adds -- see `resolve_leaf`'s doc), minus a
    /// subscription list: `EntropyPanel` has no jump event to observe
    /// (egui's entropy panel has no click-to-jump either).
    entropy_panels: Vec<Entity<EntropyPanel>>,
    /// `ChecksumsPanel` entities the workspace has opened. Same
    /// registry shape and staleness caveat as `entropy_panels` (no
    /// jump event either).
    checksums_panels: Vec<Entity<ChecksumsPanel>>,
    /// `VisualizerPanel` entities the workspace has opened. Same
    /// registry shape and staleness caveat as `entropy_panels`.
    visualizer_panels: Vec<Entity<VisualizerPanel>>,
    /// One [`OpenVisualizerRequested`] subscription per entry in
    /// `open_files`, so a template row's visualizer marker click
    /// reaches `on_open_visualizer_request`. Same lifecycle rules as
    /// `strings_panel_subs`: never pruned individually, replaced
    /// wholesale whenever `open_files` is rebuilt.
    file_visualizer_subs: Vec<Subscription>,
    /// The live `GlobalSearchPanel`, if the user has opened one this
    /// session. A true singleton (unlike the per-file registries above):
    /// there is at most one at a time. May point at a closed/stale
    /// entity between opens -- liveness is confirmed against the dock
    /// dump before reuse, see `open_global_search_panel`.
    global_search_panel: Option<Entity<GlobalSearchPanel>>,
    /// Kept alive so `GlobalSearchJumped` events from the current
    /// `global_search_panel` keep reaching `on_global_search_jumped`.
    /// Replaced (dropping the old one) each time a fresh panel is built.
    _global_search_sub: Option<Subscription>,
    /// The live `SettingsPanel`, if one is open. Same singleton
    /// contract as `global_search_panel`: liveness is confirmed
    /// against the dock dump before reuse, see `open_settings_panel`.
    settings_panel: Option<Entity<SettingsPanel>>,
    layout_path: Option<PathBuf>,
    /// The in-flight debounced save; dropping it (on the next event)
    /// cancels the pending write.
    save_debounce: Option<Task<()>>,
    /// The in-flight ImHex-Patterns download, if any. Guards against
    /// starting a second concurrent fetch; cleared on completion.
    patterns_fetch: Option<Task<()>>,
    /// `None` when the platform watcher failed to start (rare --
    /// notify setup can fail on some sandboxes); external changes
    /// simply go undetected in that case, same as egui's fallback.
    file_watch: Option<crate::watch::FileWatch>,
    /// The reload prompt currently shown, if any. Only one at a time
    /// -- see [`crate::watch::PendingReloadPrompt`]'s doc.
    pending_reload: Option<crate::watch::PendingReloadPrompt>,
    /// The file whose save-before-closing prompt is currently shown, if
    /// any. The dialog's buttons resolve it via [`Self::resolve_close`];
    /// dismissal resolves as `Cancel`. Only one at a time (a second
    /// `cmd-w` while it's up is dropped by [`Self::close_active_tab`]).
    pending_close: Option<Entity<FilePanel>>,
    /// LIFO ring of recently closed file tabs, capped at
    /// [`CLOSED_TABS_CAPACITY`]. `cmd-shift-t` pops the most recent.
    closed_tabs: std::collections::VecDeque<ClosedTab>,
    /// The unsaved-edits restore prompt currently shown, if any, staged
    /// when a freshly opened file has a sidecar from a previous session.
    /// One at a time.
    pending_restore: Option<PendingRestore>,
    /// Session-restored file tabs still awaiting a patch-restore prompt.
    /// A restored layout never routes its tabs through `open_or_focus`,
    /// so `build_initial` enqueues them here and drains one prompt at a
    /// time (`stage_next_restore`), advancing as each is resolved.
    restore_queue: std::collections::VecDeque<Entity<FilePanel>>,
    /// Kept alive so the on-quit unsaved-patch persistence hook stays
    /// registered for the workspace's lifetime.
    _quit_subscription: Subscription,
    /// The file-watch reconcile-and-drain loop. Held so dropping the
    /// workspace cancels it; never read otherwise.
    _watch_poll_task: Option<Task<()>>,
    _appearance_subscription: Subscription,
    /// Fires [`Self::on_settings_changed`] whenever `update_settings`
    /// republishes [`SettingsGlobal`], live-applying the mutated
    /// fields to open panes / the watcher.
    _settings_observe: Subscription,
    /// The settings snapshot the last live-apply pass ran against, so
    /// `on_settings_changed` only pushes fields that actually changed
    /// (a recents update must not clobber a palette-set per-pane
    /// column count, for example).
    applied_settings: AppSettings,
    /// Theme darkness observed by the last render, so an appearance
    /// flip re-derives the theme-dependent byte-value palette.
    applied_dark: Option<bool>,
    /// The reload dialog's "always do this for this file" checkbox
    /// state; reset every time a prompt is staged. A shared cell
    /// rather than a plain field because the dialog builder runs
    /// inside this workspace's own render pass, where reading the
    /// entity would panic.
    reload_remember: std::rc::Rc<std::cell::Cell<bool>>,
    /// The command-palette overlay. Always childed by `render`; renders
    /// an inert empty element while closed (see [`Palette`]).
    palette: Entity<Palette>,
    /// The vimium-style pane-focus picker (`cmd-k`). Always childed by
    /// `render`; renders an inert empty element while inactive.
    pane_picker: Entity<DockPicker>,
    /// Test-only handle to the inspector panel `ensure_inspector_dock`
    /// creates on fresh construction (never populated on the registry-
    /// restore path -- see that method's doc), so integration tests can
    /// observe the inspector through the real workspace instead of only
    /// through the `ActiveHexPane` global directly.
    #[cfg(test)]
    inspector_for_test: Option<Entity<InspectorPanel>>,
}

impl Workspace {
    /// `initial` is the CLI path arguments (unread; each is opened and
    /// deduped the same way `cmd-o` opens paths -- see `build_initial`);
    /// `layout_path` is where the dock layout persists (injectable for
    /// tests; production passes [`persist::layout_path`]).
    pub fn new(
        initial: Vec<PathBuf>,
        appearance_subscription: Subscription,
        layout_path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let dock = cx.new(|cx| DockArea::new("workspace", Some(persist::LAYOUT_VERSION), window, cx));
        let dock_subscription = cx.subscribe(&dock, |workspace, _dock, event: &DockEvent, cx| match event {
            DockEvent::LayoutChanged => {
                workspace.needs_reconcile = true;
                workspace.schedule_save(cx);
                cx.notify();
            }
            DockEvent::DragDrop(_) => {}
        });

        let palette = {
            let weak = cx.entity().downgrade();
            cx.new(|cx| Palette::new(weak, window, cx))
        };
        let pane_picker = cx.new(DockPicker::new);

        let settings_observe = cx.observe_global::<SettingsGlobal>(|workspace, cx| workspace.on_settings_changed(cx));
        let boot_settings = crate::settings::settings(cx);

        let mut workspace = Self {
            dock,
            welcome: None,
            welcome_sub: None,
            active_file: None,
            last_active_file: None,
            active_pane_observe: None,
            _dock_subscription: dock_subscription,
            last_title: None,
            focus_handle: cx.focus_handle(),
            focus_pending: true,
            needs_reconcile: false,
            open_files: Vec::new(),
            strings_panels: Vec::new(),
            strings_panel_subs: Vec::new(),
            entropy_panels: Vec::new(),
            checksums_panels: Vec::new(),
            visualizer_panels: Vec::new(),
            file_visualizer_subs: Vec::new(),
            global_search_panel: None,
            _global_search_sub: None,
            settings_panel: None,
            layout_path,
            save_debounce: None,
            patterns_fetch: None,
            file_watch: None,
            pending_reload: None,
            pending_close: None,
            closed_tabs: std::collections::VecDeque::with_capacity(CLOSED_TABS_CAPACITY),
            pending_restore: None,
            restore_queue: std::collections::VecDeque::new(),
            _quit_subscription: register_quit_persistence(cx),
            _watch_poll_task: None,
            _appearance_subscription: appearance_subscription,
            _settings_observe: settings_observe,
            applied_settings: boot_settings.clone(),
            applied_dark: None,
            reload_remember: std::rc::Rc::new(std::cell::Cell::new(false)),
            palette,
            pane_picker,
            #[cfg(test)]
            inspector_for_test: None,
        };
        workspace.file_watch = match crate::watch::FileWatch::new(crate::watch::polling_prefs(&boot_settings)) {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::warn!(error = %e, "filesystem watcher unavailable; external changes will go undetected");
                None
            }
        };
        workspace._watch_poll_task = Some(spawn_watch_poll(window, cx));
        workspace.build_initial(initial, window, cx);
        workspace
    }

    /// Populate the dock at construction: a restored layout comes back
    /// first, then every CLI path opens on top of it (deduped against
    /// the restore and against each other, same as `cmd-o` -- see
    /// `open_or_focus`); with neither, reconciliation adds the welcome
    /// placeholder.
    fn build_initial(&mut self, initial: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let restored = self
            .layout_path
            .as_ref()
            .and_then(|path| persist::load(path))
            .filter(|state| state.version == Some(persist::LAYOUT_VERSION));

        if let Some(mut state) = restored {
            let pruned = persist::prune_for_restore(&mut state);
            if let Err(err) = self.dock.update(cx, |dock, cx| dock.load(state, window, cx)) {
                tracing::warn!(%err, "load dock layout failed; using default");
                // `Root` (the window's actual top-level view) isn't
                // installed yet -- it wraps this `Workspace` entity
                // after `Workspace::new` returns (see `main.rs`) --
                // so `push_notification` would panic here. Defer to
                // after the current update finishes.
                window.defer(cx, |window, cx| {
                    let text = hxy_i18n::t("gpui-status-layout-restore-failed");
                    window.push_notification(Notification::warning(text), cx);
                });
            }
            // The load-built cache is accurate; register the restored
            // panels so a later resync can reuse them.
            collect_file_entities(self.dock.read(cx).items(), &mut self.open_files);
            self.resubscribe_file_visualizer_events(window, cx);
            // Session-restored tabs bypass `open_or_focus`, so their
            // patch sidecars are never consulted; queue every restored
            // file for a restore prompt drained one at a time at the end
            // of this method.
            self.restore_queue.extend(self.open_files.iter().cloned());
            let mut restored_strings = Vec::new();
            collect_strings_entities(self.dock.read(cx).items(), &mut restored_strings);
            for panel in restored_strings {
                self.track_strings_panel(panel, window, cx);
            }
            let mut restored_entropy = Vec::new();
            collect_entropy_entities(self.dock.read(cx).items(), &mut restored_entropy);
            for panel in restored_entropy {
                self.track_entropy_panel(panel);
            }
            let mut restored_checksums = Vec::new();
            collect_checksums_entities(self.dock.read(cx).items(), &mut restored_checksums);
            for panel in restored_checksums {
                self.track_checksums_panel(panel);
            }
            let mut restored_visualizers = Vec::new();
            collect_visualizer_entities(self.dock.read(cx).items(), &mut restored_visualizers);
            for panel in restored_visualizers {
                self.track_visualizer_panel(panel);
            }
            // Singleton: a restored layout has at most one. Without
            // this, `self.global_search_panel` stays `None` even
            // though the tab is live, so `open_global_search_panel`'s
            // dump-presence check and its `None` registry disagree --
            // `toggle_global_search` would then build a SECOND panel
            // on top of the restored one instead of finding it.
            let mut restored_global_search = Vec::new();
            collect_global_search_entities(self.dock.read(cx).items(), &mut restored_global_search);
            if let Some(panel) = restored_global_search.into_iter().next() {
                self._global_search_sub = Some(cx.subscribe_in(&panel, window, Self::on_global_search_jumped));
                self.global_search_panel = Some(panel);
            }
            // Same singleton recovery for the settings tab (no
            // subscription: the panel talks to the settings global,
            // not the workspace).
            let mut restored_settings = Vec::new();
            collect_settings_entities(self.dock.read(cx).items(), &mut restored_settings);
            self.settings_panel = restored_settings.into_iter().next();
            if !pruned.is_empty() {
                // Deferred like the load-failure toast above: `Root`
                // is not installed yet, so `push_notification` would
                // panic if called inline (see that branch's note).
                window.defer(cx, move |window, cx| {
                    for text in restore_pruned_texts(&pruned) {
                        window.push_notification(Notification::warning(text), cx);
                    }
                });
            }
        }
        // Must run before opening the CLI paths below and before
        // `reconcile` (which calls `set_active_file`, publishing the
        // boot-time active pane into `ActiveHexPane`): the inspector's
        // global subscription has to be live before that first publish,
        // or a CLI-opened file would show an empty inspector until the
        // user switched tabs.
        self.ensure_inspector_dock(window, cx);

        // Each successful open makes its tab the active one (see
        // `open_or_focus` -> `add_file_panel` -> `TabPanel::add_panel`),
        // so the last readable CLI path ends up focused -- mirrors the
        // egui app, where `push_to_focused_leaf` does the same for each
        // path in turn.
        let open_errors: Vec<(PathBuf, std::io::Error)> =
            initial.into_iter().filter_map(|path| self.open_or_focus(path, window, cx).err()).collect();
        if !open_errors.is_empty() {
            // Deferred like the restore toasts above: `Root` is not
            // installed yet.
            window.defer(cx, move |window, cx| {
                for (path, error) in open_errors {
                    window
                        .push_notification(Notification::error(status_open_error_text(&path, &error.to_string())), cx);
                }
            });
        }

        self.reconcile(window, cx);

        // Restored tabs bypass the fresh-open restore staging; offer their
        // sidecars (behind any CLI-opened file's prompt, one at a time).
        // Deferred like the restore toasts above: the dialog layer's
        // `Root` is not installed until after `Workspace::new` returns.
        let this = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            if let Some(this) = this.upgrade() {
                this.update(cx, |workspace, cx| workspace.stage_next_restore(window, cx));
            }
        });
    }

    /// Drain the session-restore prompt queue until one file with a live
    /// sidecar stages a prompt or the queue empties. No-op while a
    /// restore prompt is already up -- resolving it advances the queue.
    fn stage_next_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while self.pending_restore.is_none() {
            let Some(file) = self.restore_queue.pop_front() else { return };
            let Some(path) = file.read(cx).path().map(Path::to_path_buf) else { continue };
            self.maybe_stage_restore(file, &path, window, cx);
        }
    }

    /// Make sure the right dock has an inspector panel. A restored
    /// layout that already had one (from a prior session that opened
    /// it) wins -- `DockArea::load` rebuilt it via the panel registry,
    /// preserving its persisted endian/radix. Otherwise (first launch,
    /// or a layout saved before the inspector existed) install a fresh
    /// one, closed by default.
    fn ensure_inspector_dock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dock.read(cx).has_dock(DockPlacement::Right) {
            return;
        }
        let inspector = cx.new(InspectorPanel::new);
        #[cfg(test)]
        {
            self.inspector_for_test = Some(inspector.clone());
        }
        let inspector: Arc<dyn PanelView> = Arc::new(inspector);
        let weak = self.dock.downgrade();
        let item = DockItem::tabs(vec![inspector], &weak, window, cx);
        self.dock.update(cx, |dock, cx| dock.set_right_dock(item, Some(INSPECTOR_DOCK_WIDTH), false, window, cx));
    }

    fn on_toggle_inspector(&mut self, _: &ToggleInspector, window: &mut Window, cx: &mut Context<Self>) {
        self.dock.update(cx, |dock, cx| dock.toggle_dock(DockPlacement::Right, window, cx));
    }

    fn on_toggle_global_search(&mut self, _: &ToggleGlobalSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_global_search(window, cx);
    }

    fn on_open_strings(&mut self, _: &OpenStrings, window: &mut Window, cx: &mut Context<Self>) {
        self.open_strings_for_active_file(window, cx);
    }

    fn on_open_entropy(&mut self, _: &OpenEntropy, window: &mut Window, cx: &mut Context<Self>) {
        self.open_entropy_for_active_file(window, cx);
    }

    fn on_open_checksums(&mut self, _: &OpenChecksums, window: &mut Window, cx: &mut Context<Self>) {
        self.open_checksums_for_active_file(window, cx);
    }

    /// Capture the reference file's current patched bytes as a snapshot,
    /// toasting the new id. FILE-SCOPED. No-op with no reference file.
    fn on_take_snapshot(&mut self, _: &TakeSnapshot, window: &mut Window, cx: &mut Context<Self>) {
        self.capture_active_snapshot(String::new(), window, cx);
    }

    /// Open the per-file snapshots dialog for the reference file.
    fn on_open_snapshots(&mut self, _: &OpenSnapshots, window: &mut Window, cx: &mut Context<Self>) {
        self.open_snapshots_dialog(window, cx);
    }

    /// Capture the reference file's patched bytes as a named snapshot
    /// (empty name -> `Snapshot N`). Toasts the new id, or the "no store"
    /// message for a pathless buffer. Mirrors egui's `capture_snapshot`.
    pub(crate) fn capture_active_snapshot(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.reference_active_file(cx) else { return };
        if !file.read(cx).has_snapshot_store() {
            window.push_notification(Notification::warning(hxy_i18n::t("snapshot-no-store")), cx);
            return;
        }
        let id = file.update(cx, |file, cx| file.capture_snapshot(name, cx));
        if let Some(id) = id {
            let text = hxy_i18n::t_args("snapshot-capture-toast", &[("id", &id.get().to_string())]);
            window.push_notification(Notification::info(text), cx);
        }
    }

    /// Spawn a Compare tab between one snapshot's frozen bytes (side A,
    /// the older capture) and the file's live patched bytes (side B).
    /// Both sides are non-restorable (frozen in-memory buffers, dropped on
    /// layout restore). Mirrors egui's snapshot "Compare with current".
    pub(crate) fn compare_snapshot_with_current(
        &mut self,
        file: Entity<FilePanel>,
        id: hxy_panels::files::snapshot::SnapshotId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(snap_bytes) = file.read(cx).snapshot_bytes(id) else { return };
        let snap_name = file.read(cx).snapshots().iter().find(|s| s.id == id).map(|s| s.name.clone());
        let current = read_pane_bytes(&file.read(cx).pane().clone(), cx);
        let a = CompareSideInit {
            name: snap_name.unwrap_or_else(|| hxy_i18n::t("snapshot-pick-empty")),
            bytes: snap_bytes.as_ref().clone(),
            restore_path: None,
        };
        let b = CompareSideInit { name: hxy_i18n::t("snapshot-pick-current"), bytes: current, restore_path: None };
        self.resync_center_if_stale(window, cx);
        let panel = cx.new(|cx| ComparePanel::from_sources(a, b, window, cx));
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// Open the per-file snapshots dialog: a list of captures (name, size,
    /// cache state) each with Compare-with-current and Delete, plus a Take
    /// button. Rebuilt on each mutating action (close + reopen) so the
    /// list stays current. No-op with no reference file; a pathless buffer
    /// gets the "no store" message instead of a list.
    pub(crate) fn open_snapshots_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.reference_active_file(cx) else { return };
        let has_store = file.read(cx).has_snapshot_store();
        let name = file
            .read(cx)
            .path()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| hxy_i18n::t("gpui-file-untitled"));
        let rows: Vec<SnapshotRow> = file
            .read(cx)
            .snapshots()
            .iter()
            .map(|s| SnapshotRow { id: s.id, label: format!("{} ({} B)", s.name, s.byte_len) })
            .collect();
        let weak = cx.entity().downgrade();
        let file_weak = file.downgrade();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let mut body = v_flex().gap_2().min_w(px(360.0));
            if !has_store {
                body = body.child(Label::new(hxy_i18n::t("snapshot-no-store")).text_color(cx.theme().muted_foreground));
            } else if rows.is_empty() {
                body = body.child(Label::new(hxy_i18n::t("snapshot-empty")).text_color(cx.theme().muted_foreground));
            } else {
                for row in &rows {
                    body = body.child(
                        h_flex().gap_2().items_center().justify_between().child(Label::new(row.label.clone())).child(
                            h_flex()
                                .gap_1()
                                .child(snapshot_compare_button(&weak, &file_weak, row.id))
                                .child(snapshot_delete_button(&weak, &file_weak, row.id)),
                        ),
                    );
                }
            }
            let weak_for_footer = weak.clone();
            dialog.title(hxy_i18n::t_args("snapshot-dialog-title", &[("name", &name)])).child(body).footer(
                move |_ok, _cancel, _window, _cx| {
                    if has_store { vec![snapshot_take_button(&weak_for_footer)] } else { Vec::new() }
                },
            )
        });
    }

    /// Open (or focus an existing) `StringsPanel` tab for the
    /// reference file (see `reference_active_file`'s doc -- FILE-
    /// SCOPED: "find strings" while already looking at that file's own
    /// strings tab, or any other non-file tab, still targets it). No-op
    /// with no reference file (callers gate on `has_active_file`, same
    /// as the other file-scoped palette entries).
    pub(crate) fn open_strings_for_active_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.reference_active_file(cx) else { return };
        let path = file.read(cx).path().map(Path::to_path_buf);

        if let Some(panel) = self.open_strings_panel_for_path(path.as_deref(), cx) {
            self.focus_strings_tab(panel, window, cx);
            return;
        }

        let pane = file.read(cx).pane().clone();
        self.resync_center_if_stale(window, cx);
        let panel = cx.new(|cx| StringsPanel::new(pane, path, window, cx));
        self.track_strings_panel(panel.clone(), window, cx);
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// Register `panel` in `strings_panels` (if not already tracked)
    /// and subscribe to its `StringsJumped` events, so a row click
    /// brings the owning file's tab to the front. Called for every
    /// `StringsPanel` the workspace discovers: freshly opened
    /// (`open_strings_for_active_file`), restored at boot
    /// (`build_initial`), and rebuilt by a center-cache resync
    /// (`rebuild_center_cache`).
    fn track_strings_panel(&mut self, panel: Entity<StringsPanel>, window: &mut Window, cx: &mut Context<Self>) {
        if self.strings_panels.iter().any(|p| p.entity_id() == panel.entity_id()) {
            return;
        }
        let sub = cx.subscribe_in(&panel, window, Self::on_strings_panel_jumped);
        self.strings_panel_subs.push(sub);
        self.strings_panels.push(panel);
    }

    /// Bring the owning file's tab to the front after a strings-panel
    /// row jump. No-op if the owning path is unset or its file isn't
    /// open (shouldn't happen in practice: the panel can't have
    /// applied a jump without a live `owning_pane`, which only exists
    /// while its file is open).
    fn on_strings_panel_jumped(
        &mut self,
        panel: &Entity<StringsPanel>,
        _event: &StringsJumped,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = panel.read(cx).owning_path().map(Path::to_path_buf) else { return };
        let Some(file) = self.open_file_for_path(&path, cx) else { return };
        self.focus_existing_tab(file, window, cx);
    }

    /// The live `StringsPanel` for `path` if it already has a tab, else
    /// `None`. Confirms liveness against `dump()` (reliable, unlike the
    /// `DockItem` cache -- see `strings_panels`'s doc) before mapping to
    /// the registered entity, mirroring `open_file_for_path`.
    fn open_strings_panel_for_path(&self, path: Option<&Path>, cx: &App) -> Option<Entity<StringsPanel>> {
        let dump = self.dock.read(cx).dump(cx);
        if !dump_has_strings_path(&dump.center, path) {
            return None;
        }
        self.strings_panels.iter().rev().find(|panel| panel.read(cx).owning_path() == path).cloned()
    }

    /// Bring an already-open strings tab to the foreground: same
    /// "already active, just refocus, else resync-then-remove-then-
    /// re-add" shape as `focus_existing_tab` (including the same
    /// stale-cache hazard that guard exists for -- an incremental add
    /// can route into a detached, orphaned `TabPanel` reference left
    /// over from an unrelated collapse elsewhere in the tree; see
    /// `resync_center_if_stale`'s doc), using `active_strings_panel`
    /// (a live query) rather than a tracked "currently active strings
    /// tab" field the way `focus_existing_tab` uses `self.active_file`.
    ///
    /// One difference from `focus_existing_tab`: a resync here can
    /// rebuild (not reuse) `panel`'s entity -- `StringsPanel` isn't in
    /// `resolve_leaf`'s FilePanel-only reuse map, see its doc -- so
    /// `panel` is re-resolved by owning path afterward rather than
    /// trusting it to still be the live entity `strings_panels` points
    /// at.
    fn focus_strings_tab(&mut self, panel: Entity<StringsPanel>, window: &mut Window, cx: &mut Context<Self>) {
        if active_strings_panel(self.dock.read(cx).items(), cx).as_ref().map(Entity::entity_id)
            == Some(panel.entity_id())
        {
            window.focus(&panel.read(cx).focus_handle(cx));
            return;
        }
        let path = panel.read(cx).owning_path().map(Path::to_path_buf);
        self.resync_center_if_stale(window, cx);
        let panel = self.strings_panels.iter().find(|p| p.read(cx).owning_path() == path.as_deref()).cloned();
        let Some(panel) = panel else { return };
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.remove_panel(view.clone(), DockPlacement::Center, window, cx));
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// Open (or focus an existing) `EntropyPanel` tab for the
    /// reference file. Mirrors `open_strings_for_active_file` exactly,
    /// minus the jump-event subscription `EntropyPanel` has no
    /// equivalent of.
    pub(crate) fn open_entropy_for_active_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.reference_active_file(cx) else { return };
        let path = file.read(cx).path().map(Path::to_path_buf);

        if let Some(panel) = self.open_entropy_panel_for_path(path.as_deref(), cx) {
            self.focus_entropy_tab(panel, window, cx);
            return;
        }

        let pane = file.read(cx).pane().clone();
        self.resync_center_if_stale(window, cx);
        let panel = cx.new(|cx| EntropyPanel::new(pane, path, window, cx));
        self.track_entropy_panel(panel.clone());
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// Register `panel` in `entropy_panels` if not already tracked.
    /// Called for every `EntropyPanel` the workspace discovers: freshly
    /// opened (`open_entropy_for_active_file`), restored at boot
    /// (`build_initial`), and rebuilt by a center-cache resync
    /// (`rebuild_center_cache`).
    fn track_entropy_panel(&mut self, panel: Entity<EntropyPanel>) {
        if self.entropy_panels.iter().any(|p| p.entity_id() == panel.entity_id()) {
            return;
        }
        self.entropy_panels.push(panel);
    }

    /// The live `EntropyPanel` for `path` if it already has a tab, else
    /// `None`. Mirrors `open_strings_panel_for_path`.
    fn open_entropy_panel_for_path(&self, path: Option<&Path>, cx: &App) -> Option<Entity<EntropyPanel>> {
        let dump = self.dock.read(cx).dump(cx);
        if !dump_has_entropy_path(&dump.center, path) {
            return None;
        }
        self.entropy_panels.iter().rev().find(|panel| panel.read(cx).owning_path() == path).cloned()
    }

    /// Bring an already-open entropy tab to the foreground. Mirrors
    /// `focus_strings_tab`.
    fn focus_entropy_tab(&mut self, panel: Entity<EntropyPanel>, window: &mut Window, cx: &mut Context<Self>) {
        if active_entropy_panel(self.dock.read(cx).items(), cx).as_ref().map(Entity::entity_id)
            == Some(panel.entity_id())
        {
            window.focus(&panel.read(cx).focus_handle(cx));
            return;
        }
        let path = panel.read(cx).owning_path().map(Path::to_path_buf);
        self.resync_center_if_stale(window, cx);
        let panel = self.entropy_panels.iter().find(|p| p.read(cx).owning_path() == path.as_deref()).cloned();
        let Some(panel) = panel else { return };
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.remove_panel(view.clone(), DockPlacement::Center, window, cx));
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// Open (or focus an existing) `ChecksumsPanel` tab for the
    /// reference file. Mirrors `open_strings_for_active_file` exactly,
    /// minus the jump-event subscription neither `ChecksumsPanel` nor
    /// `EntropyPanel` have an equivalent of.
    pub(crate) fn open_checksums_for_active_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.reference_active_file(cx) else { return };
        let path = file.read(cx).path().map(Path::to_path_buf);

        if let Some(panel) = self.open_checksums_panel_for_path(path.as_deref(), cx) {
            self.focus_checksums_tab(panel, window, cx);
            return;
        }

        let pane = file.read(cx).pane().clone();
        self.resync_center_if_stale(window, cx);
        let panel = cx.new(|cx| ChecksumsPanel::new(pane, path, window, cx));
        self.track_checksums_panel(panel.clone());
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// Register `panel` in `checksums_panels` if not already tracked.
    /// Called for every `ChecksumsPanel` the workspace discovers:
    /// freshly opened (`open_checksums_for_active_file`), restored at
    /// boot (`build_initial`), and rebuilt by a center-cache resync
    /// (`rebuild_center_cache`).
    fn track_checksums_panel(&mut self, panel: Entity<ChecksumsPanel>) {
        if self.checksums_panels.iter().any(|p| p.entity_id() == panel.entity_id()) {
            return;
        }
        self.checksums_panels.push(panel);
    }

    /// The live `ChecksumsPanel` for `path` if it already has a tab,
    /// else `None`. Mirrors `open_entropy_panel_for_path`.
    fn open_checksums_panel_for_path(&self, path: Option<&Path>, cx: &App) -> Option<Entity<ChecksumsPanel>> {
        let dump = self.dock.read(cx).dump(cx);
        if !dump_has_checksums_path(&dump.center, path) {
            return None;
        }
        self.checksums_panels.iter().rev().find(|panel| panel.read(cx).owning_path() == path).cloned()
    }

    /// Bring an already-open checksums tab to the foreground. Mirrors
    /// `focus_entropy_tab`.
    fn focus_checksums_tab(&mut self, panel: Entity<ChecksumsPanel>, window: &mut Window, cx: &mut Context<Self>) {
        if active_checksums_panel(self.dock.read(cx).items(), cx).as_ref().map(Entity::entity_id)
            == Some(panel.entity_id())
        {
            window.focus(&panel.read(cx).focus_handle(cx));
            return;
        }
        let path = panel.read(cx).owning_path().map(Path::to_path_buf);
        self.resync_center_if_stale(window, cx);
        let panel = self.checksums_panels.iter().find(|p| p.read(cx).owning_path() == path.as_deref()).cloned();
        let Some(panel) = panel else { return };
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.remove_panel(view.clone(), DockPlacement::Center, window, cx));
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// Palette "Show Visualizer panel": open (or focus) the
    /// visualizer tab for the reference file. No specific sub-tab is
    /// requested, so an existing selection (or the first target) wins.
    pub(crate) fn open_visualizer_for_active_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.reference_active_file(cx) else { return };
        self.open_visualizer_for_file(file, None, window, cx);
    }

    /// A template row's visualizer marker was clicked in `panel`'s
    /// template table: open/focus its visualizer tab on that node.
    fn on_open_visualizer_request(
        &mut self,
        panel: &Entity<FilePanel>,
        event: &OpenVisualizerRequested,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_visualizer_for_file(panel.clone(), Some(event.0), window, cx);
    }

    /// Open (or focus an existing) `VisualizerPanel` tab for `file`,
    /// selecting `key`'s sub-tab when given. Mirrors
    /// `open_entropy_for_active_file`'s open-or-focus shape, plus the
    /// active-key handoff.
    fn open_visualizer_for_file(
        &mut self,
        file: Entity<FilePanel>,
        key: Option<hxy_templates::visualize::VisualizerKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = file.read(cx).path().map(Path::to_path_buf);
        if let Some(panel) = self.open_visualizer_panel_for_path(path.as_deref(), cx) {
            if let Some(key) = key {
                panel.update(cx, |panel, cx| panel.set_active(key, cx));
            }
            self.focus_visualizer_tab(panel, window, cx);
            return;
        }
        self.resync_center_if_stale(window, cx);
        let panel = cx.new(|cx| VisualizerPanel::new(file, path, window, cx));
        if let Some(key) = key {
            panel.update(cx, |panel, cx| panel.set_active(key, cx));
        }
        self.track_visualizer_panel(panel.clone());
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// Register `panel` in `visualizer_panels` if not already tracked.
    /// Mirrors `track_entropy_panel` (fresh opens, boot restore,
    /// center-cache rebuild).
    fn track_visualizer_panel(&mut self, panel: Entity<VisualizerPanel>) {
        if self.visualizer_panels.iter().any(|p| p.entity_id() == panel.entity_id()) {
            return;
        }
        self.visualizer_panels.push(panel);
    }

    /// The live `VisualizerPanel` for `path` if it already has a tab,
    /// else `None`. Mirrors `open_entropy_panel_for_path`.
    fn open_visualizer_panel_for_path(&self, path: Option<&Path>, cx: &App) -> Option<Entity<VisualizerPanel>> {
        let dump = self.dock.read(cx).dump(cx);
        if !dump_has_visualizer_path(&dump.center, path) {
            return None;
        }
        self.visualizer_panels.iter().rev().find(|panel| panel.read(cx).owning_path() == path).cloned()
    }

    /// Bring an already-open visualizer tab to the foreground. Mirrors
    /// `focus_entropy_tab`.
    fn focus_visualizer_tab(&mut self, panel: Entity<VisualizerPanel>, window: &mut Window, cx: &mut Context<Self>) {
        if active_visualizer_panel(self.dock.read(cx).items(), cx).as_ref().map(Entity::entity_id)
            == Some(panel.entity_id())
        {
            window.focus(&panel.read(cx).focus_handle(cx));
            return;
        }
        let path = panel.read(cx).owning_path().map(Path::to_path_buf);
        self.resync_center_if_stale(window, cx);
        let panel = self.visualizer_panels.iter().find(|p| p.read(cx).owning_path() == path.as_deref()).cloned();
        let Some(panel) = panel else { return };
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.remove_panel(view.clone(), DockPlacement::Center, window, cx));
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// `cmd-shift-f` / the palette entry: close the global search tab if
    /// one is open, else open (or focus) one. Mirrors egui's
    /// `toggle_global_search` exactly (open-or-close, not just focus).
    pub(crate) fn toggle_global_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.open_global_search_panel(cx) {
            let view: Arc<dyn PanelView> = Arc::new(panel);
            self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
            return;
        }
        self.open_global_search(window, cx);
    }

    /// Open (or focus an existing) `GlobalSearchPanel` tab. Workspace-
    /// scoped like Compare, not file-scoped: it needs no active file
    /// (searches every open one), so callers never gate on
    /// `has_active_file`.
    pub(crate) fn open_global_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.open_global_search_panel(cx) {
            self.focus_global_search_tab(panel, window, cx);
            return;
        }
        self.resync_center_if_stale(window, cx);
        let panel = cx.new(|cx| GlobalSearchPanel::new(window, cx));
        self._global_search_sub = Some(cx.subscribe_in(&panel, window, Self::on_global_search_jumped));
        self.global_search_panel = Some(panel.clone());
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// The live `GlobalSearchPanel` if its tab is still open, else
    /// `None`. Mirrors `open_checksums_panel_for_path` minus the path
    /// filter (there is only ever one).
    fn open_global_search_panel(&self, cx: &App) -> Option<Entity<GlobalSearchPanel>> {
        let dump = self.dock.read(cx).dump(cx);
        if !dump_has_global_search(&dump.center) {
            return None;
        }
        self.global_search_panel.clone()
    }

    /// Bring an already-open global search tab to the foreground.
    /// Mirrors `focus_checksums_tab`.
    fn focus_global_search_tab(
        &mut self,
        panel: Entity<GlobalSearchPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if active_global_search_panel(self.dock.read(cx).items(), cx).as_ref().map(Entity::entity_id)
            == Some(panel.entity_id())
        {
            window.focus(&panel.read(cx).focus_handle(cx));
            return;
        }
        self.resync_center_if_stale(window, cx);
        let Some(panel) = self.global_search_panel.clone() else { return };
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.remove_panel(view.clone(), DockPlacement::Center, window, cx));
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// A global search result row was clicked: bring the matched file's
    /// tab to the front. The selection/scroll were already applied by
    /// `GlobalSearchPanel::jump_to_row` (it owns a direct handle to the
    /// target pane); only the tab focus needs the workspace's dock
    /// handle, which the panel doesn't have. Mirrors
    /// `on_strings_panel_jumped`.
    fn on_global_search_jumped(
        &mut self,
        _panel: &Entity<GlobalSearchPanel>,
        event: &GlobalSearchJumped,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_existing_tab(event.file.clone(), window, cx);
    }

    fn on_open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(window, cx);
    }

    /// Open (or focus an existing) settings tab. Mirrors egui's
    /// `show_settings` singleton: focus the live tab if present, else
    /// push a fresh one into the center. (The egui menu item toggles
    /// close-if-open instead; the gpui entry points all open-or-focus.)
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.open_settings_panel(cx) {
            self.focus_settings_tab(panel, window, cx);
            return;
        }
        self.resync_center_if_stale(window, cx);
        let panel = cx.new(|cx| SettingsPanel::new(window, cx));
        self.settings_panel = Some(panel.clone());
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// The live `SettingsPanel` if its tab is still open, else `None`.
    /// Mirrors `open_global_search_panel` (dump-presence check guards
    /// the possibly-stale singleton handle).
    fn open_settings_panel(&self, cx: &App) -> Option<Entity<SettingsPanel>> {
        let dump = self.dock.read(cx).dump(cx);
        if !dump_has_settings(&dump.center) {
            return None;
        }
        self.settings_panel.clone()
    }

    /// Bring an already-open settings tab to the foreground. Mirrors
    /// `focus_global_search_tab`.
    fn focus_settings_tab(&mut self, panel: Entity<SettingsPanel>, window: &mut Window, cx: &mut Context<Self>) {
        if active_settings_panel(self.dock.read(cx).items(), cx).as_ref().map(Entity::entity_id)
            == Some(panel.entity_id())
        {
            window.focus(&panel.read(cx).focus_handle(cx));
            return;
        }
        self.resync_center_if_stale(window, cx);
        let Some(panel) = self.settings_panel.clone() else { return };
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.remove_panel(view.clone(), DockPlacement::Center, window, cx));
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// The open files that can seed a compare pick: every live center
    /// `FilePanel` that has a path, as `(leaf name, path)`. Read straight
    /// off the live dock tree so a closed file never lingers in the list.
    pub(crate) fn open_compare_choices(&self, cx: &App) -> Vec<(String, PathBuf)> {
        let mut files = Vec::new();
        collect_file_entities(self.dock.read(cx).items(), &mut files);
        let mut out = Vec::new();
        for file in files {
            if let Some(path) = file.read(cx).path().map(Path::to_path_buf) {
                out.push((leaf_name(&path), path));
            }
        }
        out
    }

    /// Spawn a compare tab over two resolved sides. `*_open` marks a side
    /// as sourced from an already-open file (read from its live in-memory
    /// buffer, dropped on restore) versus a disk pick (re-read on
    /// restore). Mirrors the egui picker's `spawn_compare_from_picker`.
    pub(crate) fn open_compare(
        &mut self,
        a_path: PathBuf,
        a_open: bool,
        b_path: PathBuf,
        b_open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let a = self.resolve_compare_side(a_path, a_open, cx);
        let b = self.resolve_compare_side(b_path, b_open, cx);
        self.resync_center_if_stale(window, cx);
        let panel = cx.new(|cx| ComparePanel::from_sources(a, b, window, cx));
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// Read one compare side's bytes. An open-file pick reads the live
    /// (possibly edited) buffer and is marked non-restorable; a disk pick
    /// (or an open-file pick whose tab has since closed) reads from disk
    /// and is disk-restorable.
    fn resolve_compare_side(&self, path: PathBuf, from_open_file: bool, cx: &App) -> CompareSideInit {
        let name = leaf_name(&path);
        if from_open_file && let Some(file) = self.open_file_for_path(&path, cx) {
            let bytes = read_pane_bytes(&file.read(cx).pane().clone(), cx);
            return CompareSideInit { name, bytes, restore_path: None };
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(?path, %err, "compare: read side from disk failed; empty buffer");
                Vec::new()
            }
        };
        // A pick that came from an open file stays non-restorable even
        // when its tab has closed (matches the brief's drop-on-restore
        // rule); a disk browse pick is restorable.
        let restore_path = if from_open_file { None } else { Some(path) };
        CompareSideInit { name, bytes, restore_path }
    }

    /// Open a disk file dialog for one compare side. For side A the
    /// result reopens the palette at the B pick with A pre-set; for side
    /// B it completes the pair (using the A the caller passed in).
    pub(crate) fn compare_browse(
        &mut self,
        side: CompareSide,
        prior_a: Option<(PathBuf, bool)>,
        restore: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let receiver =
            cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            let _ = this.update_in(cx, |workspace, window, cx| {
                let path = match result {
                    Ok(Ok(Some(paths))) => paths.into_iter().next(),
                    _ => None,
                };
                let Some(path) = path else { return };
                match side {
                    CompareSide::A => {
                        let palette = workspace.palette.clone();
                        palette.update(cx, |palette, cx| {
                            palette.open_compare_b_with_a((path, false), restore.clone(), window, cx);
                        });
                    }
                    CompareSide::B => {
                        let Some((a_path, a_open)) = prior_a.clone() else { return };
                        workspace.open_compare(a_path, a_open, path, false, window, cx);
                    }
                }
            });
        })
        .detach();
    }

    /// Add a file tab to the center dock, making it active (the dock
    /// focuses the new tab's pane itself when the active tab changes).
    fn add_file_panel(&mut self, panel: Entity<FilePanel>, window: &mut Window, cx: &mut Context<Self>) {
        // A center add routes through `DockItem::Split.items`; resync that
        // cache from the live tree first so the add never lands in a
        // collapsed-away tab panel (see `resync_center_if_stale`).
        self.resync_center_if_stale(window, cx);
        self.register_open_file(&panel, window, cx);
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// Track a newly opened file for later resync reuse, replacing any
    /// prior entry for the same path (a reopen supersedes) so the
    /// registry does not accumulate duplicate paths. Also subscribes
    /// the panel's [`OpenVisualizerRequested`] events (skipped when
    /// the same entity is re-registered, e.g. a Save As rename, so it
    /// never double-fires).
    fn register_open_file(&mut self, panel: &Entity<FilePanel>, window: &mut Window, cx: &mut Context<Self>) {
        let already_tracked = self.open_files.iter().any(|file| file.entity_id() == panel.entity_id());
        if let Some(path) = panel.read(cx).path().map(Path::to_path_buf) {
            self.open_files.retain(|file| file.read(cx).path() != Some(path.as_path()));
        }
        self.open_files.push(panel.clone());
        if !already_tracked {
            self.file_visualizer_subs.push(cx.subscribe_in(panel, window, Self::on_open_visualizer_request));
        }
    }

    /// Rebuild the [`OpenVisualizerRequested`] subscriptions for the
    /// current `open_files` set. Called wherever that registry is
    /// replaced wholesale (boot restore, center-cache rebuild); the
    /// old subscriptions target dead entities and are dropped.
    fn resubscribe_file_visualizer_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.file_visualizer_subs.clear();
        let files = self.open_files.clone();
        for file in &files {
            self.file_visualizer_subs.push(cx.subscribe_in(file, window, Self::on_open_visualizer_request));
        }
    }

    /// Rebuild the center's cached `DockItem` tree from the live panel
    /// tree when a tab panel has collapsed out from under the cache.
    ///
    /// `DockArea` keeps `items: DockItem` as a cache that it only
    /// rebuilds in `new`/`load`/`set_center`. When a tab panel empties
    /// (its last tab closed -- reachable via the close button or a
    /// drag-created split whose pane is then emptied), `TabPanel`'s
    /// `remove_self_if_empty` detaches it from the live `StackPanel`, but
    /// the cached `DockItem::Split.items` keeps the orphan. A later
    /// `DockArea::add_panel(Center)` iterates that cache and can route
    /// the new panel into the detached tab panel, which never renders --
    /// bricking the center (verified against gpui-component mod.rs:411).
    ///
    /// `DockArea::dump` walks the LIVE tree, giving the exact structure
    /// (splits, tab order, active index) to rebuild. We mirror that
    /// `PanelState` into a fresh `DockItem` tree but install the EXISTING
    /// panel entities (looked up by path in `open_files`) rather than
    /// letting `PanelState::to_item` call `PanelRegistry::build_panel`,
    /// which would re-read every file from disk and throw away in-memory
    /// editor state (dirty edits, vim mode, scroll). Only genuine restore
    /// (boot `load`) constructs fresh panels. We install via `set_center`
    /// (not `load`): only `set_center` re-subscribes the rebuilt subtree
    /// for `LayoutChanged`. Runs only when a cached tab panel has actually
    /// gone empty (rare: after a collapse), so healthy layouts pay
    /// nothing.
    fn resync_center_if_stale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !center_has_empty_tab_panel(self.dock.read(cx).items(), cx) {
            return;
        }
        self.rebuild_center_cache(window, cx);
    }

    /// Whether the center's cached `DockItem` tree (`DockArea::items()`)
    /// is missing a live leaf entirely -- e.g. a user drag-splitting a
    /// pane (`TabPanel::add_panel_at`) grows the LIVE `StackPanel` without
    /// gpui-component ever touching `DockArea`'s cached
    /// `DockItem::Split.items` (verified against the gpui-component 0.5.1
    /// source). `DockArea::dump` walks the live tree, so its tab-leaf
    /// count is ground truth to compare the cache's own leaf count
    /// against.
    ///
    /// Deliberately NOT folded into `resync_center_if_stale`'s general
    /// hazard check (which fires on every `reconcile`, i.e. on every
    /// `DockEvent::LayoutChanged`): a rebuild replaces every live
    /// `TabPanel` entity (`rebuild_item` -> `DockItem::tabs` always
    /// constructs fresh ones), so firing it on every drag-split -- rather
    /// than only when something actually needs an accurate snapshot right
    /// now -- would invalidate in-flight `TabPanel` handles far more
    /// often than today. The pane picker is the one caller that genuinely
    /// needs `items()` to be a complete leaf list at a specific moment
    /// (right before it enumerates targets), so it checks this directly
    /// instead of widening the general resync cadence.
    fn center_cache_missing_a_live_leaf(&self, cx: &App) -> bool {
        let items = self.dock.read(cx).items();
        count_dock_item_tab_leaves(items) != count_panel_state_tab_leaves(&self.dock.read(cx).dump(cx).center)
    }

    /// Rebuild the center's cached `DockItem` tree from the live panel
    /// tree unconditionally (see `resync_center_if_stale`'s doc for why
    /// the cache goes stale, and this fn's reuse of live entities rather
    /// than calling `PanelRegistry::build_panel`). Callers should check a
    /// staleness predicate first (`resync_center_if_stale` for the
    /// "orphaned empty tab panel" hazard, `center_cache_missing_a_live_leaf`
    /// for the "drag-split the cache never learned about" one) -- calling
    /// this unconditionally leaks a `Subscription` into `DockArea`'s
    /// internal subscription list on every call (gpui-component 0.5.1's
    /// `set_center` -> `subscribe_item` never prunes it) and silently
    /// drops any `TabPanel` UI state `dump()` doesn't capture (e.g.
    /// zoom). Sets `focus_pending`, which refocuses the active pane on
    /// the next render; `render` itself skips that refocus while the pane
    /// picker is active, so a rebuild triggered mid-pick session cannot
    /// steal focus back from the picker overlay.
    fn rebuild_center_cache(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.dock.read(cx).dump(cx);
        let mut reusable: HashMap<PathBuf, Arc<dyn PanelView>> = HashMap::new();
        for file in &self.open_files {
            if let Some(path) = file.read(cx).path().map(Path::to_path_buf) {
                // Last write wins: a reopened path maps to its live entity.
                reusable.insert(path, Arc::new(file.clone()));
            }
        }
        let welcome = self.welcome.clone();
        let weak = self.dock.downgrade();
        self.dock.update(cx, |dock, cx| {
            let center = rebuild_item(&state.center, &mut reusable, welcome.as_ref(), &weak, window, cx);
            dock.set_center(center, window, cx);
        });
        // The rebuilt cache is accurate: refresh every registry from
        // it. Neither `StringsPanel`, `EntropyPanel`, nor
        // `ChecksumsPanel` entities are carried over by this rebuild
        // (see `resolve_leaf`'s doc), so the old entries (and
        // `strings_panels`'s `StringsJumped` subscriptions) are dead
        // and must be replaced, not merged.
        let mut live_files = Vec::new();
        collect_file_entities(self.dock.read(cx).items(), &mut live_files);
        self.open_files = live_files;
        self.resubscribe_file_visualizer_events(window, cx);
        let mut live_strings = Vec::new();
        collect_strings_entities(self.dock.read(cx).items(), &mut live_strings);
        self.strings_panels.clear();
        self.strings_panel_subs.clear();
        for panel in live_strings {
            self.track_strings_panel(panel, window, cx);
        }
        let mut live_entropy = Vec::new();
        collect_entropy_entities(self.dock.read(cx).items(), &mut live_entropy);
        self.entropy_panels.clear();
        for panel in live_entropy {
            self.track_entropy_panel(panel);
        }
        let mut live_checksums = Vec::new();
        collect_checksums_entities(self.dock.read(cx).items(), &mut live_checksums);
        self.checksums_panels.clear();
        for panel in live_checksums {
            self.track_checksums_panel(panel);
        }
        let mut live_visualizers = Vec::new();
        collect_visualizer_entities(self.dock.read(cx).items(), &mut live_visualizers);
        self.visualizer_panels.clear();
        for panel in live_visualizers {
            self.track_visualizer_panel(panel);
        }
        // Singleton, so a direct overwrite (rather than a clear-then-
        // track loop) is correct: at most one survives the rebuild,
        // and there is no dedup concern. The old subscription (if any)
        // is dropped here, since it targets a now-detached entity.
        let mut live_global_search = Vec::new();
        collect_global_search_entities(self.dock.read(cx).items(), &mut live_global_search);
        self.global_search_panel = live_global_search.into_iter().next();
        self._global_search_sub = self
            .global_search_panel
            .as_ref()
            .map(|panel| cx.subscribe_in(panel, window, Self::on_global_search_jumped));
        let mut live_settings = Vec::new();
        collect_settings_entities(self.dock.read(cx).items(), &mut live_settings);
        self.settings_panel = live_settings.into_iter().next();
        self.focus_pending = true;
    }

    /// Read `path` and add it as a new file tab. Called by `cmd-o`
    /// (which can pass several paths), CLI boot (`build_initial`), and
    /// by tests. A path that is already open does NOT get a second tab:
    /// the existing one is focused instead (standard editor behavior;
    /// the egui app does the same, though it also offers a
    /// focus/second-copy/cancel dialog that is out of scope here).
    /// Shows a read error as a toast right away; `build_initial` instead
    /// collects errors via `open_or_focus` directly and defers them
    /// (see its doc for why).
    pub fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Err((path, error)) = self.open_or_focus(path, window, cx) {
            window.push_notification(Notification::error(status_open_error_text(&path, &error.to_string())), cx);
        }
        cx.notify();
    }

    /// Shared open logic: focus an already-open tab for `path` instead
    /// of duplicating it, or read the file and add a new tab. Deduping
    /// at the source keeps the resync reuse registry one-entry-per-path,
    /// so a reopened file can never lose its live (possibly dirty)
    /// entity to a rebuild. Returns the read error rather than showing
    /// it, so callers decide how to surface it.
    fn open_or_focus(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), (PathBuf, std::io::Error)> {
        if let Some(existing) = self.open_file_for_path(&path, cx) {
            self.focus_existing_tab(existing, window, cx);
            return Ok(());
        }
        // Deviation from egui: `byte_cache_limit_mib` has nothing to
        // drive here -- opens read the whole file into a
        // `MemorySource`; no `hxy_core::ByteCache` exists in this app.
        match std::fs::read(&path) {
            Ok(bytes) => {
                // Detect a VFS handler against the first ~4 KiB so the
                // "Browse VFS" command can enable itself for this tab
                // (mirrors the egui app's per-open `registry.detect`).
                let handler = crate::panels::workspace_host::detect_handler(cx, &bytes[..bytes.len().min(4096)]);
                let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
                let panel = cx.new(|cx| FilePanel::new(source, Some(path.clone()), window, cx));
                panel.update(cx, |panel, _cx| panel.set_detected_handler(handler));
                self.add_file_panel(panel.clone(), window, cx);
                self.suggest_template_for(&panel, window, cx);
                self.maybe_stage_restore(panel, &path, window, cx);
                // Every successful disk open lands on the welcome
                // screen's recents (egui: `add_open_file`'s
                // `record_recent`), persisted immediately.
                update_settings(cx, |s| s.record_recent(path));
                Ok(())
            }
            Err(error) => {
                tracing::error!(?path, %error, "failed to open file");
                Err((path, error))
            }
        }
    }

    /// The live `FilePanel` for `path` if it already has a tab, else
    /// `None`. Confirms liveness against the dump (reliable, unlike the
    /// `DockItem` cache) before mapping to the registered entity.
    fn open_file_for_path(&self, path: &Path, cx: &App) -> Option<Entity<FilePanel>> {
        let dump = self.dock.read(cx).dump(cx);
        if !dump_has_file_path(&dump.center, path) {
            return None;
        }
        // Most-recently registered entity for the path is the live one.
        self.open_files.iter().rev().find(|file| file.read(cx).path() == Some(path)).cloned()
    }

    /// Bring an already-open file's tab to the foreground. If it is
    /// already the active tab, just take keyboard focus; otherwise
    /// re-add it through the dock (0.5.1 has no public "activate tab"),
    /// which reuses the same entity (state intact), makes it active, and
    /// focuses its pane.
    fn focus_existing_tab(&mut self, existing: Entity<FilePanel>, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_file.as_ref().map(Entity::entity_id) == Some(existing.entity_id()) {
            let handle = existing.read(cx).pane().read(cx).focus_handle(cx);
            window.focus(&handle);
            return;
        }
        self.resync_center_if_stale(window, cx);
        let view: Arc<dyn PanelView> = Arc::new(existing.clone());
        self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
        self.add_file_panel(existing, window, cx);
    }

    /// Reconcile the file watcher's registered paths against the
    /// currently open files, then handle whatever it drained. No-op
    /// with no watcher (`file_watch` is `None` -- see its doc).
    fn poll_file_watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // `open_files` can outlive a tab closed directly through the dock
        // (an X-click that bypasses `close_active_tab`); confirm each path
        // against the live dump before re-registering it, or a closed
        // background file stays watched and fires ghost reload prompts.
        let dump = self.dock.read(cx).dump(cx);
        let settings = crate::settings::settings(cx);
        let live_paths: Vec<PathBuf> = self
            .open_files
            .iter()
            .filter_map(|f| f.read(cx).path().map(Path::to_path_buf))
            .filter(|path| dump_has_file_path(&dump.center, path))
            // `Never` means "don't even watch": no notify registration,
            // no polling cost (egui's `watch_root_for_file` skips
            // enrolment the same way). The reconcile diff unwatches a
            // path the moment its pref flips to Never.
            .filter(|path| settings.auto_reload_for(path) != hxy_settings::AutoReloadMode::Never)
            .collect();
        let Some(file_watch) = self.file_watch.as_mut() else { return };
        let events = file_watch.poll(live_paths.into_iter());
        for event in events {
            self.handle_watch_event(event, window, cx);
        }
    }

    fn handle_watch_event(&mut self, event: crate::watch::WatchEvent, window: &mut Window, cx: &mut Context<Self>) {
        use crate::watch::ExternalChangeKind;
        use crate::watch::WatchTarget;
        match event {
            crate::watch::WatchEvent::Modified(WatchTarget::Filesystem(path)) => {
                self.handle_external_change(path, ExternalChangeKind::Modified, window, cx);
            }
            crate::watch::WatchEvent::Removed(WatchTarget::Filesystem(path)) => {
                self.handle_external_change(path, ExternalChangeKind::Removed, window, cx);
            }
            // A rename surfaces as a removal of the old name, same as
            // egui's `drain_file_watch_events` -- the new name isn't
            // one of our open paths, so there's nothing to reload it
            // into.
            crate::watch::WatchEvent::Renamed { from, .. } => {
                self.handle_external_change(from, ExternalChangeKind::Removed, window, cx);
            }
            // M3 never registers a VFS-entry watch (see `crate::watch`'s
            // module doc), so these can't fire; kept exhaustive rather
            // than wildcarded so a future VFS-watch wire-up can't
            // silently forget this arm.
            crate::watch::WatchEvent::Modified(WatchTarget::Vfs(_))
            | crate::watch::WatchEvent::Removed(WatchTarget::Vfs(_)) => {}
        }
    }

    /// Route one filesystem change to every open tab backed by `path`.
    /// A removal always just toasts (mirrors egui's
    /// `handle_external_change`: there's nothing to reload). A
    /// modification honors the effective auto-reload mode for the
    /// path (per-file `file_watch_prefs` override, else the global
    /// `auto_reload`, egui semantics): `Always` re-reads silently,
    /// `Never` drops the event, `Ask` stages the reload prompt --
    /// dropped if one is already pending (the file stays watched, so
    /// a later change re-fires).
    fn handle_external_change(
        &mut self,
        path: PathBuf,
        kind: crate::watch::ExternalChangeKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        let matches: Vec<Entity<FilePanel>> = self
            .open_files
            .iter()
            .filter(|f| {
                f.read(cx).path().is_some_and(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()) == canonical)
            })
            .cloned()
            .collect();
        // Pref key is the event's path, as in egui's
        // `handle_external_change`; for plain disk opens it equals the
        // panel's recorded path that `poll_file_watch`'s enrolment
        // filter checks.
        let mode = crate::settings::settings(cx).auto_reload_for(&path);
        let mut auto_reloaded = false;
        for file in matches {
            let display_name = leaf_name(&path);
            if matches!(kind, crate::watch::ExternalChangeKind::Removed) {
                let text = hxy_i18n::t_args("reload-prompt-body-removed", &[("name", &display_name)]);
                window.push_notification(Notification::warning(text), cx);
                continue;
            }
            match mode {
                hxy_settings::AutoReloadMode::Always => {
                    if !crate::watch::apply_reload(&file, &path, ReloadDecision::DiscardEdits, cx) {
                        let text = hxy_i18n::t_args("reload-prompt-failed", &[("name", &display_name)]);
                        window.push_notification(Notification::error(text), cx);
                        continue;
                    }
                    self.recompute_panels_for_path(&path, cx);
                    file.update(cx, |panel, cx| panel.rerun_templates(window, cx));
                    auto_reloaded = true;
                }
                hxy_settings::AutoReloadMode::Never => {
                    tracing::debug!(target = %path.display(), "auto-reload set to Never; ignoring change");
                }
                hxy_settings::AutoReloadMode::Ask => {
                    if self.pending_reload.is_some() {
                        continue;
                    }
                    let has_unsaved = file.read(cx).pane().read(cx).editor().is_dirty();
                    self.reload_remember.set(false);
                    self.pending_reload =
                        Some(crate::watch::PendingReloadPrompt { file, display_name, path: path.clone(), has_unsaved });
                    self.open_reload_dialog(window, cx);
                }
            }
        }
        if auto_reloaded && let Some(file_watch) = self.file_watch.as_mut() {
            file_watch.mark_synced(&path);
        }
    }

    /// Show the Reload / Keep My Edits / Ignore dialog for
    /// `self.pending_reload`. No-op if it's unset (shouldn't happen --
    /// only called right after setting it).
    fn open_reload_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = &self.pending_reload else { return };
        let display_name = pending.display_name.clone();
        let path_display = pending.path.display().to_string();
        let has_unsaved = pending.has_unsaved;
        let weak = cx.entity().downgrade();
        let remember_cell = self.reload_remember.clone();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let body_key =
                if has_unsaved { "reload-prompt-body-modified-dirty" } else { "reload-prompt-body-modified-clean" };
            let mut body = v_flex()
                .gap_2()
                .child(Label::new(hxy_i18n::t_args(body_key, &[("name", &display_name)])))
                .child(Label::new(path_display.clone()).text_color(cx.theme().muted_foreground));
            if has_unsaved {
                body = body.child(Label::new(hxy_i18n::t("reload-prompt-warn-unsaved")).text_color(cx.theme().warning));
            }
            // "Always do this for this file": maps the chosen button to
            // a per-file auto-reload pref on resolve (egui's
            // `reload-prompt-remember` checkbox). State lives in the
            // shared cell, not the workspace entity: this builder runs
            // inside the workspace's own render pass, where reading
            // the entity would panic.
            let cell_for_click = remember_cell.clone();
            let weak_for_remember = weak.clone();
            body = body.child(
                gpui_component::checkbox::Checkbox::new("reload-remember")
                    .label(hxy_i18n::t("reload-prompt-remember"))
                    .checked(remember_cell.get())
                    .on_click(move |checked: &bool, _window, cx| {
                        cell_for_click.set(*checked);
                        // Repaint so the checkbox reflects the cell.
                        if let Some(ws) = weak_for_remember.upgrade() {
                            ws.update(cx, |_, cx| cx.notify());
                        }
                    }),
            );
            let weak_for_footer = weak.clone();
            let weak_for_cancel = weak.clone();
            let weak_for_close = weak.clone();
            dialog
                .title(hxy_i18n::t("reload-prompt-title"))
                .child(body)
                .on_cancel(move |_, window, cx| {
                    dismiss_reload_as_ignore(&weak_for_cancel, window, cx);
                    true
                })
                .on_close(move |_, window, cx| dismiss_reload_as_ignore(&weak_for_close, window, cx))
                .footer(move |_ok, _cancel, _window, _cx| {
                    // "Reload (discard edits)" reads oddly on a clean
                    // buffer (nothing to discard), so the label swaps --
                    // mirrors egui's `reload-prompt-discard` /
                    // `reload-prompt-reload` split. "Keep my edits" is
                    // hidden on a clean buffer for the same reason egui
                    // hides it: the choice collapses into Ignore.
                    let reload_key = if has_unsaved { "reload-prompt-discard" } else { "reload-prompt-reload" };
                    let mut buttons = vec![reload_button(
                        "reload-discard",
                        hxy_i18n::t(reload_key),
                        weak_for_footer.clone(),
                        ReloadDecision::DiscardEdits,
                    )];
                    if has_unsaved {
                        buttons.push(reload_button(
                            "reload-keep",
                            hxy_i18n::t("reload-prompt-keep"),
                            weak_for_footer.clone(),
                            ReloadDecision::KeepEdits,
                        ));
                    }
                    buttons.push(reload_button(
                        "reload-ignore",
                        hxy_i18n::t("reload-prompt-ignore"),
                        weak_for_footer.clone(),
                        ReloadDecision::Ignore,
                    ));
                    buttons
                })
        });
    }

    /// Apply the user's reload choice: re-read disk bytes into the
    /// pane (Discard/Keep) or leave them alone (Ignore), bump the
    /// watcher's snapshot so it doesn't immediately re-fire, and
    /// recompute any analysis panels already open for the file.
    fn resolve_reload(&mut self, decision: ReloadDecision, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_reload.take() else { return };
        if self.reload_remember.replace(false) {
            // egui's remember mapping (`render_reload_prompt`): Reload
            // -> Always, Ignore -> Never, Keep-edits -> clear the
            // override so the user is asked again. An override equal
            // to the global default is stored as None to keep
            // `file_watch_prefs` free of redundant entries.
            let mode = match decision {
                ReloadDecision::DiscardEdits => Some(hxy_settings::AutoReloadMode::Always),
                ReloadDecision::Ignore => Some(hxy_settings::AutoReloadMode::Never),
                ReloadDecision::KeepEdits => None,
            };
            let path = pending.path.clone();
            update_settings(cx, |s| {
                let pref = mode.filter(|m| *m != s.auto_reload);
                s.set_auto_reload_for(path, pref);
            });
        }
        if !matches!(decision, ReloadDecision::Ignore) {
            let ok = crate::watch::apply_reload(&pending.file, &pending.path, decision, cx);
            if !ok {
                let text = hxy_i18n::t_args("reload-prompt-failed", &[("name", &pending.display_name)]);
                window.push_notification(Notification::error(text), cx);
                return;
            }
            self.recompute_panels_for_path(&pending.path, cx);
            // Re-fire the tab's completed templates against the
            // reloaded bytes right away (no debounce -- the reload is
            // a single discrete event, mirroring egui's cascade).
            pending.file.update(cx, |panel, cx| panel.rerun_templates(window, cx));
        }
        if let Some(file_watch) = self.file_watch.as_mut() {
            file_watch.mark_synced(&pending.path);
        }
        cx.notify();
    }

    /// Re-run any already-open strings/entropy/checksums panel for
    /// `path` against its owning file's freshly reloaded bytes.
    /// Mirrors egui's `cascade_byte_change` (minus the template rerun,
    /// handled on the `FilePanel` itself).
    /// Move every strings / entropy / checksums panel anchored to `old`
    /// onto `new` after a Save As rename. Global search / compare record
    /// paths but are unaffected: global search re-derives its file set
    /// from live entities each run, and a compare side holds the file
    /// entity, not a path key.
    fn rebind_analysis_panels(&mut self, old: &Path, new: &Path, cx: &mut Context<Self>) {
        let strings: Vec<Entity<StringsPanel>> =
            self.strings_panels.iter().filter(|p| p.read(cx).owning_path() == Some(old)).cloned().collect();
        for panel in strings {
            panel.update(cx, |p, _cx| p.set_owning_path(new.to_path_buf()));
        }
        let entropy: Vec<Entity<EntropyPanel>> =
            self.entropy_panels.iter().filter(|p| p.read(cx).owning_path() == Some(old)).cloned().collect();
        for panel in entropy {
            panel.update(cx, |p, _cx| p.set_owning_path(new.to_path_buf()));
        }
        let checksums: Vec<Entity<ChecksumsPanel>> =
            self.checksums_panels.iter().filter(|p| p.read(cx).owning_path() == Some(old)).cloned().collect();
        for panel in checksums {
            panel.update(cx, |p, _cx| p.set_owning_path(new.to_path_buf()));
        }
        let visualizers: Vec<Entity<VisualizerPanel>> =
            self.visualizer_panels.iter().filter(|p| p.read(cx).owning_path() == Some(old)).cloned().collect();
        for panel in visualizers {
            panel.update(cx, |p, _cx| p.set_owning_path(new.to_path_buf()));
        }
    }

    /// Live-apply whatever changed in the settings global to the open
    /// panes and the watcher, mirroring the egui settings panel's
    /// inline side effects. Only fields that differ from the last
    /// applied snapshot are pushed, so unrelated mutations (e.g.
    /// recording a recent file) never clobber per-pane state like a
    /// palette-set column count. Nested workspace-host file panes pick
    /// settings up at construction only (they are not in
    /// `open_files`), matching their M3 scope.
    fn on_settings_changed(&mut self, cx: &mut Context<Self>) {
        let next = crate::settings::settings(cx);
        let prev = std::mem::replace(&mut self.applied_settings, next.clone());

        let panes: Vec<Entity<HexPane>> = self.open_files.iter().map(|f| f.read(cx).pane().clone()).collect();
        if next.hex_columns != prev.hex_columns {
            for pane in &panes {
                pane.update(cx, |pane, cx| pane.set_columns(next.hex_columns, cx));
            }
        }
        if next.input_mode != prev.input_mode {
            for pane in &panes {
                pane.update(cx, |pane, cx| {
                    pane.editor_mut().set_input_mode(next.input_mode);
                    cx.notify();
                });
            }
        }
        if next.show_minimap != prev.show_minimap {
            for pane in &panes {
                pane.update(cx, |pane, cx| pane.set_show_minimap(next.show_minimap, cx));
            }
        }
        if next.minimap_colored != prev.minimap_colored {
            for pane in &panes {
                pane.update(cx, |pane, cx| pane.set_minimap_colored(next.minimap_colored, cx));
            }
        }
        if next.byte_value_highlight != prev.byte_value_highlight
            || next.byte_highlight_scheme != prev.byte_highlight_scheme
            || next.byte_highlight_mode != prev.byte_highlight_mode
        {
            self.sync_byte_palettes(cx);
        }
        if (next.file_poll_interval_ms != prev.file_poll_interval_ms || next.file_poll_all != prev.file_poll_all)
            && let Some(watch) = self.file_watch.as_mut()
        {
            watch.set_polling(crate::watch::polling_prefs(&next));
        }
        // offset_base / numeric_format / template_value_formats /
        // compare_recompute_deadline / palette_escape_pops_to_parent /
        // auto_reload / file_watch_prefs are read at their use sites
        // each pass; the repaint below is enough for them.
        cx.notify();
    }

    /// Re-derive every open file's settings/theme-dependent byte
    /// palette (and template overlays with it).
    fn sync_byte_palettes(&mut self, cx: &mut Context<Self>) {
        for file in self.open_files.clone() {
            file.update(cx, |file, cx| file.sync_pane_overlays(cx));
        }
    }

    /// A welcome-screen recents row was clicked: open the path through
    /// the normal flow (dedup, error toast; a fresh open also bumps
    /// the recents list -- focusing an already-open tab does not).
    fn on_welcome_open_recent(
        &mut self,
        _welcome: &Entity<WelcomePanel>,
        event: &OpenRecentRequested,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_path(event.0.clone(), window, cx);
    }

    fn recompute_panels_for_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(panel) = self.open_entropy_panel_for_path(Some(path), cx) {
            panel.update(cx, |p, cx| p.recompute_after_reload(cx));
        }
        if let Some(panel) = self.open_strings_panel_for_path(Some(path), cx) {
            panel.update(cx, |p, cx| p.recompute_after_reload(cx));
        }
        if let Some(panel) = self.open_checksums_panel_for_path(Some(path), cx) {
            panel.update(cx, |p, cx| p.recompute_after_reload(cx));
        }
    }

    fn on_open_file(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        let receiver =
            cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            let _ = this.update_in(cx, |workspace, window, cx| match result {
                Ok(Ok(Some(paths))) => {
                    for path in paths {
                        workspace.open_path(path, window, cx);
                    }
                }
                Ok(Ok(None)) => {
                    // User cancelled the dialog; normal no-op.
                }
                Ok(Err(err)) => {
                    tracing::error!(%err, "file picker failed");
                    let text = hxy_i18n::t_args("gpui-status-open-error-dialog", &[("error", &err.to_string())]);
                    window.push_notification(Notification::error(text), cx);
                }
                Err(_) => {
                    // Channel dropped (window closing); nothing to show.
                }
            });
        })
        .detach();
    }

    /// `cmd-s` / File > Save: write the reference file to its existing
    /// path (or prompt for one if it has none). FILE-SCOPED via
    /// `reference_active_file` (Task 3), so it still saves the right
    /// file with a strings/entropy tab focused.
    fn on_save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        self.save_reference_file(SaveKind::Save, window, cx);
    }

    /// `cmd-shift-s` / File > Save As: always prompt for a destination.
    fn on_save_as(&mut self, _: &SaveAs, window: &mut Window, cx: &mut Context<Self>) {
        self.save_reference_file(SaveKind::SaveAs, window, cx);
    }

    /// Save the reference file. `Save` writes straight to the tab's path
    /// when it has one and only prompts for a path otherwise; `SaveAs`
    /// always prompts. No-op with no reference file. Mirrors egui's
    /// `save_active_file` / `save_file_by_id` (`crates/hxy/src/files/save.rs`).
    fn save_reference_file(&mut self, kind: SaveKind, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.reference_active_file(cx) else { return };
        let path = file.read(cx).path().map(Path::to_path_buf);
        match (kind, path) {
            (SaveKind::Save, Some(path)) => {
                self.write_file(&file, path, window, cx);
            }
            // Save As, or a Save on an untitled buffer: ask for a path.
            _ => self.prompt_save_path(file, None, window, cx),
        }
    }

    /// Read the file's patched bytes, write them atomically, then
    /// re-anchor the editor onto the freshly written bytes so the buffer
    /// goes clean (the patch is now on disk) and drop the watcher's
    /// pending change so the post-save mtime bump doesn't boomerang back
    /// as a phantom reload prompt. Returns whether the bytes hit disk;
    /// the close-on-success path (`resolve_close`) conditions on it.
    /// Mirrors egui's `save_file_by_id` filesystem branch.
    fn write_file(
        &mut self,
        file: &Entity<FilePanel>,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let pane = file.read(cx).pane().clone();
        let bytes = read_pane_bytes(&pane, cx);
        if let Err(err) = hxy_panels::files::write_atomic(&path, &bytes) {
            let text =
                hxy_i18n::t_args("gpui-save-failed", &[("name", &leaf_name(&path)), ("error", &err.to_string())]);
            window.push_notification(Notification::error(text), cx);
            return false;
        }
        // Re-anchor onto the just-written bytes: swap_source drops the
        // patch overlay, so the editor reports clean and dependent panels
        // recompute against what is now on disk. Mirrors egui swapping to
        // a fresh source over the saved file.
        let previous_path = file.read(cx).path().map(Path::to_path_buf);
        let fresh: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        pane.update(cx, |pane, cx| {
            pane.editor_mut().swap_source(fresh);
            cx.notify();
        });
        // Save As lands the tab on a new path: retitle it and refresh the
        // path-keyed reuse registry so a resync keeps this live entity.
        if previous_path.as_deref() != Some(path.as_path()) {
            file.update(cx, |file, _cx| file.set_path(path.clone()));
            self.register_open_file(file, window, cx);
            // Save As renamed the file: re-anchor its analysis panels onto
            // the new path before the recompute below, or they keep owning
            // the stale path (recompute misses, cascade misses, dump wrong).
            if let Some(old) = previous_path.as_deref() {
                self.rebind_analysis_panels(old, &path, cx);
            }
        }
        if let Some(file_watch) = self.file_watch.as_mut() {
            file_watch.mark_synced(&path);
        }
        // The patch is now on disk, so any unsaved-edits sidecar for this
        // path is stale (mirrors egui's save discarding the sidecar).
        crate::patches::discard(&path);
        self.recompute_panels_for_path(&path, cx);
        cx.notify();
        true
    }

    /// Open the native save dialog for `file`, then write it to whatever
    /// path the user picks. `and_close` closes the tab after a successful
    /// write (the save-before-closing "Save" path on an untitled buffer).
    /// A cancelled dialog is a normal no-op.
    fn prompt_save_path(
        &mut self,
        file: Entity<FilePanel>,
        and_close: Option<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = file.read(cx).path().map(Path::to_path_buf);
        let directory = current
            .as_ref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let suggested = current
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| hxy_i18n::t("gpui-file-untitled"));
        let receiver = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await;
            let _ = this.update_in(cx, |workspace, window, cx| {
                let path = match result {
                    Ok(Ok(Some(path))) => path,
                    _ => return,
                };
                if workspace.write_file(&file, path, window, cx) && and_close.is_some() {
                    workspace.close_file_tab(file, window, cx);
                }
            });
        })
        .detach();
    }

    /// If a freshly opened `file` at `path` has an unsaved-edits sidecar
    /// from a previous session, stage a restore prompt for it. No-op when
    /// there is no sidecar (the common case) or a restore prompt is
    /// already up. Mirrors egui's open path checking for a sidecar and
    /// staging `pending_patch_restore`.
    fn maybe_stage_restore(
        &mut self,
        file: Entity<FilePanel>,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pending_restore.is_some() {
            return;
        }
        let Some((sidecar, integrity)) = crate::patches::load_restore(path) else { return };
        self.pending_restore = Some(PendingRestore { file, sidecar, integrity });
        self.open_restore_dialog(window, cx);
    }

    /// Show the restore-unsaved-edits dialog for `self.pending_restore`. A
    /// clean sidecar gets a plain "Restore"; a modified / unknown one gets
    /// a warning banner and a worded "Restore anyway" so the risk isn't
    /// accidental. Dismissal keeps the sidecar (see [`RestoreDecision`]).
    fn open_restore_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use hxy_panels::files::patch_persist::RestoreIntegrity;
        let Some(pending) = &self.pending_restore else { return };
        let op_count = pending.sidecar.patch.len();
        let path_display = pending.sidecar.source_path.display().to_string();
        let integrity = pending.integrity.clone();
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let mut body = v_flex()
                .gap_2()
                .child(Label::new(hxy_i18n::t_args("restore-patch-body", &[("ops", &op_count.to_string())])))
                .child(Label::new(path_display.clone()).text_color(cx.theme().muted_foreground));
            let (clean, warn) = match &integrity {
                RestoreIntegrity::Clean => (true, None),
                RestoreIntegrity::Modified { reason } => (false, Some(("restore-patch-warn-modified", reason.clone()))),
                RestoreIntegrity::Unknown { reason } => (false, Some(("restore-patch-warn-unknown", reason.clone()))),
            };
            if let Some((key, reason)) = warn {
                body = body
                    .child(Label::new(hxy_i18n::t(key)).text_color(cx.theme().warning))
                    .child(Label::new(reason).text_color(cx.theme().muted_foreground));
            }
            let restore_key = if clean { "restore-patch-restore" } else { "restore-patch-restore-anyway" };
            let weak_for_footer = weak.clone();
            let weak_for_cancel = weak.clone();
            let weak_for_close = weak.clone();
            dialog
                .title(hxy_i18n::t("restore-patch-title"))
                .child(body)
                .on_cancel(move |_, window, cx| {
                    dismiss_restore(&weak_for_cancel, window, cx);
                    true
                })
                .on_close(move |_, window, cx| dismiss_restore(&weak_for_close, window, cx))
                .footer(move |_ok, _cancel, _window, _cx| {
                    vec![
                        restore_button(
                            "restore-apply",
                            hxy_i18n::t(restore_key),
                            weak_for_footer.clone(),
                            RestoreDecision::Restore,
                        ),
                        restore_button(
                            "restore-discard",
                            hxy_i18n::t("restore-patch-discard"),
                            weak_for_footer.clone(),
                            RestoreDecision::Discard,
                        ),
                    ]
                })
        });
    }

    /// Apply the restore decision. `Restore` re-applies the sidecar's
    /// patch and undo/redo stacks onto the tab's editor and forces it
    /// mutable; `Discard` drops the sidecar without applying.
    ///
    /// The sidecar is dropped from disk only along a path that actually
    /// reapplied (or explicitly discarded) the edits -- never on a
    /// verification failure, which keeps the sidecar and warns instead,
    /// so a mismatch can't silently lose the user's work.
    ///
    /// A `Clean` sidecar is verified against the reloaded bytes first:
    /// `persist_unsaved_on_quit` digests the BASE source, so this checks
    /// the on-disk file is unchanged since the patch was cut. Mirrors
    /// egui's `RestoreAction` handling (`crates/hxy/src/app/dialogs.rs`).
    fn resolve_restore(&mut self, decision: RestoreDecision, window: &mut Window, cx: &mut Context<Self>) {
        use hxy_panels::files::patch_persist::RestoreIntegrity;
        let Some(pending) = self.pending_restore.take() else { return };
        let path = pending.sidecar.source_path.clone();
        if let RestoreDecision::Discard = decision {
            crate::patches::discard(&path);
            cx.notify();
            return;
        }
        let clean = matches!(pending.integrity, RestoreIntegrity::Clean);
        let pane = pending.file.read(cx).pane().clone();
        if clean {
            let bytes = read_pane_bytes(&pane, cx);
            if let Err(err) = pending.sidecar.metadata.verify(&bytes) {
                // The on-disk bytes no longer match the base the patch was
                // cut against. Do NOT reapply onto mismatched bytes, and --
                // critically -- do NOT drop the sidecar: keep it so the
                // edits stay recoverable, and tell the user why.
                tracing::warn!(%err, path = %path.display(), "restore: source verification failed; keeping sidecar");
                window.push_notification(Notification::warning(hxy_i18n::t("gpui-restore-verify-failed")), cx);
                cx.notify();
                return;
            }
        }
        pane.update(cx, |pane, cx| {
            *pane.editor_mut().patch().write().expect("patch lock poisoned") = pending.sidecar.patch;
            pane.editor_mut().set_undo_stack(pending.sidecar.undo_stack);
            pane.editor_mut().set_redo_stack(pending.sidecar.redo_stack);
            pane.editor_mut().push_history_boundary();
            pane.editor_mut().set_edit_mode(EditMode::Mutable);
            cx.notify();
        });
        // Only now that the edits are safely reapplied is it safe to drop
        // the sidecar.
        crate::patches::discard(&path);
        cx.notify();
    }

    /// Write an unsaved-edits sidecar for every still-open dirty file tab,
    /// and drop stale sidecars for the clean ones. Runs from the on-quit
    /// hook (see [`register_quit_persistence`]). Best-effort: a store
    /// failure only logs. Mirrors egui's `on_exit`
    /// (`crates/hxy/src/app/desktop.rs:2026-2054`).
    fn persist_unsaved_on_quit(&self, cx: &App) {
        use hxy_panels::files::patch_persist;
        let Some(dir) = crate::patches::edits_dir() else { return };
        let dump = self.dock.read(cx).dump(cx);
        let mut seen = std::collections::HashSet::new();
        // Only outer disk-backed `FilePanel` tabs are persisted here. VFS
        // inner entry tabs live inside a `WorkspaceHostPanel`'s nested
        // dock (never in `open_files`) and open read-only for writerless
        // mounts (see `WorkspaceHostPanel::open_entry`), so they can't be
        // dirty and have nothing to persist. Untitled outer buffers are
        // skipped too -- no source path to key a sidecar under.
        for file in self.open_files.iter().rev() {
            let Some(path) = file.read(cx).path().map(Path::to_path_buf) else { continue };
            // `open_files` can hold stale closed entities and duplicate
            // paths; keep only paths still live in the dock, once each.
            if !dump_has_file_path(&dump.center, &path) || !seen.insert(path.clone()) {
                continue;
            }
            let pane = file.read(cx).pane().clone();
            let pane = pane.read(cx);
            let editor = pane.editor();
            if !editor.is_dirty() {
                let _ = patch_persist::discard(&dir, &path);
                continue;
            }
            let patch = editor.patch().read().expect("patch lock poisoned").clone();
            // Digest the BASE (unpatched) source, not the patched view:
            // the sidecar's content digest must record the on-disk
            // baseline the patch was cut against so a later Clean restore
            // can verify the disk is unchanged and reapply. Digesting the
            // dirty view (as `editor.source()` would) stores the digest of
            // the very bytes we are trying to restore, which never matches
            // the reloaded base and rejects every legitimate restore.
            let Some(sidecar) = patch_persist::snapshot(
                path.clone(),
                editor.base_source().as_ref(),
                patch,
                editor.undo_stack().to_vec(),
                editor.redo_stack().to_vec(),
            ) else {
                continue;
            };
            if let Err(err) = patch_persist::store(&dir, &sidecar) {
                tracing::warn!(%err, path = %path.display(), "store patch sidecar");
            }
        }
    }

    fn on_toggle_vim(&mut self, _: &ToggleVim, _window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_active_vim(cx);
    }

    /// Flip vim mode: rotate the persisted `input_mode` setting, which
    /// the settings observer then applies to every open editor
    /// (mirrors egui's `toggle_vim_mode` -- the toggle and the
    /// settings value stay in sync in both directions). Shared by the
    /// `cmd-alt-v` action and the palette's Toggle Vim entry.
    pub(crate) fn toggle_active_vim(&mut self, cx: &mut Context<Self>) {
        let next = match crate::settings::settings(cx).input_mode {
            InputMode::Default => InputMode::Vim,
            InputMode::Vim => InputMode::Default,
        };
        update_settings(cx, |s| s.input_mode = next);
    }

    /// `cmd-e` / Edit > Toggle Edit Mode: flip the reference file (see
    /// `reference_active_file`'s doc) between read-only and mutable.
    /// No-op with no reference file.
    fn on_toggle_edit_mode(&mut self, _: &ToggleEditMode, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.reference_active_file(cx) else { return };
        let pane = file.read(cx).pane().clone();
        pane.update(cx, |pane, cx| {
            let next = match pane.editor().edit_mode() {
                EditMode::Readonly => EditMode::Mutable,
                EditMode::Mutable => EditMode::Readonly,
            };
            pane.editor_mut().set_edit_mode(next);
            cx.notify();
        });
    }

    /// `cmd-z` / Edit > Undo: revert the active file's most recent edit
    /// and park the caret at the change site. No-op with no active file
    /// or an empty undo stack.
    fn on_undo(&mut self, _: &Undo, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.active_pane(cx) else { return };
        pane.update(cx, |pane, cx| {
            if let Some(entry) = pane.editor_mut().undo() {
                jump_cursor_to(pane, entry.offset, cx);
            }
            cx.notify();
        });
    }

    /// `cmd-shift-z` / Edit > Redo: mirrors [`Self::on_undo`].
    fn on_redo(&mut self, _: &Redo, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(pane) = self.active_pane(cx) else { return };
        pane.update(cx, |pane, cx| {
            if let Some(entry) = pane.editor_mut().redo() {
                jump_cursor_to(pane, entry.offset, cx);
            }
            cx.notify();
        });
    }

    /// `cmd-w` / File > Close Tab: reuses the palette's `CloseTab`
    /// dispatch so the menu, the palette entry, and any future shortcut
    /// all go through the same code path.
    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        apply::apply(self, PaletteAction::CloseTab, window, cx);
    }

    /// `cmd-c` / Edit > Copy Bytes: copies the active selection as
    /// lossy UTF-8 text. Reuses the palette's copy dispatch (same
    /// formatting the vim ASCII-pane yank uses); no-op with no
    /// selection.
    fn on_copy_bytes(&mut self, _: &CopyBytes, window: &mut Window, cx: &mut Context<Self>) {
        apply::apply(self, PaletteAction::CopySelection(CopyFormat::Bytes), window, cx);
    }

    /// `cmd-shift-c` / Edit > Copy Hex: copies the active selection as
    /// space-separated uppercase hex (matching the vim hex-pane yank).
    /// No-op with no selection.
    fn on_copy_hex(&mut self, _: &CopyHex, window: &mut Window, cx: &mut Context<Self>) {
        apply::apply(self, PaletteAction::CopySelection(CopyFormat::Hex), window, cx);
    }

    /// App > About: a minimal info dialog naming the app and its
    /// version. gpui 0.2.2 has no predefined "About" menu item (unlike
    /// `muda`'s `PredefinedMenuItem::about`), so this hand-rolls one.
    fn on_show_about(&mut self, _: &ShowAbout, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, |dialog, _window, _cx| {
            dialog.title(hxy_i18n::t("menu-help-about")).child(Label::new(hxy_i18n::t_args(
                "gpui-menu-about-body",
                &[("name", &hxy_i18n::t("app-name")), ("version", env!("CARGO_PKG_VERSION"))],
            )))
        });
    }

    /// Open the command palette in Main mode (or toggle it closed if it
    /// is already open there). Stashes the currently-focused element so
    /// the palette can restore focus to the grid on close.
    fn on_open_palette(&mut self, _: &OpenPalette, window: &mut Window, cx: &mut Context<Self>) {
        let restore = window.focused(cx);
        self.palette.update(cx, |palette, cx| palette.toggle(restore, window, cx));
    }

    /// `cmd-k`: activate the vimium-style pane picker over the center
    /// dock's tab-panel leaves plus the inspector (a side-dock panel
    /// `gpui_dock_picker` cannot discover on its own -- see
    /// `PickTarget::from_dock_area`'s doc -- so it's composed on here by
    /// hand, sourced from the [`ActiveInspectorPanel`](crate::panels::inspector)
    /// global). No-op while a pick session is already active (mirrors the
    /// egui app's `dispatch_focus_pane_shortcut`) so a double-press
    /// doesn't rebind state mid-pick.
    ///
    /// Resyncs the center cache first, checking both known staleness
    /// hazards (only rebuilding, at most once, when either actually
    /// applies -- see `resync_center_if_stale` and
    /// `center_cache_missing_a_live_leaf`): `gpui_dock_picker` enumerates
    /// leaves through `DockArea::items()`, and without the second check a
    /// freshly drag-split pane would never show up as a pickable target
    /// (the first check alone only heals AFTER a leaf later goes empty).
    /// A rebuild, when it fires, sets `focus_pending`; that's safe to
    /// leave alone here because `render`'s `focus_pending` handling itself
    /// skips the refocus while the picker is active, so it can never
    /// steal focus back from the overlay `activate` is about to focus.
    fn on_pick_pane(&mut self, _: &PickPane, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane_picker.read(cx).is_active() {
            return;
        }
        self.resync_center_if_stale(window, cx);
        if self.center_cache_missing_a_live_leaf(cx) {
            self.rebuild_center_cache(window, cx);
        }
        let mut targets = PickTarget::from_dock_area(&self.dock, hxy_i18n::t("gpui-dock-picker-empty-pane"), cx);
        if let Some(inspector) = crate::panels::inspector::active_inspector_panel(cx) {
            let focus = inspector.read(cx).focus_handle(cx);
            let focus_for_activate = focus.clone();
            let dock = self.dock.clone();
            targets.push(PickTarget::new(hxy_i18n::t("tab-inspector"), focus).with_on_activate(move |window, cx| {
                // The inspector's own `FocusHandle` doesn't attach to a
                // rendered node while its dock is collapsed, so open the
                // dock first (a no-op when it's already open) before
                // moving focus -- otherwise picking it while collapsed
                // would silently do nothing visible.
                if !dock.read(cx).is_dock_open(DockPlacement::Right, cx) {
                    dock.update(cx, |dock, cx| dock.toggle_dock(DockPlacement::Right, window, cx));
                }
                window.focus(&focus_for_activate);
            }));
        }
        let workspace = cx.entity().downgrade();
        self.pane_picker.update(cx, |picker, cx| {
            picker.activate(
                targets,
                move |_target, window, cx| {
                    if let Some(workspace) = workspace.upgrade() {
                        workspace.update(cx, |workspace, cx| workspace.sync_active_file_after_pick(window, cx));
                    }
                },
                window,
                cx,
            );
        });
    }

    /// The picker jumps focus straight to a target's `FocusHandle`
    /// (default activation) or wherever its `on_activate` override moves
    /// it, bypassing the dock's own tab-activation path that `reconcile`
    /// otherwise relies on (which always tracks the first tab container
    /// with an active file). Since `PickTarget` no longer carries the
    /// underlying `TabPanel` identity, re-derive the newly active file
    /// from wherever focus actually landed rather than from the picked
    /// target itself: walk the center tree for the leaf whose live
    /// `focus_handle` now matches, and point the status bar /
    /// `ActiveHexPane` global at its `FilePanel`. No-op when focus landed
    /// somewhere that isn't a center-dock file leaf (e.g. the inspector).
    fn sync_active_file_after_pick(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(focused) = window.focused(cx) else { return };
        let Some(file) = focused_center_file_panel(self.dock.read(cx).items(), &focused, cx) else { return };
        self.set_active_file(Some(file), cx);
    }

    /// Snapshot of the reference file (see `reference_active_file`'s
    /// doc) for the palette's entry builders: FILE-SCOPED entries
    /// (Go To Offset, Select, Set Columns, Copy Selection) gate on
    /// `has_active_file` and act on the same file this describes, so a
    /// strings tab being focused must not disable them.
    pub(crate) fn palette_context(&self, cx: &App) -> PaletteContext {
        let Some(file) = self.reference_active_file(cx) else { return PaletteContext::default() };
        let pane = file.read(cx).pane().read(cx);
        let editor = pane.editor();
        let selection = editor.selection();
        let cursor = selection.map(|s| s.cursor.get()).unwrap_or(0);
        let source_len = editor.source().len().get();
        let selection = selection.map(|s| {
            let range = s.range();
            (range.start().get(), range.end().get())
        });
        let can_browse_vfs = file.read(cx).detected_handler().is_some();
        // 0 = no template run (or a run with no fields); the jump
        // entries gate on this.
        let template_field_count = file.read(cx).active_template().map(|t| t.state.leaf_boundaries.len()).unwrap_or(0);
        // 0 = no field carries a visualize attribute; the visualizer
        // entry is only offered when nonzero (egui `has_visualizer`).
        let visualizer_target_count = hxy_templates::visualize::collect_targets(&file.read(cx).templates).len();
        PaletteContext {
            has_active_file: true,
            cursor,
            source_len,
            selection,
            vim_on: matches!(editor.input_mode(), InputMode::Vim),
            can_browse_vfs,
            template_field_count,
            visualizer_target_count,
        }
    }

    /// Extension + head bytes of the reference file, for ranking the
    /// palette's template entries against its content (ports egui's
    /// `template_palette_context`). Empty when no file is focused.
    pub(crate) fn template_palette_seed(&self, cx: &App) -> (Option<String>, Vec<u8>) {
        let Some(file) = self.reference_active_file(cx) else { return (None, Vec::new()) };
        let extension =
            file.read(cx).path().and_then(|p| p.extension()).and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase());
        let source = file.read(cx).pane().read(cx).editor().source().clone();
        let window = source.len().get().min(hxy_templates::library::DETECTION_WINDOW as u64);
        // A read miss here only costs magic-byte ranking; fall through
        // to the library's default ordering (mirrors egui).
        let head_bytes = match ByteRange::new(ByteOffset::new(0), ByteOffset::new(window)) {
            Ok(range) if window > 0 => source.read(range).unwrap_or_default(),
            _ => Vec::new(),
        };
        (extension, head_bytes)
    }

    /// Run the template at `path` against the reference file (whole
    /// file, or `range` when the pick was selection-bound).
    pub(crate) fn run_template_on_active(
        &mut self,
        path: PathBuf,
        range: Option<ByteRange>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(file) = self.reference_active_file(cx) else { return };
        file.update(cx, |panel, cx| run_template(panel, path, range, RestoreContext::default(), window, cx));
    }

    /// "Run template from file...": pick a template source from disk
    /// and run it against the whole reference file.
    pub(crate) fn run_template_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let picked = rfd::AsyncFileDialog::new()
                .add_filter(hxy_i18n::t("gpui-template-filter-any"), &["bt", "hexpat", "pat"])
                .pick_file()
                .await;
            let Some(handle) = picked else { return };
            let path = handle.path().to_path_buf();
            let _ = this.update_in(cx, |ws, window, cx| ws.run_template_on_active(path, None, window, cx));
        })
        .detach();
    }

    /// "Install template...": pick a `.bt` from disk and copy it (plus
    /// its `#include` closure) into the user templates directory, then
    /// refresh the library so the new entries rank immediately.
    pub(crate) fn install_template_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let picked = rfd::AsyncFileDialog::new()
                .add_filter(hxy_i18n::t("gpui-template-filter-bt"), &["bt"])
                .pick_file()
                .await;
            let Some(handle) = picked else { return };
            let src = handle.path().to_path_buf();
            let _ = this.update_in(cx, |ws, window, cx| ws.finish_template_install(src, window, cx));
        })
        .detach();
    }

    fn finish_template_install(&mut self, src: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = hxy_templates::user_templates_dir() else {
            window.push_notification(Notification::error(hxy_i18n::t("gpui-template-install-no-dir")), cx);
            return;
        };
        if let Err(err) = std::fs::create_dir_all(&dir) {
            window.push_notification(
                Notification::error(hxy_i18n::t_args(
                    "gpui-template-install-failed",
                    &[("path", &dir.display().to_string()), ("error", &err.to_string())],
                )),
                cx,
            );
            return;
        }
        let report = hxy_templates::library::install_template_with_deps(&src, &dir);
        // One summary toast; per-file problems get their own warning
        // so a partially-broken include closure is visible (ports the
        // egui console log lines onto notifications).
        window.push_notification(
            Notification::info(hxy_i18n::t_args(
                "gpui-template-install-summary",
                &[("copied", &report.copied.len().to_string()), ("existing", &report.existing.len().to_string())],
            )),
            cx,
        );
        for (referrer, target) in &report.missing {
            window.push_notification(
                Notification::warning(hxy_i18n::t_args(
                    "gpui-template-install-missing",
                    &[("path", &referrer.display().to_string()), ("target", target)],
                )),
                cx,
            );
        }
        for (path, error) in &report.errors {
            window.push_notification(
                Notification::error(hxy_i18n::t_args(
                    "gpui-template-install-failed",
                    &[("path", &path.display().to_string()), ("error", error)],
                )),
                cx,
            );
        }
        crate::templates::refresh_library(cx);
    }

    /// Delete an installed template source file and refresh the
    /// library (palette uninstall cascade pick).
    pub(crate) fn uninstall_template(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let name =
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        match std::fs::remove_file(path) {
            Ok(()) => {
                window.push_notification(
                    Notification::info(hxy_i18n::t_args("gpui-template-uninstalled", &[("name", &name)])),
                    cx,
                );
                crate::templates::refresh_library(cx);
            }
            Err(err) => {
                window.push_notification(
                    Notification::error(hxy_i18n::t_args(
                        "gpui-template-uninstall-failed",
                        &[("name", &name), ("error", &err.to_string())],
                    )),
                    cx,
                );
            }
        }
    }

    /// Move the reference file's caret to the next / previous template
    /// field boundary (palette jump entries).
    pub(crate) fn jump_template_field(&mut self, jump: FieldJump, cx: &mut Context<Self>) {
        let Some(file) = self.reference_active_file(cx) else { return };
        file.update(cx, |panel, cx| panel.jump_to_template_field(jump, cx));
    }

    /// Download the ImHex-Patterns corpus into the shared install
    /// directory and refresh the template library. One info toast on
    /// start and one on completion (success or failure); per-byte
    /// progress is intentionally not surfaced. A second invocation
    /// while a download is running is a no-op.
    pub(crate) fn fetch_imhex_patterns(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.patterns_fetch.is_some() {
            return;
        }
        let Some(dest) = hxy_templates::patterns_fetch::install_dir() else {
            window.push_notification(Notification::error(hxy_i18n::t("patterns-fetch-no-data-dir")), cx);
            return;
        };
        if let Some(parent) = dest.parent() {
            // Best-effort pre-create (egui's spawn_default_fetch does
            // the same): if it fails, extraction fails loudly below
            // and surfaces through the failure toast.
            let _ = std::fs::create_dir_all(parent);
        }
        window.push_notification(Notification::info(hxy_i18n::t("patterns-fetch-started")), cx);
        self.patterns_fetch = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { hxy_templates::patterns_fetch::run_fetch(&dest, |_progress| {}) })
                .await;
            let _ = this.update_in(cx, |ws, window, cx| {
                ws.patterns_fetch = None;
                match result {
                    Ok((sha256, _root)) => {
                        // Record the installed corpus hash so the
                        // shared settings blob agrees with what egui's
                        // update check expects.
                        update_settings(cx, |s| s.imhex_patterns.installed_hash = Some(sha256));
                        crate::templates::refresh_library(cx);
                        window.push_notification(Notification::success(hxy_i18n::t("patterns-fetch-done")), cx);
                    }
                    Err(error) => {
                        window.push_notification(
                            Notification::error(hxy_i18n::t_args("patterns-fetch-failed", &[("error", &error)])),
                            cx,
                        );
                    }
                }
            });
        }));
    }

    /// After a successful open: if the library recognises the file's
    /// extension or magic and no template has run on the tab yet,
    /// offer a one-shot "Run <name>?" toast with a Run action button
    /// (ports egui's `suggest_templates_for` prompt). The offer is
    /// recorded on the panel so the tab is never nagged twice; the
    /// notification widget has no dismissal hook, so dismissing and
    /// ignoring both count as declining (the closest mirror of the
    /// egui prompt's semantics the widget allows). Not persisted.
    /// Disk opens only: VFS-entry tabs have no filesystem path for
    /// library matching, so they never get a suggestion.
    fn suggest_template_for(&mut self, panel: &Entity<FilePanel>, window: &mut Window, cx: &mut Context<Self>) {
        // Only harnesses run without the library global (`main`
        // installs it at startup); no library, no suggestions.
        let Some(library) = cx.try_global::<crate::templates::TemplateLibraryGlobal>() else { return };
        {
            let panel = panel.read(cx);
            if panel.template_suggestion_declined || !panel.templates.is_empty() || !panel.templates_running.is_empty()
            {
                return;
            }
        }
        let extension =
            panel.read(cx).path().and_then(|p| p.extension()).and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase());
        let source = panel.read(cx).pane().read(cx).editor().source().clone();
        let window_len = source.len().get().min(hxy_templates::library::DETECTION_WINDOW as u64);
        // A read miss only costs magic detection; extension matching
        // still applies (mirrors egui).
        let head_bytes = match ByteRange::new(ByteOffset::new(0), ByteOffset::new(window_len)) {
            Ok(range) if window_len > 0 => source.read(range).unwrap_or_default(),
            _ => Vec::new(),
        };
        let Some(entry) = library.0.suggest(extension.as_deref(), &head_bytes) else { return };
        let template_path = entry.path.clone();
        let template_name = entry.name.clone();
        panel.update(cx, |panel, _cx| panel.template_suggestion_declined = true);
        let weak = panel.downgrade();
        // Boot-time opens run before the Root notification layer is
        // installed; defer like `build_initial`'s other toasts.
        window.defer(cx, move |window, cx| {
            let note = Notification::info(hxy_i18n::t_args("gpui-template-suggest-body", &[("name", &template_name)]))
                .title(hxy_i18n::t("toast-template-group-title"))
                // A prompt, not a status blip: stays until the user runs
                // or dismisses it (the egui prompt lingers ~30 s).
                .autohide(false)
                .action(move |_, _, cx| {
                    let weak = weak.clone();
                    let template_path = template_path.clone();
                    Button::new("run-suggested-template").label(hxy_i18n::t("toast-template-run")).on_click(
                        cx.listener(move |this: &mut Notification, _, window, cx| {
                            if let Some(panel) = weak.upgrade() {
                                panel.update(cx, |panel, cx| {
                                    run_template(
                                        panel,
                                        template_path.clone(),
                                        None,
                                        RestoreContext::default(),
                                        window,
                                        cx,
                                    );
                                });
                            }
                            this.dismiss(window, cx);
                        }),
                    )
                });
            window.push_notification(note, cx);
        });
    }

    /// The reference file's [`HexPane`] (see `reference_active_file`'s
    /// doc), for FILE-SCOPED palette action dispatch and undo/redo --
    /// every current caller acts on file content/state, so this always
    /// routes through the fallback, never the strict `active_file`.
    pub(crate) fn active_pane(&self, cx: &App) -> Option<Entity<HexPane>> {
        self.reference_active_file(cx).map(|file| file.read(cx).pane().clone())
    }

    /// Palette dispatch entry points, wrapping the action handlers so
    /// the palette can route into the same code paths as the shortcuts.
    pub(crate) fn open_file_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.on_open_file(&OpenFile, window, cx);
    }

    pub(crate) fn toggle_inspector_dock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.on_toggle_inspector(&ToggleInspector, window, cx);
    }

    /// "Browse VFS": mount the reference file through its detected handler
    /// and swap its tab for a nested-dock [`WorkspaceHostPanel`]. Mirrors
    /// the egui app's `mount_active_file` (palette "Browse VFS" entry) --
    /// the file tab is removed and replaced by the workspace tab. No-op
    /// with no reference file or no detected handler (the palette entry is
    /// disabled in that case, but a direct caller is guarded here too).
    pub(crate) fn browse_active_file_as_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.reference_active_file(cx) else { return };
        let Some(handler) = file.read(cx).detected_handler() else { return };
        // Browsing swaps the file tab for the unpatched base mount, which
        // would silently drop any unsaved edits. egui keeps the file
        // entity alive across the swap; replicating that here is larger
        // surgery, so the M3 floor is to block the browse and tell the
        // user to save first.
        if file.read(cx).is_dirty(cx) {
            window.push_notification(Notification::warning(hxy_i18n::t("gpui-browse-vfs-dirty")), cx);
            return;
        }
        let source = file.read(cx).pane().read(cx).editor().source().clone();
        let parent_path = file.read(cx).path().map(Path::to_path_buf);
        let mount = match handler.mount(source) {
            Ok(mount) => Arc::new(mount),
            Err(err) => {
                tracing::warn!(%err, handler = handler.name(), "browse vfs: mount failed");
                let text = hxy_i18n::t_args("gpui-status-open-error-dialog", &[("error", &err.to_string())]);
                window.push_notification(Notification::error(text), cx);
                return;
            }
        };

        // Remove the plain file tab, then add the workspace tab in its
        // place (mirrors egui swapping Tab::File for Tab::Workspace).
        self.resync_center_if_stale(window, cx);
        // The file tab is going away for a workspace tab; record it on the
        // reopen ring so cmd-shift-t brings the plain file back.
        self.remember_closed(&file, cx);
        let closed = file.entity_id();
        let view: Arc<dyn PanelView> = Arc::new(file);
        self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
        self.open_files.retain(|f| f.entity_id() != closed);

        let outer = self.dock.downgrade();
        let host = cx.new(|cx| WorkspaceHostPanel::new(outer, mount, parent_path, window, cx));
        let view: Arc<dyn PanelView> = Arc::new(host);
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn palette(&self) -> Entity<Palette> {
        self.palette.clone()
    }

    /// Whether the strict `active_file` is set (i.e. the front-most
    /// center tab is literally a `FilePanel`), for tests outside this
    /// module asserting a fallback path (`reference_active_file`) is
    /// genuinely being exercised rather than trivially matching.
    #[cfg(test)]
    pub(crate) fn has_strict_active_file(&self) -> bool {
        self.active_file.is_some()
    }

    /// Close the active center tab. A strings, entropy, or checksums
    /// tab takes priority when it's the front-most one (`self.active_file`
    /// only ever names a `FilePanel` -- see `active_file_panel`'s doc --
    /// so without this check `cmd-w` would silently no-op, or close a
    /// background file tab, while the user is looking at one of
    /// those). Otherwise closes the active file tab, if one is
    /// focused, and drops its entity from the reuse registry so the
    /// closed file's buffer is released promptly rather than lingering
    /// until a rare cache rebuild; any strings/entropy/checksums tab
    /// bound to that file closes with it.
    ///
    /// Pruning here (on the close path) rather than in `reconcile` is
    /// deliberate: a file is transiently absent from `dump()` mid-split
    /// (`remove_panel` then `add_file_panel`), so a dump-diff prune in
    /// reconcile would drop a still-live file's reuse handle and the next
    /// resync would rebuild it fresh -- the exact data loss the
    /// entity-preserving resync exists to prevent (see the Task 2 round-2
    /// rationale). The closed entity here is genuinely gone, so removing
    /// only it by identity is safe.
    pub(crate) fn close_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(strings) = active_strings_panel(self.dock.read(cx).items(), cx) {
            self.close_strings_tab(strings, window, cx);
            return;
        }
        if let Some(entropy) = active_entropy_panel(self.dock.read(cx).items(), cx) {
            self.close_entropy_tab(entropy, window, cx);
            return;
        }
        if let Some(checksums) = active_checksums_panel(self.dock.read(cx).items(), cx) {
            self.close_checksums_tab(checksums, window, cx);
            return;
        }
        if let Some(visualizer) = active_visualizer_panel(self.dock.read(cx).items(), cx) {
            self.close_visualizer_tab(visualizer, window, cx);
            return;
        }
        if let Some(active) = self.active_file.clone() {
            self.request_close_file(active, window, cx);
            return;
        }
        // Fallback for an unrecognized active tab that has no bookkeeping
        // here -- most importantly a stale-layout `InvalidPanel`
        // placeholder that slipped past pruning in an older build. Remove
        // it straight from the dock so `cmd-w` can never leave a tab
        // un-closable. Scoped to genuinely-foreign panels (see
        // `active_unrecognized_panel`): our own tabs that simply lack a
        // `cmd-w` path (compare, workspace host, global search) keep their
        // existing tab-close-button behavior and their state bookkeeping.
        if let Some(view) = active_unrecognized_panel(self.dock.read(cx).items(), cx) {
            self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
        }
    }

    /// Close `file`'s tab, first running the save-before-closing prompt
    /// when it has unsaved edits. A clean tab closes immediately; a dirty
    /// one stages [`Self::pending_close`] and opens the Save / Don't Save
    /// / Cancel dialog, whose buttons drive [`Self::resolve_close`]. A
    /// second request while a prompt is already up is dropped (the first
    /// prompt stays put). Mirrors egui's `request_close`
    /// (`crates/hxy/src/tabs/close.rs:198`).
    fn request_close_file(&mut self, file: Entity<FilePanel>, window: &mut Window, cx: &mut Context<Self>) {
        if !file.read(cx).is_dirty(cx) {
            self.close_file_tab(file, window, cx);
            return;
        }
        if self.pending_close.is_some() {
            return;
        }
        self.pending_close = Some(file);
        self.open_close_dialog(window, cx);
    }

    /// Show the save-before-closing dialog for `self.pending_close`. The
    /// three footer buttons resolve `Save` / `DontSave`; any dismissal
    /// (Escape, overlay click, close icon) resolves `Cancel` and leaves
    /// the tab open. No-op if nothing is pending.
    fn open_close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = &self.pending_close else { return };
        let name = file
            .read(cx)
            .path()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| hxy_i18n::t("gpui-file-untitled"));
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let body = Label::new(hxy_i18n::t_args("close-prompt-body", &[("name", &name)]))
                .text_color(cx.theme().muted_foreground);
            let weak_for_footer = weak.clone();
            let weak_for_cancel = weak.clone();
            let weak_for_close = weak.clone();
            dialog
                .title(hxy_i18n::t("close-prompt-title"))
                .child(body)
                .on_cancel(move |_, window, cx| {
                    resolve_close_on(&weak_for_cancel, CloseDecision::Cancel, window, cx);
                    true
                })
                .on_close(move |_, window, cx| {
                    resolve_close_on(&weak_for_close, CloseDecision::Cancel, window, cx);
                })
                .footer(move |_ok, _cancel, _window, _cx| {
                    vec![
                        close_button(
                            "close-save",
                            hxy_i18n::t("close-prompt-save"),
                            weak_for_footer.clone(),
                            CloseDecision::Save,
                        ),
                        close_button(
                            "close-discard",
                            hxy_i18n::t("close-prompt-discard"),
                            weak_for_footer.clone(),
                            CloseDecision::DontSave,
                        ),
                        close_button(
                            "close-cancel",
                            hxy_i18n::t("close-prompt-cancel"),
                            weak_for_footer.clone(),
                            CloseDecision::Cancel,
                        ),
                    ]
                })
        });
    }

    /// Apply the user's save-before-closing choice to the pending tab.
    /// `Save` writes first and closes only on success (a failed or
    /// cancelled Save As leaves the tab open, mirroring egui's
    /// close.rs:446-450); `DontSave` closes discarding the patch;
    /// `Cancel` leaves the tab untouched. Takes the pending file so a
    /// re-entrant call can't double-close.
    fn resolve_close(&mut self, decision: CloseDecision, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.pending_close.take() else { return };
        match decision {
            CloseDecision::Cancel => {}
            CloseDecision::DontSave => self.close_file_tab(file, window, cx),
            CloseDecision::Save => match file.read(cx).path().map(Path::to_path_buf) {
                Some(path) => {
                    if self.write_file(&file, path, window, cx) {
                        self.close_file_tab(file, window, cx);
                    }
                }
                // Untitled: the save needs a destination, so the write
                // (and the close-on-success) happen in the dialog's
                // async continuation.
                None => self.prompt_save_path(file, Some(()), window, cx),
            },
        }
    }

    /// Close `file`'s tab and drop its entity from the reuse registry,
    /// cascading to close any `StringsPanel` / `EntropyPanel` /
    /// `ChecksumsPanel` tab bound to it (see
    /// `close_strings_tabs_for_path`'s doc). Factored out of
    /// `close_active_tab` so tests (and, if a future task adds a
    /// per-tab close button, that button too) can close a specific
    /// file regardless of which tab is currently front-most --
    /// `close_active_tab` itself only ever closes the front-most tab.
    fn close_file_tab(&mut self, file: Entity<FilePanel>, window: &mut Window, cx: &mut Context<Self>) {
        let closed = file.entity_id();
        let closed_path = file.read(cx).path().map(Path::to_path_buf);
        self.remember_closed(&file, cx);
        let view: Arc<dyn PanelView> = Arc::new(file);
        self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
        self.open_files.retain(|f| f.entity_id() != closed);
        self.close_strings_tabs_for_path(closed_path.as_deref(), window, cx);
        self.close_entropy_tabs_for_path(closed_path.as_deref(), window, cx);
        self.close_checksums_tabs_for_path(closed_path.as_deref(), window, cx);
        self.close_visualizer_tabs_for_path(closed_path.as_deref(), window, cx);
    }

    /// Push a just-closed file tab onto the reopen ring, capturing the
    /// path plus a little view state (caret offset, column count) to
    /// re-park on reopen. Only disk-backed tabs enter the ring -- an
    /// untitled buffer has no path to reopen from. Drops the oldest entry
    /// past [`CLOSED_TABS_CAPACITY`]. Mirrors egui's `remember_closed`
    /// (`crates/hxy/src/tabs/close.rs`).
    fn remember_closed(&mut self, file: &Entity<FilePanel>, cx: &App) {
        let Some(path) = file.read(cx).path().map(Path::to_path_buf) else { return };
        let pane = file.read(cx).pane().read(cx);
        let selection = pane.editor().selection().map(|s| s.cursor.get());
        let columns = pane.columns();
        if self.closed_tabs.len() == CLOSED_TABS_CAPACITY {
            self.closed_tabs.pop_front();
        }
        self.closed_tabs.push_back(ClosedTab { path, selection, columns });
    }

    /// `cmd-shift-t` / File > Reopen Closed Tab: pop the most recently
    /// closed tab and reopen it (focusing an existing tab if the path was
    /// reopened by hand in the meantime), re-parking its caret and column
    /// count. No-op with an empty ring. Mirrors egui's
    /// `reopen_last_closed_tab`.
    fn on_reopen_closed(&mut self, _: &ReopenClosedTab, window: &mut Window, cx: &mut Context<Self>) {
        let Some(closed) = self.closed_tabs.pop_back() else { return };
        self.open_path(closed.path.clone(), window, cx);
        let Some(file) = self.open_file_for_path(&closed.path, cx) else { return };
        let pane = file.read(cx).pane().clone();
        pane.update(cx, |pane, cx| {
            if let Some(offset) = closed.selection {
                let len = pane.editor().source().len().get();
                let clamped = ByteOffset::new(offset.min(len.saturating_sub(1)));
                pane.editor_mut().set_selection(Some(Selection::caret(clamped)));
                if !pane.editor().is_offset_visible(clamped) {
                    pane.editor_mut().set_scroll_to_byte(clamped);
                }
                pane.sync_pending_scroll(cx);
            }
            pane.set_columns(closed.columns, cx);
            cx.notify();
        });
    }

    /// Close one `EntropyPanel` tab by identity. Mirrors `close_strings_tab`.
    fn close_entropy_tab(&mut self, panel: Entity<EntropyPanel>, window: &mut Window, cx: &mut Context<Self>) {
        self.entropy_panels.retain(|p| p.entity_id() != panel.entity_id());
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
    }

    /// Close one `ChecksumsPanel` tab by identity. Mirrors `close_strings_tab`.
    fn close_checksums_tab(&mut self, panel: Entity<ChecksumsPanel>, window: &mut Window, cx: &mut Context<Self>) {
        self.checksums_panels.retain(|p| p.entity_id() != panel.entity_id());
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
    }

    /// Close every `EntropyPanel` tab bound to `path`. Mirrors
    /// `close_strings_tabs_for_path`: a left-open entropy tab would pin
    /// the closed file's `HexPane` alive and block a later reopen of
    /// the same path from rebinding.
    fn close_entropy_tabs_for_path(&mut self, path: Option<&Path>, window: &mut Window, cx: &mut Context<Self>) {
        let mut closing = Vec::new();
        self.entropy_panels.retain(|panel| {
            if panel.read(cx).owning_path() == path {
                closing.push(panel.clone());
                false
            } else {
                true
            }
        });
        for panel in closing {
            let view: Arc<dyn PanelView> = Arc::new(panel);
            self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
        }
    }

    /// Close every `ChecksumsPanel` tab bound to `path`. Mirrors
    /// `close_entropy_tabs_for_path`.
    fn close_checksums_tabs_for_path(&mut self, path: Option<&Path>, window: &mut Window, cx: &mut Context<Self>) {
        let mut closing = Vec::new();
        self.checksums_panels.retain(|panel| {
            if panel.read(cx).owning_path() == path {
                closing.push(panel.clone());
                false
            } else {
                true
            }
        });
        for panel in closing {
            let view: Arc<dyn PanelView> = Arc::new(panel);
            self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
        }
    }

    /// Close one `VisualizerPanel` tab by identity. Mirrors
    /// `close_entropy_tab`.
    fn close_visualizer_tab(&mut self, panel: Entity<VisualizerPanel>, window: &mut Window, cx: &mut Context<Self>) {
        self.visualizer_panels.retain(|p| p.entity_id() != panel.entity_id());
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
    }

    /// Close every `VisualizerPanel` tab bound to `path`. Mirrors
    /// `close_entropy_tabs_for_path` (a left-open visualizer tab would
    /// pin the closed file's `FilePanel` alive through its strong
    /// entity handle).
    fn close_visualizer_tabs_for_path(&mut self, path: Option<&Path>, window: &mut Window, cx: &mut Context<Self>) {
        let mut closing = Vec::new();
        self.visualizer_panels.retain(|panel| {
            if panel.read(cx).owning_path() == path {
                closing.push(panel.clone());
                false
            } else {
                true
            }
        });
        for panel in closing {
            let view: Arc<dyn PanelView> = Arc::new(panel);
            self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
        }
    }

    /// Close one `StringsPanel` tab by identity.
    fn close_strings_tab(&mut self, panel: Entity<StringsPanel>, window: &mut Window, cx: &mut Context<Self>) {
        self.strings_panels.retain(|p| p.entity_id() != panel.entity_id());
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
    }

    /// Close every `StringsPanel` tab bound to `path`. Called when the
    /// owning file's tab closes: a strings tab left open would pin the
    /// closed file's `HexPane` alive (it holds a strong `Entity<HexPane>`)
    /// and show results for a file no longer open, and would block a
    /// later reopen of the same path from rebinding (`bind_pane` is a
    /// no-op once `owning_pane` is set).
    fn close_strings_tabs_for_path(&mut self, path: Option<&Path>, window: &mut Window, cx: &mut Context<Self>) {
        let mut closing = Vec::new();
        self.strings_panels.retain(|panel| {
            if panel.read(cx).owning_path() == path {
                closing.push(panel.clone());
                false
            } else {
                true
            }
        });
        for panel in closing {
            let view: Arc<dyn PanelView> = Arc::new(panel);
            self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
        }
    }

    /// Bring the workspace's own tracking back in sync with the dock's
    /// live tab set: add or remove the welcome placeholder so it shows
    /// exactly when no file tab is open, and refresh the active file.
    /// Runs after layout changes, where a `&mut Window` is available.
    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let has_content = {
            let state = self.dock.read(cx).dump(cx);
            // A workspace host tab is content too (its own file tabs live
            // inside its nested dock, invisible to `count_file_panels`),
            // so it must suppress the welcome placeholder just like a
            // plain file tab does. Same for the global search tab: it is
            // workspace-scoped (needs no open file, like Compare), so a
            // user who opens it on an empty workspace must not have it
            // silently wiped out from under them the next time this
            // "no content -> show welcome" branch rebuilds the center
            // from scratch (`DockItem::split` below replaces the WHOLE
            // center, not just adds welcome alongside).
            count_file_panels(&state.center) > 0
                || count_workspace_host_panels(&state.center) > 0
                || count_global_search_panels(&state.center) > 0
                || count_settings_panels(&state.center) > 0
        };

        if !has_content {
            if self.welcome.is_none() {
                let welcome = cx.new(WelcomePanel::new);
                self.welcome_sub = Some(cx.subscribe_in(&welcome, window, Self::on_welcome_open_recent));
                let view: Arc<dyn PanelView> = Arc::new(welcome.clone());
                let weak = self.dock.downgrade();
                self.dock.update(cx, |dock, cx| {
                    // Rebuild the center as a fresh, subscribed Split before
                    // adding welcome. Closing the last tab (or emptying every
                    // split pane) detaches its TabPanel from the live tree
                    // while the center's `DockItem` cache keeps the orphan;
                    // adding welcome through that stale cache would attach it
                    // to a detached panel that never renders. A fresh Split
                    // resets the cache and re-subscribes for LayoutChanged.
                    let center = DockItem::split(Axis::Horizontal, vec![], &weak, window, cx);
                    dock.set_center(center, window, cx);
                    dock.add_panel(view, DockPlacement::Center, None, window, cx);
                });
                self.welcome = Some(welcome);
            }
        } else {
            // At least one file remains. If a split pane collapsed, heal the
            // cache from the live tree before touching the welcome tab.
            self.resync_center_if_stale(window, cx);
            if let Some(welcome) = self.welcome.take() {
                self.welcome_sub = None;
                let view: Arc<dyn PanelView> = Arc::new(welcome);
                self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
            }
        }

        let active = active_file_panel(self.dock.read(cx).items(), cx);
        self.set_active_file(active, cx);
        // Republish for `StringsPanel`'s restore-time rebind (see
        // `crate::panels::strings`'s module doc): a panel that
        // couldn't find its owning file at construction picks it up
        // here the next time reconcile runs.
        cx.set_global(OpenFilePanels(self.open_files.clone()));
    }

    /// Point `cmd-w` / the vim toggle at `active` (the strict, literal
    /// active-tab value -- see `active_file`'s doc for why this never
    /// widens), remember it in `last_active_file` when it's a real
    /// file, and refresh what the inspector / title / status bar
    /// display via `publish_reference_active_file`.
    fn set_active_file(&mut self, active: Option<Entity<FilePanel>>, cx: &mut Context<Self>) {
        if let Some(file) = &active {
            self.last_active_file = Some(file.clone());
        }
        let changed = self.active_file.as_ref().map(Entity::entity_id) != active.as_ref().map(Entity::entity_id);
        self.active_file = active;
        self.publish_reference_active_file(cx);
        if changed {
            cx.notify();
        }
    }

    /// The file every FILE-SCOPED command (undo/redo, copy, goto/
    /// select/columns, vim/edit-mode toggle, "open strings panel for
    /// active file", palette gating) should act on: the literal
    /// active-tab file if there is one, else the last file tab that
    /// was active, as long as it's still open, else (mirroring egui's
    /// third `active_file_id` tier, `crates/hxy/src/app/mod.rs:3506-
    /// 3529`) any open file at all. Returning `None` means genuinely no
    /// file is open.
    ///
    /// TAB-SCOPED operations (`close_active_tab`, `focus_existing_tab`'s
    /// "already active" fast path, `focus_pending`'s keyboard-focus
    /// routing, dock/picker mechanics) must NOT use this -- they read
    /// `self.active_file` directly, which stays the strict, never-
    /// widened value (see its doc). Mixing the two up is exactly the
    /// regression the round-1 fix avoided by keeping them separate
    /// fields in the first place.
    fn reference_active_file(&self, cx: &App) -> Option<Entity<FilePanel>> {
        if let Some(active) = &self.active_file {
            return Some(active.clone());
        }
        let dump = self.dock.read(cx).dump(cx);
        // Checked against `dump()` (always accurate, unlike `open_files`
        // -- see its doc) rather than registry membership: a tab closed
        // by any path other than `close_file_tab` (e.g. the tab bar's
        // own close button on a background tab, which bypasses the
        // workspace entirely, same gap `open_file_for_path` works
        // around the same way) would otherwise leave `open_files` stale
        // and this fallback pointing at a file that isn't open anymore.
        // Untitled files have no recorded path to check against
        // `dump()` at all; trusted on registry membership alone
        // (best-effort -- untitled buffers aren't robustly tracked
        // anywhere else in this codebase either).
        let is_live = |file: &Entity<FilePanel>| match file.read(cx).path() {
            Some(path) => dump_has_file_path(&dump.center, path),
            None => true,
        };
        if let Some(last) = self.last_active_file.as_ref().filter(|f| is_live(f)) {
            return Some(last.clone());
        }
        self.open_files.iter().find(|f| is_live(f)).cloned()
    }

    /// Publish `reference_active_file`'s pane into [`ActiveHexPane`] so
    /// the inspector (which the workspace has no direct handle to once
    /// restored -- see `panels::inspector`'s module doc) stays in sync,
    /// re-observing it so editor changes repaint the status bar. No-op
    /// when the published pane is unchanged (checked against the
    /// global itself rather than a tracked field, since this can run
    /// on every `reconcile` regardless of whether `self.active_file`
    /// -- a narrower value -- changed: e.g. switching which strings
    /// tab is focused changes the reference file without changing
    /// `self.active_file`, which stays `None` throughout).
    ///
    /// `ActiveHexPane` is a single App-level global, so this assumes
    /// one live `Workspace` per process (true today: `main.rs` opens
    /// exactly one window). A second concurrent workspace would have
    /// its inspector hijacked by whichever one last called this.
    fn publish_reference_active_file(&mut self, cx: &mut Context<Self>) {
        let pane = self.reference_active_file(cx).map(|file| file.read(cx).pane().clone());
        let current = cx.try_global::<ActiveHexPane>().and_then(|active| active.0.clone());
        if pane.as_ref().map(Entity::entity_id) == current.as_ref().map(Entity::entity_id) {
            return;
        }
        self.active_pane_observe = pane.as_ref().map(|pane| cx.observe(pane, |_workspace, _pane, cx| cx.notify()));
        cx.set_global(ActiveHexPane(pane));
    }

    /// Debounced layout save: replaces any pending timer, so only the
    /// last change in a burst is written.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.layout_path.clone() else { return };
        let dock = self.dock.clone();
        self.save_debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let _ = this.update(cx, |_workspace, cx| {
                let state = dock.read(cx).dump(cx);
                if let Err(err) = persist::save(&path, &state) {
                    tracing::warn!(?path, %err, "save dock layout failed");
                }
            });
        }));
    }

    /// Write the current layout to disk immediately, bypassing the
    /// debounce. Used by tests for deterministic round-trips.
    #[cfg(test)]
    fn save_now(&self, cx: &App) -> Option<Result<(), persist::SaveError>> {
        let path = self.layout_path.as_ref()?;
        let state = self.dock.read(cx).dump(cx);
        Some(persist::save(path, &state))
    }

    /// Every panel name in the center dock, for tests asserting a tab
    /// (e.g. a compare tab) was actually spawned.
    #[cfg(test)]
    pub(crate) fn center_panel_names(&self, cx: &App) -> Vec<String> {
        fn walk(state: &PanelState, out: &mut Vec<String>) {
            if state.children.is_empty() {
                out.push(state.panel_name.clone());
            }
            for child in &state.children {
                walk(child, out);
            }
        }
        let mut out = Vec::new();
        walk(&self.dock.read(cx).dump(cx).center, &mut out);
        out
    }

    /// The path shown in the window title / status bar: the reference
    /// file (see `reference_active_file`'s doc), not the strict
    /// `active_file` -- so this keeps naming a file while a strings
    /// tab bound to it is focused.
    fn active_path(&self, cx: &App) -> Option<PathBuf> {
        self.reference_active_file(cx).and_then(|file| file.read(cx).path().map(Path::to_path_buf))
    }

    fn render_status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let file_label = Label::new(status_file_name_text(self.active_path(cx).as_deref()));

        let offset_base = crate::settings::settings(cx).offset_base;
        let (offset, mode) = match self.reference_active_file(cx) {
            Some(file) => {
                let file = file.read(cx);
                let editor = file.pane().read(cx).editor();
                let offset = status_offset_text(editor.selection(), offset_base);
                let mut mode = String::new();
                if matches!(editor.input_mode(), InputMode::Vim) {
                    mode.push_str(&status_vim_mode_text(editor.vim_state().mode));
                    mode.push(' ');
                }
                mode.push_str(dirty_marker(editor.is_dirty()));
                (offset, mode)
            }
            None => (String::new(), String::new()),
        };

        h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .gap_3()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .text_color(cx.theme().muted_foreground)
            .child(file_label)
            .child(Label::new(offset))
            .child(Label::new(mode))
    }
}

/// Park the caret at `offset` (clamped to the source length) after an
/// undo/redo, scrolling it into view if needed, and reset the edit
/// nibble so typing right after the jump starts on the high nibble.
/// Mirrors `crates/hxy/src/app/mod.rs::jump_cursor_to`.
fn jump_cursor_to(pane: &mut HexPane, offset: u64, cx: &mut Context<HexPane>) {
    let len = pane.editor().source().len().get();
    let clamped = ByteOffset::new(offset.min(len.saturating_sub(1)));
    pane.editor_mut().set_selection(Some(Selection::caret(clamped)));
    pane.editor_mut().reset_edit_nibble();
    if !pane.editor().is_offset_visible(clamped) {
        pane.editor_mut().set_scroll_to_byte(clamped);
    }
    pane.sync_pending_scroll(cx);
}

/// Warning-toast text for tabs dropped during layout restore: one toast
/// naming each dropped file when few were dropped, or a single
/// count-summary toast past a small threshold so a large stale layout
/// does not spray the notification stack. Stale/foreign tabs (older-build
/// placeholders with no file to name) add one further count-summary line.
fn restore_pruned_texts(pruned: &persist::PrunedTabs) -> Vec<String> {
    let mut texts = if pruned.dropped_files.len() > 2 {
        vec![hxy_i18n::t_args(
            "gpui-status-restore-dropped-summary",
            &[("count", &pruned.dropped_files.len().to_string())],
        )]
    } else {
        pruned
            .dropped_files
            .iter()
            .map(|path| {
                // A path with no final component (root, `..`) is not a real
                // restored tab; fall back to its full display so the toast
                // still names something.
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                hxy_i18n::t_args("gpui-status-restore-dropped-file", &[("file", &name)])
            })
            .collect()
    };
    if pruned.dropped_incompatible > 0 {
        texts.push(hxy_i18n::t_args(
            "gpui-status-restore-dropped-incompatible",
            &[("count", &pruned.dropped_incompatible.to_string())],
        ));
    }
    texts
}

/// Total file panels anywhere under `state`, used to decide whether the
/// welcome placeholder should show.
fn count_file_panels(state: &PanelState) -> usize {
    let here = usize::from(state.panel_name == FILE_PANEL_NAME);
    here + state.children.iter().map(count_file_panels).sum::<usize>()
}

/// Total workspace-host tabs anywhere under `state`. Counted alongside
/// file panels when deciding whether the welcome placeholder should show
/// -- a workspace tab is content whose nested file tabs `count_file_panels`
/// cannot see (they are serialized inside the host's own `PanelInfo`).
fn count_workspace_host_panels(state: &PanelState) -> usize {
    let here = usize::from(state.panel_name == WORKSPACE_HOST_PANEL_NAME);
    here + state.children.iter().map(count_workspace_host_panels).sum::<usize>()
}

#[cfg(test)]
fn count_strings_panels(state: &PanelState) -> usize {
    let here = usize::from(state.panel_name == STRINGS_PANEL_NAME);
    here + state.children.iter().map(count_strings_panels).sum::<usize>()
}

#[cfg(test)]
fn count_entropy_panels(state: &PanelState) -> usize {
    let here = usize::from(state.panel_name == ENTROPY_PANEL_NAME);
    here + state.children.iter().map(count_entropy_panels).sum::<usize>()
}

#[cfg(test)]
fn count_checksums_panels(state: &PanelState) -> usize {
    let here = usize::from(state.panel_name == CHECKSUMS_PANEL_NAME);
    here + state.children.iter().map(count_checksums_panels).sum::<usize>()
}

/// Total global search panels anywhere under `state` (0 or 1 -- it's a
/// singleton, but tests want to assert that invariant directly).
fn count_global_search_panels(state: &PanelState) -> usize {
    let here = usize::from(state.panel_name == GLOBAL_SEARCH_PANEL_NAME);
    here + state.children.iter().map(count_global_search_panels).sum::<usize>()
}

/// Total settings panels anywhere under `state` (0 or 1 -- singleton,
/// same contract as `count_global_search_panels`).
fn count_settings_panels(state: &PanelState) -> usize {
    let here = usize::from(state.panel_name == SETTINGS_PANEL_NAME);
    here + state.children.iter().map(count_settings_panels).sum::<usize>()
}

#[cfg(test)]
fn count_visualizer_panels(state: &PanelState) -> usize {
    let here = usize::from(state.panel_name == VISUALIZER_PANEL_NAME);
    here + state.children.iter().map(count_visualizer_panels).sum::<usize>()
}

/// Whether any tab container in the center cache has no live panels,
/// i.e. a `TabPanel` that emptied and detached itself from the live tree
/// while its `DockItem::Tabs` entry lingers in the cache. This is the
/// signature of the stale-cache hazard `resync_center_if_stale` heals: a
/// populated tab panel always reports an active panel, and empty ones
/// self-remove, so an empty one still present in the cache is orphaned.
fn center_has_empty_tab_panel(item: &DockItem, cx: &App) -> bool {
    match item {
        DockItem::Tabs { view, .. } => view.read(cx).active_panel(cx).is_none(),
        DockItem::Split { items, .. } => items.iter().any(|item| center_has_empty_tab_panel(item, cx)),
        DockItem::Panel { .. } | DockItem::Tiles { .. } => false,
    }
}

/// Number of tab-panel leaves in the CACHED `DockItem` tree (one per
/// `DockItem::Tabs`). Compare against [`count_panel_state_tab_leaves`] on
/// a live `dump()` to detect a cache the live tree has outgrown.
fn count_dock_item_tab_leaves(item: &DockItem) -> usize {
    match item {
        DockItem::Split { items, .. } => items.iter().map(count_dock_item_tab_leaves).sum(),
        DockItem::Tabs { .. } => 1,
        DockItem::Panel { .. } | DockItem::Tiles { .. } => 0,
    }
}

/// Number of tab-panel leaves in a LIVE `dump()` tree (one per
/// `PanelInfo::Tabs`). See [`count_dock_item_tab_leaves`].
fn count_panel_state_tab_leaves(state: &PanelState) -> usize {
    match &state.info {
        PanelInfo::Tabs { .. } => 1,
        PanelInfo::Stack { .. } => state.children.iter().map(count_panel_state_tab_leaves).sum(),
        PanelInfo::Panel(_) | PanelInfo::Tiles { .. } => 0,
    }
}

/// Mirror a dumped `PanelState` subtree into a fresh `DockItem`,
/// reusing existing panel entities (via `resolve_leaf`) so their editor
/// state survives. Structure (splits, tab order, active index) matches
/// the dump.
fn rebuild_item(
    state: &PanelState,
    reusable: &mut HashMap<PathBuf, Arc<dyn PanelView>>,
    welcome: Option<&Entity<WelcomePanel>>,
    dock_area: &gpui::WeakEntity<DockArea>,
    window: &mut Window,
    cx: &mut App,
) -> DockItem {
    match &state.info {
        PanelInfo::Stack { sizes, axis } => {
            let items: Vec<DockItem> = state
                .children
                .iter()
                .map(|child| rebuild_item(child, reusable, welcome, dock_area, window, cx))
                .collect();
            let axis = if *axis == 0 { Axis::Horizontal } else { Axis::Vertical };
            let sizes: Vec<Option<gpui::Pixels>> = sizes.iter().map(|size| Some(*size)).collect();
            DockItem::split_with_sizes(axis, items, sizes, dock_area, window, cx)
        }
        PanelInfo::Tabs { active_index } => {
            let panels: Vec<Arc<dyn PanelView>> = state
                .children
                .iter()
                .map(|leaf| resolve_leaf(leaf, reusable, welcome, dock_area, window, cx))
                .collect();
            let count = panels.len();
            let item = DockItem::tabs(panels, dock_area, window, cx);
            if count > 0 { item.active_index((*active_index).min(count - 1)) } else { item }
        }
        PanelInfo::Panel(_) | PanelInfo::Tiles { .. } => {
            let panel = resolve_leaf(state, reusable, welcome, dock_area, window, cx);
            DockItem::tabs(vec![panel], dock_area, window, cx)
        }
    }
}

/// Resolve one panel leaf to a live entity where possible: an existing
/// `FilePanel` by path, or the live `WelcomePanel`. Only when no live
/// entity exists (the genuine restore case) fall back to
/// `PanelRegistry::build_panel`, which constructs a fresh one.
///
/// `StringsPanel` is not in `reusable` (it isn't tracked in a
/// path-keyed registry the way `FilePanel` is -- see
/// `open_strings_for_active_file`'s doc), so a resync triggered while
/// one is open (drag-split, or healing a collapsed-to-empty tab panel)
/// always falls through to `PanelRegistry::build_panel` for it: a
/// fresh `StringsPanel::restore` that keeps the persisted path/
/// encoding/min_length but loses in-memory filter text, sort order,
/// and scan results (auto-run then re-populates them). Rare in
/// practice (resync only fires on those two triggers), so this is
/// accepted rather than widening the reuse registry to a second panel
/// kind.
fn resolve_leaf(
    leaf: &PanelState,
    reusable: &mut HashMap<PathBuf, Arc<dyn PanelView>>,
    welcome: Option<&Entity<WelcomePanel>>,
    dock_area: &gpui::WeakEntity<DockArea>,
    window: &mut Window,
    cx: &mut App,
) -> Arc<dyn PanelView> {
    if leaf.panel_name == FILE_PANEL_NAME
        && let Some(path) = file_path_from_info(&leaf.info)
        && let Some(panel) = reusable.remove(&path)
    {
        return panel;
    }
    if leaf.panel_name == WELCOME_PANEL_NAME
        && let Some(welcome) = welcome
    {
        return Arc::new(welcome.clone());
    }
    Arc::from(PanelRegistry::build_panel(&leaf.panel_name, dock_area.clone(), leaf, &leaf.info, window, cx))
}

/// Spawn the file-watch reconcile-and-drain loop: a fixed-cadence
/// timer, not the watcher's own wake (which only signals liveness --
/// see `crate::watch::FileWatch`'s doc), because GPUI has no per-frame
/// idle-vs-active distinction for a background poll to piggyback on
/// the way egui's repaint-on-wake does. Exits once the workspace
/// entity is gone.
fn spawn_watch_poll(window: &mut Window, cx: &mut Context<Workspace>) -> Task<()> {
    cx.spawn_in(window, async move |this, cx| {
        loop {
            gpui::Timer::after(crate::watch::POLL_INTERVAL).await;
            if this.update_in(cx, |workspace, window, cx| workspace.poll_file_watch(window, cx)).is_err() {
                return;
            }
        }
    })
}

/// One button in the reload dialog's footer: clicking it resolves the
/// pending prompt with `decision` on the workspace, then closes the
/// dialog. A plain closure (not `cx.listener`) because the dialog's
/// footer builder runs outside any entity's `Context` -- see
/// `Workspace::open_reload_dialog`.
fn reload_button(id: &'static str, label: String, weak: WeakEntity<Workspace>, decision: ReloadDecision) -> Button {
    Button::new(id).label(label).on_click(move |_, window, cx| {
        if let Some(workspace) = weak.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.resolve_reload(decision, window, cx));
        }
        window.close_dialog(cx);
    })
}

/// One button in the save-before-closing dialog's footer: clicking it
/// resolves the pending prompt with `decision` on the workspace, then
/// closes the dialog. A plain closure (not `cx.listener`) because the
/// footer builder runs outside any entity's `Context` -- see
/// `Workspace::open_close_dialog`.
fn close_button(id: &'static str, label: String, weak: WeakEntity<Workspace>, decision: CloseDecision) -> Button {
    Button::new(id).label(label).on_click(move |_, window, cx| {
        resolve_close_on(&weak, decision, window, cx);
        window.close_dialog(cx);
    })
}

/// Resolve the pending save-before-closing prompt with `decision` on the
/// workspace behind `weak`. Used both by the footer buttons and by the
/// dialog's dismissal hooks (Escape / overlay / close icon all resolve
/// `Cancel`), so a dismissal can never leave `pending_close` stuck.
fn resolve_close_on(weak: &WeakEntity<Workspace>, decision: CloseDecision, window: &mut Window, cx: &mut App) {
    if let Some(workspace) = weak.upgrade() {
        workspace.update(cx, |workspace, cx| workspace.resolve_close(decision, window, cx));
    }
}

/// The snapshots dialog's Take button: capture a snapshot of the
/// reference file (auto-named), then reopen the dialog so the new row
/// shows. A plain closure because the footer builder runs outside a
/// `Context`.
fn snapshot_take_button(weak: &WeakEntity<Workspace>) -> Button {
    let weak = weak.clone();
    Button::new("snapshot-take").label(hxy_i18n::t("snapshot-take-button")).on_click(move |_, window, cx| {
        window.close_dialog(cx);
        if let Some(workspace) = weak.upgrade() {
            workspace.update(cx, |workspace, cx| {
                workspace.capture_active_snapshot(String::new(), window, cx);
                workspace.open_snapshots_dialog(window, cx);
            });
        }
    })
}

/// A snapshot row's Delete button: drop the snapshot, then reopen the
/// dialog to refresh the list.
fn snapshot_delete_button(
    weak: &WeakEntity<Workspace>,
    file: &WeakEntity<FilePanel>,
    id: hxy_panels::files::snapshot::SnapshotId,
) -> Button {
    let weak = weak.clone();
    let file = file.clone();
    Button::new(SharedString::from(format!("snapshot-delete-{}", id.get())))
        .label(hxy_i18n::t("snapshot-delete"))
        .on_click(move |_, window, cx| {
            window.close_dialog(cx);
            if let Some(file) = file.upgrade() {
                file.update(cx, |file, _cx| file.delete_snapshot(id));
            }
            if let Some(workspace) = weak.upgrade() {
                workspace.update(cx, |workspace, cx| workspace.open_snapshots_dialog(window, cx));
            }
        })
}

/// A snapshot row's Compare button: spawn a Compare tab between the
/// snapshot's frozen bytes and the live buffer, then close the dialog.
fn snapshot_compare_button(
    weak: &WeakEntity<Workspace>,
    file: &WeakEntity<FilePanel>,
    id: hxy_panels::files::snapshot::SnapshotId,
) -> Button {
    let weak = weak.clone();
    let file = file.clone();
    Button::new(SharedString::from(format!("snapshot-compare-{}", id.get())))
        .label(hxy_i18n::t("snapshot-compare-current"))
        .on_click(move |_, window, cx| {
            window.close_dialog(cx);
            if let (Some(workspace), Some(file)) = (weak.upgrade(), file.upgrade()) {
                workspace.update(cx, |workspace, cx| workspace.compare_snapshot_with_current(file, id, window, cx));
            }
        })
}

/// Register the on-quit unsaved-patch persistence hook. gpui runs
/// `on_app_quit` callbacks during shutdown with a short budget and no way
/// to cancel; the work here (a few small JSON writes, content-hashing
/// capped at 32 MiB per file) stays synchronous and returns immediately.
/// The returned `Subscription` is held on the workspace so the hook lives
/// as long as the window does.
fn register_quit_persistence(cx: &mut Context<Workspace>) -> Subscription {
    cx.on_app_quit(|workspace: &mut Workspace, cx: &mut Context<Workspace>| {
        workspace.persist_unsaved_on_quit(cx);
        // Close the settings sink's pool on the way out (egui parity:
        // its shutdown path calls `SaveSink::close`). Every write was
        // already committed synchronously; this just checkpoints WAL.
        if cx.has_global::<crate::settings::SettingsSink>()
            && let Some(sink) = cx.remove_global::<crate::settings::SettingsSink>().0
        {
            sink.close();
        }
        // Nothing to await -- the persistence pass is synchronous.
        async {}
    })
}

/// One button in the restore-unsaved-edits dialog's footer: resolves the
/// pending restore with `decision`, then closes the dialog.
fn restore_button(id: &'static str, label: String, weak: WeakEntity<Workspace>, decision: RestoreDecision) -> Button {
    Button::new(id).label(label).on_click(move |_, window, cx| {
        if let Some(workspace) = weak.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.resolve_restore(decision, window, cx));
        }
        window.close_dialog(cx);
        // Offer the next queued session-restore prompt after this dialog
        // is torn down, so at most one is ever on screen.
        if let Some(workspace) = weak.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.stage_next_restore(window, cx));
        }
    })
}

/// Dismissing the restore dialog (Escape / overlay / close icon) clears
/// the pending prompt but leaves the sidecar on disk, so the next open
/// re-offers it. Distinct from the Discard button, which drops it.
fn dismiss_restore(weak: &WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    if let Some(workspace) = weak.upgrade() {
        workspace.update(cx, |workspace, _cx| {
            workspace.pending_restore = None;
        });
    }
    // Advance the session-restore queue after the current dialog closes,
    // so escaping one restored file's prompt still surfaces the next.
    let weak = weak.clone();
    window.defer(cx, move |window, cx| {
        if let Some(workspace) = weak.upgrade() {
            workspace.update(cx, |workspace, cx| workspace.stage_next_restore(window, cx));
        }
    });
}

/// Dismissing the dialog any way other than a footer button -- Escape,
/// a click on the overlay, or the close icon -- must still resolve the
/// pending prompt (`Ignore`), not just close the dialog and leave
/// `pending_reload` set forever: gpui-component's `Dialog` fires
/// `on_cancel`/`on_close` for all three of those paths but never calls
/// back into `resolve_reload` on its own, unlike the footer buttons,
/// which call it directly. Matches egui's dialog, whose window-chrome
/// close routes to the same "ignore" branch as the Ignore button.
fn dismiss_reload_as_ignore(weak: &WeakEntity<Workspace>, window: &mut Window, cx: &mut App) {
    if let Some(workspace) = weak.upgrade() {
        workspace.update(cx, |workspace, cx| {
            // Dismissal is a non-decision: egui's Cancel branch returns
            // before its remember mapping, so a ticked checkbox must
            // not persist a Never pref here.
            workspace.reload_remember.set(false);
            workspace.resolve_reload(ReloadDecision::Ignore, window, cx);
        });
    }
}

/// Whether any file panel in a dumped `PanelState` tree has `target` as
/// its path.
fn dump_has_file_path(state: &PanelState, target: &Path) -> bool {
    (state.panel_name == FILE_PANEL_NAME && file_path_from_info(&state.info).as_deref() == Some(target))
        || state.children.iter().any(|child| dump_has_file_path(child, target))
}

/// Same idea as `dump_has_file_path` but for a `StringsPanel`'s owning
/// path, which may itself be `None` (an untitled file's strings tab).
fn dump_has_strings_path(state: &PanelState, target: Option<&Path>) -> bool {
    (state.panel_name == STRINGS_PANEL_NAME && file_path_from_info(&state.info).as_deref() == target)
        || state.children.iter().any(|child| dump_has_strings_path(child, target))
}

/// Same idea as `dump_has_strings_path` but for an `EntropyPanel`'s
/// owning path.
fn dump_has_entropy_path(state: &PanelState, target: Option<&Path>) -> bool {
    (state.panel_name == ENTROPY_PANEL_NAME && file_path_from_info(&state.info).as_deref() == target)
        || state.children.iter().any(|child| dump_has_entropy_path(child, target))
}

/// Same idea as `dump_has_strings_path` but for a `ChecksumsPanel`'s
/// owning path.
fn dump_has_checksums_path(state: &PanelState, target: Option<&Path>) -> bool {
    (state.panel_name == CHECKSUMS_PANEL_NAME && file_path_from_info(&state.info).as_deref() == target)
        || state.children.iter().any(|child| dump_has_checksums_path(child, target))
}

/// Same idea as `dump_has_strings_path` but for a `VisualizerPanel`'s
/// owning path.
fn dump_has_visualizer_path(state: &PanelState, target: Option<&Path>) -> bool {
    (state.panel_name == VISUALIZER_PANEL_NAME && file_path_from_info(&state.info).as_deref() == target)
        || state.children.iter().any(|child| dump_has_visualizer_path(child, target))
}

/// Whether the dumped tree has a `GlobalSearchPanel` anywhere -- a
/// singleton with no owning path, so unlike its `dump_has_*_path`
/// siblings this only needs to check presence.
fn dump_has_global_search(state: &PanelState) -> bool {
    state.panel_name == GLOBAL_SEARCH_PANEL_NAME || state.children.iter().any(dump_has_global_search)
}

/// Whether a settings tab is anywhere in the dumped center tree. Same
/// singleton shape as `dump_has_global_search`.
fn dump_has_settings(state: &PanelState) -> bool {
    state.panel_name == SETTINGS_PANEL_NAME || state.children.iter().any(dump_has_settings)
}

fn file_path_from_info(info: &PanelInfo) -> Option<PathBuf> {
    match info {
        PanelInfo::Panel(value) => value.get("path").and_then(|path| path.as_str()).map(PathBuf::from),
        _ => None,
    }
}

/// Read a hex pane's whole in-memory source into an owned buffer for a
/// compare snapshot; empty on read failure (logged).
fn read_pane_bytes(pane: &Entity<HexPane>, cx: &App) -> Vec<u8> {
    let source = pane.read(cx).editor().source().clone();
    let len = source.len().get();
    if len == 0 {
        return Vec::new();
    }
    let range = match hxy_core::ByteRange::new(ByteOffset::new(0), ByteOffset::new(len)) {
        Ok(range) => range,
        Err(err) => {
            tracing::warn!(%err, "compare: open-file byte range");
            return Vec::new();
        }
    };
    match source.read(range) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(%err, "compare: read open-file bytes");
            Vec::new()
        }
    }
}

/// Collect the live `FilePanel` entities from a `DockItem` tree. Only
/// accurate right after the cache is rebuilt (`load` / `set_center`);
/// `DockItem::Tabs.items` is stale after incremental add/remove.
fn collect_file_entities(item: &DockItem, out: &mut Vec<Entity<FilePanel>>) {
    match item {
        DockItem::Split { items, .. } => items.iter().for_each(|item| collect_file_entities(item, out)),
        DockItem::Tabs { items, .. } => {
            for panel in items {
                if let Ok(file) = panel.view().downcast::<FilePanel>() {
                    out.push(file);
                }
            }
        }
        DockItem::Panel { view, .. } => {
            if let Ok(file) = view.view().downcast::<FilePanel>() {
                out.push(file);
            }
        }
        DockItem::Tiles { .. } => {}
    }
}

/// Collect the live `StringsPanel` entities from a `DockItem` tree, for
/// the open-or-focus dedup in `open_strings_for_active_file`. Same
/// staleness caveat as `collect_file_entities`.
fn collect_strings_entities(item: &DockItem, out: &mut Vec<Entity<StringsPanel>>) {
    match item {
        DockItem::Split { items, .. } => items.iter().for_each(|item| collect_strings_entities(item, out)),
        DockItem::Tabs { items, .. } => {
            for panel in items {
                if let Ok(strings) = panel.view().downcast::<StringsPanel>() {
                    out.push(strings);
                }
            }
        }
        DockItem::Panel { view, .. } => {
            if let Ok(strings) = view.view().downcast::<StringsPanel>() {
                out.push(strings);
            }
        }
        DockItem::Tiles { .. } => {}
    }
}

/// Collect the live `EntropyPanel` entities from a `DockItem` tree.
/// Mirrors `collect_strings_entities`.
fn collect_entropy_entities(item: &DockItem, out: &mut Vec<Entity<EntropyPanel>>) {
    match item {
        DockItem::Split { items, .. } => items.iter().for_each(|item| collect_entropy_entities(item, out)),
        DockItem::Tabs { items, .. } => {
            for panel in items {
                if let Ok(entropy) = panel.view().downcast::<EntropyPanel>() {
                    out.push(entropy);
                }
            }
        }
        DockItem::Panel { view, .. } => {
            if let Ok(entropy) = view.view().downcast::<EntropyPanel>() {
                out.push(entropy);
            }
        }
        DockItem::Tiles { .. } => {}
    }
}

/// Collect the live `ChecksumsPanel` entities from a `DockItem` tree.
/// Mirrors `collect_strings_entities`.
fn collect_checksums_entities(item: &DockItem, out: &mut Vec<Entity<ChecksumsPanel>>) {
    match item {
        DockItem::Split { items, .. } => items.iter().for_each(|item| collect_checksums_entities(item, out)),
        DockItem::Tabs { items, .. } => {
            for panel in items {
                if let Ok(checksums) = panel.view().downcast::<ChecksumsPanel>() {
                    out.push(checksums);
                }
            }
        }
        DockItem::Panel { view, .. } => {
            if let Ok(checksums) = view.view().downcast::<ChecksumsPanel>() {
                out.push(checksums);
            }
        }
        DockItem::Tiles { .. } => {}
    }
}

/// Collect the live `VisualizerPanel` entities from a `DockItem` tree.
/// Mirrors `collect_strings_entities`.
fn collect_visualizer_entities(item: &DockItem, out: &mut Vec<Entity<VisualizerPanel>>) {
    match item {
        DockItem::Split { items, .. } => items.iter().for_each(|item| collect_visualizer_entities(item, out)),
        DockItem::Tabs { items, .. } => {
            for panel in items {
                if let Ok(visualizer) = panel.view().downcast::<VisualizerPanel>() {
                    out.push(visualizer);
                }
            }
        }
        DockItem::Panel { view, .. } => {
            if let Ok(visualizer) = view.view().downcast::<VisualizerPanel>() {
                out.push(visualizer);
            }
        }
        DockItem::Tiles { .. } => {}
    }
}

/// Collect the live `GlobalSearchPanel` entities from a `DockItem` tree
/// (at most one -- it's a singleton -- but shaped like its siblings for
/// reuse in `rebuild_center_cache`). Mirrors `collect_checksums_entities`.
fn collect_global_search_entities(item: &DockItem, out: &mut Vec<Entity<GlobalSearchPanel>>) {
    match item {
        DockItem::Split { items, .. } => items.iter().for_each(|item| collect_global_search_entities(item, out)),
        DockItem::Tabs { items, .. } => {
            for panel in items {
                if let Ok(search) = panel.view().downcast::<GlobalSearchPanel>() {
                    out.push(search);
                }
            }
        }
        DockItem::Panel { view, .. } => {
            if let Ok(search) = view.view().downcast::<GlobalSearchPanel>() {
                out.push(search);
            }
        }
        DockItem::Tiles { .. } => {}
    }
}

/// The file panel backing the active tab, if the active tab is a file.
/// With splits, the first tab container that has an active file wins.
///
/// Returns `None` when the active tab is a `StringsPanel` instead, so
/// `reconcile`'s `set_active_file(None)` blanks the inspector, the
/// window title, and the file-scoped palette entries until a file tab
/// regains focus. `close_active_tab` does not have this gap: it checks
/// `active_strings_panel` itself first, so `cmd-w` still closes the
/// strings tab, not a background file. Feeding `active_strings_panel`'s
/// owning file into `self.active_file` here too was tried and reverted
/// -- `self.active_file` also drives `focus_existing_tab`'s "already
/// the active file, just refocus" fast path, and widening its meaning
/// broke that: reopening a file whose own strings tab is front-most
/// would then skip re-adding the file tab, since `self.active_file`
/// already matched. Fixing the inspector/title blanking needs a field
/// that means "reference file" without also meaning "the tab
/// `focus_existing_tab`/`open_file_for_path` treat as already focused"
/// -- its own design decision, not folded into this task.
fn active_file_panel(item: &DockItem, cx: &App) -> Option<Entity<FilePanel>> {
    match item {
        DockItem::Tabs { view, .. } => {
            view.read(cx).active_panel(cx).and_then(|panel| panel.view().downcast::<FilePanel>().ok())
        }
        DockItem::Split { items, .. } => items.iter().find_map(|item| active_file_panel(item, cx)),
        DockItem::Panel { view, .. } => view.view().downcast::<FilePanel>().ok(),
        DockItem::Tiles { .. } => None,
    }
}

/// The `StringsPanel` backing the active tab, if the active tab is a
/// strings tab. Mirrors `active_file_panel`; `close_active_tab` checks
/// this first so `cmd-w` closes whichever tab is actually front-most
/// (a strings tab included), not just file tabs.
///
/// Safe to walk the (possibly cache-stale, see `resolve_leaf`'s doc)
/// `DockItem` tree here: unlike `DockItem::Tabs.items`, the `view:
/// Entity<TabPanel>` handle each `Tabs`/`Panel` node carries is never
/// stale -- incremental adds mutate the live `TabPanel` entity in
/// place, and `active_panel` reads through that handle, not the cached
/// vec.
fn active_strings_panel(item: &DockItem, cx: &App) -> Option<Entity<StringsPanel>> {
    match item {
        DockItem::Tabs { view, .. } => {
            view.read(cx).active_panel(cx).and_then(|panel| panel.view().downcast::<StringsPanel>().ok())
        }
        DockItem::Split { items, .. } => items.iter().find_map(|item| active_strings_panel(item, cx)),
        DockItem::Panel { view, .. } => view.view().downcast::<StringsPanel>().ok(),
        DockItem::Tiles { .. } => None,
    }
}

/// The `EntropyPanel` backing the active tab, if the active tab is an
/// entropy tab. Mirrors `active_strings_panel`.
fn active_entropy_panel(item: &DockItem, cx: &App) -> Option<Entity<EntropyPanel>> {
    match item {
        DockItem::Tabs { view, .. } => {
            view.read(cx).active_panel(cx).and_then(|panel| panel.view().downcast::<EntropyPanel>().ok())
        }
        DockItem::Split { items, .. } => items.iter().find_map(|item| active_entropy_panel(item, cx)),
        DockItem::Panel { view, .. } => view.view().downcast::<EntropyPanel>().ok(),
        DockItem::Tiles { .. } => None,
    }
}

/// The `ChecksumsPanel` backing the active tab, if the active tab is a
/// checksums tab. Mirrors `active_strings_panel`.
fn active_checksums_panel(item: &DockItem, cx: &App) -> Option<Entity<ChecksumsPanel>> {
    match item {
        DockItem::Tabs { view, .. } => {
            view.read(cx).active_panel(cx).and_then(|panel| panel.view().downcast::<ChecksumsPanel>().ok())
        }
        DockItem::Split { items, .. } => items.iter().find_map(|item| active_checksums_panel(item, cx)),
        DockItem::Panel { view, .. } => view.view().downcast::<ChecksumsPanel>().ok(),
        DockItem::Tiles { .. } => None,
    }
}

/// The `VisualizerPanel` backing the active tab, if the active tab is a
/// visualizer tab. Mirrors `active_strings_panel`.
fn active_visualizer_panel(item: &DockItem, cx: &App) -> Option<Entity<VisualizerPanel>> {
    match item {
        DockItem::Tabs { view, .. } => {
            view.read(cx).active_panel(cx).and_then(|panel| panel.view().downcast::<VisualizerPanel>().ok())
        }
        DockItem::Split { items, .. } => items.iter().find_map(|item| active_visualizer_panel(item, cx)),
        DockItem::Panel { view, .. } => view.view().downcast::<VisualizerPanel>().ok(),
        DockItem::Tiles { .. } => None,
    }
}

/// The active center tab's panel view when its type is not one this
/// build registers -- an `InvalidPanel` placeholder from a stale layout,
/// or any foreign leaf. Returns `None` for every recognized hxy panel
/// (so `close_active_tab`'s fallback never disturbs a real tab and its
/// bookkeeping). Identity is by `panel_name`: `InvalidPanel` reports
/// "InvalidPanel", absent from the known set. Mirrors
/// `active_strings_panel`'s tree walk.
fn active_unrecognized_panel(item: &DockItem, cx: &App) -> Option<Arc<dyn PanelView>> {
    let active = match item {
        DockItem::Tabs { view, .. } => view.read(cx).active_panel(cx),
        DockItem::Split { items, .. } => items.iter().find_map(|item| active_unrecognized_panel(item, cx)),
        DockItem::Panel { view, .. } => Some(view.clone()),
        DockItem::Tiles { .. } => None,
    }?;
    if is_known_panel_name(active.panel_name(cx)) { None } else { Some(active) }
}

/// Whether `name` is a panel type this build registers (see
/// `panels::register`). An unknown name means a stale/foreign tab, most
/// notably gpui-component's `InvalidPanel` fallback.
fn is_known_panel_name(name: &str) -> bool {
    matches!(
        name,
        FILE_PANEL_NAME
            | WELCOME_PANEL_NAME
            | INSPECTOR_PANEL_NAME
            | STRINGS_PANEL_NAME
            | ENTROPY_PANEL_NAME
            | CHECKSUMS_PANEL_NAME
            | COMPARE_PANEL_NAME
            | VISUALIZER_PANEL_NAME
            | GLOBAL_SEARCH_PANEL_NAME
            | SETTINGS_PANEL_NAME
            | WORKSPACE_HOST_PANEL_NAME
    )
}

/// The `GlobalSearchPanel` backing the active tab, if the active tab is
/// the global search tab. Mirrors `active_checksums_panel`.
fn active_global_search_panel(item: &DockItem, cx: &App) -> Option<Entity<GlobalSearchPanel>> {
    match item {
        DockItem::Tabs { view, .. } => {
            view.read(cx).active_panel(cx).and_then(|panel| panel.view().downcast::<GlobalSearchPanel>().ok())
        }
        DockItem::Split { items, .. } => items.iter().find_map(|item| active_global_search_panel(item, cx)),
        DockItem::Panel { view, .. } => view.view().downcast::<GlobalSearchPanel>().ok(),
        DockItem::Tiles { .. } => None,
    }
}

/// Collect the live `SettingsPanel` entities from a `DockItem` tree
/// (at most one -- singleton). Mirrors `collect_global_search_entities`.
fn collect_settings_entities(item: &DockItem, out: &mut Vec<Entity<SettingsPanel>>) {
    match item {
        DockItem::Split { items, .. } => items.iter().for_each(|item| collect_settings_entities(item, out)),
        DockItem::Tabs { items, .. } => {
            for panel in items {
                if let Ok(settings) = panel.view().downcast::<SettingsPanel>() {
                    out.push(settings);
                }
            }
        }
        DockItem::Panel { view, .. } => {
            if let Ok(settings) = view.view().downcast::<SettingsPanel>() {
                out.push(settings);
            }
        }
        DockItem::Tiles { .. } => {}
    }
}

/// The `SettingsPanel` backing the active tab, if the active tab is
/// the settings tab. Mirrors `active_global_search_panel`.
fn active_settings_panel(item: &DockItem, cx: &App) -> Option<Entity<SettingsPanel>> {
    match item {
        DockItem::Tabs { view, .. } => {
            view.read(cx).active_panel(cx).and_then(|panel| panel.view().downcast::<SettingsPanel>().ok())
        }
        DockItem::Split { items, .. } => items.iter().find_map(|item| active_settings_panel(item, cx)),
        DockItem::Panel { view, .. } => view.view().downcast::<SettingsPanel>().ok(),
        DockItem::Tiles { .. } => None,
    }
}

/// The `FilePanel` in whichever center-dock `Tabs` leaf's live
/// `focus_handle` equals `focused`, if any. `TabPanel::focus_handle`
/// resolves to its active panel's own handle (gpui-component 0.5.1), so
/// this correctly identifies "which leaf does the currently-focused
/// element belong to" without needing the leaf's `TabPanel` identity
/// tracked separately. Used by `sync_active_file_after_pick` to re-derive
/// the active file after the pane picker moves focus directly.
fn focused_center_file_panel(item: &DockItem, focused: &FocusHandle, cx: &App) -> Option<Entity<FilePanel>> {
    match item {
        DockItem::Tabs { view, .. } => {
            if &view.read(cx).focus_handle(cx) != focused {
                return None;
            }
            view.read(cx).active_panel(cx).and_then(|panel| panel.view().downcast::<FilePanel>().ok())
        }
        DockItem::Split { items, .. } => items.iter().find_map(|item| focused_center_file_panel(item, focused, cx)),
        DockItem::Panel { .. } | DockItem::Tiles { .. } => None,
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = window_title_text(self.active_path(cx).as_deref());
        if self.last_title.as_deref() != Some(title.as_str()) {
            window.set_window_title(&title);
            self.last_title = Some(title);
        }

        // The settings-driven byte-value palette is theme-dependent
        // (dark/light gradient constants); a system appearance flip
        // re-derives it for every open pane. Deferred out of render
        // like `reconcile` below.
        let dark = cx.theme().mode.is_dark();
        if self.applied_dark != Some(dark) {
            let first_observation = self.applied_dark.is_none();
            self.applied_dark = Some(dark);
            // The boot render observes the theme for the first time;
            // panes already derived their palette against it at
            // construction, so only later flips re-sync.
            if !first_observation {
                let this = cx.entity().downgrade();
                window.defer(cx, move |_window, cx| {
                    if let Some(this) = this.upgrade() {
                        this.update(cx, |workspace, cx| workspace.sync_byte_palettes(cx));
                    }
                });
            }
        }

        if self.needs_reconcile {
            self.needs_reconcile = false;
            let this = cx.entity().downgrade();
            window.defer(cx, move |window, cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |workspace, cx| workspace.reconcile(window, cx));
                }
            });
        }

        if self.focus_pending {
            self.focus_pending = false;
            // Skip the actual focus move while an overlay (the pane picker
            // or the command palette) holds keyboard focus: any center-
            // cache rebuild that lands mid-overlay (e.g. `cmd-w` closing a
            // tab down to an empty leaf while the overlay is up) would
            // otherwise steal focus back to the active pane, leaving the
            // overlay visibly open but deaf to further letter/Escape
            // presses.
            if !self.pane_picker.read(cx).is_active() && !self.palette.read(cx).is_open() {
                let handle = match &self.active_file {
                    Some(file) => file.read(cx).pane().read(cx).focus_handle(cx),
                    None => self.focus_handle.clone(),
                };
                window.focus(&handle);
            }
        }

        let mut root = div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .on_action(cx.listener(Self::on_open_file))
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_save_as))
            .on_action(cx.listener(Self::on_reopen_closed))
            .on_action(cx.listener(Self::on_toggle_vim))
            .on_action(cx.listener(Self::on_toggle_inspector))
            .on_action(cx.listener(Self::on_toggle_global_search))
            .on_action(cx.listener(Self::on_open_strings))
            .on_action(cx.listener(Self::on_open_entropy))
            .on_action(cx.listener(Self::on_open_checksums))
            .on_action(cx.listener(Self::on_open_settings))
            .on_action(cx.listener(Self::on_take_snapshot))
            .on_action(cx.listener(Self::on_open_snapshots))
            .on_action(cx.listener(Self::on_open_palette))
            .on_action(cx.listener(Self::on_pick_pane))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::on_toggle_edit_mode))
            .on_action(cx.listener(Self::on_copy_bytes))
            .on_action(cx.listener(Self::on_copy_hex))
            .on_action(cx.listener(Self::on_show_about))
            .child(div().flex_1().child(self.dock.clone()));

        root = root.child(self.render_status_bar(cx));

        // The palette overlay is always childed; it renders an inert
        // empty element while closed and a full top-center overlay while
        // open (absolutely positioned, so it floats over the dock).
        root = root.child(self.palette.clone());

        // The pane picker overlay, likewise always childed: inert while
        // inactive, a centered target list while a `cmd-k` session is
        // open (see `gpui_dock_picker`'s crate docs for why it's a list
        // rather than badges over each pane).
        root = root.child(self.pane_picker.clone());

        // `gpui_component::Root` (the window's actual top-level view,
        // see `main.rs`) only renders its child; the child is
        // responsible for appending the dialog/sheet/notification
        // layers each frame. File-open errors, layout-restore warnings,
        // and the search bar's wrap/replace toasts all render through
        // the notification layer; the search bar's length-mismatch and
        // replace-all confirms render through the dialog layer.
        root.children(gpui_component::Root::render_dialog_layer(window, cx))
            .children(gpui_component::Root::render_notification_layer(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use gpui::AppContext as _;
    use gpui::TestAppContext;
    use gpui::WindowHandle;
    use hxy_core::ByteOffset;
    use hxy_core::ByteRange;
    use hxy_core::HexSource;
    use hxy_core::MemorySource;
    use hxy_core::Selection;

    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::panels::register(cx);
            init_keybindings(cx);
            crate::menu::init_keybindings(cx);
        });
    }

    fn temp_file(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn open_workspace(
        cx: &mut TestAppContext,
        initial: Vec<PathBuf>,
        layout_path: Option<PathBuf>,
    ) -> WindowHandle<Workspace> {
        let window = cx.add_window(move |window, cx| {
            let subscription = window.observe_window_appearance(|_, _| {});
            Workspace::new(initial, subscription, layout_path, window, cx)
        });
        cx.run_until_parked();
        window
    }

    /// Like [`open_workspace`], but wraps the `Workspace` in a real
    /// `gpui_component::Root` -- required by anything that touches the
    /// notification layer (`WindowExt::push_notification` panics
    /// without a `Root` as the window's actual root view). Returns the
    /// `Workspace` entity directly rather than a `WindowHandle<Workspace>`,
    /// since that handle type requires the window's root to literally
    /// be a `Workspace`, which it is not here (it's `Root`, mirroring
    /// the `DialogTestHost` pattern in `panels::search_bar`'s tests).
    fn open_workspace_with_root(
        cx: &mut TestAppContext,
        initial: Vec<PathBuf>,
        layout_path: Option<PathBuf>,
    ) -> (WindowHandle<gpui_component::Root>, Entity<Workspace>) {
        let window = cx.add_window(move |window, cx| {
            let subscription = window.observe_window_appearance(|_, _| {});
            let workspace = cx.new(|cx| Workspace::new(initial, subscription, layout_path, window, cx));
            gpui_component::Root::new(workspace, window, cx)
        });
        let root = window.root(cx).unwrap();
        let workspace = root.read_with(cx, |root, _| root.view().clone().downcast::<Workspace>().unwrap());
        cx.run_until_parked();
        (window, workspace)
    }

    fn file_count(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> usize {
        window.read_with(cx, |ws, cx| count_file_panels(&ws.dock.read(cx).dump(cx).center)).unwrap()
    }

    fn strings_tab_count(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> usize {
        window.read_with(cx, |ws, cx| count_strings_panels(&ws.dock.read(cx).dump(cx).center)).unwrap()
    }

    fn entropy_tab_count(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> usize {
        window.read_with(cx, |ws, cx| count_entropy_panels(&ws.dock.read(cx).dump(cx).center)).unwrap()
    }

    fn checksums_tab_count(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> usize {
        window.read_with(cx, |ws, cx| count_checksums_panels(&ws.dock.read(cx).dump(cx).center)).unwrap()
    }

    fn settings_tab_count(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> usize {
        window.read_with(cx, |ws, cx| count_settings_panels(&ws.dock.read(cx).dump(cx).center)).unwrap()
    }

    /// `open_settings` is a singleton: the first call adds one tab, a
    /// second call focuses the existing panel (same entity) instead of
    /// building another -- mirroring egui's `show_settings`.
    #[gpui::test]
    fn open_settings_focuses_the_existing_singleton(cx: &mut TestAppContext) {
        setup(cx);
        let window = open_workspace(cx, Vec::new(), None);

        window.update(cx, |ws, window, cx| ws.open_settings(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(settings_tab_count(window, cx), 1, "first open adds the tab");
        let first = window.read_with(cx, |ws, _| ws.settings_panel.clone()).unwrap().expect("singleton handle tracked");

        window.update(cx, |ws, window, cx| ws.open_settings(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(settings_tab_count(window, cx), 1, "second open focuses instead of duplicating");
        let second = window.read_with(cx, |ws, _| ws.settings_panel.clone()).unwrap().expect("singleton handle kept");
        assert_eq!(first.entity_id(), second.entity_id(), "the same panel entity is reused");
    }

    fn active_path(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> Option<PathBuf> {
        window.read_with(cx, |ws, cx| ws.active_path(cx)).unwrap()
    }

    /// Focus the active file's grid and type one hex digit so its buffer
    /// goes dirty (byte 0's high nibble becomes 0xA0). Returns the active
    /// `FilePanel`.
    fn dirty_active_file(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> Entity<FilePanel> {
        let file = window.read_with(cx, |ws, _| ws.active_file.clone()).unwrap().expect("a file is active");
        window
            .update(cx, |_ws, window, cx| {
                let pane = file.read(cx).pane().clone();
                pane.update(cx, |pane, cx| {
                    pane.editor_mut().set_selection(Some(Selection::caret(ByteOffset::new(0))));
                    cx.notify();
                });
                let handle = file.read(cx).pane().read(cx).focus_handle(cx);
                window.focus(&handle);
            })
            .unwrap();
        cx.run_until_parked();
        // Type one hex digit at byte 0: high nibble becomes 0xA -> 0xA0.
        cx.simulate_keystrokes(window.into(), "a");
        assert!(
            window.read_with(cx, |_ws, cx| file.read(cx).pane().read(cx).editor().is_dirty()).unwrap(),
            "typing should dirty the buffer",
        );
        file
    }

    /// `Save` writes the patched bytes to the tab's existing path and
    /// re-anchors the editor so the buffer goes clean -- the round-trip
    /// egui's `save_file_by_id` performs.
    #[gpui::test]
    fn save_writes_patched_bytes_and_reanchors(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "save.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &path, cx);
        let file = dirty_active_file(window, cx);

        window.update(cx, |ws, window, cx| ws.save_reference_file(SaveKind::Save, window, cx)).unwrap();
        cx.run_until_parked();

        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk[0], 0xA0, "the patched first byte reached disk");
        assert!(
            !window.read_with(cx, |_ws, cx| file.read(cx).pane().read(cx).editor().is_dirty()).unwrap(),
            "the editor re-anchors onto the saved bytes and goes clean",
        );
    }

    /// The save-before-closing prompt's Save answer writes then closes the
    /// tab (close-on-success, egui close.rs:446-450).
    #[gpui::test]
    fn dirty_close_save_writes_and_closes(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "close-save.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &path, cx);
        let file = dirty_active_file(window, cx);

        // Drive the resolution directly (the dialog's Save button calls
        // exactly this), bypassing the modal UI.
        window
            .update(cx, |ws, window, cx| {
                ws.pending_close = Some(file.clone());
                ws.resolve_close(CloseDecision::Save, window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        assert_eq!(std::fs::read(&path).unwrap()[0], 0xA0, "Save wrote the patch to disk");
        assert_eq!(file_count(window, cx), 0, "Save closed the tab after a successful write");
    }

    /// Cancelling the save-before-closing prompt leaves the tab open and
    /// still dirty (nothing written, nothing closed).
    #[gpui::test]
    fn dirty_close_cancel_keeps_the_tab(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "close-cancel.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &path, cx);
        let file = dirty_active_file(window, cx);

        window
            .update(cx, |ws, window, cx| {
                ws.pending_close = Some(file.clone());
                ws.resolve_close(CloseDecision::Cancel, window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        assert_eq!(file_count(window, cx), 1, "Cancel keeps the tab open");
        assert_eq!(std::fs::read(&path).unwrap(), vec![0u8; 16], "Cancel wrote nothing to disk");
        assert!(
            window.read_with(cx, |_ws, cx| file.read(cx).pane().read(cx).editor().is_dirty()).unwrap(),
            "Cancel keeps the buffer dirty",
        );
    }

    /// Don't Save closes the tab discarding the patch (nothing written).
    #[gpui::test]
    fn dirty_close_dont_save_discards_and_closes(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "close-discard.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &path, cx);
        let file = dirty_active_file(window, cx);

        window
            .update(cx, |ws, window, cx| {
                ws.pending_close = Some(file.clone());
                ws.resolve_close(CloseDecision::DontSave, window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        assert_eq!(file_count(window, cx), 0, "Don't Save closed the tab");
        assert_eq!(std::fs::read(&path).unwrap(), vec![0u8; 16], "Don't Save wrote nothing");
    }

    /// Closing a clean file tab remembers it, and Reopen Closed Tab
    /// (`cmd-shift-t`) brings it back, re-parking the caret.
    #[gpui::test]
    fn reopen_closed_tab_restores_the_last_closed_file(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "reopen.bin", &[1u8; 32]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &path, cx);
        assert_eq!(file_count(window, cx), 1);

        // Park the caret so the reopen has view state to restore.
        window
            .update(cx, |ws, _window, cx| {
                let pane = ws.active_file.as_ref().unwrap().read(cx).pane().clone();
                pane.update(cx, |pane, cx| {
                    pane.editor_mut().set_selection(Some(Selection::caret(ByteOffset::new(7))));
                    cx.notify();
                });
            })
            .unwrap();
        window.update(cx, |ws, window, cx| ws.close_active_tab(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 0, "the tab closed");

        window.update(cx, |ws, window, cx| ws.on_reopen_closed(&ReopenClosedTab, window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 1, "reopen brought the tab back");
        assert_eq!(active_path(window, cx), Some(path));
        let cursor = window
            .read_with(cx, |ws, cx| {
                ws.active_file.as_ref().unwrap().read(cx).pane().read(cx).editor().selection().map(|s| s.cursor.get())
            })
            .unwrap();
        assert_eq!(cursor, Some(7), "reopen re-parks the caret");
    }

    /// Restoring an unsaved-edits sidecar reapplies its patch onto the
    /// freshly opened (clean) buffer and forces the editor mutable -- the
    /// reopen-time restore round-trip. Uses a `Modified` sidecar so the
    /// apply is unconditional (the `Clean` path additionally digest-
    /// verifies; that is exercised by the store/load test in
    /// `crate::patches`).
    #[gpui::test]
    fn restore_reapplies_the_sidecar_patch(cx: &mut TestAppContext) {
        use hxy_panels::files::patch_persist;
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let path = temp_file(&dir, "restore.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &path, cx);
        let file = window.read_with(cx, |ws, _| ws.active_file.clone()).unwrap().expect("file open");

        // Build a sidecar carrying a one-byte patch, as a previous
        // session's dirty buffer would have persisted on quit.
        let base: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![0u8; 16]));
        let mut editor = hxy_editor::HexEditor::new(base);
        editor.splice(0, 1, vec![0xCD]).unwrap();
        let patch = editor.patch().read().unwrap().clone();
        let sidecar = patch_persist::snapshot(
            path.clone(),
            editor.source().as_ref(),
            patch,
            editor.undo_stack().to_vec(),
            editor.redo_stack().to_vec(),
        )
        .expect("non-empty patch");

        window
            .update(cx, |ws, window, cx| {
                ws.pending_restore = Some(PendingRestore {
                    file: file.clone(),
                    sidecar,
                    integrity: hxy_panels::files::patch_persist::RestoreIntegrity::Modified { reason: "test".into() },
                });
                ws.resolve_restore(RestoreDecision::Restore, window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        window
            .read_with(cx, |_ws, cx| {
                let editor = file.read(cx).pane().read(cx).editor();
                assert!(editor.is_dirty(), "restore reapplies the patch (buffer goes dirty)");
                let byte0 = editor
                    .source()
                    .read(hxy_core::ByteRange::new(ByteOffset::new(0), ByteOffset::new(1)).unwrap())
                    .unwrap();
                assert_eq!(byte0[0], 0xCD, "the sidecar patch's byte landed");
            })
            .unwrap();
    }

    /// Full quit-dirty -> relaunch -> accept round-trip: a dirty file's
    /// on-quit sidecar is offered as a restore prompt on session restore
    /// (the `build_initial` -> `dock.load` -> `FilePanel::restore` path,
    /// which never routes through `open_or_focus`), and ACCEPTING it on an
    /// untouched-on-disk file reapplies the edit AND the undo history, then
    /// drops the sidecar only after the successful apply. Exercises the
    /// base-digest fix: the Clean-path verify now passes for an unchanged
    /// disk instead of rejecting every restore.
    #[gpui::test]
    fn session_restore_accept_reapplies_edits_and_undo(cx: &mut TestAppContext) {
        use hxy_panels::files::patch_persist;
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let edits = dir.path().join("edits");
        crate::patches::set_edits_dir_for_test(edits.clone());
        let layout = dir.path().join("layout.json");
        let path = temp_file(&dir, "restore-session.bin", &[0u8; 16]);

        // First session: open, dirty (byte 0 -> 0xA0), persist the sidecar
        // as the on-quit hook would, and write the layout.
        let first = open_workspace(cx, Vec::new(), Some(layout.clone()));
        window_open(first, &path, cx);
        dirty_active_file(first, cx);
        first.read_with(cx, |ws, cx| ws.persist_unsaved_on_quit(cx)).unwrap();
        first.read_with(cx, |ws, cx| ws.save_now(cx).unwrap().unwrap()).unwrap();
        assert!(
            patch_persist::load(&edits, &path).unwrap().is_some(),
            "the on-quit hook wrote a sidecar for the dirty tab",
        );

        // Second session: rebuild from the layout. The restored tab, which
        // never routes through `open_or_focus`, stages a restore prompt.
        let (window, second) = open_workspace_with_root(cx, Vec::new(), Some(layout));
        let staged = second
            .read_with(cx, |ws, cx| ws.pending_restore.as_ref().map(|p| p.file.read(cx).path().map(Path::to_path_buf)));
        assert_eq!(staged, Some(Some(path.clone())), "the restored tab staged its restore prompt");
        let file = second.read_with(cx, |ws, _| ws.pending_restore.as_ref().unwrap().file.clone());

        // Accept: the on-disk file is untouched, so the Clean verify passes
        // and the patch + undo history reapply.
        cx.update_window(window.into(), |_, window, cx| {
            second.update(cx, |ws, cx| ws.resolve_restore(RestoreDecision::Restore, window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        second.read_with(cx, |ws, cx| {
            assert!(ws.pending_restore.is_none(), "resolving cleared the prompt");
            let editor = file.read(cx).pane().read(cx).editor();
            assert!(editor.is_dirty(), "accept reapplied the patch");
            assert!(editor.can_undo(), "accept reinstated the undo history");
            let byte0 = editor
                .source()
                .read(hxy_core::ByteRange::new(ByteOffset::new(0), ByteOffset::new(1)).unwrap())
                .unwrap();
            assert_eq!(byte0[0], 0xA0, "the persisted edit landed");
        });
        assert!(
            patch_persist::load(&edits, &path).unwrap().is_none(),
            "the sidecar is dropped only after a successful apply",
        );

        // Undo walks back to the on-disk baseline.
        cx.update_window(window.into(), |_, window, cx| {
            second.update(cx, |_ws, cx| {
                file.read(cx).pane().clone().update(cx, |pane, cx| {
                    pane.editor_mut().undo();
                    cx.notify();
                });
            });
            let _ = window;
        })
        .unwrap();
        second.read_with(cx, |_ws, cx| {
            let byte0 = file
                .read(cx)
                .pane()
                .read(cx)
                .editor()
                .source()
                .read(hxy_core::ByteRange::new(ByteOffset::new(0), ByteOffset::new(1)).unwrap())
                .unwrap();
            assert_eq!(byte0[0], 0x00, "undo reverted to the on-disk byte");
        });
    }

    /// Opening a file the template library recognises raises exactly
    /// one "Run <name>?" suggestion toast (the recognised file's, not
    /// the unrecognised sibling's), and records the offer on the panel
    /// so it is never re-raised.
    #[gpui::test]
    fn open_suggests_a_matching_template_once(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("png.bt"), "// ID Bytes: 89 50 4E 47\nuint32 a;\n").unwrap();
        cx.update(|cx| {
            cx.set_global(crate::templates::TemplateLibraryGlobal(hxy_templates::library::TemplateLibrary::load_from(
                Some(dir.path()),
            )));
        });
        let png = temp_file(&dir, "pic.png", &[0x89, 0x50, 0x4E, 0x47, 0, 0, 0, 0]);
        let plain = temp_file(&dir, "plain.xyz", &[0u8; 8]);

        let (window, ws) = open_workspace_with_root(cx, vec![png.clone(), plain.clone()], None);
        cx.run_until_parked();

        let count = cx.update_window(window.into(), |_, window, cx| window.notifications(cx).len()).unwrap();
        assert_eq!(count, 1, "one suggestion toast for the recognised file only");

        let panels = cx.update(|cx| cx.global::<OpenFilePanels>().0.clone());
        let declined_by_path: Vec<(Option<PathBuf>, bool)> = panels
            .iter()
            .map(|p| {
                cx.read(|cx| {
                    let p = p.read(cx);
                    (p.path().map(Path::to_path_buf), p.template_suggestion_declined)
                })
            })
            .collect();
        assert!(
            declined_by_path.contains(&(Some(png), true)),
            "the offer is recorded on the matched panel: {declined_by_path:?}"
        );
        assert!(
            declined_by_path.contains(&(Some(plain), false)),
            "the unmatched panel was never offered: {declined_by_path:?}"
        );
        drop(ws);
    }

    /// A Clean restore whose on-disk bytes no longer match the sidecar's
    /// base digest is NOT applied and, critically, the sidecar is KEPT
    /// (not silently discarded) with a warning toast -- so a stale verify
    /// can never lose the user's edits.
    #[gpui::test]
    fn failed_verify_keeps_the_sidecar_and_warns(cx: &mut TestAppContext) {
        use hxy_panels::files::patch_persist;
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let edits = dir.path().join("edits");
        crate::patches::set_edits_dir_for_test(edits.clone());
        let path = temp_file(&dir, "verify.bin", &[0u8; 16]);

        let (window, ws) = open_workspace_with_root(cx, vec![path.clone()], None);
        let file = ws.read_with(cx, |ws, _| ws.active_file.clone()).expect("file open");

        // Build a Clean-classified sidecar whose digest is of DIFFERENT
        // base bytes than what is on disk, so the verify must fail.
        let other: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![0x55u8; 16]));
        let mut editor = hxy_editor::HexEditor::new(other);
        editor.splice(0, 1, vec![0xCD]).unwrap();
        let patch = editor.patch().read().unwrap().clone();
        let sidecar = patch_persist::snapshot(
            path.clone(),
            editor.base_source().as_ref(),
            patch,
            editor.undo_stack().to_vec(),
            editor.redo_stack().to_vec(),
        )
        .expect("non-empty patch");
        patch_persist::store(&edits, &sidecar).unwrap();

        cx.update_window(window.into(), |_, window, cx| {
            ws.update(cx, |ws, cx| {
                ws.pending_restore = Some(PendingRestore {
                    file: file.clone(),
                    sidecar,
                    integrity: hxy_panels::files::patch_persist::RestoreIntegrity::Clean,
                });
                ws.resolve_restore(RestoreDecision::Restore, window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();

        assert!(!ws.read_with(cx, |_ws, cx| file.read(cx).is_dirty(cx)), "a failed verify does not apply the patch");
        assert!(patch_persist::load(&edits, &path).unwrap().is_some(), "a failed verify keeps the sidecar");
        let toasts = cx.update_window(window.into(), |_, window, cx| window.notifications(cx).len()).unwrap();
        assert_eq!(toasts, 1, "a failed verify warns the user");
    }

    /// Save As re-anchors the file's analysis panels onto the new path.
    /// A strings and a checksums tab both follow the rename, so their
    /// owning path (which keys recompute, cascade close, and dump) tracks
    /// the live file rather than the stale original.
    #[gpui::test]
    fn save_as_rebinds_analysis_panels(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "orig.bin", &[0u8; 16]);
        let f2 = dir.path().join("renamed.bin");
        let window = open_workspace(cx, vec![f1.clone()], None);

        window.update(cx, |ws, window, cx| ws.open_strings_for_active_file(window, cx)).unwrap();
        window.update(cx, |ws, window, cx| ws.open_checksums_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        window
            .read_with(cx, |ws, cx| {
                assert_eq!(ws.strings_panels.len(), 1);
                assert_eq!(ws.strings_panels[0].read(cx).owning_path(), Some(f1.as_path()));
                assert_eq!(ws.checksums_panels[0].read(cx).owning_path(), Some(f1.as_path()));
            })
            .unwrap();

        // Save As: write the file to a new path (the post-dialog branch of
        // `prompt_save_path`), which must rebind both panels onto f2.
        let file = window.read_with(cx, |ws, cx| ws.open_file_for_path(&f1, cx)).unwrap().expect("f1 open");
        window
            .update(cx, |ws, window, cx| {
                ws.write_file(&file, f2.clone(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .read_with(cx, |ws, cx| {
                assert_eq!(
                    ws.strings_panels[0].read(cx).owning_path(),
                    Some(f2.as_path()),
                    "the strings panel followed the rename",
                );
                assert_eq!(
                    ws.checksums_panels[0].read(cx).owning_path(),
                    Some(f2.as_path()),
                    "the checksums panel followed the rename",
                );
            })
            .unwrap();
    }

    /// Browse VFS is blocked while the archive tab is dirty: a toast tells
    /// the user to save first, the file tab stays put, and nothing mounts.
    #[gpui::test]
    fn browse_vfs_blocked_while_dirty(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let zip = temp_file(&dir, "archive.zip", &crate::panels::vfs_tree::test_support::fixture_zip_bytes());

        let (window, ws) = open_workspace_with_root(cx, vec![zip.clone()], None);
        let file = ws.read_with(cx, |ws, _| ws.active_file.clone()).expect("zip opened as a file");
        assert!(
            ws.read_with(cx, |_ws, cx| file.read(cx).detected_handler().is_some()),
            "the zip handler was detected on open",
        );

        // Dirty the buffer, then attempt to browse: blocked, with a toast.
        ws.update(cx, |_ws, cx| {
            file.read(cx).pane().clone().update(cx, |pane, cx| {
                pane.editor_mut().splice(0, 1, vec![0xFF]).unwrap();
                cx.notify();
            });
        });
        cx.update_window(window.into(), |_, window, cx| {
            ws.update(cx, |ws, cx| ws.browse_active_file_as_workspace(window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        ws.read_with(cx, |ws, cx| {
            assert_eq!(
                count_file_panels(&ws.dock.read(cx).dump(cx).center),
                1,
                "a dirty browse leaves the file tab intact",
            );
            assert!(
                !ws.center_panel_names(cx).iter().any(|n| n == crate::panels::WORKSPACE_HOST_PANEL_NAME),
                "nothing was mounted",
            );
        });
        let toasts = cx.update_window(window.into(), |_, window, cx| window.notifications(cx).len()).unwrap();
        assert_eq!(toasts, 1, "the block surfaced a toast");
    }

    /// Browsing a clean archive swaps the file tab for a workspace host and
    /// records the file on the reopen ring, so cmd-shift-t brings it back.
    /// A second plain file tab stays open so the center never empties (an
    /// empty center is an unrelated welcome-panel path).
    #[gpui::test]
    fn browse_vfs_clean_mounts_host_and_records_reopen(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let other = temp_file(&dir, "other.bin", &[0u8; 16]);
        let zip = temp_file(&dir, "archive.zip", &crate::panels::vfs_tree::test_support::fixture_zip_bytes());

        // Open the plain file first, then the zip (now the active tab).
        let window = open_workspace(cx, vec![other.clone(), zip.clone()], None);
        window.update(cx, |ws, window, cx| ws.browse_active_file_as_workspace(window, cx)).unwrap();
        cx.run_until_parked();
        window
            .read_with(cx, |ws, cx| {
                assert!(
                    ws.center_panel_names(cx).iter().any(|n| n == crate::panels::WORKSPACE_HOST_PANEL_NAME),
                    "a clean browse mounts a workspace host: {:?}",
                    ws.center_panel_names(cx),
                );
                assert_eq!(
                    count_file_panels(&ws.dock.read(cx).dump(cx).center),
                    1,
                    "the browsed zip's file tab was swapped out, the other stays",
                );
            })
            .unwrap();

        // The removed file tab is on the reopen ring; cmd-shift-t restores it.
        window.update(cx, |ws, window, cx| ws.on_reopen_closed(&ReopenClosedTab, window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 2, "cmd-shift-t restored the browsed file tab");
    }

    /// Capturing a snapshot freezes the file's current patched bytes, and
    /// "Compare with current" spawns a Compare tab whose A side holds
    /// exactly those frozen bytes -- the snapshot -> compare round-trip.
    #[gpui::test]
    fn snapshot_capture_then_compare_freezes_bytes(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        crate::persist::set_snapshots_base_for_test(dir.path().join("snapshots"));
        let path = temp_file(&dir, "snap.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &path, cx);
        let file = dirty_active_file(window, cx);

        // Capture the current (edited: byte 0 = 0xA0) bytes as a snapshot.
        let id = window
            .update(cx, |_ws, _window, cx| file.update(cx, |file, cx| file.capture_snapshot("v1".into(), cx)))
            .unwrap()
            .expect("capture returns an id");
        let frozen =
            window.read_with(cx, |_ws, cx| file.read(cx).snapshot_bytes(id).map(|b| b.as_ref().clone())).unwrap();
        assert_eq!(frozen.as_deref().map(|b| b[0]), Some(0xA0), "the snapshot froze the edited byte");

        // Mutate the live buffer further; the snapshot must not change.
        window
            .update(cx, |_ws, _window, cx| {
                file.read(cx).pane().clone().update(cx, |pane, cx| {
                    pane.editor_mut().splice(0, 1, vec![0x11]).unwrap();
                    cx.notify();
                });
            })
            .unwrap();

        window.update(cx, |ws, window, cx| ws.compare_snapshot_with_current(file.clone(), id, window, cx)).unwrap();
        cx.run_until_parked();
        let names = window.read_with(cx, |ws, cx| ws.center_panel_names(cx)).unwrap();
        assert!(names.iter().any(|n| n == crate::panels::COMPARE_PANEL_NAME), "a compare tab spawned: {names:?}");
        // The snapshot bytes are still the captured 0xA0, independent of
        // the later live edit to 0x11.
        let still =
            window.read_with(cx, |_ws, cx| file.read(cx).snapshot_bytes(id).map(|b| b.as_ref().clone())).unwrap();
        assert_eq!(still.as_deref().map(|b| b[0]), Some(0xA0), "the snapshot stays frozen after further edits");
    }

    /// A pathless buffer has no stable snapshot key, so capture is a no-op
    /// (the dialog reports "no store" for it).
    #[gpui::test]
    fn snapshot_capture_is_a_noop_without_a_path(cx: &mut TestAppContext) {
        setup(cx);
        let window = open_workspace(cx, Vec::new(), None);
        let file = window
            .update(cx, |ws, window, cx| {
                let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![0u8; 8]));
                let panel = cx.new(|cx| FilePanel::new_vfs_entry(source, "entry".into(), window, cx));
                ws.add_file_panel(panel.clone(), window, cx);
                panel
            })
            .unwrap();
        cx.run_until_parked();
        let id = window
            .update(cx, |_ws, _window, cx| file.update(cx, |file, cx| file.capture_snapshot(String::new(), cx)))
            .unwrap();
        assert!(id.is_none(), "a pathless buffer captures nothing");
        assert!(!window.read_with(cx, |_ws, cx| file.read(cx).has_snapshot_store()).unwrap());
    }

    /// The reopen ring is bounded at [`CLOSED_TABS_CAPACITY`]: closing
    /// more than that many tabs drops the oldest, so a reopen can never
    /// resurrect a tab past the cap.
    #[gpui::test]
    fn reopen_ring_is_capped(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let window = open_workspace(cx, Vec::new(), None);
        for i in 0..(CLOSED_TABS_CAPACITY + 3) {
            let path = temp_file(&dir, &format!("ring-{i}.bin"), &[0u8; 4]);
            window_open(window, &path, cx);
            window.update(cx, |ws, window, cx| ws.close_active_tab(window, cx)).unwrap();
            cx.run_until_parked();
        }
        let len = window.read_with(cx, |ws, _| ws.closed_tabs.len()).unwrap();
        assert_eq!(len, CLOSED_TABS_CAPACITY, "the ring never exceeds its cap");
    }

    /// A CLI-loaded file lands in a focused pane so keystrokes reach the
    /// editor with no click first (regression of the M1 focus test,
    /// adapted to the dock's active-panel routing).
    #[gpui::test]
    fn cli_open_focuses_the_active_pane(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "test.bin", &[0u8; 16]);
        let window = open_workspace(cx, vec![f1], None);

        let pane_handle = window
            .read_with(cx, |ws, cx| ws.active_file.as_ref().unwrap().read(cx).pane().read(cx).focus_handle(cx))
            .unwrap();
        let focused = window.update(cx, |_ws, window, cx| window.focused(cx)).unwrap();
        assert_eq!(focused, Some(pane_handle));
    }

    /// Every readable CLI path becomes a tab, and the last one opens on
    /// top and ends up focused -- mirrors the egui app, where each
    /// `push_to_focused_leaf`'d open makes its own tab active in turn.
    #[gpui::test]
    fn cli_boot_opens_every_path_and_focuses_the_last(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);
        let f3 = temp_file(&dir, "c.bin", &[3u8; 16]);

        let window = open_workspace(cx, vec![f1.clone(), f2.clone(), f3.clone()], None);

        assert_eq!(file_count(window, cx), 3);
        assert_eq!(active_path(window, cx), Some(f3));
        let paths = window.read_with(cx, |ws, cx| collect_file_paths(&ws.dock.read(cx).dump(cx).center)).unwrap();
        assert!(paths.contains(&f1) && paths.contains(&f2));
    }

    /// An unreadable path among the CLI args does not abort the others:
    /// the readable ones still open, and the failure surfaces as the
    /// same error toast a failed `cmd-o` open would show, not a process
    /// exit.
    #[gpui::test]
    fn cli_boot_with_unreadable_path_opens_the_rest_and_surfaces_an_error(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let missing = dir.path().join("missing.bin");
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);

        let (window, ws) = open_workspace_with_root(cx, vec![f1.clone(), missing, f2.clone()], None);

        let count = ws.read_with(cx, |ws, cx| count_file_panels(&ws.dock.read(cx).dump(cx).center));
        assert_eq!(count, 2, "the unreadable path must not stop the readable ones from opening");
        let paths = ws.read_with(cx, |ws, cx| collect_file_paths(&ws.dock.read(cx).dump(cx).center));
        assert!(paths.contains(&f1) && paths.contains(&f2));

        let toasts = cx.update_window(window.into(), |_, window, cx| window.notifications(cx).len()).unwrap();
        assert_eq!(toasts, 1, "the unreadable path must surface an error toast");
    }

    /// A path repeated on the command line does not open a second tab;
    /// it focuses the tab already opened for it, same as a repeated
    /// `cmd-o`.
    #[gpui::test]
    fn cli_boot_dedups_duplicate_paths(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);

        let window = open_workspace(cx, vec![f1.clone(), f2.clone(), f1.clone()], None);

        assert_eq!(file_count(window, cx), 2, "a repeated CLI path must not duplicate the tab");
        assert_eq!(active_path(window, cx), Some(f1), "the repeated path's tab must end up focused");
    }

    /// With no file open the welcome placeholder shows and focus rests
    /// on the workspace handle so `cmd-o` stays reachable.
    #[gpui::test]
    fn no_file_shows_welcome_and_focuses_workspace(cx: &mut TestAppContext) {
        setup(cx);
        let window = open_workspace(cx, Vec::new(), None);

        assert_eq!(file_count(window, cx), 0);
        let (welcome_present, workspace_handle) =
            window.read_with(cx, |ws, cx| (ws.welcome.is_some(), ws.focus_handle(cx))).unwrap();
        assert!(welcome_present);
        let focused = window.update(cx, |_ws, window, cx| window.focused(cx)).unwrap();
        assert_eq!(focused, Some(workspace_handle));
    }

    /// `cmd-o` accumulates tabs; each open becomes the active tab, so
    /// the status bar / vim toggle follow the newest file.
    #[gpui::test]
    fn opening_files_adds_tabs_and_switches_active(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);

        window.update(cx, |ws, window, cx| ws.open_path(f1.clone(), window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 1);
        assert_eq!(active_path(window, cx), Some(f1.clone()));
        assert!(window.read_with(cx, |ws, _| ws.welcome.is_none()).unwrap());

        window.update(cx, |ws, window, cx| ws.open_path(f2.clone(), window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 2);
        assert_eq!(active_path(window, cx), Some(f2));
    }

    /// Closing tabs shrinks the set; closing the last file tab brings
    /// the welcome placeholder back and clears the active file.
    #[gpui::test]
    fn closing_last_tab_returns_to_welcome(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);

        window.update(cx, |ws, window, cx| ws.open_path(f1, window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 1);

        window
            .update(cx, |ws, window, cx| {
                let active = ws.active_file.clone().unwrap();
                let view: Arc<dyn PanelView> = Arc::new(active);
                ws.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
            })
            .unwrap();
        cx.run_until_parked();

        assert_eq!(file_count(window, cx), 0);
        assert_eq!(active_path(window, cx), None);
        assert!(window.read_with(cx, |ws, _| ws.welcome.is_some()).unwrap());
        // The welcome tab must be live in the rendered tree, not attached to
        // an orphaned TabPanel: it has to appear in the dock's own dump.
        let welcome_live =
            window.read_with(cx, |ws, cx| count_welcome_panels(&ws.dock.read(cx).dump(cx).center) == 1).unwrap();
        assert!(welcome_live, "welcome tab must be attached to the live center");

        // Reopening after closing to zero must show the file again -- proves
        // the center was rebuilt rather than left pointing at a dead panel.
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);
        window_open(window, &f2, cx);
        assert_eq!(file_count(window, cx), 1);
        assert_eq!(active_path(window, cx), Some(f2));
    }

    fn count_welcome_panels(state: &PanelState) -> usize {
        let here = usize::from(state.panel_name == crate::panels::WELCOME_PANEL_NAME);
        here + state.children.iter().map(count_welcome_panels).sum::<usize>()
    }

    fn first_live_tab_panel(item: &DockItem) -> Option<Entity<gpui_component::dock::TabPanel>> {
        match item {
            DockItem::Tabs { view, .. } => Some(view.clone()),
            DockItem::Split { items, .. } => items.iter().find_map(first_live_tab_panel),
            _ => None,
        }
    }

    /// Regression for the drag-to-split stale-cache brick: split a pane
    /// off, empty the original pane so its `TabPanel` detaches from the
    /// live tree while a file survives in the split, then open another
    /// file. The center `DockItem` cache still references the detached
    /// pane; without the live-tree resync the add routes into that
    /// orphan and vanishes. Assert it lands in a rendered tab panel
    /// (visible in `dump()`).
    #[gpui::test]
    fn add_after_split_pane_collapse_lands_in_a_live_tab_panel(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);
        let f3 = temp_file(&dir, "c.bin", &[3u8; 16]);
        let f4 = temp_file(&dir, "d.bin", &[4u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);
        window_open(window, &f2, cx);

        let original = window
            .read_with(cx, |ws, cx| first_live_tab_panel(ws.dock.read(cx).items()))
            .unwrap()
            .expect("a center tab panel");

        // Split a new pane (f3) beside the original -- the UI drag-split path.
        window
            .update(cx, |_ws, window, cx| {
                let bytes = std::fs::read(&f3).unwrap();
                let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
                let f3_panel = cx.new(|cx| FilePanel::new(source, Some(f3.clone()), window, cx));
                let view: Arc<dyn PanelView> = Arc::new(f3_panel);
                original
                    .update(cx, |tab, cx| tab.add_panel_at(view, gpui_component::Placement::Right, None, window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 3);

        // Empty the original pane so its TabPanel detaches from the live tree.
        window
            .update(cx, |_ws, window, cx| {
                original.update(cx, |tab, cx| {
                    while let Some(panel) = tab.active_panel(cx) {
                        tab.remove_panel(panel, window, cx);
                    }
                });
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 1);

        window_open(window, &f4, cx);
        assert_eq!(file_count(window, cx), 2);
        let paths = window.read_with(cx, |ws, cx| collect_file_paths(&ws.dock.read(cx).dump(cx).center)).unwrap();
        assert!(paths.contains(&f4), "added file must live in a rendered tab panel");
        assert!(paths.contains(&f3));
    }

    fn file_panel_by_path(item: &DockItem, cx: &App, target: &Path) -> Option<Entity<FilePanel>> {
        match item {
            DockItem::Split { items, .. } => items.iter().find_map(|item| file_panel_by_path(item, cx, target)),
            DockItem::Tabs { items, .. } => items.iter().find_map(|panel| {
                let file = panel.view().downcast::<FilePanel>().ok()?;
                (file.read(cx).path() == Some(target)).then_some(file)
            }),
            DockItem::Panel { view, .. } => {
                let file = view.view().downcast::<FilePanel>().ok()?;
                (file.read(cx).path() == Some(target)).then_some(file)
            }
            DockItem::Tiles { .. } => None,
        }
    }

    /// A center-cache resync (triggered by an unrelated pane collapsing)
    /// must reuse the surviving file's live panel entity, not rebuild it
    /// from disk -- otherwise in-memory dirty edits are silently lost.
    #[gpui::test]
    fn resync_preserves_live_panel_editor_state(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let fx = temp_file(&dir, "x.bin", &[0u8; 16]);
        let fa = temp_file(&dir, "a.bin", &[0u8; 32]);
        let fc = temp_file(&dir, "c.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &fx, cx);

        // Split file A into a new pane and register it as an open file --
        // mirrors dragging an already-open file into a split.
        let original = window
            .read_with(cx, |ws, cx| first_live_tab_panel(ws.dock.read(cx).items()))
            .unwrap()
            .expect("a center tab panel");
        let fa_entity = window
            .update(cx, |ws, window, cx| {
                let bytes = std::fs::read(&fa).unwrap();
                let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
                let panel = cx.new(|cx| FilePanel::new(source, Some(fa.clone()), window, cx));
                ws.open_files.push(panel.clone());
                let view: Arc<dyn PanelView> = Arc::new(panel.clone());
                original
                    .update(cx, |tab, cx| tab.add_panel_at(view, gpui_component::Placement::Right, None, window, cx));
                panel
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 2);

        // Focus file A's grid, then dirty it.
        window
            .update(cx, |_ws, window, cx| {
                let handle = fa_entity.read(cx).pane().read(cx).focus_handle(cx);
                window.focus(&handle);
            })
            .unwrap();
        cx.run_until_parked();
        cx.simulate_keystrokes(window.into(), "down");
        cx.simulate_keystrokes(window.into(), "a");
        assert!(
            window.read_with(cx, |_ws, cx| fa_entity.read(cx).pane().read(cx).editor().is_dirty()).unwrap(),
            "file A should be dirty after typing",
        );

        // Collapse the original pane so its cached tab panel goes empty and
        // the reconcile resync fires while file A survives in the split.
        window
            .update(cx, |_ws, window, cx| {
                original.update(cx, |tab, cx| {
                    while let Some(panel) = tab.active_panel(cx) {
                        tab.remove_panel(panel, window, cx);
                    }
                });
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 1);

        // The surviving file must keep its dirty edits AND be the same entity.
        assert!(
            window.read_with(cx, |_ws, cx| fa_entity.read(cx).pane().read(cx).editor().is_dirty()).unwrap(),
            "resync must preserve the surviving file's dirty edits",
        );
        let live_a = window
            .read_with(cx, |ws, cx| file_panel_by_path(ws.dock.read(cx).items(), cx, &fa))
            .unwrap()
            .expect("file A still live");
        assert_eq!(live_a.entity_id(), fa_entity.entity_id(), "resync must reuse the same panel entity");

        // A later open still lands in a rendered tab panel.
        window_open(window, &fc, cx);
        assert_eq!(file_count(window, cx), 2);
        let paths = window.read_with(cx, |ws, cx| collect_file_paths(&ws.dock.read(cx).dump(cx).center)).unwrap();
        assert!(paths.contains(&fc) && paths.contains(&fa));
    }

    /// Opening an already-open file focuses its existing tab instead of
    /// adding a duplicate, and reuses the SAME live entity so its editor
    /// state (dirty edits) survives -- both when it is already active and
    /// when it is a background tab. Deduping keeps the resync reuse
    /// registry one-entry-per-path, closing the duplicate-path data-loss
    /// class.
    #[gpui::test]
    fn opening_an_already_open_file_focuses_its_tab(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let fa = temp_file(&dir, "a.bin", &[0u8; 32]);
        let fb = temp_file(&dir, "b.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &fa, cx);
        assert_eq!(file_count(window, cx), 1);

        // Dirty file A (the active tab).
        let fa_entity = window.read_with(cx, |ws, _| ws.active_file.clone()).unwrap().expect("A active");
        window
            .update(cx, |_ws, window, cx| {
                let handle = fa_entity.read(cx).pane().read(cx).focus_handle(cx);
                window.focus(&handle);
            })
            .unwrap();
        cx.run_until_parked();
        cx.simulate_keystrokes(window.into(), "down");
        cx.simulate_keystrokes(window.into(), "a");
        assert!(window.read_with(cx, |_ws, cx| fa_entity.read(cx).pane().read(cx).editor().is_dirty()).unwrap());

        // Reopening the active file: no duplicate, same entity, still dirty.
        window_open(window, &fa, cx);
        assert_eq!(file_count(window, cx), 1, "reopening the active file must not duplicate it");
        assert_eq!(active_path(window, cx), Some(fa.clone()));
        assert_eq!(
            window.read_with(cx, |ws, _| ws.active_file.as_ref().unwrap().entity_id()).unwrap(),
            fa_entity.entity_id(),
            "reopen must reuse the same entity",
        );

        // Open B (now active), then reopen A from the background: it must
        // activate the existing A tab (not add a second) and keep A's edits.
        window_open(window, &fb, cx);
        assert_eq!(active_path(window, cx), Some(fb));
        window_open(window, &fa, cx);
        assert_eq!(file_count(window, cx), 2, "background reopen must not duplicate A");
        assert_eq!(active_path(window, cx), Some(fa.clone()), "background reopen must focus A");
        assert_eq!(
            window.read_with(cx, |ws, _| ws.active_file.as_ref().unwrap().entity_id()).unwrap(),
            fa_entity.entity_id(),
            "background reopen must reuse A's entity",
        );
        assert!(
            window.read_with(cx, |_ws, cx| fa_entity.read(cx).pane().read(cx).editor().is_dirty()).unwrap(),
            "background reopen must preserve A's dirty edits",
        );

        // Registry holds exactly one entry for A's path.
        let registered = window
            .read_with(cx, |ws, cx| {
                ws.open_files.iter().filter(|file| file.read(cx).path() == Some(fa.as_path())).count()
            })
            .unwrap();
        assert_eq!(registered, 1, "one registry entry per open path");
    }

    /// A restored tab whose file has since disappeared is pruned, the
    /// rest are kept, restore does not crash, and the drop surfaces a
    /// warning toast. Uses [`open_workspace_with_root`] for the restoring
    /// workspace so the deferred `push_notification` has a real `Root`.
    #[gpui::test]
    fn restore_prunes_missing_file_and_keeps_the_rest(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("layout.json");
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);

        let first = open_workspace(cx, Vec::new(), Some(layout.clone()));
        window_open(first, &f1, cx);
        window_open(first, &f2, cx);
        first.read_with(cx, |ws, cx| ws.save_now(cx).unwrap().unwrap()).unwrap();

        std::fs::remove_file(&f1).unwrap();

        let (window, second) = open_workspace_with_root(cx, Vec::new(), Some(layout));
        let count = second.read_with(cx, |ws, cx| count_file_panels(&ws.dock.read(cx).dump(cx).center));
        assert_eq!(count, 1);
        let paths = second.read_with(cx, |ws, cx| collect_file_paths(&ws.dock.read(cx).dump(cx).center));
        assert!(!paths.contains(&f1), "missing file must be pruned");
        assert!(paths.contains(&f2), "readable file must survive");

        let toasts = cx.update_window(window.into(), |_, window, cx| window.notifications(cx).len()).unwrap();
        assert_eq!(toasts, 1, "the pruned tab must surface a warning toast");
    }

    /// Nodes in a `dump()` that `DockArea::load` would have rebuilt as an
    /// uncloseable `InvalidPanel`: a `Panel(_)` leaf whose name this build
    /// does not register. `InvalidPanel::dump` echoes the original state
    /// back, so a live placeholder still shows up here under its stale name.
    fn count_invalid_leaves(state: &PanelState) -> usize {
        let here = usize::from(matches!(state.info, PanelInfo::Panel(_)) && !is_known_panel_name(&state.panel_name));
        here + state.children.iter().map(count_invalid_leaves).sum::<usize>()
    }

    /// The user's stale-layout bug end to end: an older build saved a
    /// layout where an empty container serialized as a LEAF named
    /// "TabPanel" (gpui-component 0.5.1's empty-container quirk) sits
    /// beside a still-readable file. Restoring it must drop the
    /// placeholder (never rebuild it as an uncloseable `InvalidPanel`),
    /// keep the file tab, and surface a warning toast.
    #[gpui::test]
    fn restore_of_stale_container_leaf_drops_placeholder_and_toasts(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("layout.json");
        let file = temp_file(&dir, "kept.bin", &[7u8; 16]);
        let fixture = format!(
            r#"{{
              "version": 1,
              "center": {{
                "panel_name": "StackPanel",
                "children": [
                  {{ "panel_name": "TabPanel", "children": [], "info": {{ "panel": null }} }},
                  {{ "panel_name": "FilePanel", "children": [], "info": {{ "panel": {{ "path": {path:?} }} }} }}
                ],
                "info": {{ "stack": {{ "sizes": [], "axis": 0 }} }}
              }},
              "left_dock": null,
              "right_dock": null,
              "bottom_dock": null
            }}"#,
            path = file.to_string_lossy(),
        );
        std::fs::write(&layout, fixture).unwrap();

        let (window, ws) = open_workspace_with_root(cx, Vec::new(), Some(layout));

        let invalid = ws.read_with(cx, |ws, cx| count_invalid_leaves(&ws.dock.read(cx).dump(cx).center));
        assert_eq!(invalid, 0, "the container-as-leaf placeholder must never rebuild as an InvalidPanel");
        let files = ws.read_with(cx, |ws, cx| count_file_panels(&ws.dock.read(cx).dump(cx).center));
        assert_eq!(files, 1, "the readable file tab survives");

        let toasts = cx.update_window(window.into(), |_, window, cx| window.notifications(cx).len()).unwrap();
        assert_eq!(toasts, 1, "the dropped placeholder surfaces a warning toast");
    }

    /// Belt-and-braces for fix (d): even if an unrecognized placeholder
    /// ever reaches the live dock, `cmd-w` on it removes it via the
    /// fallback arm. Forces the placeholder in by loading a layout with a
    /// foreign leaf directly (bypassing the restore-path prune) beside a
    /// readable file, with the placeholder front-most so `active_file` is
    /// `None` and the fallback is the arm that fires.
    #[gpui::test]
    fn cmd_w_removes_an_unrecognized_active_tab(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let file = temp_file(&dir, "real.bin", &[3u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);

        window
            .update(cx, |ws, window, cx| {
                let state = gpui_component::dock::DockAreaState {
                    version: Some(persist::LAYOUT_VERSION),
                    center: PanelState {
                        panel_name: "TabPanel".to_string(),
                        children: vec![
                            PanelState {
                                panel_name: "GhostPanel".to_string(),
                                children: Vec::new(),
                                info: PanelInfo::panel(serde_json::json!({})),
                            },
                            PanelState {
                                panel_name: FILE_PANEL_NAME.to_string(),
                                children: Vec::new(),
                                info: PanelInfo::panel(serde_json::json!({ "path": file.to_string_lossy() })),
                            },
                        ],
                        // Placeholder front-most (index 0): `active_file`
                        // resolves to the active tab, which is not a file, so
                        // it stays `None` and the fallback arm is what fires.
                        info: PanelInfo::Tabs { active_index: 0 },
                    },
                    left_dock: None,
                    right_dock: None,
                    bottom_dock: None,
                };
                ws.dock.update(cx, |dock, cx| dock.load(state, window, cx)).unwrap();
            })
            .unwrap();
        cx.run_until_parked();

        assert_eq!(invalid_leaf_count(window, cx), 1, "the foreign placeholder is present before close");
        assert_eq!(file_count(window, cx), 1);

        window.update(cx, |ws, window, cx| ws.close_active_tab(window, cx)).unwrap();
        cx.run_until_parked();

        assert_eq!(invalid_leaf_count(window, cx), 0, "cmd-w removed the unrecognized placeholder");
        assert_eq!(file_count(window, cx), 1, "the real file tab is untouched");
    }

    fn invalid_leaf_count(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> usize {
        window.read_with(cx, |ws, cx| count_invalid_leaves(&ws.dock.read(cx).dump(cx).center)).unwrap()
    }

    /// A burst of layout changes coalesces into a single debounced write:
    /// nothing is written until the debounce window elapses, then the
    /// final state lands.
    #[gpui::test]
    fn layout_save_is_debounced_and_coalesced(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("layout.json");
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);
        let f3 = temp_file(&dir, "c.bin", &[3u8; 16]);

        let window = open_workspace(cx, Vec::new(), Some(layout.clone()));
        window_open(window, &f1, cx);
        window_open(window, &f2, cx);
        window_open(window, &f3, cx);
        assert!(!layout.exists(), "a burst of changes must not write eagerly");

        cx.executor().advance_clock(SAVE_DEBOUNCE + Duration::from_millis(50));
        cx.run_until_parked();

        assert!(layout.exists(), "debounced save must fire once the window elapses");
        let state: gpui_component::dock::DockAreaState =
            serde_json::from_slice(&std::fs::read(&layout).unwrap()).unwrap();
        let paths = collect_file_paths(&state.center);
        assert!(
            paths.contains(&f1) && paths.contains(&f2) && paths.contains(&f3),
            "coalesced write must hold the final state"
        );
    }

    /// Layout persistence round-trips: a saved session's tab count and
    /// file paths are restored in a fresh workspace pointed at the same
    /// (temp) layout file.
    #[gpui::test]
    fn layout_round_trips_tab_count_and_paths(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("layout.json");
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);

        let first = open_workspace(cx, Vec::new(), Some(layout.clone()));
        window_open(first, &f1, cx);
        window_open(first, &f2, cx);
        first.read_with(cx, |ws, cx| ws.save_now(cx).unwrap().unwrap()).unwrap();

        let second = open_workspace(cx, Vec::new(), Some(layout.clone()));
        assert_eq!(file_count(second, cx), 2);
        let restored: Vec<PathBuf> =
            second.read_with(cx, |ws, cx| collect_file_paths(&ws.dock.read(cx).dump(cx).center)).unwrap();
        assert!(restored.contains(&f1));
        assert!(restored.contains(&f2));
    }

    /// `cmd-i` opens and closes the inspector dock, exercising the
    /// action and keybinding end to end.
    #[gpui::test]
    fn cmd_i_toggles_the_inspector_dock(cx: &mut TestAppContext) {
        setup(cx);
        let window = open_workspace(cx, Vec::new(), None);

        let is_open = |cx: &mut TestAppContext| {
            window.read_with(cx, |ws, cx| ws.dock.read(cx).is_dock_open(DockPlacement::Right, cx)).unwrap()
        };
        assert!(!is_open(cx), "inspector starts closed");

        cx.simulate_keystrokes(window.into(), "cmd-i");
        assert!(is_open(cx), "cmd-i opens the inspector dock");

        cx.simulate_keystrokes(window.into(), "cmd-i");
        assert!(!is_open(cx), "cmd-i closes it again");
    }

    /// The inspector dock's open/closed state (and the panel itself)
    /// survive a layout save + reload, same as any other dock content
    /// -- this is what lets the endian/radix `InspectorPanel::dump`
    /// persists actually come back on relaunch.
    #[gpui::test]
    fn inspector_dock_open_state_round_trips(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("layout.json");

        let first = open_workspace(cx, Vec::new(), Some(layout.clone()));
        cx.simulate_keystrokes(first.into(), "cmd-i");
        assert!(first.read_with(cx, |ws, cx| ws.dock.read(cx).is_dock_open(DockPlacement::Right, cx)).unwrap());
        first.read_with(cx, |ws, cx| ws.save_now(cx).unwrap().unwrap()).unwrap();

        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&layout).unwrap()).unwrap();
        assert_eq!(value["right_dock"]["open"], serde_json::json!(true));
        assert_eq!(value["right_dock"]["panel"]["children"][0]["panel_name"], serde_json::json!("InspectorPanel"));

        let second = open_workspace(cx, Vec::new(), Some(layout));
        assert!(
            second.read_with(cx, |ws, cx| ws.dock.read(cx).is_dock_open(DockPlacement::Right, cx)).unwrap(),
            "inspector open state must restore",
        );
    }

    /// Switching the active tab through the real workspace path (opening
    /// a second file, which the dock makes active) updates the
    /// inspector's decoded caret window end to end -- the panel's own
    /// unit tests only prove the `ActiveHexPane` global wiring works when
    /// poked directly; this proves `Workspace::set_active_file` actually
    /// publishes the real active pane on a real tab switch.
    #[gpui::test]
    fn switching_tabs_updates_the_inspectors_decoded_window(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0xAAu8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[0xBBu8; 16]);
        let window = open_workspace(cx, Vec::new(), None);

        let inspector = window
            .read_with(cx, |ws, _| ws.inspector_for_test.clone())
            .unwrap()
            .expect("inspector stashed on fresh construction");

        window_open(window, &f1, cx);
        seed_active_caret(window, cx);
        let (_, bytes_a) = inspector.read_with(cx, |insp, cx| insp.caret_window(cx)).expect("caret window for A");
        assert_eq!(bytes_a, vec![0xAAu8; 16], "inspector must decode file A's bytes while A is active");

        window_open(window, &f2, cx);
        seed_active_caret(window, cx);
        let (_, bytes_b) = inspector.read_with(cx, |insp, cx| insp.caret_window(cx)).expect("caret window for B");
        assert_eq!(bytes_b, vec![0xBBu8; 16], "switching the active tab must update the inspector's decoded window");
    }

    /// Seed a caret at offset 0 on the workspace's currently active
    /// file's pane, so the inspector has a window to decode.
    fn seed_active_caret(window: WindowHandle<Workspace>, cx: &mut TestAppContext) {
        window
            .update(cx, |ws, _window, cx| {
                let pane = ws.active_file.as_ref().unwrap().read(cx).pane().clone();
                pane.update(cx, |pane, _| pane.editor_mut().set_selection(Some(Selection::caret(ByteOffset::new(0)))));
            })
            .unwrap();
        cx.run_until_parked();
    }

    fn window_open(window: WindowHandle<Workspace>, path: &Path, cx: &mut TestAppContext) {
        window.update(cx, |ws, window, cx| ws.open_path(path.to_path_buf(), window, cx)).unwrap();
        cx.run_until_parked();
    }

    /// `cmd-k` then a target letter jumps focus to a background split
    /// pane's `HexPane` and re-points the status bar / `ActiveHexPane`
    /// global at that pane's file -- proving `sync_active_file_after_pick`
    /// overrides `reconcile`'s "first tab container" default, not just
    /// that focus moved.
    #[gpui::test]
    fn cmd_k_picks_a_split_pane_and_syncs_the_active_file(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);

        let original = window
            .read_with(cx, |ws, cx| first_live_tab_panel(ws.dock.read(cx).items()))
            .unwrap()
            .expect("a center tab panel");
        let f2_pane = window
            .update(cx, |ws, window, cx| {
                let bytes = std::fs::read(&f2).unwrap();
                let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
                let panel = cx.new(|cx| FilePanel::new(source, Some(f2.clone()), window, cx));
                // Register like a real drag-split would find it already
                // registered (the panel being dragged always already went
                // through `add_file_panel`/`register_open_file`); without
                // this the picker's forced `rebuild_center_cache` (see
                // `on_pick_pane`'s doc) would not find a live entity to
                // reuse and would silently rebuild a fresh FilePanel.
                ws.open_files.push(panel.clone());
                let pane = panel.read(cx).pane().clone();
                let view: Arc<dyn PanelView> = Arc::new(panel);
                original
                    .update(cx, |tab, cx| tab.add_panel_at(view, gpui_component::Placement::Right, None, window, cx));
                pane
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 2);
        // Sanity: the split-add alone must not have moved the status bar
        // off A (the first/left leaf) -- otherwise this test would not
        // be exercising the picker's override at all.
        assert_eq!(active_path(window, cx), Some(f1.clone()));

        window
            .update(cx, |ws, window, cx| {
                window.focus(&ws.active_file.as_ref().unwrap().read(cx).pane().read(cx).focus_handle(cx))
            })
            .unwrap();
        cx.simulate_keystrokes(window.into(), "cmd-k");
        // Read the letter the picker actually assigned to B's leaf back
        // out through its public `targets()` accessor (matched by label,
        // i.e. B's file name -- `FilePanel::tab_name`), rather than
        // assuming tree-walk order -- this test only cares that *some*
        // letter reaches B, not which one.
        let b_name = f2.file_name().unwrap().to_string_lossy().into_owned();
        let letter = window
            .read_with(cx, |ws, cx| {
                ws.pane_picker
                    .read(cx)
                    .targets()
                    .iter()
                    .find(|(_, target)| target.label().as_ref() == b_name)
                    .map(|(letter, _)| *letter)
            })
            .unwrap()
            .expect("B's leaf must be a pickable target");
        cx.simulate_keystrokes(window.into(), &letter.to_string());

        let f2_handle = f2_pane.read_with(cx, |pane, cx| pane.focus_handle(cx));
        assert_eq!(
            window.update(cx, |_ws, window, cx| window.focused(cx)).unwrap(),
            Some(f2_handle),
            "cmd-k moved focus to B's pane"
        );
        assert_eq!(active_path(window, cx), Some(f2), "picking B's leaf must re-point the status bar at B");
    }

    /// `cmd-k` then Escape cancels without touching the active file and
    /// restores whatever pane had focus -- no leak through the real
    /// production wiring (not just the picker crate's own unit tests).
    #[gpui::test]
    fn cmd_k_then_escape_restores_focus_and_active_file(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "t.bin", &[0u8; 16]);
        let window = open_workspace(cx, vec![f1.clone()], None);
        let pane_handle = window
            .read_with(cx, |ws, cx| ws.active_file.as_ref().unwrap().read(cx).pane().read(cx).focus_handle(cx))
            .unwrap();

        cx.simulate_keystrokes(window.into(), "cmd-k");
        cx.simulate_keystrokes(window.into(), "escape");

        assert_eq!(
            window.update(cx, |_ws, window, cx| window.focused(cx)).unwrap(),
            Some(pane_handle),
            "escape restores the pane's focus"
        );
        assert_eq!(active_path(window, cx), Some(f1), "cancelling must not change the active file");
    }

    /// The inspector -- a right-dock panel `gpui_dock_picker` cannot
    /// discover on its own (see `on_pick_pane`'s doc) -- is composed onto
    /// the picker's target list by hand and is pickable like any center
    /// leaf. Picking it while its dock is collapsed (the default) opens
    /// the dock and focuses the panel; the active file is untouched (the
    /// inspector isn't a `FilePanel`, so `sync_active_file_after_pick`'s
    /// focus-based lookup finds nothing to sync).
    #[gpui::test]
    fn cmd_k_picks_the_inspector_and_opens_its_collapsed_dock(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "t.bin", &[0u8; 16]);
        let window = open_workspace(cx, vec![f1.clone()], None);
        assert!(
            !window.read_with(cx, |ws, cx| ws.dock.read(cx).is_dock_open(DockPlacement::Right, cx)).unwrap(),
            "inspector dock starts collapsed"
        );

        cx.simulate_keystrokes(window.into(), "cmd-k");
        let inspector_letter = window
            .read_with(cx, |ws, cx| {
                ws.pane_picker
                    .read(cx)
                    .targets()
                    .iter()
                    .find(|(_, target)| target.label().as_ref() == "Inspector")
                    .map(|(letter, _)| *letter)
            })
            .unwrap()
            .expect("the inspector must be a pickable target");
        cx.simulate_keystrokes(window.into(), &inspector_letter.to_string());

        assert!(
            window.read_with(cx, |ws, cx| ws.dock.read(cx).is_dock_open(DockPlacement::Right, cx)).unwrap(),
            "picking the inspector opens its collapsed dock"
        );
        let inspector_handle = window
            .read_with(cx, |_ws, cx| {
                crate::panels::inspector::active_inspector_panel(cx).map(|insp| insp.read(cx).focus_handle(cx))
            })
            .unwrap()
            .expect("inspector panel must be live");
        assert_eq!(
            window.update(cx, |_ws, window, cx| window.focused(cx)).unwrap(),
            Some(inspector_handle),
            "picking the inspector focuses it"
        );
        assert_eq!(active_path(window, cx), Some(f1), "the active file is untouched");
    }

    /// Regression: the picker only intercepts letters/Escape via raw
    /// key-down, not gpui's action-dispatch path, so other shortcuts
    /// (`cmd-w` here) still fire while a pick session is open -- mirrors
    /// `egui_dock_picker`'s own choice not to consume unrelated keys. If
    /// that shortcut triggers a center-cache resync (closing a tab down to
    /// an empty, orphaned leaf does), the resync's `focus_pending` must
    /// NOT steal keyboard focus back from the still-open picker overlay,
    /// or the picker would keep rendering while going deaf to further
    /// letter/Escape presses.
    #[gpui::test]
    fn cmd_w_while_picker_is_open_does_not_steal_its_focus(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);

        let original = window
            .read_with(cx, |ws, cx| first_live_tab_panel(ws.dock.read(cx).items()))
            .unwrap()
            .expect("a center tab panel");
        window
            .update(cx, |ws, window, cx| {
                let bytes = std::fs::read(&f2).unwrap();
                let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
                let panel = cx.new(|cx| FilePanel::new(source, Some(f2.clone()), window, cx));
                ws.open_files.push(panel.clone());
                let view: Arc<dyn PanelView> = Arc::new(panel);
                original
                    .update(cx, |tab, cx| tab.add_panel_at(view, gpui_component::Placement::Right, None, window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 2);
        assert_eq!(active_path(window, cx), Some(f1.clone()), "A (first/left leaf) is still active");

        cx.simulate_keystrokes(window.into(), "cmd-k");
        let picker_handle = window.read_with(cx, |ws, cx| ws.pane_picker.read(cx).focus_handle(cx)).unwrap();
        assert_eq!(
            window.update(cx, |_ws, window, cx| window.focused(cx)).unwrap(),
            Some(picker_handle.clone()),
            "activating the picker takes keyboard focus"
        );

        // Close A's tab (still `active_file`) while the picker overlay is
        // up. This collapses A's leaf, orphaning it in the center cache --
        // the exact hazard that used to trigger a focus-stealing rebuild.
        cx.simulate_keystrokes(window.into(), "cmd-w");
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 1, "A's tab actually closed");

        assert!(
            window.read_with(cx, |ws, cx| ws.pane_picker.read(cx).is_active()).unwrap(),
            "the picker session survives the collapse"
        );
        assert_eq!(
            window.update(cx, |_ws, window, cx| window.focused(cx)).unwrap(),
            Some(picker_handle),
            "the collapse's cache resync must not steal focus back from the picker"
        );

        // And it must still be responsive, not just visually open.
        cx.simulate_keystrokes(window.into(), "escape");
        assert!(
            !window.read_with(cx, |ws, cx| ws.pane_picker.read(cx).is_active()).unwrap(),
            "escape still cancels it"
        );
    }

    /// Closing a tab prunes its `FilePanel` from the reuse registry so
    /// the closed file's buffer is released promptly, not held until a
    /// rare cache rebuild. The still-open file's entry survives.
    #[gpui::test]
    fn closing_a_tab_prunes_its_entity_from_the_registry(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);
        window_open(window, &f2, cx);

        let registered = |cx: &mut TestAppContext| {
            window
                .read_with(cx, |ws, cx| {
                    ws.open_files
                        .iter()
                        .filter_map(|file| file.read(cx).path().map(Path::to_path_buf))
                        .collect::<Vec<_>>()
                })
                .unwrap()
        };
        assert_eq!(registered(cx).len(), 2, "both open files are registered");

        // Close the active tab (f2); f1 remains open.
        cx.simulate_keystrokes(window.into(), "cmd-w");
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 1);

        let paths = registered(cx);
        assert!(!paths.contains(&f2), "the closed file's entity must be pruned from the registry");
        assert!(paths.contains(&f1), "the still-open file's entity must survive");
    }

    /// `cmd-w` while the command palette is open must not steal keyboard
    /// focus back from the palette: closing the active tab down to an
    /// orphaned leaf triggers a focus-stealing center-cache rebuild, and
    /// the render guard has to skip that refocus while the palette holds
    /// focus (mirrors the pane-picker guard). The palette stays open,
    /// keeps focus, and still answers Escape. Uses
    /// [`open_workspace_with_root`] because the palette overlay reads the
    /// window's `gpui_component::Root`.
    #[gpui::test]
    fn cmd_w_while_palette_open_keeps_its_focus(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);
        let (window, ws) = open_workspace_with_root(cx, Vec::new(), None);

        cx.update_window(window.into(), |_, window, cx| ws.update(cx, |ws, cx| ws.open_path(f1.clone(), window, cx)))
            .unwrap();
        cx.run_until_parked();

        // Split f2 into a new pane beside the original so closing f1 (the
        // left/active leaf) collapses its leaf into a cache orphan -- the
        // hazard that fires the focus-stealing rebuild.
        let original =
            ws.read_with(cx, |ws, cx| first_live_tab_panel(ws.dock.read(cx).items())).expect("a center tab panel");
        cx.update_window(window.into(), |_, window, cx| {
            ws.update(cx, |ws, cx| {
                let bytes = std::fs::read(&f2).unwrap();
                let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
                let panel = cx.new(|cx| FilePanel::new(source, Some(f2.clone()), window, cx));
                ws.open_files.push(panel.clone());
                let view: Arc<dyn PanelView> = Arc::new(panel);
                original
                    .update(cx, |tab, cx| tab.add_panel_at(view, gpui_component::Placement::Right, None, window, cx));
            });
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(ws.read_with(cx, |ws, cx| ws.active_path(cx)), Some(f1.clone()), "A (left leaf) is active");

        // Focus the active grid, then open the palette (it takes focus).
        cx.update_window(window.into(), |_, window, cx| {
            ws.update(cx, |ws, cx| {
                let handle = ws.active_file.as_ref().unwrap().read(cx).pane().read(cx).focus_handle(cx);
                window.focus(&handle);
            });
        })
        .unwrap();
        cx.simulate_keystrokes(window.into(), "cmd-shift-p");
        assert!(ws.read_with(cx, |ws, cx| ws.palette.read(cx).is_open()), "palette opened");
        let palette_focus = cx.update_window(window.into(), |_, window, cx| window.focused(cx)).unwrap();

        // Close A's tab underneath the palette (collapses A's leaf).
        cx.simulate_keystrokes(window.into(), "cmd-w");
        cx.run_until_parked();
        assert_eq!(
            ws.read_with(cx, |ws, cx| count_file_panels(&ws.dock.read(cx).dump(cx).center)),
            1,
            "A's tab actually closed"
        );

        assert!(ws.read_with(cx, |ws, cx| ws.palette.read(cx).is_open()), "the palette survives the collapse");
        assert_eq!(
            cx.update_window(window.into(), |_, window, cx| window.focused(cx)).unwrap(),
            palette_focus,
            "the collapse's cache rebuild must not steal focus back from the palette"
        );

        // Still responsive: Escape closes it.
        cx.simulate_keystrokes(window.into(), "escape");
        assert!(!ws.read_with(cx, |ws, cx| ws.palette.read(cx).is_open()), "escape still closes the palette");
    }

    /// An unparseable layout file must not crash startup; the workspace
    /// falls back to the empty (welcome) default.
    #[gpui::test]
    fn corrupt_layout_falls_back_to_welcome(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("layout.json");
        std::fs::write(&layout, b"{ not valid json").unwrap();

        let window = open_workspace(cx, Vec::new(), Some(layout));
        assert_eq!(file_count(window, cx), 0);
        assert!(window.read_with(cx, |ws, _| ws.welcome.is_some()).unwrap());
    }

    /// A layout saved under a different schema version is discarded
    /// wholesale rather than partially restored.
    #[gpui::test]
    fn version_mismatch_discards_layout(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("layout.json");
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);

        let first = open_workspace(cx, Vec::new(), Some(layout.clone()));
        window_open(first, &f1, cx);
        first.read_with(cx, |ws, cx| ws.save_now(cx).unwrap().unwrap()).unwrap();

        let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(&layout).unwrap()).unwrap();
        value["version"] = serde_json::json!(persist::LAYOUT_VERSION + 1);
        std::fs::write(&layout, serde_json::to_vec(&value).unwrap()).unwrap();

        let second = open_workspace(cx, Vec::new(), Some(layout));
        assert_eq!(file_count(second, cx), 0);
        assert!(second.read_with(cx, |ws, _| ws.welcome.is_some()).unwrap());
    }

    /// Typing reaches the active pane's editor: an arrow key moves the
    /// cursor and a following hex digit dirties the buffer, confirming
    /// keystrokes route to the active grid (not swallowed by the dock
    /// chrome). Guards the M1 focus-routing behavior through the dock.
    #[gpui::test]
    fn typing_reaches_the_active_pane(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0u8; 32]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);

        let editor_state = |cx: &mut TestAppContext| {
            window
                .read_with(cx, |ws, cx| {
                    let editor = ws.active_file.as_ref().unwrap().read(cx).pane().read(cx).editor();
                    (editor.selection().map(|s| s.cursor.get()), editor.is_dirty())
                })
                .unwrap()
        };

        assert_eq!(editor_state(cx), (None, false));

        // Arrow-down establishes and advances the cursor by one row.
        cx.simulate_keystrokes(window.into(), "down");
        assert_eq!(editor_state(cx).0, Some(16));

        // A hex digit at the cursor edits the buffer.
        cx.simulate_keystrokes(window.into(), "a");
        assert!(editor_state(cx).1, "hex digit typed into the active pane must edit its buffer");
    }

    /// `cmd-z` (Edit > Undo, and the shared `Undo` action) reverts the
    /// most recent typed edit and clears the dirty flag -- the same
    /// action a native Edit menu click would dispatch.
    #[gpui::test]
    fn cmd_z_undoes_a_typed_edit(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0u8; 32]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);

        let dirty = |cx: &mut TestAppContext| {
            window
                .read_with(cx, |ws, cx| ws.active_file.as_ref().unwrap().read(cx).pane().read(cx).editor().is_dirty())
                .unwrap()
        };

        cx.simulate_keystrokes(window.into(), "down");
        cx.simulate_keystrokes(window.into(), "a");
        assert!(dirty(cx), "typing a hex digit must dirty the buffer");

        cx.simulate_keystrokes(window.into(), "cmd-z");
        assert!(!dirty(cx), "cmd-z must undo the typed edit");
    }

    /// `cmd-shift-c` (Edit > Copy Hex) copies the active selection as
    /// space-separated uppercase hex -- the same format the palette's
    /// `CopySelection(Hex)` entry and the vim hex-pane yank use.
    #[gpui::test]
    fn cmd_shift_c_copies_selection_as_hex(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0xDE, 0xAD, 0xBE, 0xEF]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);

        window
            .update(cx, |ws, _window, cx| {
                let pane = ws.active_file.as_ref().unwrap().read(cx).pane().clone();
                pane.update(cx, |pane, _| {
                    let selection = Selection { anchor: ByteOffset::new(0), cursor: ByteOffset::new(1) };
                    pane.editor_mut().set_selection(Some(selection));
                });
            })
            .unwrap();

        cx.simulate_keystrokes(window.into(), "cmd-shift-c");
        let clip = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(clip.as_deref(), Some("DE AD"));
    }

    /// `cmd-c` (Edit > Copy Bytes) copies the active selection as lossy
    /// UTF-8 text, matching `CopyKind::BytesLossyUtf8` (the egui app's
    /// Copy Bytes semantics -- see `crates/hxy/src/files/copy.rs`).
    #[gpui::test]
    fn cmd_c_copies_selection_as_lossy_utf8(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", b"hxy!!!!");
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);

        window
            .update(cx, |ws, _window, cx| {
                let pane = ws.active_file.as_ref().unwrap().read(cx).pane().clone();
                pane.update(cx, |pane, _| {
                    let selection = Selection { anchor: ByteOffset::new(0), cursor: ByteOffset::new(2) };
                    pane.editor_mut().set_selection(Some(selection));
                });
            })
            .unwrap();

        cx.simulate_keystrokes(window.into(), "cmd-c");
        let clip = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(clip.as_deref(), Some("hxy"));
    }

    /// `cmd-w` (File > Close Tab) closes the active tab through the
    /// shared `CloseTab` action -- the same dispatch the palette's
    /// entry uses (`palette::apply::apply`).
    #[gpui::test]
    fn cmd_w_closes_the_active_tab(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);
        assert_eq!(file_count(window, cx), 1);

        cx.simulate_keystrokes(window.into(), "cmd-w");
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 0);
    }

    /// Closing a file's tab also closes any `StringsPanel` tab bound to
    /// it: otherwise the strings tab would keep the closed file's
    /// `HexPane` alive and pinned to content no longer open. A second
    /// file's own strings tab is untouched.
    #[gpui::test]
    fn cmd_w_closes_the_files_strings_tab_too(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[1u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);
        window_open(window, &f2, cx);
        window.update(cx, |ws, window, cx| ws.open_strings_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        window_open(window, &f1, cx);
        window.update(cx, |ws, window, cx| ws.open_strings_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        // Opening a strings tab makes it the front-most tab; bring f1's
        // own file tab back to the front so this exercises the
        // file-close cascade, not the front-most-tab-is-strings branch
        // (covered separately below).
        window_open(window, &f1, cx);
        assert_eq!(strings_tab_count(window, cx), 2, "both files have a strings tab");

        window.update(cx, |ws, window, cx| ws.close_active_tab(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 1);
        assert_eq!(strings_tab_count(window, cx), 1, "f2's strings tab survives");
    }

    /// `cmd-w` while a strings tab is the front-most center tab closes
    /// just that tab, not the (background) file tab it's bound to --
    /// `self.active_file` only ever names a `FilePanel`, so without the
    /// `active_strings_panel` check in `close_active_tab` this would
    /// either no-op or close the wrong tab.
    #[gpui::test]
    fn cmd_w_closes_the_front_most_strings_tab_not_the_file(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);
        window.update(cx, |ws, window, cx| ws.open_strings_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(strings_tab_count(window, cx), 1);

        window.update(cx, |ws, window, cx| ws.close_active_tab(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(strings_tab_count(window, cx), 0, "the strings tab closed");
        assert_eq!(file_count(window, cx), 1, "the file tab is untouched");
    }

    /// Closing a file's tab also closes any `EntropyPanel` tab bound to
    /// it, for the same reason as the strings cascade above (a left-open
    /// entropy tab would pin the closed file's `HexPane` alive).
    #[gpui::test]
    fn cmd_w_closes_the_files_entropy_tab_too(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[1u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);
        window_open(window, &f2, cx);
        window.update(cx, |ws, window, cx| ws.open_entropy_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        window_open(window, &f1, cx);
        window.update(cx, |ws, window, cx| ws.open_entropy_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        window_open(window, &f1, cx);
        assert_eq!(entropy_tab_count(window, cx), 2, "both files have an entropy tab");

        window.update(cx, |ws, window, cx| ws.close_active_tab(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 1);
        assert_eq!(entropy_tab_count(window, cx), 1, "f2's entropy tab survives");
    }

    /// `cmd-w` while an entropy tab is the front-most center tab closes
    /// just that tab, not the (background) file tab it's bound to.
    /// Mirrors `cmd_w_closes_the_front_most_strings_tab_not_the_file`.
    #[gpui::test]
    fn cmd_w_closes_the_front_most_entropy_tab_not_the_file(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);
        window.update(cx, |ws, window, cx| ws.open_entropy_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(entropy_tab_count(window, cx), 1);

        window.update(cx, |ws, window, cx| ws.close_active_tab(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(entropy_tab_count(window, cx), 0, "the entropy tab closed");
        assert_eq!(file_count(window, cx), 1, "the file tab is untouched");
    }

    /// Opening the entropy panel twice for the same file focuses the
    /// existing tab instead of duplicating it (mirrors the strings
    /// open-or-focus dedup).
    #[gpui::test]
    fn open_entropy_twice_focuses_the_existing_tab(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);
        window.update(cx, |ws, window, cx| ws.open_entropy_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(entropy_tab_count(window, cx), 1);

        window_open(window, &f1, cx);
        window.update(cx, |ws, window, cx| ws.open_entropy_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(entropy_tab_count(window, cx), 1, "no duplicate tab");
    }

    /// Closing a file's tab also closes any `ChecksumsPanel` tab bound
    /// to it. Mirrors `cmd_w_closes_the_files_entropy_tab_too`.
    #[gpui::test]
    fn cmd_w_closes_the_files_checksums_tab_too(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[1u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);
        window_open(window, &f2, cx);
        window.update(cx, |ws, window, cx| ws.open_checksums_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        window_open(window, &f1, cx);
        window.update(cx, |ws, window, cx| ws.open_checksums_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        window_open(window, &f1, cx);
        assert_eq!(checksums_tab_count(window, cx), 2, "both files have a checksums tab");

        window.update(cx, |ws, window, cx| ws.close_active_tab(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 1);
        assert_eq!(checksums_tab_count(window, cx), 1, "f2's checksums tab survives");
    }

    /// `cmd-w` while a checksums tab is the front-most center tab closes
    /// just that tab, not the (background) file tab it's bound to.
    /// Mirrors `cmd_w_closes_the_front_most_entropy_tab_not_the_file`.
    #[gpui::test]
    fn cmd_w_closes_the_front_most_checksums_tab_not_the_file(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);
        window.update(cx, |ws, window, cx| ws.open_checksums_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(checksums_tab_count(window, cx), 1);

        window.update(cx, |ws, window, cx| ws.close_active_tab(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(checksums_tab_count(window, cx), 0, "the checksums tab closed");
        assert_eq!(file_count(window, cx), 1, "the file tab is untouched");
    }

    /// Opening the checksums panel twice for the same file focuses the
    /// existing tab instead of duplicating it. Mirrors
    /// `open_entropy_twice_focuses_the_existing_tab`.
    #[gpui::test]
    fn open_checksums_twice_focuses_the_existing_tab(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);
        window.update(cx, |ws, window, cx| ws.open_checksums_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(checksums_tab_count(window, cx), 1);

        window_open(window, &f1, cx);
        window.update(cx, |ws, window, cx| ws.open_checksums_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(checksums_tab_count(window, cx), 1, "no duplicate tab");
    }

    /// Hovering a strings-panel row paints a hover band on the owning
    /// pane; closing the tab with the pointer still "resting" there
    /// must clear it (`StringsPanel::on_removed`) -- otherwise the hex
    /// view is left with a stale highlight nothing else ever clears.
    #[gpui::test]
    fn closing_a_strings_tab_clears_its_hover_band(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);
        window.update(cx, |ws, window, cx| ws.open_strings_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();

        let f1_panel = window.read_with(cx, |ws, _| ws.open_files.first().cloned()).unwrap().expect("f1 open");
        let strings_panel =
            window.read_with(cx, |ws, _| ws.strings_panels.first().cloned()).unwrap().expect("strings tab for f1");

        let range = ByteRange::new(ByteOffset::new(0), ByteOffset::new(4)).unwrap();
        window.update(cx, |_ws, _window, cx| strings_panel.update(cx, |p, cx| p.set_hover(Some(range), cx))).unwrap();
        assert_eq!(
            window.read_with(cx, |_ws, cx| f1_panel.read(cx).pane().read(cx).hover_span()).unwrap(),
            Some(range)
        );

        // The strings tab is front-most, so `close_active_tab` targets it.
        window.update(cx, |ws, window, cx| ws.close_active_tab(window, cx)).unwrap();
        cx.run_until_parked();

        assert_eq!(strings_tab_count(window, cx), 0, "sanity: the tab actually closed");
        assert_eq!(
            window.read_with(cx, |_ws, cx| f1_panel.read(cx).pane().read(cx).hover_span()).unwrap(),
            None,
            "hover band must not survive the tab that painted it",
        );
    }

    /// A row click on a background file's strings tab must bring that
    /// file's own tab to the front, not just move its (invisible)
    /// selection -- mirrors egui's `jump_to_strings_match`, which calls
    /// `focus_file_tab` before applying the selection. Split A|B, open
    /// strings for A, focus B (backgrounding A and its strings tab),
    /// then click a strings row: A's tab must become frontmost with the
    /// jumped-to range selected.
    #[gpui::test]
    fn strings_row_jump_focuses_the_owning_files_background_tab(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let fa = temp_file(&dir, "a.bin", b"\x00hello\x00world\x00");
        let fb = temp_file(&dir, "b.bin", &[0u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &fa, cx);
        window.update(cx, |ws, window, cx| ws.open_strings_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();

        let original = window
            .read_with(cx, |ws, cx| first_live_tab_panel(ws.dock.read(cx).items()))
            .unwrap()
            .expect("a center tab panel");
        window
            .update(cx, |ws, window, cx| {
                let bytes = std::fs::read(&fb).unwrap();
                let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
                let b_panel = cx.new(|cx| FilePanel::new(source, Some(fb.clone()), window, cx));
                ws.open_files.push(b_panel.clone());
                let view: Arc<dyn PanelView> = Arc::new(b_panel.clone());
                original
                    .update(cx, |tab, cx| tab.add_panel_at(view, gpui_component::Placement::Right, None, window, cx));
                // A plain split does not itself change `active_file`:
                // the newly split-off `TabPanel` has no node at all in
                // the cached `DockItem` tree yet (only a live resync or
                // the pane picker's explicit override discovers it --
                // see `resolve_leaf`'s doc), so `active_file_panel`'s
                // walk can only ever re-find `original` (whose active
                // tab is now the non-file strings(A)) and comes back
                // `None`. Set B active directly to background A/its
                // strings tab, matching what the real pane-picker flow
                // (`cmd-k`) would leave in place after picking B.
                ws.set_active_file(Some(b_panel), cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(active_path(window, cx), Some(fb.clone()), "sanity: B is active after the split");

        let strings_panel = window
            .read_with(cx, |ws, cx| {
                ws.strings_panels.iter().find(|p| p.read(cx).owning_path() == Some(fa.as_path())).cloned()
            })
            .unwrap()
            .expect("strings tab for A");

        window.update(cx, |_ws, _window, cx| strings_panel.update(cx, |p, cx| p.jump_to_row(0, cx))).unwrap();
        cx.run_until_parked();

        assert_eq!(active_path(window, cx), Some(fa), "A's tab is now frontmost");
        let selection = window
            .read_with(cx, |ws, cx| ws.active_file.as_ref().unwrap().read(cx).pane().read(cx).editor().selection())
            .unwrap();
        // "hello" sits at offset 1..6 in the fixture; end is exclusive.
        assert_eq!(selection, Some(Selection { anchor: ByteOffset::new(1), cursor: ByteOffset::new(5) }));
    }

    /// A global search result click must bring the MATCHED file's tab
    /// to the front, not just move its (invisible, backgrounded)
    /// selection -- the cross-file counterpart of
    /// `strings_row_jump_focuses_the_owning_files_background_tab`.
    /// `GlobalSearchPanel::jump_to_row` applies the selection directly
    /// (it holds a handle to the matched file's pane via `GlobalMatch`);
    /// `Workspace::on_global_search_jumped` does the tab focus.
    #[gpui::test]
    fn global_search_row_jump_focuses_the_matched_files_tab(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let fa = temp_file(&dir, "a.bin", b"\x00hello\x00");
        let fb = temp_file(&dir, "b.bin", b"\x00hello\x00");
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &fa, cx);
        window_open(window, &fb, cx);
        assert_eq!(active_path(window, cx), Some(fb.clone()), "sanity: B opened last, so it's active");

        window.update(cx, |ws, window, cx| ws.open_global_search(window, cx)).unwrap();
        cx.run_until_parked();

        let panel = window.read_with(cx, |ws, _| ws.global_search_panel.clone()).unwrap().expect("global search panel");
        panel.update(cx, |p, _| p.set_query_for_test(hxy_panels::search::SearchKind::Text, "hello".to_string()));
        window.update(cx, |_ws, _window, cx| panel.update(cx, |p, cx| p.run(cx))).unwrap();
        cx.run_until_parked();

        let matches = panel.read_with(cx, |p, _| p.state().matches.clone());
        assert_eq!(matches.len(), 2, "one match per file");

        // A's match is index 0 (open order) -- it is currently
        // backgrounded (B is active, and the search tab is frontmost).
        window.update(cx, |_ws, _window, cx| panel.update(cx, |p, cx| p.jump_to_row(0, cx))).unwrap();
        cx.run_until_parked();

        assert_eq!(active_path(window, cx), Some(fa), "A's tab is now frontmost");
        let selection = window
            .read_with(cx, |ws, cx| ws.active_file.as_ref().unwrap().read(cx).pane().read(cx).editor().selection())
            .unwrap();
        // "hello" sits at the half-open range [1, 6) in the fixture;
        // cursor holds the inclusive last byte, 5.
        assert_eq!(selection, Some(Selection { anchor: ByteOffset::new(1), cursor: ByteOffset::new(5) }));
    }

    /// Opening global search on a workspace with zero file tabs must not
    /// have `reconcile`'s "no content -> show welcome" branch wipe it
    /// out from under the user: that branch REBUILDS the whole center
    /// (not an additive welcome-alongside), so without counting the
    /// search tab as content, the very next reconcile pass -- triggered
    /// by the tab's own `LayoutChanged` -- discards it and shows
    /// Welcome instead. Global search is workspace-scoped like Compare
    /// (needs no open file), so this is reachable in ordinary use, not
    /// just restore.
    #[gpui::test]
    fn opening_global_search_on_an_empty_workspace_survives_reconcile(cx: &mut TestAppContext) {
        setup(cx);
        let window = open_workspace(cx, Vec::new(), None);
        window.update(cx, |ws, window, cx| ws.open_global_search(window, cx)).unwrap();
        cx.run_until_parked();

        let count =
            window.read_with(cx, |ws, cx| count_global_search_panels(&ws.dock.read(cx).dump(cx).center)).unwrap();
        assert_eq!(count, 1, "the tab must survive the reconcile pass its own open triggers");
    }

    /// A layout saved with the global search tab open must restore it
    /// AND register it in `global_search_panel` -- without that
    /// registration, `open_global_search_panel`'s dump-presence check
    /// would see the tab but the registry would be empty, so
    /// `toggle_global_search` would build a second panel instead of
    /// closing the restored one.
    #[gpui::test]
    fn restored_global_search_tab_is_registered_not_duplicated(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("layout.json");

        let first = open_workspace(cx, Vec::new(), Some(layout.clone()));
        first.update(cx, |ws, window, cx| ws.open_global_search(window, cx)).unwrap();
        cx.run_until_parked();
        first.read_with(cx, |ws, cx| ws.save_now(cx).unwrap().unwrap()).unwrap();

        let second = open_workspace(cx, Vec::new(), Some(layout));
        let restored_count =
            second.read_with(cx, |ws, cx| count_global_search_panels(&ws.dock.read(cx).dump(cx).center)).unwrap();
        assert_eq!(restored_count, 1, "restore must bring back the search tab");
        assert!(
            second.read_with(cx, |ws, _| ws.global_search_panel.is_some()).unwrap(),
            "registry must be populated on restore, not left empty"
        );

        // Toggling must CLOSE the restored tab, not open a second one.
        second.update(cx, |ws, window, cx| ws.toggle_global_search(window, cx)).unwrap();
        cx.run_until_parked();
        let after_toggle =
            second.read_with(cx, |ws, cx| count_global_search_panels(&ws.dock.read(cx).dump(cx).center)).unwrap();
        assert_eq!(after_toggle, 0, "toggle on a restored tab must close it, not duplicate it");
    }

    /// Focusing a file's strings tab must not blank the inspector or
    /// the window title / status bar: `active_file` alone only ever
    /// names a `FilePanel` (`None` while a strings tab is active), so
    /// without `reference_active_file`'s fallback to `last_active_file`
    /// these would all go blank the moment a strings tab opens.
    #[gpui::test]
    fn focusing_a_strings_tab_falls_back_to_its_file_for_status_and_inspector(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0xAAu8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        let inspector = window
            .read_with(cx, |ws, _| ws.inspector_for_test.clone())
            .unwrap()
            .expect("inspector stashed on fresh construction");

        window_open(window, &f1, cx);
        seed_active_caret(window, cx);
        assert_eq!(active_path(window, cx), Some(f1.clone()));

        window.update(cx, |ws, window, cx| ws.open_strings_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();

        assert_eq!(active_path(window, cx), Some(f1), "status/title fall back to the owning file");
        let (_, bytes) = inspector
            .read_with(cx, |insp, cx| insp.caret_window(cx))
            .expect("inspector still decodes via the fallback");
        assert_eq!(bytes, vec![0xAAu8; 16]);
    }

    /// Closing the file behind a focused strings tab must blank the
    /// status bar / title / inspector gracefully (not panic, not keep
    /// pointing at a dead entity) -- `reference_active_file`'s fallback
    /// is checked against `dump()` on every read, so a closed file
    /// simply stops being found rather than needing to be pruned from
    /// `last_active_file` eagerly.
    #[gpui::test]
    fn closing_the_file_behind_a_focused_strings_tab_blanks_status_and_inspector(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0xAAu8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        let inspector = window
            .read_with(cx, |ws, _| ws.inspector_for_test.clone())
            .unwrap()
            .expect("inspector stashed on fresh construction");

        window_open(window, &f1, cx);
        seed_active_caret(window, cx);
        window.update(cx, |ws, window, cx| ws.open_strings_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        assert_eq!(active_path(window, cx), Some(f1.clone()), "sanity: the fallback is engaged");

        // Close F1 directly (not via `close_active_tab`, which -- now
        // that a strings tab is the literal front-most tab -- would
        // close that tab instead): this is the scenario under test,
        // the file closing while its strings tab remains focused.
        let f1_panel = window.read_with(cx, |ws, _| ws.open_files.first().cloned()).unwrap().expect("f1 open");
        window.update(cx, |ws, window, cx| ws.close_file_tab(f1_panel, window, cx)).unwrap();
        cx.run_until_parked();

        assert_eq!(active_path(window, cx), None, "status/title blank once the file is gone");
        assert!(inspector.read_with(cx, |insp, cx| insp.caret_window(cx)).is_none(), "inspector blanks gracefully");
    }

    /// `cmd-z` while a strings tab is focused undoes the *reference*
    /// file's most recent edit, not nothing: `active_pane` (which
    /// `on_undo` reads) must route through `reference_active_file`'s
    /// fallback, mirroring egui's `active_file_id` (used by undo/redo
    /// among "dozens of dispatch sites" per its own doc).
    #[gpui::test]
    fn cmd_z_undoes_the_reference_files_edit_while_a_strings_tab_is_focused(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0u8; 32]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);

        cx.simulate_keystrokes(window.into(), "down");
        cx.simulate_keystrokes(window.into(), "a");
        let dirty = |cx: &mut TestAppContext| {
            window
                .read_with(cx, |ws, cx| ws.open_files.first().unwrap().read(cx).pane().read(cx).editor().is_dirty())
                .unwrap()
        };
        assert!(dirty(cx), "typing a hex digit must dirty the buffer");

        window.update(cx, |ws, window, cx| ws.open_strings_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        assert!(
            !window.read_with(cx, |ws, _| ws.has_strict_active_file()).unwrap(),
            "sanity: strings tab is front-most"
        );

        cx.simulate_keystrokes(window.into(), "cmd-z");
        assert!(!dirty(cx), "cmd-z must undo via the reference-file fallback while a strings tab is focused");
    }

    /// `cmd-shift-c` while a strings tab is focused copies the
    /// *reference* file's selection as hex, not nothing.
    #[gpui::test]
    fn cmd_shift_c_copies_the_reference_files_selection_while_a_strings_tab_is_focused(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[0xDE, 0xAD, 0xBE, 0xEF]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);

        window
            .update(cx, |ws, _window, cx| {
                let pane = ws.active_file.as_ref().unwrap().read(cx).pane().clone();
                pane.update(cx, |pane, _| {
                    let selection = Selection { anchor: ByteOffset::new(0), cursor: ByteOffset::new(1) };
                    pane.editor_mut().set_selection(Some(selection));
                });
            })
            .unwrap();

        window.update(cx, |ws, window, cx| ws.open_strings_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        assert!(
            !window.read_with(cx, |ws, _| ws.has_strict_active_file()).unwrap(),
            "sanity: strings tab is front-most"
        );

        cx.simulate_keystrokes(window.into(), "cmd-shift-c");
        let clip = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(clip.as_deref(), Some("DE AD"));
    }

    /// Reopening an existing strings tab after a collapse elsewhere
    /// must land it in a live panel, not lose or duplicate it. Mirrors
    /// `add_after_split_pane_collapse_lands_in_a_live_tab_panel`'s
    /// shape (build a two-`TabPanel` center directly -- left: F1;
    /// right: F2 + a strings tab for F2 -- so both are genuinely
    /// tracked by the `DockItem` cache, then empty the left panel so it
    /// detaches, leaving a dead cache entry ahead of the live one).
    ///
    /// Exercises `focus_strings_tab`'s resync-then-re-resolve-by-path
    /// guard (added to mirror `focus_existing_tab`'s: an incremental
    /// add can route into a stale/dead `TabPanel` reference --
    /// `DockItem::Split::add_panel`'s "first `Tabs` found" walk doesn't
    /// check liveness), though in this harness `reconcile`'s own
    /// ambient `resync_center_if_stale` call already heals the cache
    /// on the layout-changed pass the collapse itself triggers (true
    /// of the FilePanel sibling test too -- verified by temporarily
    /// removing each guard and re-running both, which still passed).
    /// The explicit guard remains defense-in-depth for a collapse and
    /// reopen landing in the same update with no intervening reconcile
    /// pass, a window this harness can't isolate; this test still
    /// pins the observable contract (no loss, no duplicate).
    #[gpui::test]
    fn reopening_a_strings_tab_after_a_collapse_lands_in_a_live_tab_panel(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);
        let window = open_workspace(cx, Vec::new(), None);
        window_open(window, &f1, cx);

        let f1_panel = window.read_with(cx, |ws, _| ws.active_file.clone()).unwrap().expect("f1 open");
        let (left, f2_panel) = window
            .update(cx, |ws, window, cx| {
                let weak = ws.dock.downgrade();
                let bytes = std::fs::read(&f2).unwrap();
                let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
                let f2_panel = cx.new(|cx| FilePanel::new(source, Some(f2.clone()), window, cx));
                let strings_f2 =
                    cx.new(|cx| StringsPanel::new(f2_panel.read(cx).pane().clone(), Some(f2.clone()), window, cx));
                ws.open_files = vec![f1_panel.clone(), f2_panel.clone()];
                ws.track_strings_panel(strings_f2.clone(), window, cx);

                let left_view: Arc<dyn PanelView> = Arc::new(f1_panel.clone());
                let right_views: Vec<Arc<dyn PanelView>> = vec![Arc::new(f2_panel.clone()), Arc::new(strings_f2)];
                let left = DockItem::tabs(vec![left_view], &weak, window, cx);
                let right = DockItem::tabs(right_views, &weak, window, cx);
                let split = DockItem::split(Axis::Horizontal, vec![left.clone(), right], &weak, window, cx);
                ws.dock.update(cx, |dock, cx| dock.set_center(split, window, cx));
                ws.set_active_file(Some(f2_panel.clone()), cx);
                (left, f2_panel)
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 2);
        assert_eq!(strings_tab_count(window, cx), 1);

        // Empty the left (F1) panel so its `TabPanel` detaches, leaving
        // a dead entry in the cache ahead of the still-live right one.
        let DockItem::Tabs { view: left_view, .. } = &left else { unreachable!() };
        window
            .update(cx, |_ws, window, cx| {
                left_view.update(cx, |tab, cx| {
                    while let Some(panel) = tab.active_panel(cx) {
                        tab.remove_panel(panel, window, cx);
                    }
                });
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(file_count(window, cx), 1, "only f2 survives");
        assert_eq!(strings_tab_count(window, cx), 1, "f2's strings tab survives the collapse elsewhere");

        // Reopen F2's strings tab: it already exists (found via
        // `dump()`), so this exercises `focus_strings_tab`'s
        // resync-then-re-resolve-then-re-add path, not the "create a
        // fresh one" path.
        window
            .update(cx, |ws, window, cx| {
                ws.set_active_file(Some(f2_panel), cx);
                ws.open_strings_for_active_file(window, cx);
            })
            .unwrap();
        cx.run_until_parked();

        assert_eq!(strings_tab_count(window, cx), 1, "the existing strings tab is reused, not lost or duplicated");
        assert_eq!(file_count(window, cx), 1);
    }

    /// Opening a path that fails to read surfaces an error toast
    /// through the `Root` notification layer (replacing the old inline
    /// status-text banner). Uses [`open_workspace_with_root`] (not the
    /// plain [`open_workspace`] every other test uses): `push_notification`
    /// needs a real `gpui_component::Root` as the window's root view,
    /// which production always has (see `main.rs`) but the bare
    /// `open_workspace` helper does not.
    #[gpui::test]
    fn open_missing_file_surfaces_an_error_toast(cx: &mut TestAppContext) {
        setup(cx);
        let (window, workspace) = open_workspace_with_root(cx, Vec::new(), None);
        let missing = PathBuf::from("/definitely/not/a/real/path-for-hxy-gpui-tests.bin");

        // Not `window.update(cx, |_root, window, cx| ...)`: that goes
        // through `Root::update`, and `ws.open_path`'s failure branch
        // calls `push_notification`, which does its own `Root::update`
        // -- nesting two updates of the same `Root` entity panics.
        // `AppContext::update_window` gives `window`/`cx` without
        // borrowing the root view, so the later `push_notification`
        // inside `open_path` is the only (successful) `Root` borrow.
        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.open_path(missing.clone(), window, cx));
        })
        .unwrap();
        cx.run_until_parked();

        assert_eq!(cx.update_window(window.into(), |_, window, cx| window.notifications(cx).len()).unwrap(), 1);
    }

    /// An external modification of an open file's backing path stages
    /// a reload prompt and opens the dialog; a second modification
    /// before the user responds is dropped (still watched, so it
    /// isn't lost -- a later change would re-fire).
    #[gpui::test]
    fn external_modify_stages_a_reload_prompt(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "watched.bin", b"aaaa");
        let (window, workspace) = open_workspace_with_root(cx, vec![f1.clone()], None);
        cx.run_until_parked();

        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.handle_external_change(f1.clone(), crate::watch::ExternalChangeKind::Modified, window, cx)
            });
        })
        .unwrap();

        assert!(workspace.read_with(cx, |ws, _| ws.pending_reload.is_some()), "modify stages a prompt");
        assert!(
            cx.update_window(window.into(), |_, window, cx| window.has_active_dialog(cx)).unwrap(),
            "modify opens the reload dialog"
        );

        // A second change lands while the first prompt is still
        // pending; it's dropped rather than replacing/queuing.
        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.handle_external_change(f1.clone(), crate::watch::ExternalChangeKind::Modified, window, cx)
            });
        })
        .unwrap();
        assert_eq!(workspace.read_with(cx, |ws, _| ws.pending_reload.as_ref().map(|p| p.path.clone())), Some(f1));
    }

    /// A file whose panel is removed directly through the dock (an X-click
    /// that bypasses `close_active_tab`, leaving a stale `open_files`
    /// entry) is reconciled out of the watcher on the next poll, so it
    /// stops firing ghost reload prompts. Poll-driven and deterministic:
    /// it asserts the reconciled watch set, not a real filesystem event.
    #[gpui::test]
    fn dock_removed_file_is_unwatched(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "f1.bin", b"aaaa");
        let f2 = temp_file(&dir, "f2.bin", b"bbbb");
        let window = open_workspace(cx, vec![f1.clone(), f2.clone()], None);

        // First poll registers both open files.
        window.update(cx, |ws, window, cx| ws.poll_file_watch(window, cx)).unwrap();
        let watch_available = window.read_with(cx, |ws, _| ws.file_watch.is_some()).unwrap();
        if !watch_available {
            // No platform watcher in this environment; the poll is a no-op
            // and there is nothing to assert.
            return;
        }
        window
            .read_with(cx, |ws, _| {
                let watched = ws.file_watch.as_ref().unwrap().watched_paths();
                assert!(watched.contains(&f1) && watched.contains(&f2), "both open files are watched: {watched:?}");
            })
            .unwrap();

        // Remove f1's panel straight through the dock, bypassing
        // `close_active_tab` -- `open_files` still holds the stale entry.
        let f1_panel = window.read_with(cx, |ws, cx| ws.open_file_for_path(&f1, cx)).unwrap().expect("f1 open");
        window
            .update(cx, |ws, window, cx| {
                let view: Arc<dyn PanelView> = Arc::new(f1_panel);
                ws.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
            })
            .unwrap();
        cx.run_until_parked();

        // The next poll must drop f1 (no longer in the dump) while keeping
        // f2, so a later external change to f1 can't stage a reload.
        window.update(cx, |ws, window, cx| ws.poll_file_watch(window, cx)).unwrap();
        window
            .read_with(cx, |ws, _| {
                let watched = ws.file_watch.as_ref().unwrap().watched_paths();
                assert!(!watched.contains(&f1), "the dock-removed file is unwatched: {watched:?}");
                assert!(watched.contains(&f2), "the still-open file stays watched: {watched:?}");
            })
            .unwrap();
    }

    /// Dismissing the reload dialog via Escape must resolve the
    /// pending prompt as `Ignore` (dirty state untouched, prompt
    /// cleared), not just close the dialog and leave `pending_reload`
    /// set forever -- that would silently swallow every later
    /// external-change event for any file (`handle_external_change`
    /// drops a new Modified while one is already pending). Mirrors
    /// egui's window-chrome close, which routes to the same Ignore
    /// branch as its Ignore button.
    #[gpui::test]
    fn escape_dismisses_the_reload_dialog_as_ignore(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "watched.bin", b"aaaa");
        let (window, workspace) = open_workspace_with_root(cx, vec![f1.clone()], None);
        cx.run_until_parked();

        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                let pane = ws.active_file.as_ref().unwrap().read(cx).pane().clone();
                pane.update(cx, |pane, cx| {
                    pane.editor_mut().splice(0, 1, vec![b'b']).unwrap();
                    cx.notify();
                });
                ws.handle_external_change(f1.clone(), crate::watch::ExternalChangeKind::Modified, window, cx);
            });
        })
        .unwrap();
        assert!(workspace.read_with(cx, |ws, _| ws.pending_reload.is_some()));
        assert!(cx.update_window(window.into(), |_, window, cx| window.has_active_dialog(cx)).unwrap());
        // Tick the remember checkbox: a dismissal (as opposed to a
        // footer button) is a non-decision and must not persist a
        // per-file pref from it.
        workspace.read_with(cx, |ws, _| ws.reload_remember.set(true));

        cx.simulate_keystrokes(window.into(), "escape");
        cx.run_until_parked();

        assert!(workspace.read_with(cx, |ws, _| ws.pending_reload.is_none()), "escape clears the pending prompt");
        assert!(
            cx.update(|cx| crate::settings::settings(cx).file_watch_prefs.is_empty()),
            "dismissal must not persist the ticked remember checkbox"
        );
        assert!(
            !cx.update_window(window.into(), |_, window, cx| window.has_active_dialog(cx)).unwrap(),
            "escape closes the dialog"
        );
        assert!(
            workspace.read_with(cx, |ws, cx| ws
                .active_file
                .as_ref()
                .unwrap()
                .read(cx)
                .pane()
                .read(cx)
                .editor()
                .is_dirty()),
            "ignore (via escape) leaves the pane -- and its dirty patch -- untouched"
        );

        // A later change for the same file must still raise a new
        // prompt -- the earlier dismissal didn't leave the "one
        // pending at a time" guard permanently stuck.
        std::fs::write(&f1, b"bbbb").unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.handle_external_change(f1.clone(), crate::watch::ExternalChangeKind::Modified, window, cx)
            });
        })
        .unwrap();
        assert!(workspace.read_with(cx, |ws, _| ws.pending_reload.is_some()), "a subsequent modify re-raises a prompt");
        assert!(cx.update_window(window.into(), |_, window, cx| window.has_active_dialog(cx)).unwrap());
    }

    /// A removal always just toasts -- there's nothing to reload --
    /// and never stages a prompt, matching egui's
    /// `handle_external_change`.
    #[gpui::test]
    fn external_removal_only_toasts(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "watched.bin", b"aaaa");
        let (window, workspace) = open_workspace_with_root(cx, vec![f1.clone()], None);
        cx.run_until_parked();

        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.handle_external_change(f1, crate::watch::ExternalChangeKind::Removed, window, cx)
            });
        })
        .unwrap();

        assert!(workspace.read_with(cx, |ws, _| ws.pending_reload.is_none()));
        assert_eq!(cx.update_window(window.into(), |_, window, cx| window.notifications(cx).len()).unwrap(), 1);
    }

    /// `resolve_reload(DiscardEdits)` re-reads disk bytes into the
    /// pane and drops the dirty patch; an entropy panel already open
    /// for the file re-runs against the new bytes (the cascade).
    #[gpui::test]
    fn resolve_reload_discard_updates_pane_and_cascades(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "watched.bin", b"aaaa");
        let (window, workspace) = open_workspace_with_root(cx, vec![f1.clone()], None);
        cx.run_until_parked();

        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.open_entropy_for_active_file(window, cx))
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(workspace.read_with(cx, |ws, cx| count_entropy_panels(&ws.dock.read(cx).dump(cx).center)), 1);

        // `open_entropy_for_active_file` focuses the new entropy tab,
        // so `active_file` (which tracks a `FilePanel` specifically)
        // is `None` now -- `reference_active_file` is the fallback-
        // aware read that still resolves to the owning file.
        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                let pane = ws.reference_active_file(cx).unwrap().read(cx).pane().clone();
                pane.update(cx, |pane, cx| {
                    pane.editor_mut().splice(0, 1, vec![b'b']).unwrap();
                    cx.notify();
                });
                ws.handle_external_change(f1.clone(), crate::watch::ExternalChangeKind::Modified, window, cx);
            });
        })
        .unwrap();
        assert!(workspace.read_with(cx, |ws, cx| {
            ws.reference_active_file(cx).unwrap().read(cx).pane().read(cx).editor().is_dirty()
        }));

        std::fs::write(&f1, b"cccc").unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.resolve_reload(ReloadDecision::DiscardEdits, window, cx));
        })
        .unwrap();
        cx.run_until_parked();

        workspace.read_with(cx, |ws, cx| {
            let file = ws.reference_active_file(cx).unwrap();
            let editor = file.read(cx).pane().read(cx).editor();
            assert!(!editor.is_dirty(), "discard drops the patch");
            let bytes = editor.source().read(ByteRange::new(ByteOffset::new(0), ByteOffset::new(4)).unwrap()).unwrap();
            assert_eq!(&*bytes, b"cccc");
        });
        assert!(workspace.read_with(cx, |ws, _| ws.pending_reload.is_none()));
    }

    /// `resolve_reload(KeepEdits)` re-reads disk bytes but replays the
    /// dirty patch on top of the new base.
    #[gpui::test]
    fn resolve_reload_keep_edits_preserves_the_patch(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "watched.bin", b"aaaa");
        let (window, workspace) = open_workspace_with_root(cx, vec![f1.clone()], None);
        cx.run_until_parked();

        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                let pane = ws.active_file.as_ref().unwrap().read(cx).pane().clone();
                pane.update(cx, |pane, cx| {
                    pane.editor_mut().splice(0, 1, vec![b'b']).unwrap();
                    cx.notify();
                });
                ws.handle_external_change(f1.clone(), crate::watch::ExternalChangeKind::Modified, window, cx);
            });
        })
        .unwrap();

        std::fs::write(&f1, b"cccc").unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.resolve_reload(ReloadDecision::KeepEdits, window, cx));
        })
        .unwrap();
        cx.run_until_parked();

        workspace.read_with(cx, |ws, cx| {
            let editor = ws.active_file.as_ref().unwrap().read(cx).pane().read(cx).editor();
            assert!(editor.is_dirty(), "keep-edits preserves the patch");
            let bytes = editor.source().read(ByteRange::new(ByteOffset::new(0), ByteOffset::new(4)).unwrap()).unwrap();
            assert_eq!(&*bytes, b"bccc");
        });
    }

    /// `resolve_reload(Ignore)` leaves the pane untouched and clears
    /// the pending prompt.
    #[gpui::test]
    fn resolve_reload_ignore_leaves_pane_untouched(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "watched.bin", b"aaaa");
        let (window, workspace) = open_workspace_with_root(cx, vec![f1.clone()], None);
        cx.run_until_parked();

        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.handle_external_change(f1.clone(), crate::watch::ExternalChangeKind::Modified, window, cx)
            });
        })
        .unwrap();

        std::fs::write(&f1, b"cccc").unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.resolve_reload(ReloadDecision::Ignore, window, cx));
        })
        .unwrap();

        assert!(workspace.read_with(cx, |ws, _| ws.pending_reload.is_none()));
        workspace.read_with(cx, |ws, cx| {
            let bytes = ws
                .active_file
                .as_ref()
                .unwrap()
                .read(cx)
                .pane()
                .read(cx)
                .editor()
                .source()
                .read(ByteRange::new(ByteOffset::new(0), ByteOffset::new(4)).unwrap())
                .unwrap();
            assert_eq!(&*bytes, b"aaaa");
        });
    }

    fn collect_file_paths(state: &PanelState) -> Vec<PathBuf> {
        let mut out = Vec::new();
        collect_file_paths_into(state, &mut out);
        out
    }

    fn collect_file_paths_into(state: &PanelState, out: &mut Vec<PathBuf>) {
        if state.panel_name == FILE_PANEL_NAME
            && let gpui_component::dock::PanelInfo::Panel(value) = &state.info
            && let Some(path) = value.get("path").and_then(|p| p.as_str())
        {
            out.push(PathBuf::from(path));
        }
        for child in &state.children {
            collect_file_paths_into(child, out);
        }
    }

    /// Inert stand-in for a parsed template, so the visualizer tests
    /// can install a completed instance without a real runtime.
    struct InertParsed;

    impl hxy_plugin_host::ParsedTemplate for InertParsed {
        fn execute(
            &self,
            _args: &[hxy_plugin_host::template::Arg],
        ) -> Result<hxy_plugin_host::template::ResultTree, hxy_vfs::HandlerError> {
            Err(hxy_vfs::HandlerError::Unsupported("test template never re-executes".into()))
        }

        fn expand_array(
            &self,
            _array_id: u64,
            _start: u64,
            _end: u64,
        ) -> Result<Vec<hxy_plugin_host::template::Node>, hxy_vfs::HandlerError> {
            Ok(Vec::new())
        }
    }

    /// Install a completed template instance whose node `1` carries a
    /// `hxy_visualize` attribute on `window`'s active file, returning
    /// the instance id and the file's entity (the file tab stops being
    /// the ACTIVE tab once the visualizer tab opens, so later steps
    /// need the handle, not `ws.active_file`).
    fn install_visualizer_fixture(
        window: WindowHandle<Workspace>,
        cx: &mut TestAppContext,
    ) -> (hxy_templates::state::TemplateInstanceId, Entity<FilePanel>) {
        use hxy_plugin_host::template::Node;
        use hxy_plugin_host::template::NodeType;
        use hxy_plugin_host::template::ScalarKind;
        use hxy_plugin_host::template::Span;

        let node = |name: &str, span: (u64, u64), visualize: Option<&str>| Node {
            name: name.to_owned(),
            type_name: NodeType::Scalar(ScalarKind::U8K),
            span: Span { offset: span.0, length: span.1 },
            value: None,
            parent: None,
            array: None,
            display: None,
            attributes: visualize
                .map(|spec| vec![(hxy_plugin_host::VISUALIZE_ATTR.to_owned(), spec.to_owned())])
                .into_iter()
                .flatten()
                .collect(),
        };
        let tree = hxy_plugin_host::template::ResultTree {
            nodes: vec![node("plain", (0, 4), None), node("pixels", (4, 4), Some("digram"))],
            diagnostics: Vec::new(),
            byte_palette: None,
        };
        window
            .update(cx, |ws, _window, cx| {
                let file = ws.active_file.clone().expect("a file is active");
                let id = file.update(cx, |file, cx| {
                    let state = hxy_templates::state::new_state_from(
                        Arc::new(InertParsed),
                        tree,
                        std::collections::HashMap::new(),
                    );
                    let id = file.fresh_template_instance_id();
                    file.upsert_template_instance(hxy_templates::state::TemplateInstance {
                        id,
                        source_path: PathBuf::from("/tmp/fixture.bt"),
                        display_name: "fixture.bt".to_owned(),
                        range: ByteRange::new(ByteOffset::new(0), ByteOffset::new(8)).unwrap(),
                        source_fingerprint: None,
                        state,
                    });
                    file.active_template = Some(id);
                    cx.notify();
                    id
                });
                (id, file)
            })
            .unwrap()
    }

    /// The end-to-end OpenVisualizer round trip: a template row's
    /// visualizer marker click (its `TemplateEvent`) makes the
    /// workspace open a `VisualizerPanel` tab for that file with the
    /// clicked node's sub-tab active; a second request reuses the tab
    /// and just moves the active key.
    #[gpui::test]
    fn open_visualizer_event_opens_panel_with_active_key(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let a = temp_file(&dir, "a.bin", &[0u8; 8]);
        let window = open_workspace(cx, vec![a], None);
        let (id, file) = install_visualizer_fixture(window, cx);
        cx.run_until_parked();

        let idx = hxy_templates::state::TemplateNodeIdx(1);
        window
            .update(cx, |_ws, window, cx| {
                file.update(cx, |file, cx| {
                    file.apply_template_event(&hxy_templates::state::TemplateEvent::OpenVisualizer(idx), window, cx);
                });
            })
            .unwrap();
        cx.run_until_parked();

        let count = window.read_with(cx, |ws, cx| count_visualizer_panels(&ws.dock.read(cx).dump(cx).center)).unwrap();
        assert_eq!(count, 1, "the visualizer tab opened");
        let key = hxy_templates::visualize::VisualizerKey { instance: id, node: idx };
        window
            .read_with(cx, |ws, cx| {
                let panel = ws.visualizer_panels.first().expect("tracked");
                assert_eq!(panel.read(cx).active_key(), Some(key));
            })
            .unwrap();

        // Second request on the same node: no second tab.
        window
            .update(cx, |_ws, window, cx| {
                file.update(cx, |file, cx| {
                    file.apply_template_event(&hxy_templates::state::TemplateEvent::OpenVisualizer(idx), window, cx);
                });
            })
            .unwrap();
        cx.run_until_parked();
        let count = window.read_with(cx, |ws, cx| count_visualizer_panels(&ws.dock.read(cx).dump(cx).center)).unwrap();
        assert_eq!(count, 1, "open-or-focus never duplicates the tab");
    }

    /// The palette context only advertises visualizer targets once a
    /// template with visualize-bearing fields has run, so the palette
    /// entry appears exactly when egui's `has_visualizer` gate would
    /// list it.
    #[gpui::test]
    fn palette_context_counts_visualizer_targets(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let a = temp_file(&dir, "a.bin", &[0u8; 8]);
        let window = open_workspace(cx, vec![a], None);
        let before = window.read_with(cx, |ws, cx| ws.palette_context(cx).visualizer_target_count).unwrap();
        assert_eq!(before, 0);
        install_visualizer_fixture(window, cx);
        cx.run_until_parked();
        let after = window.read_with(cx, |ws, cx| ws.palette_context(cx).visualizer_target_count).unwrap();
        assert_eq!(after, 1);
    }

    /// Closing a file tab cascades to its visualizer tab, exactly as
    /// it does for strings/entropy/checksums.
    #[gpui::test]
    fn closing_the_file_closes_its_visualizer_tab(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let a = temp_file(&dir, "a.bin", &[0u8; 8]);
        let window = open_workspace(cx, vec![a], None);
        let (_id, file) = install_visualizer_fixture(window, cx);
        window.update(cx, |ws, window, cx| ws.open_visualizer_for_active_file(window, cx)).unwrap();
        cx.run_until_parked();
        let count = window.read_with(cx, |ws, cx| count_visualizer_panels(&ws.dock.read(cx).dump(cx).center)).unwrap();
        assert_eq!(count, 1);

        window
            .update(cx, |ws, window, cx| {
                ws.close_file_tab(file.clone(), window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        let count = window.read_with(cx, |ws, cx| count_visualizer_panels(&ws.dock.read(cx).dump(cx).center)).unwrap();
        assert_eq!(count, 0, "the visualizer tab closed with its file");
        assert!(window.read_with(cx, |ws, _| ws.visualizer_panels.is_empty()).unwrap());
    }

    /// A `hex_columns` settings change live-applies to every open
    /// pane through the workspace's `SettingsGlobal` observer.
    #[gpui::test]
    fn settings_columns_live_apply_to_open_panes(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let a = temp_file(&dir, "a.bin", &[0u8; 64]);
        let b = temp_file(&dir, "b.bin", &[1u8; 64]);
        let window = open_workspace(cx, vec![a, b], None);

        cx.update(|cx| update_settings(cx, |s| s.hex_columns = hxy_core::ColumnCount::new(24).unwrap()));
        cx.run_until_parked();

        let columns: Vec<u16> = window
            .read_with(cx, |ws, cx| ws.open_files.iter().map(|f| f.read(cx).pane().read(cx).columns().get()).collect())
            .unwrap();
        assert_eq!(columns, vec![24, 24], "both open panes picked up the new column count");
    }

    /// A successful disk open records the path at the top of the
    /// persisted recent-files list.
    #[gpui::test]
    fn open_records_recent_files(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let a = temp_file(&dir, "a.bin", &[0u8; 8]);
        let _window = open_workspace(cx, vec![a.clone()], None);

        let recents = cx.update(|cx| crate::settings::settings(cx).recent_files);
        assert_eq!(recents.first().map(|r| r.path.clone()), Some(a));
    }

    /// The vim toggle rotates the persisted `input_mode` setting and
    /// the observer applies it to every open editor (egui
    /// `toggle_vim_mode` parity: toggle and setting stay in sync).
    #[gpui::test]
    fn toggle_vim_updates_settings_and_all_editors(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let a = temp_file(&dir, "a.bin", &[0u8; 8]);
        let b = temp_file(&dir, "b.bin", &[0u8; 8]);
        let window = open_workspace(cx, vec![a, b], None);

        window.update(cx, |ws, _window, cx| ws.toggle_active_vim(cx)).unwrap();
        cx.run_until_parked();

        assert_eq!(cx.update(|cx| crate::settings::settings(cx).input_mode), InputMode::Vim);
        let modes: Vec<InputMode> = window
            .read_with(cx, |ws, cx| {
                ws.open_files.iter().map(|f| f.read(cx).pane().read(cx).editor().input_mode()).collect()
            })
            .unwrap();
        assert_eq!(modes, vec![InputMode::Vim, InputMode::Vim], "every open editor flipped");
    }

    /// Clicking a welcome-screen recents row routes through the normal
    /// open flow and lands a file tab.
    #[gpui::test]
    fn welcome_recent_click_opens_the_file(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let a = temp_file(&dir, "recent.bin", &[7u8; 8]);
        let (_window, workspace) = open_workspace_with_root(cx, Vec::new(), None);

        let welcome = workspace.read_with(cx, |ws, _| ws.welcome.clone().expect("welcome shown on empty workspace"));
        cx.update(|cx| welcome.update(cx, |_, cx| cx.emit(OpenRecentRequested(a.clone()))));
        cx.run_until_parked();

        let count = workspace.read_with(cx, |ws, cx| count_file_panels(&ws.dock.read(cx).dump(cx).center));
        assert_eq!(count, 1, "the recents click opened the file");
        let active = workspace.read_with(cx, |ws, cx| ws.active_path(cx));
        assert_eq!(active, Some(a));
    }

    /// Resolving the reload prompt with the remember checkbox ticked
    /// persists the per-file pref (Ignore -> Never), mirroring egui's
    /// `render_reload_prompt` mapping.
    #[gpui::test]
    fn resolving_with_remember_persists_the_per_file_pref(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let f1 = temp_file(&dir, "watched.bin", b"aaaa");
        let (window, workspace) = open_workspace_with_root(cx, vec![f1.clone()], None);

        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.handle_external_change(f1.clone(), crate::watch::ExternalChangeKind::Modified, window, cx);
            });
        })
        .unwrap();
        assert!(workspace.read_with(cx, |ws, _| ws.pending_reload.is_some()));
        workspace.read_with(cx, |ws, _| ws.reload_remember.set(true));

        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.resolve_reload(ReloadDecision::Ignore, window, cx));
        })
        .unwrap();

        let prefs = cx.update(|cx| crate::settings::settings(cx).file_watch_prefs);
        assert_eq!(prefs.len(), 1);
        assert_eq!(prefs[0].path, f1);
        assert_eq!(prefs[0].auto_reload, hxy_settings::AutoReloadMode::Never);
    }

    /// `auto_reload: Always` applies an external modification
    /// silently: bytes re-read, no prompt staged (egui
    /// `handle_external_change` parity).
    #[gpui::test]
    fn auto_reload_always_swaps_bytes_without_a_prompt(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let a = temp_file(&dir, "watched.bin", b"aaaa");
        let (window, workspace) = open_workspace_with_root(cx, vec![a.clone()], None);
        cx.update(|cx| update_settings(cx, |s| s.auto_reload = hxy_settings::AutoReloadMode::Always));

        std::fs::write(&a, b"bbbb").unwrap();
        let a_for_event = a.clone();
        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.handle_external_change(a_for_event, crate::watch::ExternalChangeKind::Modified, window, cx)
            })
        })
        .unwrap();
        cx.run_until_parked();

        workspace.read_with(cx, |ws, cx| {
            assert!(ws.pending_reload.is_none(), "no prompt in Always mode");
            let file = ws.open_files.first().expect("file open");
            let bytes = file
                .read(cx)
                .pane()
                .read(cx)
                .editor()
                .source()
                .read(ByteRange::new(ByteOffset::new(0), ByteOffset::new(4)).unwrap())
                .unwrap();
            assert_eq!(&*bytes, b"bbbb", "bytes were re-read from disk silently");
        });
    }
}
