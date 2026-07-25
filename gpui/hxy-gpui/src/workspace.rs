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
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_editor::InputMode;

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

actions!(hxy_gpui, [OpenFile, ToggleVim, ToggleInspector, ToggleSearch, CloseSearch]);

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
    /// Message for the most recent failed open attempt; cleared on the
    /// next successful open.
    open_error: Option<String>,
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
    /// Test-only handle to the inspector panel `ensure_inspector_dock`
    /// creates on fresh construction (never populated on the registry-
    /// restore path -- see that method's doc), so integration tests can
    /// observe the inspector through the real workspace instead of only
    /// through the `ActiveHexPane` global directly.
    #[cfg(test)]
    inspector_for_test: Option<Entity<InspectorPanel>>,
}

impl Workspace {
    /// `initial` is the CLI path argument's already-read bytes, if any;
    /// `layout_path` is where the dock layout persists (injectable for
    /// tests; production passes [`persist::layout_path`]).
    pub fn new(
        initial: Option<(Arc<dyn HexSource>, PathBuf)>,
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

        let mut workspace = Self {
            dock,
            welcome: None,
            active_file: None,
            active_pane_observe: None,
            _dock_subscription: dock_subscription,
            open_error: None,
            last_title: None,
            focus_handle: cx.focus_handle(),
            focus_pending: true,
            needs_reconcile: false,
            open_files: Vec::new(),
            layout_path,
            save_debounce: None,
            _appearance_subscription: appearance_subscription,
            #[cfg(test)]
            inspector_for_test: None,
        };
        workspace.build_initial(initial, window, cx);
        workspace
    }

    /// Populate the dock at construction: a restored layout wins;
    /// otherwise a CLI file becomes the first tab, or nothing (leaving
    /// reconciliation to add the welcome placeholder).
    fn build_initial(&mut self, initial: Option<(Arc<dyn HexSource>, PathBuf)>, window: &mut Window, cx: &mut Context<Self>) {
        let restored = self
            .layout_path
            .as_ref()
            .and_then(|path| persist::load(path))
            .filter(|state| state.version == Some(persist::LAYOUT_VERSION));

        match restored {
            Some(mut state) => {
                persist::prune_for_restore(&mut state);
                if let Err(err) = self.dock.update(cx, |dock, cx| dock.load(state, window, cx)) {
                    tracing::warn!(%err, "load dock layout failed; using default");
                }
                // The load-built cache is accurate; register the restored
                // panels so a later resync can reuse them.
                collect_file_entities(self.dock.read(cx).items(), &mut self.open_files);
            }
            None => {
                if let Some((source, path)) = initial {
                    let panel = cx.new(|cx| FilePanel::new(source, Some(path), window, cx));
                    self.add_file_panel(panel, window, cx);
                }
            }
        }
        // Must run before `reconcile` (which calls `set_active_file`,
        // publishing the boot-time active pane into `ActiveHexPane`):
        // the inspector's global subscription has to be live before that
        // first publish, or a CLI-opened file would show an empty
        // inspector until the user switched tabs.
        self.ensure_inspector_dock(window, cx);
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
    /// (which can pass several paths) and by tests. A path that is
    /// already open does NOT get a second tab: the existing one is
    /// focused instead (standard editor behavior; the egui app does the
    /// same, though it also offers a focus/second-copy/cancel dialog that
    /// is out of scope here). Deduping at the source also keeps the
    /// resync reuse registry one-entry-per-path, so a reopened file can
    /// never lose its live (possibly dirty) entity to a rebuild.
    pub fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(existing) = self.open_file_for_path(&path, cx) {
            self.focus_existing_tab(existing, window, cx);
            self.open_error = None;
            cx.notify();
            return;
        }
        match std::fs::read(&path) {
            Ok(bytes) => {
                let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
                let panel = cx.new(|cx| FilePanel::new(source, Some(path), window, cx));
                self.add_file_panel(panel, window, cx);
                self.open_error = None;
            }
            Err(error) => {
                tracing::error!(?path, %error, "failed to open file");
                self.open_error = Some(status_open_error_text(&path, &error.to_string()));
            }
        }
        cx.notify();
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
        let receiver = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: None });
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
                    workspace.open_error = Some(hxy_i18n::t_args("gpui-status-open-error-dialog", &[("error", &err.to_string())]));
                    cx.notify();
                }
                Err(_) => {
                    // Channel dropped (window closing); nothing to show.
                }
            });
        })
        .detach();
    }

    fn on_toggle_vim(&mut self, _: &ToggleVim, _window: &mut Window, cx: &mut Context<Self>) {
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
            let items: Vec<DockItem> =
                state.children.iter().map(|child| rebuild_item(child, reusable, welcome, dock_area, window, cx)).collect();
            let axis = if *axis == 0 { Axis::Horizontal } else { Axis::Vertical };
            let sizes: Vec<Option<gpui::Pixels>> = sizes.iter().map(|size| Some(*size)).collect();
            DockItem::split_with_sizes(axis, items, sizes, dock_area, window, cx)
        }
        PanelInfo::Tabs { active_index } => {
            let panels: Vec<Arc<dyn PanelView>> =
                state.children.iter().map(|leaf| resolve_leaf(leaf, reusable, welcome, dock_area, window, cx)).collect();
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
            let handle = match &self.active_file {
                Some(file) => file.read(cx).pane().read(cx).focus_handle(cx),
                None => self.focus_handle.clone(),
            };
            window.focus(&handle);
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
            .child(div().flex_1().child(self.dock.clone()));

        if let Some(error) = &self.open_error {
            root = root.child(div().px_3().py_1().text_color(cx.theme().danger).child(error.clone()));
        }

        root = root.child(self.render_status_bar(cx));

        // `gpui_component::Root` (the window's actual top-level view,
        // see `main.rs`) only renders its child; the child is
        // responsible for appending the dialog/sheet/notification
        // layers each frame. Only the dialog layer is needed so far
        // (the in-file search bar's replace-all / length-mismatch
        // confirms); sheet and notification layers are for later tasks.
        root.children(gpui_component::Root::render_dialog_layer(window, cx))
    }
}

