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
use gpui::Styled;
use gpui::Subscription;
use gpui::Task;
use gpui::Window;
use gpui::actions;
use gpui::div;
use gpui::prelude::*;
use gpui::px;
use gpui_component::ActiveTheme;
use gpui_component::WindowExt;
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
use gpui_dock_picker::DockPicker;
use gpui_dock_picker::PickTarget;
use hxy_core::ByteOffset;
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
use crate::palette::modes::CopyFormat;
use crate::palette::modes::PaletteAction;
use crate::palette::modes::PaletteContext;
use crate::panels::FILE_PANEL_NAME;
use crate::panels::FilePanel;
use crate::panels::InspectorPanel;
use crate::panels::WELCOME_PANEL_NAME;
use crate::panels::WelcomePanel;
use crate::panels::inspector::ActiveHexPane;
use crate::persist;
use crate::status::dirty_marker;
use crate::status::status_file_name_text;
use crate::status::status_offset_text;
use crate::status::status_open_error_text;
use crate::status::status_vim_mode_text;
use crate::status::window_title_text;

actions!(hxy_gpui, [OpenFile, ToggleVim, ToggleInspector, ToggleSearch, CloseSearch, OpenPalette, PickPane]);

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
        gpui::KeyBinding::new("cmd-alt-v", ToggleVim, None),
        gpui::KeyBinding::new("cmd-i", ToggleInspector, None),
        gpui::KeyBinding::new("cmd-f", ToggleSearch, None),
        // Mirror the egui app's `COMMAND_PALETTE` chord (Cmd+Shift+P).
        gpui::KeyBinding::new("cmd-shift-p", OpenPalette, None),
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
}

pub struct Workspace {
    dock: Entity<DockArea>,
    /// The welcome placeholder while it occupies the center; `None`
    /// whenever any file tab is open. Owned so it can be removed by
    /// identity when the first file opens.
    welcome: Option<Entity<WelcomePanel>>,
    /// The file panel backing the active center tab; drives the status
    /// bar and receives the vim toggle. `None` when the welcome
    /// placeholder is showing.
    active_file: Option<Entity<FilePanel>>,
    /// Repaints the workspace (hence the status bar) when the active
    /// pane's editor changes. Re-established whenever the active file
    /// changes.
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
    layout_path: Option<PathBuf>,
    /// The in-flight debounced save; dropping it (on the next event)
    /// cancels the pending write.
    save_debounce: Option<Task<()>>,
    _appearance_subscription: Subscription,
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

        let mut workspace = Self {
            dock,
            welcome: None,
            active_file: None,
            active_pane_observe: None,
            _dock_subscription: dock_subscription,
            last_title: None,
            focus_handle: cx.focus_handle(),
            focus_pending: true,
            needs_reconcile: false,
            open_files: Vec::new(),
            layout_path,
            save_debounce: None,
            _appearance_subscription: appearance_subscription,
            palette,
            pane_picker,
            #[cfg(test)]
            inspector_for_test: None,
        };
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

