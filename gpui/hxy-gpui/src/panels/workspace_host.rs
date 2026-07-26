//! [`WorkspaceHostPanel`]: a dock [`Panel`] that wraps and owns its own
//! inner [`DockArea`], turning a VFS mount into a single outer tab that
//! contains a full nested workspace -- a [`VfsTreePanel`] in the inner
//! left dock plus one editor tab per entry opened from it.
//!
//! Promoted from the M2 nested-dock spike. The mechanism the spike
//! validated -- a `Panel` childing an `Entity<DockArea>`, with the inner
//! layout hand-composed into the wrapper's [`PanelInfo::Panel`] json --
//! is used here verbatim; see
//! `docs/superpowers/plans/2026-07-25-m2-nested-dock-verdict.md`.
//!
//! Cross-area drag guard (verdict section "Recommended M3+ approach",
//! option 1 "wrapper with guards"): gpui-component 0.5.1's
//! `TabPanel::on_drop` has no dock-area identity check, so a tab dragged
//! from the OUTER dock into this inner dock is mechanically accepted.
//! The host closes that gap reactively: it subscribes to its inner
//! dock's `LayoutChanged`, and on the next reconcile ejects any center
//! panel it does not own (i.e. anything but the entries it opened) back
//! to the outer dock with a warning toast. This is the verdict's
//! detect-and-eject correction; it is app-level and forks no upstream
//! code.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use gpui::App;
use gpui::AppContext;
use gpui::Context;
use gpui::Entity;
use gpui::EntityId;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::Global;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::SharedString;
use gpui::Styled;
use gpui::Subscription;
use gpui::WeakEntity;
use gpui::Window;
use gpui::div;
use gpui_component::WindowExt;
use gpui_component::dock::DockArea;
use gpui_component::dock::DockEvent;
use gpui_component::dock::DockItem;
use gpui_component::dock::DockPlacement;
use gpui_component::dock::Panel;
use gpui_component::dock::PanelEvent;
use gpui_component::dock::PanelInfo;
use gpui_component::dock::PanelState;
use gpui_component::dock::PanelView;
use gpui_component::dock::register_panel;
use gpui_component::notification::Notification;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_vfs::MountedVfs;
use hxy_vfs::VfsCapabilities;
use hxy_vfs::VfsRegistry;

use super::FilePanel;
use super::vfs_tree::VfsTreeEvent;
use super::vfs_tree::VfsTreePanel;

/// Stable identifier for layout (de)serialization; must never change.
pub const WORKSPACE_HOST_PANEL_NAME: &str = "WorkspaceHostPanel";

/// The app-wide VFS handler set (zip today, wasm plugins later), stashed
/// as a gpui global so both the open flow's `detect` and a layout
/// restore's re-mount reach the same handlers.
pub struct VfsRegistryGlobal(pub VfsRegistry);

impl Global for VfsRegistryGlobal {}

/// Detect the VFS handler that claims `head`, or `None` if none matches
/// (or the registry global has not been installed). Used by the file-open
/// flow to enable the "Browse VFS" command.
pub fn detect_handler(cx: &App, head: &[u8]) -> Option<Arc<dyn hxy_vfs::VfsHandler>> {
    cx.try_global::<VfsRegistryGlobal>()?.0.detect(head)
}

/// Install the VFS registry global and register the host panel's
/// restore closure. Called once from `panels::register`.
pub fn register(cx: &mut App) {
    let mut registry = VfsRegistry::new();
    registry.register(Arc::new(hxy_vfs::handlers::ZipHandler::new()));
    cx.set_global(VfsRegistryGlobal(registry));

    register_panel(cx, WORKSPACE_HOST_PANEL_NAME, |outer, _state, info, window, cx| {
        Box::new(cx.new(|cx| WorkspaceHostPanel::restore(outer, info, window, cx))) as Box<dyn PanelView>
    });
}

