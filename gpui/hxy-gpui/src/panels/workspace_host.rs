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
//!
//! Sweep boundary: the eject sweep covers the inner CENTER dock only. A
//! deliberate scope choice, not a hard limit: the inner layout ships only
//! a left tree dock (owned), and a foreign drop lands in a center tab
//! group in the common case, so the center sweep is the acceptable floor.
//! The symmetric OUTWARD escape (an owned entry dragged out to the outer
//! dock) is likewise left to a future pass.
//!
//! Removal reach: eject requires removing the foreign panel from the inner
//! dock, and gpui-component 0.5.2's only public removal takes a concrete
//! `Entity<P>` (no remove-by-PanelId/Arc, no exposed `TabGroup` to drive a
//! close). The guard recovers that entity by downcasting to `FilePanel` --
//! the type a cross-area drop realistically lands in a VFS workspace and
//! the one the guard test exercises. A foreign panel of any other type is
//! left in the inner dock rather than duplicated across both docks; see
//! `enforce_membership`.

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
use gpui::component::Icon;
use gpui::component::Sizable;
use gpui::component::WindowExt;
use gpui::component::dock::BasePanel;
use gpui::component::dock::BasePanelView;
use gpui::component::dock::DockArea;
use gpui::component::dock::DockSkin;
use gpui::component::dock::DockEvent;
use gpui::component::dock::DockLayout;
use gpui::component::dock::DockPlacement;
use gpui::component::dock::PaneRef;
use gpui::component::dock::Panel;
use gpui::component::dock::PanelEvent;
use gpui::component::dock::PanelHandle;
use gpui::component::dock::PanelInfo;
use gpui::component::dock::PanelState;
use gpui::component::dock::register_panel;
use gpui::component::h_flex;
use gpui::component::notification::Notification;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_vfs::MountedVfs;
use hxy_vfs::VfsCapabilities;
use hxy_vfs::VfsRegistry;

use super::FilePanel;
use super::vfs_tree::VfsTreeEvent;
use super::vfs_tree::VfsTreePanel;
use crate::assets::HxyIcon;

/// Stable identifier for layout (de)serialization; must never change.
pub const WORKSPACE_HOST_PANEL_NAME: &str = "WorkspaceHostPanel";

