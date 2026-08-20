//! [`FilePanel`]: one open file rendered through a [`HexPane`], with an
//! in-file search/replace bar ([`SearchBar`]) that slots in below it,
//! wrapped as a dock [`Panel`] so it can live in a tab and round-trip
//! through layout persistence.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::App;
use gpui::AppContext;
use gpui::ClipboardItem;
use gpui::Context;
use gpui::Entity;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::SharedString;
use gpui::Styled;
use gpui::Subscription;
use gpui::Window;
use gpui::div;
use gpui::prelude::FluentBuilder;
use gpui::px;
use gpui_component::ActiveTheme;
use gpui_component::WindowExt;
use gpui_component::dock::Panel;
use gpui_component::dock::PanelEvent;
use gpui_component::dock::PanelInfo;
use gpui_component::dock::PanelState;
use gpui_component::notification::Notification;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;
use hxy_templates::format::format_template_copy;
use hxy_templates::format::format_template_struct;
use hxy_templates::state::TemplateEvent;
use hxy_templates::state::TemplateInstance;
use hxy_templates::state::TemplateInstanceId;
use hxy_templates::state::TemplateNodeIdx;
use hxy_templates::state::build_visible;
use hxy_templates::state::children_by_parent;
use hxy_templates::state::expand_array;
use hxy_templates::state::recompute_leaf_colors;
use hxy_templates::state::toggle_collapse;
use hxy_templates::state::visible_node_indices;
use hxy_vfs::VfsHandler;
use hxy_view_gpui::HexPane;

use super::search_bar::SearchBar;
use super::template_view::TemplateOffsetJump;
use super::template_view::TemplateView;
use crate::templates::TemplateRunHandle;
use crate::workspace::CloseSearch;
use crate::workspace::ToggleSearch;

/// Height of the template results section under the hex pane. The
/// egui panel defaults to 300 pt (user-resizable there; fixed here
/// until a resizable-dock treatment lands).
const TEMPLATE_PANEL_HEIGHT: f32 = 300.0;

/// Stable identifier for layout (de)serialization; must never change.
pub const FILE_PANEL_NAME: &str = "FilePanel";

pub struct FilePanel {
    pane: Entity<HexPane>,
    path: Option<PathBuf>,
    /// Tab label for a pathless buffer -- set for a VFS entry (its leaf
    /// name) so the tab reads sensibly instead of "Untitled".
    title_override: Option<String>,
    /// VFS handler that claimed this file's header, if any, enabling the
    /// "Browse VFS" command. `None` for a plain file or a VFS entry.
    detected_handler: Option<Arc<dyn VfsHandler>>,
    search: Entity<SearchBar>,
    /// Per-file snapshot store, created lazily on first snapshot access
    /// (only for disk-backed tabs -- a pathless buffer has no stable key
    /// to store sidecars under). Restored from disk so snapshots survive
    /// an app restart. `None` until first touched.
    snapshots: Option<hxy_panels::files::snapshot::SnapshotStore>,
    /// Completed template runs for this tab, in the order the user
    /// kicked them off. Each instance carries the byte range it was
    /// applied to so multiple templates -- on overlapping or disjoint
    /// regions -- can coexist as separate tabs in the template panel.
    pub(crate) templates: Vec<TemplateInstance>,
    /// In-flight parse+execute jobs. Each one lands in
    /// [`Self::templates`] on completion (success or a diagnostics-only
    /// error instance).
    pub(crate) templates_running: Vec<TemplateRunHandle>,
    /// Which template tab is currently selected in the template panel.
    pub(crate) active_template: Option<TemplateInstanceId>,
    /// Counter for handing out fresh template instance ids on this tab.
    pub(crate) next_template_instance_id: u64,
    /// Path of the template most recently run against this tab, so a
    /// reload can re-fire it without asking again.
    pub(crate) last_template_path: Option<PathBuf>,
    /// Whether the per-file template panel section under the pane is
    /// shown. Off until a run starts.
    pub(crate) template_panel_visible: bool,
    /// The template results panel rendered under the pane. Always
    /// constructed; only mounted while [`Self::template_panel_visible`]
    /// and at least one instance (or run) exists.
    template_view: Entity<TemplateView>,
    _template_subs: Vec<Subscription>,
}