    /// Add a file tab to the center dock, making it active (the dock
    /// focuses the new tab's pane itself when the active tab changes).
    fn add_file_panel(&mut self, panel: Entity<FilePanel>, window: &mut Window, cx: &mut Context<Self>) {
        // A center add routes through `DockItem::Split.items`; resync that
        // cache from the live tree first so the add never lands in a
        // collapsed-away tab panel (see `resync_center_if_stale`).
        self.resync_center_if_stale(window, cx);
        self.register_open_file(&panel, cx);
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// Track a newly opened file for later resync reuse, replacing any
    /// prior entry for the same path (a reopen supersedes) so the
    /// registry does not accumulate duplicate paths.
    fn register_open_file(&mut self, panel: &Entity<FilePanel>, cx: &Context<Self>) {
        if let Some(path) = panel.read(cx).path().map(Path::to_path_buf) {
            self.open_files.retain(|file| file.read(cx).path() != Some(path.as_path()));
        }
        self.open_files.push(panel.clone());
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
        // The rebuilt cache is accurate: refresh the registry from it.
        let mut live = Vec::new();
        collect_file_entities(self.dock.read(cx).items(), &mut live);
        self.open_files = live;
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
        match std::fs::read(&path) {
            Ok(bytes) => {
                let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
                let panel = cx.new(|cx| FilePanel::new(source, Some(path), window, cx));
                self.add_file_panel(panel, window, cx);
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

    fn on_toggle_vim(&mut self, _: &ToggleVim, _window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_active_vim(cx);
    }

    /// Toggle the active pane's input mode between Default and Vim.
    /// Shared by the `cmd-alt-v` action and the palette's Toggle Vim
    /// entry.
    pub(crate) fn toggle_active_vim(&mut self, cx: &mut Context<Self>) {
        let Some(file) = self.active_file.clone() else { return };
        let pane = file.read(cx).pane().clone();
        pane.update(cx, |pane, cx| {
            let next = match pane.editor().input_mode() {
                InputMode::Default => InputMode::Vim,
                InputMode::Vim => InputMode::Default,
            };
            pane.editor_mut().set_input_mode(next);
            cx.notify();
        });
    }

    /// `cmd-e` / Edit > Toggle Edit Mode: flip the active file between
    /// read-only and mutable. No-op with no active file.
    fn on_toggle_edit_mode(&mut self, _: &ToggleEditMode, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.active_file.clone() else { return };
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

    /// Snapshot of the active file for the palette's entry builders.
    pub(crate) fn palette_context(&self, cx: &App) -> PaletteContext {
        let Some(file) = &self.active_file else { return PaletteContext::default() };
        let pane = file.read(cx).pane().read(cx);
        let editor = pane.editor();
        let selection = editor.selection();
        let cursor = selection.map(|s| s.cursor.get()).unwrap_or(0);
        let source_len = editor.source().len().get();
        let selection = selection.map(|s| {
            let range = s.range();
            (range.start().get(), range.end().get())
        });
        PaletteContext {
            has_active_file: true,
            cursor,
            source_len,
            selection,
            vim_on: matches!(editor.input_mode(), InputMode::Vim),
        }
    }

    /// The active file's [`HexPane`], for palette action dispatch.
    pub(crate) fn active_pane(&self, cx: &App) -> Option<Entity<HexPane>> {
        self.active_file.as_ref().map(|file| file.read(cx).pane().clone())
    }

    /// Palette dispatch entry points, wrapping the action handlers so
    /// the palette can route into the same code paths as the shortcuts.
    pub(crate) fn open_file_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.on_open_file(&OpenFile, window, cx);
    }

    pub(crate) fn toggle_inspector_dock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.on_toggle_inspector(&ToggleInspector, window, cx);
    }

    #[cfg(test)]
    pub(crate) fn palette(&self) -> Entity<Palette> {
        self.palette.clone()
    }

    /// Close the active file tab, if one is focused, and drop its entity
    /// from the reuse registry so the closed file's buffer is released
    /// promptly rather than lingering until a rare cache rebuild.
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
        let Some(active) = self.active_file.clone() else { return };
        let closed = active.entity_id();
        let view: Arc<dyn PanelView> = Arc::new(active);
        self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
        self.open_files.retain(|file| file.entity_id() != closed);
    }

    /// Bring the workspace's own tracking back in sync with the dock's
    /// live tab set: add or remove the welcome placeholder so it shows
    /// exactly when no file tab is open, and refresh the active file.
    /// Runs after layout changes, where a `&mut Window` is available.
    fn reconcile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let file_count = {
            let state = self.dock.read(cx).dump(cx);
            count_file_panels(&state.center)
        };

        if file_count == 0 {
            if self.welcome.is_none() {
                let welcome = cx.new(WelcomePanel::new);
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
                let view: Arc<dyn PanelView> = Arc::new(welcome);
                self.dock.update(cx, |dock, cx| dock.remove_panel(view, DockPlacement::Center, window, cx));
            }
        }

        let active = active_file_panel(self.dock.read(cx).items(), cx);
        self.set_active_file(active, cx);
    }

    /// Point the status bar / vim toggle at `active`, re-observing its
    /// pane so editor changes repaint the status bar, and publish the
    /// pane into [`ActiveHexPane`] so the inspector (which the
    /// workspace has no direct handle to once restored -- see
    /// `panels::inspector`'s module doc) stays in sync too. No-op when
    /// the active file is unchanged.
    ///
    /// `ActiveHexPane` is a single App-level global, so this assumes
    /// one live `Workspace` per process (true today: `main.rs` opens
    /// exactly one window). A second concurrent workspace would have
    /// its inspector hijacked by whichever one last called this.
    fn set_active_file(&mut self, active: Option<Entity<FilePanel>>, cx: &mut Context<Self>) {
        if self.active_file.as_ref().map(Entity::entity_id) == active.as_ref().map(Entity::entity_id) {
            return;
        }
        let pane = active.as_ref().map(|file| file.read(cx).pane().clone());
        self.active_pane_observe = pane.as_ref().map(|pane| cx.observe(pane, |_workspace, _pane, cx| cx.notify()));
        cx.set_global(ActiveHexPane(pane));
        self.active_file = active;
        cx.notify();
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

    fn active_path(&self, cx: &App) -> Option<PathBuf> {
        self.active_file.as_ref().and_then(|file| file.read(cx).path().map(Path::to_path_buf))
    }

    fn render_status_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let file_label = Label::new(status_file_name_text(self.active_path(cx).as_deref()));

        let (offset, mode) = match &self.active_file {
            Some(file) => {
                let file = file.read(cx);
                let editor = file.pane().read(cx).editor();
                let offset = status_offset_text(editor.selection());
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
/// naming each file when few were dropped, or a single count-summary
/// toast past a small threshold so a large stale layout does not spray
/// the notification stack.
fn restore_pruned_texts(pruned: &[PathBuf]) -> Vec<String> {
    if pruned.len() > 2 {
        return vec![hxy_i18n::t_args("gpui-status-restore-dropped-summary", &[("count", &pruned.len().to_string())])];
    }
    pruned
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
}

/// Total file panels anywhere under `state`, used to decide whether the
/// welcome placeholder should show.
fn count_file_panels(state: &PanelState) -> usize {
    let here = usize::from(state.panel_name == FILE_PANEL_NAME);
    here + state.children.iter().map(count_file_panels).sum::<usize>()
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

/// Whether any file panel in a dumped `PanelState` tree has `target` as
/// its path.
fn dump_has_file_path(state: &PanelState, target: &Path) -> bool {
    (state.panel_name == FILE_PANEL_NAME && file_path_from_info(&state.info).as_deref() == Some(target))
        || state.children.iter().any(|child| dump_has_file_path(child, target))
}

fn file_path_from_info(info: &PanelInfo) -> Option<PathBuf> {
    match info {
        PanelInfo::Panel(value) => value.get("path").and_then(|path| path.as_str()).map(PathBuf::from),
        _ => None,
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

/// The file panel backing the active tab, if the active tab is a file.
/// With splits, the first tab container that has an active file wins.
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
            .on_action(cx.listener(Self::on_toggle_vim))
            .on_action(cx.listener(Self::on_toggle_inspector))
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

    fn active_path(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> Option<PathBuf> {
        window.read_with(cx, |ws, cx| ws.active_path(cx)).unwrap()
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
}