#[cfg(test)]
mod tests {
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
        });
    }

    fn source() -> Arc<dyn HexSource> {
        Arc::new(MemorySource::new(vec![0u8; 16]))
    }

    fn temp_file(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn open_workspace(
        cx: &mut TestAppContext,
        initial: Option<(Arc<dyn HexSource>, PathBuf)>,
        layout_path: Option<PathBuf>,
    ) -> WindowHandle<Workspace> {
        let window = cx.add_window(move |window, cx| {
            let subscription = window.observe_window_appearance(|_, _| {});
            Workspace::new(initial, subscription, layout_path, window, cx)
        });
        cx.run_until_parked();
        window
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
        let window = open_workspace(cx, Some((source(), PathBuf::from("test.bin"))), None);

        let pane_handle = window
            .read_with(cx, |ws, cx| ws.active_file.as_ref().unwrap().read(cx).pane().read(cx).focus_handle(cx))
            .unwrap();
        let focused = window.update(cx, |_ws, window, cx| window.focused(cx)).unwrap();
        assert_eq!(focused, Some(pane_handle));
    }

    /// With no file open the welcome placeholder shows and focus rests
    /// on the workspace handle so `cmd-o` stays reachable.
    #[gpui::test]
    fn no_file_shows_welcome_and_focuses_workspace(cx: &mut TestAppContext) {
        setup(cx);
        let window = open_workspace(cx, None, None);

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
        let window = open_workspace(cx, None, None);

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
        let window = open_workspace(cx, None, None);

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
        let welcome_live = window
            .read_with(cx, |ws, cx| {
                count_welcome_panels(&ws.dock.read(cx).dump(cx).center) == 1
            })
            .unwrap();
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
        let window = open_workspace(cx, None, None);
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
                original.update(cx, |tab, cx| tab.add_panel_at(view, gpui_component::Placement::Right, None, window, cx));
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
        let window = open_workspace(cx, None, None);
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
                original.update(cx, |tab, cx| tab.add_panel_at(view, gpui_component::Placement::Right, None, window, cx));
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
        let window = open_workspace(cx, None, None);
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
            .read_with(cx, |ws, cx| ws.open_files.iter().filter(|file| file.read(cx).path() == Some(fa.as_path())).count())
            .unwrap();
        assert_eq!(registered, 1, "one registry entry per open path");
    }

    /// A restored tab whose file has since disappeared is pruned, the
    /// rest are kept, and restore does not crash.
    #[gpui::test]
    fn restore_prunes_missing_file_and_keeps_the_rest(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("layout.json");
        let f1 = temp_file(&dir, "a.bin", &[1u8; 16]);
        let f2 = temp_file(&dir, "b.bin", &[2u8; 16]);

        let first = open_workspace(cx, None, Some(layout.clone()));
        window_open(first, &f1, cx);
        window_open(first, &f2, cx);
        first.read_with(cx, |ws, cx| ws.save_now(cx).unwrap().unwrap()).unwrap();

        std::fs::remove_file(&f1).unwrap();

        let second = open_workspace(cx, None, Some(layout));
        assert_eq!(file_count(second, cx), 1);
        let paths = second.read_with(cx, |ws, cx| collect_file_paths(&ws.dock.read(cx).dump(cx).center)).unwrap();
        assert!(!paths.contains(&f1), "missing file must be pruned");
        assert!(paths.contains(&f2), "readable file must survive");
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

        let window = open_workspace(cx, None, Some(layout.clone()));
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
        assert!(paths.contains(&f1) && paths.contains(&f2) && paths.contains(&f3), "coalesced write must hold the final state");
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

        let first = open_workspace(cx, None, Some(layout.clone()));
        window_open(first, &f1, cx);
        window_open(first, &f2, cx);
        first.read_with(cx, |ws, cx| ws.save_now(cx).unwrap().unwrap()).unwrap();

        let second = open_workspace(cx, None, Some(layout.clone()));
        assert_eq!(file_count(second, cx), 2);
        let restored: Vec<PathBuf> = second
            .read_with(cx, |ws, cx| collect_file_paths(&ws.dock.read(cx).dump(cx).center))
            .unwrap();
        assert!(restored.contains(&f1));
        assert!(restored.contains(&f2));
    }

    /// `cmd-i` opens and closes the inspector dock, exercising the
    /// action and keybinding end to end.
    #[gpui::test]
    fn cmd_i_toggles_the_inspector_dock(cx: &mut TestAppContext) {
        setup(cx);
        let window = open_workspace(cx, None, None);

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

        let first = open_workspace(cx, None, Some(layout.clone()));
        cx.simulate_keystrokes(first.into(), "cmd-i");
        assert!(first.read_with(cx, |ws, cx| ws.dock.read(cx).is_dock_open(DockPlacement::Right, cx)).unwrap());
        first.read_with(cx, |ws, cx| ws.save_now(cx).unwrap().unwrap()).unwrap();

        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&layout).unwrap()).unwrap();
        assert_eq!(value["right_dock"]["open"], serde_json::json!(true));
        assert_eq!(value["right_dock"]["panel"]["children"][0]["panel_name"], serde_json::json!("InspectorPanel"));

        let second = open_workspace(cx, None, Some(layout));
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
        let window = open_workspace(cx, None, None);

        let inspector = window.read_with(cx, |ws, _| ws.inspector_for_test.clone()).unwrap().expect("inspector stashed on fresh construction");

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

    /// An unparseable layout file must not crash startup; the workspace
    /// falls back to the empty (welcome) default.
    #[gpui::test]
    fn corrupt_layout_falls_back_to_welcome(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let layout = dir.path().join("layout.json");
        std::fs::write(&layout, b"{ not valid json").unwrap();

        let window = open_workspace(cx, None, Some(layout));
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

        let first = open_workspace(cx, None, Some(layout.clone()));
        window_open(first, &f1, cx);
        first.read_with(cx, |ws, cx| ws.save_now(cx).unwrap().unwrap()).unwrap();

        let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(&layout).unwrap()).unwrap();
        value["version"] = serde_json::json!(persist::LAYOUT_VERSION + 1);
        std::fs::write(&layout, serde_json::to_vec(&value).unwrap()).unwrap();

        let second = open_workspace(cx, None, Some(layout));
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
        let window = open_workspace(cx, None, None);
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