/// One editor tab opened from the VFS tree, tracked so the host can
/// dedup re-opens, know which inner center panels are legitimately its
/// own (the drag guard), and re-open them all on restore.
struct EntryTab {
    /// The VFS path this tab shows (leading slash).
    vfs_path: String,
    /// Weak on purpose: the inner dock owns the panel (a strong
    /// `Arc<dyn PanelView>`), so once the user closes the tab via the tab
    /// bar the entity drops and `upgrade()` returns `None`. That is how a
    /// closed entry is distinguished from a live one for dedup, dump, and
    /// pruning without the host having to observe every close itself.
    panel: WeakEntity<FilePanel>,
    /// Load address the mount reports for these bytes, if any. Recorded
    /// for M4's "treat addresses as virtual" affordance; stored, never
    /// rendered yet.
    virtual_base: Option<u64>,
}

pub struct WorkspaceHostPanel {
    focus_handle: FocusHandle,
    /// The inner dock: `VfsTreePanel` in the left dock, entry editor tabs
    /// in the center.
    dock: Entity<DockArea>,
    /// The outer dock this host is a tab of -- the drag guard ejects
    /// foreign panels back into it.
    outer_dock: WeakEntity<DockArea>,
    tree: Entity<VfsTreePanel>,
    mount: Arc<MountedVfs>,
    /// Filesystem path of the mounted archive, for the restore re-mount.
    /// `None` for a mount with no on-disk origin (none today; kept for
    /// the M4 plugin-mount and nested-VFS cases).
    parent_path: Option<PathBuf>,
    entries: Vec<EntryTab>,
    /// Entity ids of panels this host legitimately owns in its inner
    /// center (its entries). The drag guard treats any other center
    /// panel as foreign.
    owned: HashSet<EntityId>,
    /// Set on inner `LayoutChanged`; consumed by `render`, which defers
    /// the guard sweep to just after the frame (where a `&mut Window` is
    /// available and `Root` is installed for the toast).
    needs_guard: bool,
    _dock_subscription: Subscription,
    _tree_subscription: Subscription,
}

impl WorkspaceHostPanel {
    /// Build a fresh host over `mount`. `outer` is the dock this host
    /// will be a tab of (for the guard's eject-back path); `parent_path`
    /// is the archive's on-disk path, recorded so restore can re-mount.
    pub fn new(
        outer: WeakEntity<DockArea>,
        mount: Arc<MountedVfs>,
        parent_path: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(outer, mount, parent_path, Default::default(), window, cx)
    }

    /// Rebuild from persisted `PanelInfo`: re-mount the archive from its
    /// recorded path, restore the tree's expansion set, and re-open every
    /// entry tab from the fresh mount. A mount that no longer reads /
    /// parses (prune should have dropped it first; this is the defensive
    /// path) yields an empty read-only mount plus a warning, never an
    /// `InvalidPanel`.
    pub fn restore(outer: WeakEntity<DockArea>, info: &PanelInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let parent_path = parent_path_from_info(info);
        let mount = parent_path.as_ref().and_then(|path| remount(path, cx)).unwrap_or_else(|| {
            tracing::warn!(?parent_path, "workspace restore: re-mount failed; empty mount");
            Arc::new(empty_mount())
        });
        let expanded = expanded_from_info(info);
        let mut host = Self::build(outer, mount, parent_path, expanded, window, cx);
        let virtual_bases = virtual_bases_from_info(info);
        for vfs_path in entries_from_info(info) {
            let hint = virtual_bases.get(&vfs_path).copied();
            host.open_entry(vfs_path, hint, window, cx);
        }
        host
    }