/// Identity of a plugin-provided VFS mount, when a host wraps one
/// instead of an on-disk archive. Mirrors egui's `TabSource::PluginMount`
/// (plugin name + opaque token + plugin-chosen title). Drives the tab
/// title and marks the dumped layout as a non-restorable plugin mount:
/// the token is a live session handle, so these are pruned on restart
/// rather than re-driven (see `persist::panel_kind`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginMountIdentity {
    pub plugin_name: String,
    pub token: String,
    pub title: String,
}

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

    register_panel(cx, WORKSPACE_HOST_PANEL_NAME, |ctx, window, cx| {
        let outer = ctx.dock_area();
        let info = ctx.info();
        Arc::new(PanelHandle::new(cx.new(|cx| WorkspaceHostPanel::restore(outer, info, window, cx))))
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
    /// `None` for a mount with no on-disk origin (a plugin mount, or a
    /// nested-VFS case).
    parent_path: Option<PathBuf>,
    /// Set when this host wraps a plugin VFS instead of an on-disk
    /// archive. Supplies the tab title and marks the dump non-restorable.
    plugin_mount: Option<PluginMountIdentity>,
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

    /// Build a host over a plugin-provided `mount`. `identity` carries
    /// the plugin name, its opaque mount token, and the tab title the
    /// plugin chose. Unlike an archive host there is no on-disk path, so
    /// this host is not restored across restart (see `dump`).
    pub fn new_plugin_mount(
        outer: WeakEntity<DockArea>,
        mount: Arc<MountedVfs>,
        identity: PluginMountIdentity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut host = Self::build(outer, mount, None, Default::default(), window, cx);
        host.plugin_mount = Some(identity);
        host
    }

    /// Rebuild from persisted `PanelInfo`: re-mount the archive from its
    /// recorded path, restore the tree's expansion set, and re-open every
    /// entry tab from the fresh mount. A mount that no longer reads /
    /// parses (prune should have dropped it first; this is the defensive
    /// path) yields an empty read-only mount plus a warning, never an
    /// `InvalidPanel`.
    pub fn restore(outer: WeakEntity<DockArea>, info: &PanelInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let parent_path = parent_path_from_info(info);
        let mount = match parent_path.as_ref().and_then(|path| remount(path, cx)) {
            Some(mount) => mount,
            None => {
                tracing::warn!(?parent_path, "workspace restore: re-mount failed; empty mount");
                // Warn only when a real archive path was recorded but no
                // longer mounts (corrupt/unparseable) -- a host with no
                // recorded path has nothing to warn about. Deferred because
                // `Root` is not installed during the restoring `dock.load`
                // (same reason as the workspace's own restore toasts).
                if let Some(path) = parent_path.as_ref() {
                    let name = display_name(path);
                    window.defer(cx, move |window, cx| {
                        let text = hxy_i18n::t_args("gpui-status-workspace-mount-failed", &[("name", &name)]);
                        window.push_notification(Notification::warning(text), cx);
                    });
                }
                Arc::new(empty_mount())
            }
        };
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
        let dock = cx.new(|cx| DockArea::new("workspace-inner", Some(1), window, cx).with_renderer(DockSkin::new(cx)));
        let tree = cx.new(|cx| VfsTreePanel::new(mount.clone(), expanded, cx));

        // Seed the inner dock: the tree in the left dock, an empty center
        // split that entry tabs are added into. In the 0.5.2 dock the layout
        // is data and every edit (set_center / add_panel) emits LayoutChanged
        // itself, so the empty split is just the neutral starting shape; the
        // guard's reconcile is driven by those explicit emissions. A fresh
        // left dock opens by default, matching the old `open: true`.
        dock.update(cx, |dock, cx| {
            dock.set_center(DockLayout::h_split(), window, cx);
            let left = DockLayout::tabs().panel_view(Arc::new(PanelHandle::new(tree.clone())), cx);
            dock.set_dock(DockPlacement::Left, left, window, cx);
            dock.set_dock_size(DockPlacement::Left, gpui::px(220.0), window, cx);
        });

        let dock_subscription = cx.subscribe(&dock, |host, _dock, event: &DockEvent, cx| match event {
            DockEvent::LayoutChanged => {
                host.needs_guard = true;
                cx.notify();
            }
            DockEvent::DragDrop { .. } => {}
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
            plugin_mount: None,
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
            // There is no "activate tab" call, so re-adding the panel is what
            // brings a background entry to the front -- add_panel_view merges
            // it into the center group as the active tab. The same
            // remove+re-add workaround `Workspace::focus_existing_tab` uses.
            // The entity (and its `owned` id) is unchanged, so the drag guard
            // still leaves it alone. Re-added through a fresh PanelHandle so
            // the tab keeps its skin title (a bare entity would draw only its
            // panel_name).
            self.dock.update(cx, |dock, cx| {
                dock.remove_panel(panel.clone(), window, cx);
                dock.add_panel_view(Arc::new(PanelHandle::new(panel.clone())), DockPlacement::Center, None, window, cx);
            });
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
        // A VFS entry can only be persisted through the mount's writer;
        // without one (every mount today -- the zip handler mounts
        // READ_ONLY with `writer: None`, and plugin writers arrive M4),
        // there is nowhere to save an edit to. Open it read-only so the
        // buffer can't be dirtied and silently lost on host-tab close or
        // quit -- parity with egui, whose `save_vfs_entry_in_place`
        // rejects a writerless mount. Writer-bearing mounts (future) keep
        // the default mutable mode for the M4 in-place writeback path.
        if self.mount.writer.is_none() {
            let pane = panel.read(cx).pane().clone();
            pane.update(cx, |pane, cx| {
                pane.editor_mut().set_edit_mode(hxy_editor::EditMode::Readonly);
                cx.notify();
            });
        }
        self.owned.insert(panel.entity_id());
        self.entries.push(EntryTab { vfs_path, panel: panel.downgrade(), virtual_base });
        self.dock.update(cx, |dock, cx| {
            dock.add_panel_view(Arc::new(PanelHandle::new(panel.clone())), DockPlacement::Center, None, window, cx)
        });
    }

    /// Eject every inner-center panel this host does not own back to the
    /// outer dock, returning the display names of what was ejected (for
    /// the caller's toasts). This is the reactive half of the cross-area
    /// drag guard: a tab dragged in from the outer dock lands in the
    /// inner center, is detected here as un-owned, and is moved back.
    /// Returns an empty vec in the common case (nothing foreign).
    pub fn enforce_membership(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<String> {
        // The 0.5.2 layout is immutable data, so read the center tree and
        // resolve each tab group's active PanelId back to a live view. A
        // foreign tab dropped in from the outer dock becomes the active tab
        // of its target group, so walking active panels surfaces it; a
        // foreign panel left as a background tab is caught the moment it is
        // activated (that emits its own `LayoutChanged`).
        let mut active = Vec::new();
        collect_active_panels(self.dock.read(cx), cx, &mut active);
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
            // Ejecting removes the panel from the inner dock and re-homes it
            // in the outer one. 0.5.2's `DockArea::remove_panel` takes a
            // concrete `Entity<P>`, and there is no public remove-by-PanelId
            // or remove-by-Arc (`remove_panel_id` is private, and no live
            // `TabGroup` handle is exposed to drive a close), so removal needs
            // the panel's concrete entity. It is recovered by downcasting the
            // leaf's view to `FilePanel` -- the type a cross-area drop
            // realistically lands here and the one the guard test exercises.
            // A foreign panel of any other type cannot be removed through the
            // public API, so it is left in the inner dock (never duplicated
            // across both docks); see the module note.
            let Ok(file) = leaf.view().downcast::<FilePanel>() else {
                tracing::warn!(
                    panel = leaf.panel_name(cx),
                    "workspace guard: foreign non-FilePanel center tab cannot be ejected \
                     (gpui-component 0.5.2 has no public remove-by-id); left in the inner dock",
                );
                continue;
            };
            let name = file.read(cx).path().map(display_name).unwrap_or_else(|| leaf.panel_name(cx).to_string());
            self.dock.update(cx, |dock, cx| dock.remove_panel(file.clone(), window, cx));
            outer.update(cx, |dock, cx| {
                dock.add_panel_view(Arc::new(PanelHandle::new(file.clone())), DockPlacement::Center, None, window, cx)
            });
            ejected.push(name);
        }
        ejected
    }

    /// The tab title: the plugin-chosen title for a plugin mount (a
    /// localized fallback when the plugin left it empty), otherwise the
    /// archive's file name, falling back to a localized "Workspace".
    /// The plugin's title text is plugin-authored and passes through
    /// untranslated (parity with plugin command labels).
    fn title_text(&self) -> String {
        if let Some(identity) = &self.plugin_mount {
            return if identity.title.is_empty() {
                hxy_i18n::t("gpui-plugin-mount-untitled")
            } else {
                identity.title.clone()
            };
        }
        self.parent_path.as_ref().map(|p| display_name(p)).unwrap_or_else(|| hxy_i18n::t("gpui-workspace-tab-untitled"))
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

    /// The live `FilePanel` behind an open entry tab (tests inspecting its
    /// edit mode / dirty state).
    #[cfg(test)]
    pub fn entry_panel(&self, vfs_path: &str) -> Option<Entity<FilePanel>> {
        self.entries.iter().find(|e| e.vfs_path == vfs_path).and_then(|e| e.panel.upgrade())
    }
}

impl BasePanel for WorkspaceHostPanel {
    fn panel_name(&self) -> &'static str {
        WORKSPACE_HOST_PANEL_NAME
    }

    /// Refuse to close the host tab while any inner entry tab is dirty, so
    /// the tab bar's own Close (no pre-close veto in gpui-component) can't
    /// discard unsaved entry edits with the whole nested dock. Moot for
    /// today's writerless mounts (entries open read-only via `open_entry`,
    /// so they never go dirty), but defense in depth for the M4
    /// writer-bearing mounts whose entries will be mutable.
    fn closable(&self, cx: &App) -> bool {
        !self.entries.iter().any(|e| e.panel.upgrade().is_some_and(|p| p.read(cx).is_dirty(cx)))
    }

    /// Hand-compose the inner layout: the archive's re-mount path, the
    /// tree's expansion set, the open entry paths, and their virtual-base
    /// hints. `DockArea::dump`/`load` know nothing about this nesting, so
    /// the host owns every byte of it (see the verdict's exercise (b)).
    fn dump(&self, cx: &App) -> PanelState {
        let mut state = PanelState::new(self.panel_name());
        // A plugin mount is a live session (its token is not re-drivable
        // offline), so the dump records only a marker; `persist::prune_for_restore`
        // drops it before load rather than re-mounting -- the documented
        // "plugin mounts do not survive restart" choice.
        if let Some(identity) = &self.plugin_mount {
            state.info = PanelInfo::panel(serde_json::json!({
                "plugin_mount": {
                    "plugin_name": identity.plugin_name,
                    "token": identity.token,
                    "title": identity.title,
                },
            }));
            return state;
        }
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

impl Panel for WorkspaceHostPanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // House prefix mirrors egui's workspace-root icon (inner entry
        // tabs stay unprefixed). Shows only in single-panel title-bar
        // mode -- the multi-tab TabBar renders `tab_name` text, which
        // has no icon slot (gpui-component 0.5.1).
        h_flex()
            .gap_1()
            .items_center()
            .child(Icon::new(HxyIcon::House).small())
            .child(SharedString::from(self.title_text()))
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(self.title_text()))
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

/// Collect the live ACTIVE panel of every tab container in `area`'s center
/// tree. 0.5.2's layout is immutable data (`PaneTree`), so this walks the
/// tree and resolves each tab group's active `PanelId` back to its live view
/// via `DockArea::panel`. A freshly dropped-in foreign tab -- which becomes
/// the active tab of its group -- is included. The inner dock never uses a
/// tiles canvas, so only tab groups are inspected.
fn collect_active_panels(area: &DockArea, _cx: &App, out: &mut Vec<Arc<dyn BasePanelView>>) {
    let Some(tree) = area.layout(DockPlacement::Center) else { return };
    tree.root().walk(&mut |node| {
        let PaneRef::Tabs { panels, active_ix } = node.kind() else { return };
        let Some(id) = panels.get(active_ix).copied() else { return };
        if let Some(view) = area.panel(id) {
            out.push(view.clone());
        }
    });
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
    use gpui::component::Root;
    use gpui::component::dock::DockAreaState;

    use super::super::vfs_tree::test_support::fixture_zip_bytes;
    use super::super::vfs_tree::test_support::mount_fixture;
    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui::component::init(cx);
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
            let view: Arc<dyn BasePanelView> = Arc::new(PanelHandle::new(host));
            outer.update(cx, |dock, cx| dock.add_panel_view(view, DockPlacement::Center, None, window, cx));
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
            collect_active_panels(outer.read(cx), cx, &mut active);
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

    /// Re-activating a background entry brings it to the front. gpui-
    /// component 0.5.1 has no "activate tab", so `open_entry` re-adds the
    /// panel through the inner dock (the same remove+re-add workaround the
    /// outer workspace uses); a bare `window.focus` would leave it behind
    /// its front sibling.
    #[gpui::test]
    fn reactivating_a_background_entry_makes_it_the_front_tab(cx: &mut TestAppContext) {
        setup(cx);
        let (_outer, host, vcx) = build(cx, mount_fixture(), None);
        let tree = host.read_with(vcx, |host, _| host.tree().clone());
        tree.update(vcx, |tree, cx| tree.activate_file("/top.txt".to_string(), cx));
        vcx.run_until_parked();
        tree.update(vcx, |tree, cx| tree.activate_file("/dir/nested.bin".to_string(), cx));
        vcx.run_until_parked();

        let top = host.read_with(vcx, |host, _| host.entry_panel("/top.txt")).expect("top open");
        let nested = host.read_with(vcx, |host, _| host.entry_panel("/dir/nested.bin")).expect("nested open");

        let active_ids = |vcx: &mut VisualTestContext| {
            host.read_with(vcx, |host, cx| {
                let mut active = Vec::new();
                collect_active_panels(host.inner_dock().read(cx), cx, &mut active);
                active.iter().map(|p| p.view().entity_id()).collect::<Vec<_>>()
            })
        };
        assert!(active_ids(vcx).contains(&nested.entity_id()), "the just-opened entry is the front tab");

        // Re-activate the background entry: it must become the front tab.
        tree.update(vcx, |tree, cx| tree.activate_file("/top.txt".to_string(), cx));
        vcx.run_until_parked();
        let ids = active_ids(vcx);
        assert!(ids.contains(&top.entity_id()), "re-activating the background entry brings it front: {ids:?}");
        host.read_with(vcx, |host, _| assert_eq!(host.entry_paths().len(), 2, "no duplicate tab was created"));
    }

    /// A VFS entry from a writerless mount (the zip handler mounts
    /// READ_ONLY with no writer) opens read-only, so it can't be dirtied
    /// and silently lost -- there is nowhere to persist an edit to.
    #[gpui::test]
    fn vfs_entry_from_writerless_mount_opens_read_only(cx: &mut TestAppContext) {
        setup(cx);
        let mount = mount_fixture();
        assert!(mount.writer.is_none(), "the zip fixture mount has no writer");
        let (_outer, host, vcx) = build(cx, mount, None);

        let tree = host.read_with(vcx, |host, _| host.tree().clone());
        tree.update(vcx, |tree, cx| tree.activate_file("/top.txt".to_string(), cx));
        vcx.run_until_parked();

        let entry = host.read_with(vcx, |host, _| host.entry_panel("/top.txt")).expect("entry tab open");
        entry.read_with(vcx, |entry, cx| {
            assert_eq!(
                entry.pane().read(cx).editor().edit_mode(),
                hxy_editor::EditMode::Readonly,
                "a writerless VFS entry opens read-only",
            );
            assert!(!entry.is_dirty(cx), "a read-only entry starts clean");
        });
    }

    /// The host tab refuses to close while an inner entry tab is dirty
    /// (defense in depth for future writer-bearing mounts). Forced mutable
    /// here since today's entries open read-only.
    #[gpui::test]
    fn host_is_not_closable_while_an_inner_entry_is_dirty(cx: &mut TestAppContext) {
        setup(cx);
        let (_outer, host, vcx) = build(cx, mount_fixture(), None);
        assert!(host.read_with(vcx, BasePanel::closable), "an empty host is closable");

        let tree = host.read_with(vcx, |host, _| host.tree().clone());
        tree.update(vcx, |tree, cx| tree.activate_file("/top.txt".to_string(), cx));
        vcx.run_until_parked();
        assert!(host.read_with(vcx, BasePanel::closable), "a clean entry keeps the host closable");

        // Force the entry mutable and dirty it (simulating a future
        // writer-bearing mount's editable entry).
        let entry = host.read_with(vcx, |host, _| host.entry_panel("/top.txt")).expect("entry open");
        let pane = entry.read_with(vcx, |entry, _| entry.pane().clone());
        pane.update(vcx, |pane, cx| {
            pane.editor_mut().set_edit_mode(hxy_editor::EditMode::Mutable);
            pane.editor_mut().splice(0, 1, vec![0xFF]).unwrap();
            cx.notify();
        });
        assert!(!host.read_with(vcx, BasePanel::closable), "a dirty inner entry makes the host non-closable");
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

    /// A restore whose recorded archive path still reads but no longer
    /// mounts (corrupt/unparseable) falls back to an empty read-only mount
    /// and warns the user, rather than coming back silently empty.
    #[gpui::test]
    fn restore_with_unmountable_archive_warns_and_falls_back(cx: &mut TestAppContext) {
        setup(cx);
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("not-really.zip");
        std::fs::write(&bogus, b"this is not a zip archive").unwrap();
        let info = PanelInfo::panel(serde_json::json!({ "parent_path": bogus.to_string_lossy() }));

        let window = cx.add_window(move |window, cx| {
            let outer = cx.new(|cx| DockArea::new("outer", None, window, cx));
            let host = cx.new(|cx| WorkspaceHostPanel::restore(outer.downgrade(), &info, window, cx));
            let view: Arc<dyn BasePanelView> = Arc::new(PanelHandle::new(host));
            outer.update(cx, |dock, cx| dock.add_panel_view(view, DockPlacement::Center, None, window, cx));
            Root::new(outer, window, cx)
        });
        let vcx = VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();

        let toasts = vcx.update(|window, cx| window.notifications(cx).len());
        assert_eq!(toasts, 1, "an unmountable archive surfaces a warning toast");
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
        let view: Arc<dyn BasePanelView> = Arc::new(PanelHandle::new(foreign));
        vcx.update(|window, cx| {
            inner.update(cx, |dock, cx| dock.add_panel_view(view, DockPlacement::Center, None, window, cx));
        });
        vcx.run_until_parked();

        // The reactive guard ran: the foreign tab is gone from the inner
        // center and now lives in the outer dock.
        let inner_has_foreign = host.read_with(vcx, |host, cx| {
            let mut active = Vec::new();
            collect_active_panels(host.inner_dock().read(cx), cx, &mut active);
            active.iter().any(|p| p.view().entity_id() == foreign_id)
        });
        assert!(!inner_has_foreign, "foreign panel ejected from the inner center");
        assert_eq!(outer_center_file_count(&outer, vcx), 1, "foreign file tab moved back to the outer dock");
    }
}