impl FilePanel {
    pub fn new(source: Arc<dyn HexSource>, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let pane = cx.new(|cx| HexPane::new(source, cx));
        let search = cx.new(|cx| SearchBar::new(pane.clone(), window, cx));
        let weak = cx.entity().downgrade();
        let template_view = cx.new(|cx| TemplateView::new(weak, window, cx));
        let template_subs = vec![
            cx.subscribe_in(&template_view, window, Self::on_template_event),
            cx.subscribe_in(&template_view, window, Self::on_template_offset_jump),
        ];
        Self {
            pane,
            path,
            title_override: None,
            detected_handler: None,
            search,
            snapshots: None,
            templates: Vec::new(),
            templates_running: Vec::new(),
            active_template: None,
            next_template_instance_id: 1,
            last_template_path: None,
            template_panel_visible: false,
            template_view,
            _template_subs: template_subs,
        }
    }

    /// A file panel over a VFS entry: no on-disk path, but a stable tab
    /// title (the entry's leaf name).
    pub fn new_vfs_entry(
        source: Arc<dyn HexSource>,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self::new(source, None, window, cx);
        panel.title_override = Some(title);
        panel
    }

    /// Record the VFS handler that matched this file's header (enables
    /// the "Browse VFS" command for this tab).
    pub fn set_detected_handler(&mut self, handler: Option<Arc<dyn VfsHandler>>) {
        self.detected_handler = handler;
    }

    /// The VFS handler matching this file, if one was detected.
    pub fn detected_handler(&self) -> Option<Arc<dyn VfsHandler>> {
        self.detected_handler.clone()
    }