    fn build(
        outer: WeakEntity<DockArea>,
        mount: Arc<MountedVfs>,
        parent_path: Option<PathBuf>,
        expanded: std::collections::BTreeSet<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let dock = cx.new(|cx| DockArea::new("workspace-inner", Some(1), window, cx));
        let tree = cx.new(|cx| VfsTreePanel::new(mount.clone(), expanded, cx));

        // Seed the inner dock: tree on the left, an empty center split
        // that entry tabs are added into. A bare center Tabs never emits
        // LayoutChanged (see the m2 notes), so start from an empty Split.
        let weak = dock.downgrade();
        let tree_item = DockItem::tabs(vec![Arc::new(tree.clone()) as Arc<dyn PanelView>], &weak, window, cx);
        let center = DockItem::split(gpui::Axis::Horizontal, vec![], &weak, window, cx);
        dock.update(cx, |dock, cx| {
            dock.set_center(center, window, cx);
            dock.set_left_dock(tree_item, Some(gpui::px(220.0)), true, window, cx);
        });

        let dock_subscription = cx.subscribe(&dock, |host, _dock, event: &DockEvent, cx| match event {
            DockEvent::LayoutChanged => {
                host.needs_guard = true;
                cx.notify();
            }
            DockEvent::DragDrop(_) => {}
        });
        let tree_subscription = cx.subscribe_in(&tree, window, Self::on_tree_event);

        // The tree is owned too: it is not registered with the global
        // PanelRegistry (it needs the live mount), so ejecting it into the
        // outer dock would persist an unrebuildable leaf (InvalidPanel on
        // restore). Marking it owned makes the guard leave it alone if a
        // user ever drags it out of the left dock into the inner center.
        let mut owned = HashSet::new();
        owned.insert(tree.entity_id());

        Self {
            focus_handle: cx.focus_handle(),
            dock,
            outer_dock: outer,
            tree,
            mount,
            parent_path,
            entries: Vec::new(),
            owned,
            needs_guard: false,
            _dock_subscription: dock_subscription,
            _tree_subscription: tree_subscription,
        }
    }

    fn on_tree_event(
        &mut self,
        _tree: &Entity<VfsTreePanel>,
        event: &VfsTreeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            VfsTreeEvent::OpenEntry(path) => self.open_entry(path.clone(), None, window, cx),
        }
    }

    /// Open (or focus) an editor tab for the VFS entry at `vfs_path`.
    /// Reads the whole entry from the mount into a `MemorySource` (M3),
    /// records the mount's virtual-base hint for M4, and adds the tab to
    /// the inner center dock.
    pub fn open_entry(
        &mut self,
        vfs_path: String,
        virtual_base_hint: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Drop any entries the user has since closed via the tab bar
        // (their weak handle no longer upgrades), so a closed entry both
        // stops being persisted and can be reopened from the tree.
        self.entries.retain(|e| e.panel.upgrade().is_some());
        if let Some(panel) = self.entries.iter().find(|e| e.vfs_path == vfs_path).and_then(|e| e.panel.upgrade()) {
            let handle = panel.read(cx).focus_handle(cx);
            window.focus(&handle);
            return;
        }
        let bytes = match read_entry(&self.mount, &vfs_path) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(entry = %vfs_path, %err, "workspace: read entry failed");
                return;
            }
        };
        let virtual_base =
            virtual_base_hint.or_else(|| self.mount.virtual_base.as_ref().and_then(|q| q.virtual_base(&vfs_path)));
        let name = leaf_name(&vfs_path);
        let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        let panel = cx.new(|cx| FilePanel::new_vfs_entry(source, name, window, cx));
        self.owned.insert(panel.entity_id());
        self.entries.push(EntryTab { vfs_path, panel: panel.downgrade(), virtual_base });
        let view: Arc<dyn PanelView> = Arc::new(panel);
        self.dock.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
    }

    /// Eject every inner-center panel this host does not own back to the
    /// outer dock, returning the display names of what was ejected (for
    /// the caller's toasts). This is the reactive half of the cross-area
    /// drag guard: a tab dragged in from the outer dock lands in the
    /// inner center, is detected here as un-owned, and is moved back.
    /// Returns an empty vec in the common case (nothing foreign).
    pub fn enforce_membership(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<String> {
        // gpui-component's `TabPanel` exposes only its ACTIVE panel, not
        // its whole list, and the cached `DockItem::Tabs.items` vec is
        // stale after an incremental drop (see the m2 notes). A foreign
        // tab dropped in from the outer dock becomes the active tab of
        // its target `TabPanel`, so walking each center tab container's
        // active panel is what reliably surfaces it; a foreign panel left
        // as a background tab is caught the moment it is activated (that
        // emits its own `LayoutChanged`).
        let mut active = Vec::new();
        collect_active_panels(self.dock.read(cx).items(), cx, &mut active);
        // If the outer dock is gone (window teardown) there is nowhere to
        // move a foreign panel to, so leave it in place rather than
        // destroy it by removing it with no re-home.
        let Some(outer) = self.outer_dock.upgrade() else { return Vec::new() };
        let mut ejected = Vec::new();
        for leaf in active {
            let id = leaf.view().entity_id();
            if self.owned.contains(&id) {
                continue;
            }
            let name = leaf
                .view()
                .downcast::<FilePanel>()
                .ok()
                .and_then(|f| f.read(cx).path().map(display_name))
                .unwrap_or_else(|| leaf.panel_name(cx).to_string());
            self.dock.update(cx, |dock, cx| dock.remove_panel(leaf.clone(), DockPlacement::Center, window, cx));
            outer.update(cx, |dock, cx| dock.add_panel(leaf.clone(), DockPlacement::Center, None, window, cx));
            ejected.push(name);
        }
        ejected
    }

    /// The inner dock area, for tests inspecting the nested layout.
    #[cfg(test)]
    pub fn inner_dock(&self) -> &Entity<DockArea> {
        &self.dock
    }

    /// The VFS tree panel, for tests driving activation / expansion.
    #[cfg(test)]
    pub fn tree(&self) -> &Entity<VfsTreePanel> {
        &self.tree
    }

    /// The VFS paths of every still-open entry tab, in open order (tests).
    #[cfg(test)]
    pub fn entry_paths(&self) -> Vec<String> {
        self.entries.iter().filter(|e| e.panel.upgrade().is_some()).map(|e| e.vfs_path.clone()).collect()
    }

    /// The whole in-memory buffer of an open entry tab, for tests
    /// asserting the correct bytes were read from the mount.
    #[cfg(test)]
    pub fn entry_bytes(&self, vfs_path: &str, cx: &App) -> Option<Vec<u8>> {
        let entry = self.entries.iter().find(|e| e.vfs_path == vfs_path)?;
        let panel = entry.panel.upgrade()?;
        let source = panel.read(cx).pane().read(cx).editor().source().clone();
        let len = source.len().get();
        if len == 0 {
            return Some(Vec::new());
        }
        let range = hxy_core::ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len)).ok()?;
        source.read(range).ok()
    }
}