    /// Rebuild a panel from persisted [`PanelInfo`]. The path is
    /// re-read from disk; callers prune unreadable paths before restore
    /// (see `persist::prune_for_restore`), so a read failure here is
    /// only a defensive fallback to an empty buffer. VFS-handler
    /// detection is re-run on the re-read bytes exactly as a fresh open
    /// does, so "Browse VFS" stays enabled for a restored zip tab
    /// (mirrors the egui app re-detecting on session restore).
    pub fn restore(info: &PanelInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = path_from_info(info);
        let bytes: Vec<u8> = match &path {
            Some(path) => match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::warn!(?path, %err, "restore: re-read failed; empty buffer");
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        let handler = crate::panels::workspace_host::detect_handler(cx, &bytes[..bytes.len().min(4096)]);
        let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        let mut panel = Self::new(source, path, window, cx);
        panel.detected_handler = handler;
        panel
    }

    pub fn pane(&self) -> &Entity<HexPane> {
        &self.pane
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Re-anchor this panel onto a new on-disk path after a Save As. The
    /// leaf name drives the tab label, so clearing `title_override` lets
    /// the new file name show through (a Save As always lands a real
    /// filesystem file, superseding any VFS-entry title the tab carried).
    pub fn set_path(&mut self, path: PathBuf) {
        self.path = Some(path);
        self.title_override = None;
    }

    /// Whether the buffer has an unsaved patch. Read by the close-tab
    /// guard and by [`Panel::closable`] so a dirty tab can't be closed
    /// without the save prompt.
    pub fn is_dirty(&self, cx: &App) -> bool {
        self.pane.read(cx).editor().is_dirty()
    }

    /// Whether this tab can store snapshots (only disk-backed tabs have a
    /// stable key). Drives the dialog's "no store" message.
    pub fn has_snapshot_store(&self) -> bool {
        self.path.is_some()
    }

    /// Lazily build (or return) this tab's snapshot store, restoring any
    /// on-disk history under the gpui-suffixed snapshot root. `None` for a
    /// pathless buffer -- there is no stable key to store snapshots under.
    fn snapshot_store_mut(&mut self) -> Option<&mut hxy_panels::files::snapshot::SnapshotStore> {
        let path = self.path.clone()?;
        if self.snapshots.is_none() {
            let base = crate::persist::snapshots_base();
            self.snapshots = Some(hxy_panels::files::snapshot::SnapshotStore::restore(base.as_deref(), &path));
        }
        self.snapshots.as_mut()
    }

    /// Capture the current patched bytes as a named snapshot (an empty
    /// name defaults to `Snapshot N`). `None` when the buffer has no
    /// stable key (pathless) or the sidecar write fails (logged).
    pub fn capture_snapshot(&mut self, name: String, cx: &App) -> Option<hxy_panels::files::snapshot::SnapshotId> {
        let bytes = read_all_bytes(&self.pane, cx);
        let store = self.snapshot_store_mut()?;
        match store.capture(name, bytes) {
            Ok(id) => Some(id),
            Err(err) => {
                tracing::warn!(%err, "capture snapshot");
                None
            }
        }
    }

    /// This tab's snapshots, oldest first. Empty when the store has never
    /// been touched or the tab is pathless.
    pub fn snapshots(&self) -> &[hxy_panels::files::snapshot::Snapshot] {
        self.snapshots.as_ref().map(|store| store.snapshots.as_slice()).unwrap_or(&[])
    }

    /// Delete a snapshot and its sidecar bytes (persisted).
    pub fn delete_snapshot(&mut self, id: hxy_panels::files::snapshot::SnapshotId) {
        if let Some(store) = self.snapshot_store_mut() {
            store.delete(id);
        }
    }

    /// Resolve a snapshot's frozen bytes (cache hit or disk read). `None`
    /// on a missing id or a read failure (logged).
    pub fn snapshot_bytes(&self, id: hxy_panels::files::snapshot::SnapshotId) -> Option<Arc<Vec<u8>>> {
        let snap = self.snapshots.as_ref()?.get(id)?;
        match snap.load_bytes() {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                tracing::warn!(%err, "load snapshot bytes");
                None
            }
        }
    }

    /// Allocate a fresh [`TemplateInstanceId`] for a new run on this
    /// tab. Counter is monotonic for the tab's lifetime.
    pub(crate) fn fresh_template_instance_id(&mut self) -> TemplateInstanceId {
        let id = TemplateInstanceId::new(self.next_template_instance_id);
        self.next_template_instance_id += 1;
        id
    }

    /// The currently-selected template instance, if any. Consumers are
    /// the template panel UI and hex-view tinting (M4a Tasks 4-5); the
    /// allow keeps the intermediate state warning-free.
    #[allow(dead_code)]
    pub(crate) fn active_template(&self) -> Option<&TemplateInstance> {
        let id = self.active_template?;
        self.templates.iter().find(|t| t.id == id)
    }

    #[allow(dead_code)]
    pub(crate) fn active_template_mut(&mut self) -> Option<&mut TemplateInstance> {
        let id = self.active_template?;
        self.templates.iter_mut().find(|t| t.id == id)
    }

    /// Insert or replace a template instance under a known id, so a
    /// re-run rebinds into the same tab without disturbing siblings.
    pub(crate) fn upsert_template_instance(&mut self, instance: TemplateInstance) {
        let id = instance.id;
        if let Some(slot) = self.templates.iter_mut().find(|t| t.id == id) {
            *slot = instance;
        } else {
            self.templates.push(instance);
        }
        if self.active_template.is_none() {
            self.active_template = Some(id);
        }
    }

    /// The template results panel entity, for tests asserting on its
    /// cached rows.
    #[cfg(test)]
    pub(crate) fn template_view(&self) -> &Entity<TemplateView> {
        &self.template_view
    }

    fn on_template_event(
        &mut self,
        _view: &Entity<TemplateView>,
        event: &TemplateEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_template_event(event, window, cx);
    }

    /// Diagnostic offset link clicked in the template panel: park the
    /// caret on that byte and scroll it into view.
    fn on_template_offset_jump(
        &mut self,
        _view: &Entity<TemplateView>,
        event: &TemplateOffsetJump,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = event.0;
        self.pane.update(cx, |pane, cx| {
            pane.editor_mut().set_selection(Some(Selection { anchor: offset, cursor: offset }));
            pane.editor_mut().set_scroll_to_byte(offset);
            pane.sync_pending_scroll(cx);
        });
    }

    /// Dispatch one event from the template panel; ports the egui
    /// reducer (`apply_template_event`, `crates/hxy/src/app/mod.rs`).
    /// Every event ends with a row-cache + overlay resync so the
    /// panel and hex view reflect the new state.
    pub(crate) fn apply_template_event(&mut self, event: &TemplateEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.handle_template_event(event, window, cx);
        self.sync_template_rows(cx);
        self.sync_pane_overlays(cx);
        cx.notify();
    }

    fn handle_template_event(&mut self, event: &TemplateEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            TemplateEvent::HidePanel => {
                self.template_panel_visible = false;
            }
            TemplateEvent::SetActive(id) => {
                self.active_template = Some(*id);
            }
            TemplateEvent::RemoveInstance(id) => {
                self.templates.retain(|t| t.id != *id);
                // Dropping a running handle cancels its background task.
                self.templates_running.retain(|r| r.id != *id);
                if self.active_template == Some(*id) {
                    self.active_template =
                        self.templates.first().map(|t| t.id).or_else(|| self.templates_running.first().map(|r| r.id));
                }
            }
            TemplateEvent::ExpandArray { array_id, count } => {
                if let Some(state) = self.active_template_mut().map(|t| &mut t.state) {
                    expand_array(state, *array_id, *count);
                }
            }
            TemplateEvent::ToggleCollapse(idx) => {
                if let Some(state) = self.active_template_mut().map(|t| &mut t.state) {
                    toggle_collapse(state, *idx);
                }
            }
            TemplateEvent::Hover(idx) => {
                if let Some(state) = self.active_template_mut().map(|t| &mut t.state) {
                    state.hovered_node = *idx;
                }
            }
            TemplateEvent::Select(idx) => {
                self.select_template_node(*idx, cx);
            }
            TemplateEvent::Copy { idx, kind } => {
                let Some(state) = self.active_template().map(|t| &t.state) else { return };
                let text = if kind.is_struct() {
                    format_template_struct(&state.tree.nodes, idx.0 as usize, *kind)
                } else if let Some(node) = state.tree.nodes.get(idx.0 as usize) {
                    let source = self.pane.read(cx).editor().source();
                    format_template_copy(source, node, *kind)
                } else {
                    None
                };
                if let Some(text) = text {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            TemplateEvent::SaveBytes(idx) => {
                self.save_template_bytes(*idx, window, cx);
            }
            TemplateEvent::ToggleColors(on) => {
                // Hex-view restyling from this flag lands with the
                // styler composition (M4a Task 5).
                if let Some(state) = self.active_template_mut().map(|t| &mut t.state) {
                    state.show_colors = *on;
                }
            }
            TemplateEvent::SetColor { idx, color } => {
                if let Some(state) = self.active_template_mut().map(|t| &mut t.state) {
                    state.node_color_overrides.insert(idx.0, *color);
                    recompute_leaf_colors(state);
                }
            }
            TemplateEvent::ResetColor(idx) => {
                if let Some(state) = self.active_template_mut().map(|t| &mut t.state) {
                    state.node_color_overrides.remove(&idx.0);
                    recompute_leaf_colors(state);
                }
            }
            TemplateEvent::MoveSelection(delta) => {
                self.move_template_selection(*delta, cx);
            }
            TemplateEvent::CollapseSelected => {
                let Some(state) = self.active_template_mut().map(|t| &mut t.state) else { return };
                if let Some(idx) = state.selected_node {
                    state.collapsed.insert(idx);
                }
            }
            TemplateEvent::ExpandSelected => {
                let Some(state) = self.active_template_mut().map(|t| &mut t.state) else { return };
                if let Some(idx) = state.selected_node {
                    state.collapsed.remove(&idx);
                }
            }
            TemplateEvent::OpenVisualizer(idx) => {
                // Visualizer dock tabs land in M4b; keep the arm
                // handled so the marker click isn't a silent drop.
                tracing::debug!(node = idx.0, "open visualizer requested (not wired until M4b)");
            }
        }
    }

    /// Set the active template's selected row and re-fire the byte
    /// selection / scroll side effects so the hex view jumps to the
    /// field. Shared between row clicks and arrow-key moves (port of
    /// egui's `select_template_node`).
    fn select_template_node(&mut self, idx: TemplateNodeIdx, cx: &mut Context<Self>) {
        let span = {
            let Some(state) = self.active_template_mut().map(|t| &mut t.state) else { return };
            state.selected_node = Some(idx);
            let Some(node) = state.tree.nodes.get(idx.0 as usize) else { return };
            (node.span.offset, node.span.length)
        };
        let (offset, length) = span;
        // Zero-length nodes still park the caret on their offset.
        let end_inclusive = offset.saturating_add(length.max(1) - 1);
        self.pane.update(cx, |pane, cx| {
            pane.editor_mut().set_selection(Some(Selection {
                anchor: ByteOffset::new(offset),
                cursor: ByteOffset::new(end_inclusive),
            }));
            pane.editor_mut().set_scroll_to_byte(ByteOffset::new(offset));
            pane.sync_pending_scroll(cx);
        });
    }

    /// Move the selection by `delta` positions in the flattened
    /// visible row list, skipping non-Node rows (port of egui's
    /// `move_template_selection`).
    fn move_template_selection(&mut self, delta: i32, cx: &mut Context<Self>) {
        let next_idx = {
            let Some(template) = self.active_template() else { return };
            let state = &template.state;
            let Some(current) = state.selected_node else { return };
            let visible = visible_node_indices(state);
            if visible.is_empty() {
                return;
            }
            // A selection collapsed out of view restarts from the top.
            let pos = visible.iter().position(|i| *i == current).unwrap_or_default();
            let next = (pos as i32 + delta).clamp(0, visible.len() as i32 - 1) as usize;
            visible[next]
        };
        self.select_template_node(next_idx, cx);
    }

    /// "Save bytes to file...": read the node's span now (against the
    /// current bytes), then prompt for a destination asynchronously.
    fn save_template_bytes(&mut self, idx: TemplateNodeIdx, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.active_template().map(|t| &t.state) else { return };
        let Some(node) = state.tree.nodes.get(idx.0 as usize).cloned() else { return };
        let start = ByteOffset::new(node.span.offset);
        let end = ByteOffset::new(node.span.offset.saturating_add(node.span.length));
        let Ok(range) = ByteRange::new(start, end) else { return };
        let bytes = match self.pane.read(cx).editor().source().read(range) {
            Ok(bytes) => bytes,
            Err(err) => {
                window.push_notification(
                    Notification::error(hxy_i18n::t_args("template-read-bytes-failed", &[("error", &err.to_string())])),
                    cx,
                );
                return;
            }
        };
        let default_name = format!("{}.bin", hxy_core::copy::sanitize_ident(&node.name));
        cx.spawn_in(window, async move |this, cx| {
            let Some(handle) = rfd::AsyncFileDialog::new().set_file_name(&default_name).save_file().await else {
                return;
            };
            let path = handle.path().to_path_buf();
            if let Err(err) = std::fs::write(&path, &bytes) {
                let _ = this.update_in(cx, |_, window, cx| {
                    window.push_notification(
                        Notification::error(hxy_i18n::t_args(
                            "template-save-failed",
                            &[("path", &path.display().to_string()), ("error", &err.to_string())],
                        )),
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    /// Recompute the active instance's visible-row list and push it
    /// into the template view (which owns no state of its own).
    pub(crate) fn sync_template_rows(&mut self, cx: &mut Context<Self>) {
        let (rows, selected) = match self.active_template() {
            Some(template) => {
                let children = children_by_parent(&template.state.tree.nodes);
                (build_visible(&template.state, &children), template.state.selected_node)
            }
            None => (Vec::new(), None),
        };
        self.template_view.update(cx, |view, cx| view.set_rows(rows, selected, cx));
    }

    /// Mirror template state into the hex pane's overlays. For now
    /// that is the hovered field's highlight band; the byte tints and
    /// palette override join in the styler composition (M4a Task 5).
    pub(crate) fn sync_pane_overlays(&mut self, cx: &mut Context<Self>) {
        let hover = self.active_template().and_then(|template| {
            let idx = template.state.hovered_node?;
            let node = template.state.tree.nodes.get(idx.0 as usize)?;
            let start = node.span.offset;
            let end = start.saturating_add(node.span.length);
            ByteRange::new(ByteOffset::new(start), ByteOffset::new(end)).ok()
        });
        self.pane.update(cx, |pane, cx| pane.set_hover_span(hover, cx));
    }

    /// The tab label: the VFS entry title if set, else the file leaf name,
    /// else the untitled placeholder.
    fn tab_label(&self) -> String {
        match &self.title_override {
            Some(title) => title.clone(),
            None => tab_title(self.path.as_deref()),
        }
    }

    /// `cmd-f`: open the bar (focusing the query field) or, if already
    /// open, close it and hand focus back to the grid.
    fn on_toggle_search(&mut self, _: &ToggleSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |bar, cx| bar.toggle(window, cx));
    }

    /// `escape`, scoped to the search bar's own key context so it only
    /// fires while a search input has focus (see [`SearchBar::render`]'s
    /// `key_context`); the input's own escape handling propagates here
    /// when it doesn't consume the key itself (not `clean_on_escape`).
    fn on_close_search(&mut self, _: &CloseSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |bar, cx| bar.close(window, cx));
    }
}

/// Read a pane's whole patched byte view into an owned buffer (empty on
/// a zero-length source or a read failure, logged). Used to freeze the
/// current bytes for a snapshot capture.
fn read_all_bytes(pane: &Entity<HexPane>, cx: &App) -> Vec<u8> {
    let source = pane.read(cx).editor().source().clone();
    let len = source.len().get();
    if len == 0 {
        return Vec::new();
    }
    let range = match hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len)) {
        Ok(range) => range,
        Err(err) => {
            tracing::warn!(%err, "snapshot: byte range");
            return Vec::new();
        }
    };
    match source.read(range) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(%err, "snapshot: read bytes");
            Vec::new()
        }
    }
}

/// Extract the stored file path from a `FilePanel` payload, if present.
fn path_from_info(info: &PanelInfo) -> Option<PathBuf> {
    let PanelInfo::Panel(value) = info else { return None };
    value.get("path").and_then(|p| p.as_str()).map(PathBuf::from)
}

/// The panel base name shown on its tab: the file's leaf name, falling
/// back to the full path when it has none, or the untitled placeholder
/// when the panel has no path.
fn tab_title(path: Option<&Path>) -> String {
    match path {
        Some(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        None => hxy_i18n::t("gpui-file-untitled"),
    }
}

impl Panel for FilePanel {
    fn panel_name(&self) -> &'static str {
        FILE_PANEL_NAME
    }

    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(self.tab_label())
    }

    /// Same text as `title`, just via the `&self` (non-rendering) path
    /// `Panel::tab_name` provides -- defaults to `None`, which would
    /// otherwise leave every file leaf's pane-picker row unlabeled.
    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(self.tab_label()))
    }

    /// Hide the tab bar's "Close" affordance while the buffer is dirty.
    ///
    /// gpui-component 0.5.1 has no pre-close veto hook (`on_removed` fires
    /// after detach and returns `()`; there is no `can_close`), and the
    /// tab bar's own Close routes straight through `TabPanel`'s
    /// `ClosePanel` action, which the workspace cannot intercept. Gating
    /// `closable` on dirtiness is the only way to stop that path from
    /// discarding unsaved edits with no prompt: a dirty tab can then only
    /// be closed via `cmd-w`, which runs the workspace's save prompt
    /// (`Workspace::close_active_tab`). Clean tabs stay freely closable.
    fn closable(&self, cx: &App) -> bool {
        !self.is_dirty(cx)
    }

    /// Persist the backing path so the tab can be re-opened next launch.
    fn dump(&self, _cx: &App) -> PanelState {
        let mut state = PanelState::new(self);
        let path = self.path.as_ref().map(|p| p.to_string_lossy().into_owned());
        state.info = PanelInfo::panel(serde_json::json!({ "path": path }));
        state
    }
}

impl Focusable for FilePanel {
    /// Delegate focus to the inner [`HexPane`] so the dock focusing the
    /// active panel lands directly on the grid that owns the key
    /// handlers -- typing reaches the editor with no extra click.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.pane.read(cx).focus_handle(cx)
    }
}