impl Panel for WorkspaceHostPanel {
    fn panel_name(&self) -> &'static str {
        WORKSPACE_HOST_PANEL_NAME
    }

    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let text = self
            .parent_path
            .as_ref()
            .map(|p| display_name(p))
            .unwrap_or_else(|| hxy_i18n::t("gpui-workspace-tab-untitled"));
        SharedString::from(text)
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        let text = self
            .parent_path
            .as_ref()
            .map(|p| display_name(p))
            .unwrap_or_else(|| hxy_i18n::t("gpui-workspace-tab-untitled"));
        Some(SharedString::from(text))
    }

    /// Hand-compose the inner layout: the archive's re-mount path, the
    /// tree's expansion set, the open entry paths, and their virtual-base
    /// hints. `DockArea::dump`/`load` know nothing about this nesting, so
    /// the host owns every byte of it (see the verdict's exercise (b)).
    fn dump(&self, cx: &App) -> PanelState {
        let mut state = PanelState::new(self);
        let expanded: Vec<String> = self.tree.read(cx).expanded().iter().cloned().collect();
        // Only persist entries whose tab is still open (a closed one's
        // weak handle no longer upgrades), so a closed entry does not come
        // back on the next restore.
        let live: Vec<&EntryTab> = self.entries.iter().filter(|e| e.panel.upgrade().is_some()).collect();
        let entries: Vec<String> = live.iter().map(|e| e.vfs_path.clone()).collect();
        let virtual_bases: serde_json::Map<String, serde_json::Value> = live
            .iter()
            .filter_map(|e| e.virtual_base.map(|base| (e.vfs_path.clone(), serde_json::json!(base))))
            .collect();
        let parent = self.parent_path.as_ref().map(|p| p.to_string_lossy().into_owned());
        state.info = PanelInfo::panel(serde_json::json!({
            "parent_path": parent,
            "expanded": expanded,
            "entries": entries,
            "virtual_bases": virtual_bases,
        }));
        state
    }
}