impl EventEmitter<PanelEvent> for FilePanel {}

impl Render for FilePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.search.read(cx).is_open();
        let show_templates =
            self.template_panel_visible && (!self.templates.is_empty() || !self.templates_running.is_empty());
        div()
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::on_toggle_search))
            .on_action(cx.listener(Self::on_close_search))
            .child(div().flex_1().min_h_0().child(self.pane.clone()))
            .when(open, |root| root.child(self.search.clone()))
            .when(show_templates, |root| {
                root.child(
                    div()
                        .h(px(TEMPLATE_PANEL_HEIGHT))
                        .flex_none()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .child(self.template_view.clone()),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::workspace::init_keybindings(cx);
        });
    }

    fn source() -> Arc<dyn HexSource> {
        Arc::new(MemorySource::new(vec![0u8; 16]))
    }

    /// Builds a `FilePanel` inside a real `gpui_component::Root` window
    /// (like the production shell does): `InputState`'s focus tracking
    /// -- and thus the search bar's query field -- needs the Root layer
    /// present, not just dialogs.
    fn build(cx: &mut TestAppContext) -> (Entity<FilePanel>, &mut gpui::VisualTestContext) {
        let window = cx.add_window(|window, cx| {
            let panel = cx.new(|cx| FilePanel::new(source(), None, window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<FilePanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        (panel, vcx)
    }

    /// `cmd-f` opens the bar and focuses the query field; `escape`
    /// closes it again and hands focus back to the grid -- the exact
    /// focus flow the search bar's UX depends on.
    #[gpui::test]
    fn cmd_f_opens_and_escape_closes_and_refocuses_grid(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);

        let grid_handle = panel.read_with(cx, |panel, cx| panel.pane.read(cx).focus_handle(cx));
        cx.update(|window, _cx| window.focus(&grid_handle));
        cx.run_until_parked();
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(grid_handle.clone()), "grid starts focused");

        cx.simulate_keystrokes("cmd-f");
        let (is_open, query_handle) =
            panel.read_with(cx, |panel, cx| (panel.search.read(cx).is_open(), panel.search.read(cx).focus_handle(cx)));
        assert!(is_open, "cmd-f opens the search bar");
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(query_handle), "cmd-f focuses the query input");

        cx.simulate_keystrokes("escape");
        let is_open_after = panel.read_with(cx, |panel, cx| panel.search.read(cx).is_open());
        assert!(!is_open_after, "escape closes the search bar");
        assert_eq!(cx.update(|window, cx| window.focused(cx)), Some(grid_handle), "escape refocuses the grid");
    }

    /// A dirty file panel reports `closable() == false` so the tab bar's
    /// own Close (which the workspace cannot veto in gpui-component 0.5.1)
    /// can't silently discard unsaved edits; a clean one stays closable.
    #[gpui::test]
    fn dirty_panel_is_not_closable(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx);
        assert!(panel.read_with(cx, Panel::closable), "a clean buffer is closable");
        panel.update(cx, |panel, cx| {
            panel.pane().update(cx, |pane, cx| {
                pane.editor_mut().splice(0, 1, vec![0xAA]).unwrap();
                cx.notify();
            });
        });
        assert!(!panel.read_with(cx, Panel::closable), "a dirty buffer is not closable");
    }

    /// A restored zip-backed tab re-runs VFS-handler detection on the
    /// re-read bytes, so "Browse VFS" stays enabled across a relaunch
    /// (regression: `restore` previously left `detected_handler` `None`).
    #[gpui::test]
    fn restore_re_detects_the_vfs_handler(cx: &mut TestAppContext) {
        // `crate::panels::register` installs the VFS registry global that
        // detection reads; the plain `setup` above does not.
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::panels::register(cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("fixture.zip");
        std::fs::write(&archive, super::super::vfs_tree::test_support::fixture_zip_bytes()).unwrap();

        let info = PanelInfo::panel(serde_json::json!({ "path": archive.to_string_lossy() }));
        let window = cx.add_window(|window, cx| {
            let panel = cx.new(|cx| FilePanel::restore(&info, window, cx));
            gpui_component::Root::new(panel, window, cx)
        });
        let panel = window.root(cx).unwrap().read_with(cx, |r, _| r.view().clone().downcast::<FilePanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();

        panel.read_with(vcx, |panel, _| {
            assert!(panel.detected_handler().is_some(), "restored zip tab re-detects its handler");
        });
    }
}