impl Focusable for WorkspaceHostPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for WorkspaceHostPanel {}

impl Render for WorkspaceHostPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.needs_guard {
            self.needs_guard = false;
            let this = cx.entity().downgrade();
            window.defer(cx, move |window, cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |host, cx| {
                        let ejected = host.enforce_membership(window, cx);
                        for name in ejected {
                            let text = hxy_i18n::t_args("gpui-workspace-ejected-foreign-tab", &[("tab", &name)]);
                            window.push_notification(Notification::warning(text), cx);
                        }
                    });
                }
            });
        }
        div().track_focus(&self.focus_handle).size_full().child(self.dock.clone())
    }
}

/// Read a whole VFS entry into an owned buffer (M3 opens entries as
/// `MemorySource`; streaming lands with M4's plugin mounts).
fn read_entry(mount: &MountedVfs, path: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut file = mount.fs.open_file(path).map_err(|e| std::io::Error::other(format!("open {path}: {e}")))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Re-mount the archive at `path` through the global registry. `None`
/// when the file no longer reads or no handler claims it.
fn remount(path: &PathBuf, cx: &App) -> Option<Arc<MountedVfs>> {
    let bytes = std::fs::read(path).ok()?;
    let handler = detect_handler(cx, &bytes)?;
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
    match handler.mount(source) {
        Ok(mount) => Some(Arc::new(mount)),
        Err(err) => {
            tracing::warn!(?path, %err, "workspace restore: mount failed");
            None
        }
    }
}

/// An empty read-only mount, used as the defensive fallback when a
/// restore re-mount fails (prune should keep this unreachable).
fn empty_mount() -> MountedVfs {
    MountedVfs {
        fs: Box::new(hxy_vfs::vfs::MemoryFS::new()),
        capabilities: VfsCapabilities::READ_ONLY,
        writer: None,
        virtual_base: None,
    }
}

/// The last non-empty segment of a VFS path (its leaf name).
fn leaf_name(vfs_path: &str) -> String {
    vfs_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(vfs_path).to_string()
}

/// The file name of a filesystem path, falling back to the full display.
fn display_name(path: &std::path::Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

/// Collect the live ACTIVE panel of every tab container in a `DockItem`
/// tree. Reads through the `TabPanel` entity (never the stale cache), so
/// a freshly dropped-in foreign tab -- which becomes active on drop -- is
/// included.
fn collect_active_panels(item: &DockItem, cx: &App, out: &mut Vec<Arc<dyn PanelView>>) {
    match item {
        DockItem::Split { items, .. } => items.iter().for_each(|item| collect_active_panels(item, cx, out)),
        DockItem::Tabs { view, .. } => {
            if let Some(active) = view.read(cx).active_panel(cx) {
                out.push(active);
            }
        }
        DockItem::Panel { view, .. } => out.push(view.clone()),
        DockItem::Tiles { .. } => {}
    }
}

fn parent_path_from_info(info: &PanelInfo) -> Option<PathBuf> {
    let PanelInfo::Panel(value) = info else { return None };
    value.get("parent_path").and_then(|p| p.as_str()).map(PathBuf::from)
}

fn expanded_from_info(info: &PanelInfo) -> std::collections::BTreeSet<String> {
    let PanelInfo::Panel(value) = info else { return Default::default() };
    value
        .get("expanded")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

fn entries_from_info(info: &PanelInfo) -> Vec<String> {
    let PanelInfo::Panel(value) = info else { return Vec::new() };
    value
        .get("entries")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

fn virtual_bases_from_info(info: &PanelInfo) -> HashMap<String, u64> {
    let PanelInfo::Panel(value) = info else { return HashMap::new() };
    value
        .get("virtual_bases")
        .and_then(|v| v.as_object())
        .map(|map| map.iter().filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n))).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use gpui::Entity;
    use gpui::TestAppContext;
    use gpui::VisualTestContext;
    use gpui_component::Root;
    use gpui_component::dock::DockAreaState;

    use super::super::vfs_tree::test_support::fixture_zip_bytes;
    use super::super::vfs_tree::test_support::mount_fixture;
    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::panels::register(cx);
        });
    }

    /// Mount an on-disk archive so a restore re-mount can read it back.
    fn mount_path(path: &std::path::Path) -> Arc<MountedVfs> {
        let bytes = std::fs::read(path).unwrap();
        let handler = {
            let mut registry = VfsRegistry::new();
            registry.register(Arc::new(hxy_vfs::handlers::ZipHandler::new()));
            registry.detect(&bytes).unwrap()
        };
        let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        Arc::new(handler.mount(source).unwrap())
    }

    /// Build a host as the single center tab of a real outer `DockArea`,
    /// wrapped in a `Root` (needed for the guard's eject toast). Returns
    /// the outer dock, the host, and a live visual context.
    fn build(
        cx: &mut TestAppContext,
        mount: Arc<MountedVfs>,
        parent_path: Option<PathBuf>,
    ) -> (Entity<DockArea>, Entity<WorkspaceHostPanel>, &mut VisualTestContext) {
        let window = cx.add_window(move |window, cx| {
            let outer = cx.new(|cx| DockArea::new("outer", None, window, cx));
            let host = cx.new(|cx| WorkspaceHostPanel::new(outer.downgrade(), mount, parent_path, window, cx));
            let view: Arc<dyn PanelView> = Arc::new(host);
            outer.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
            Root::new(outer, window, cx)
        });
        let root = window.root(cx).unwrap();
        let outer = root.read_with(cx, |root, _| root.view().clone().downcast::<DockArea>().unwrap());
        let vcx = VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        let host = fetch_host(&outer, vcx).expect("host is the outer center tab");
        (outer, host, vcx)
    }

    fn fetch_host(outer: &Entity<DockArea>, cx: &mut VisualTestContext) -> Option<Entity<WorkspaceHostPanel>> {
        cx.update(|_window, cx| {
            let mut active = Vec::new();
            collect_active_panels(outer.read(cx).items(), cx, &mut active);
            active.into_iter().find_map(|p| p.view().downcast::<WorkspaceHostPanel>().ok())
        })
    }

    fn outer_center_file_count(outer: &Entity<DockArea>, cx: &mut VisualTestContext) -> usize {
        fn walk(state: &PanelState) -> usize {
            let here = usize::from(state.panel_name == crate::panels::FILE_PANEL_NAME);
            here + state.children.iter().map(walk).sum::<usize>()
        }
        cx.update(|_window, cx| walk(&outer.read(cx).dump(cx).center))
    }

    /// Activating a file entry in the tree opens an editor tab in the
    /// inner dock whose bytes are exactly the entry's decompressed
    /// contents.
    #[gpui::test]
    fn activating_an_entry_opens_an_inner_tab_with_the_entry_bytes(cx: &mut TestAppContext) {
        setup(cx);
        let (_outer, host, vcx) = build(cx, mount_fixture(), None);

        let tree = host.read_with(vcx, |host, _| host.tree().clone());
        tree.update(vcx, |tree, cx| tree.activate_file("/top.txt".to_string(), cx));
        vcx.run_until_parked();

        host.read_with(vcx, |host, cx| {
            assert_eq!(host.entry_paths(), vec!["/top.txt".to_string()], "entry tab opened");
            assert_eq!(host.entry_bytes("/top.txt", cx).as_deref(), Some(&b"hello top"[..]), "correct entry bytes");
        });

        // Re-activating the same entry focuses, does not duplicate.
        tree.update(vcx, |tree, cx| tree.activate_file("/top.txt".to_string(), cx));
        vcx.run_until_parked();
        host.read_with(vcx, |host, _| assert_eq!(host.entry_paths().len(), 1, "re-activation does not duplicate"));
    }

    /// A host with an open entry and an expanded directory round-trips
    /// through the outer dock's JSON: dump -> string -> load rebuilds the
    /// host (re-mounting from the recorded archive path), re-opens the
    /// entry with its bytes, and restores the tree's expansion.
    #[gpui::test]
    fn dump_load_round_trips_inner_layout_and_expansion(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("fixture.zip");
        std::fs::write(&archive, fixture_zip_bytes()).unwrap();

        let (outer, host, vcx) = build(cx, mount_path(&archive), Some(archive.clone()));

        // Expand /dir and open one entry.
        let tree = host.read_with(vcx, |host, _| host.tree().clone());
        tree.update(vcx, |tree, cx| tree.toggle_dir("/dir".to_string(), cx));
        tree.update(vcx, |tree, cx| tree.activate_file("/dir/nested.bin".to_string(), cx));
        vcx.run_until_parked();

        let state: DockAreaState = outer.read_with(vcx, |outer, cx| outer.dump(cx));
        let json = serde_json::to_string(&state).unwrap();
        let reloaded: DockAreaState = serde_json::from_str(&json).unwrap();

        let window2 = cx.add_window(|window, cx| {
            let outer2 = cx.new(|cx| DockArea::new("outer2", None, window, cx));
            outer2.update(cx, |dock, cx| dock.load(reloaded, window, cx).expect("outer load succeeds"));
            Root::new(outer2, window, cx)
        });
        let root2 = window2.root(cx).unwrap();
        let outer2 = root2.read_with(cx, |root, _| root.view().clone().downcast::<DockArea>().unwrap());
        let vcx2 = VisualTestContext::from_window(*window2, cx).into_mut();
        vcx2.run_until_parked();

        let host2 = fetch_host(&outer2, vcx2).expect("host rebuilt from layout");
        host2.read_with(vcx2, |host, cx| {
            assert_eq!(host.entry_paths(), vec!["/dir/nested.bin".to_string()], "entry tab restored");
            assert_eq!(
                host.entry_bytes("/dir/nested.bin", cx).as_deref(),
                Some(&b"nested bytes here"[..]),
                "restored entry has correct bytes from the re-mount",
            );
            let expanded: BTreeSet<String> = host.tree().read(cx).expanded().clone();
            assert!(expanded.contains("/dir"), "expansion set restored");
        });
    }

    /// Cross-area drag guard (verdict's detect-and-eject): a foreign
    /// panel that lands in the inner center -- synthesized by adding a
    /// plain `FilePanel` straight into the inner dock, the state a
    /// cross-area drop leaves behind -- is moved back to the outer dock on
    /// the next reconcile, leaving the inner workspace with only its own
    /// entries.
    #[gpui::test]
    fn guard_ejects_a_foreign_center_panel_back_to_the_outer_dock(cx: &mut TestAppContext) {
        setup(cx);
        let (outer, host, vcx) = build(cx, mount_fixture(), None);
        assert_eq!(outer_center_file_count(&outer, vcx), 0, "outer center holds only the host, no file tabs yet");

        // A foreign file tab, as a cross-area drop would leave it: living
        // in the inner center but owned by nobody in the host's registry.
        let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![1u8, 2, 3, 4]));
        let foreign = vcx.update(|window, cx| cx.new(|cx| FilePanel::new(source, None, window, cx)));
        let foreign_id = foreign.entity_id();
        let inner = host.read_with(vcx, |host, _| host.inner_dock().clone());
        let view: Arc<dyn PanelView> = Arc::new(foreign);
        vcx.update(|window, cx| {
            inner.update(cx, |dock, cx| dock.add_panel(view, DockPlacement::Center, None, window, cx));
        });
        vcx.run_until_parked();

        // The reactive guard ran: the foreign tab is gone from the inner
        // center and now lives in the outer dock.
        let inner_has_foreign = host.read_with(vcx, |host, cx| {
            let mut active = Vec::new();
            collect_active_panels(host.inner_dock().read(cx).items(), cx, &mut active);
            active.iter().any(|p| p.view().entity_id() == foreign_id)
        });
        assert!(!inner_has_foreign, "foreign panel ejected from the inner center");
        assert_eq!(outer_center_file_count(&outer, vcx), 1, "foreign file tab moved back to the outer dock");
    }
}
