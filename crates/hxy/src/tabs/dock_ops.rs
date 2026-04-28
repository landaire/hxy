//! Dock topology helpers: split / merge / move tabs across leaves,
//! find tool / content leaves, classify tabs, push tool tabs into the
//! right-hand panel.

#![cfg(not(target_arch = "wasm32"))]

use crate::app::HxyApp;
use crate::commands::DockDir;
use crate::files::FileId;
use crate::tabs::Tab;

pub fn resolve_target_leaf(app: &mut HxyApp) -> Option<egui_dock::NodePath> {
    if let Some(path) = app.dock.focused_leaf() {
        return Some(path);
    }
    let id = crate::app::active_file_id(app)?;
    let tab = app.dock.find_tab(&Tab::File(id))?;
    Some(egui_dock::NodePath { surface: tab.surface, node: tab.node })
}

/// Split the target leaf in `dir` and seed the new pane with a
/// fresh Welcome placeholder. The new leaf becomes focused so the
/// next file the user opens (or tab they drag in) lands there and
/// replaces the placeholder. Duplicating the focused tab instead
/// would clone its identity (e.g. two `Tab::File(id)` pointing at
/// the same underlying file) and break close-tab semantics.
pub fn dock_split_focused(app: &mut HxyApp, dir: DockDir) {
    let Some(path) = resolve_target_leaf(app) else { return };
    let tree = &mut app.dock[path.surface];
    let [_, new_node] = match dir {
        DockDir::Right => tree.split_right(path.node, 0.5, vec![Tab::Welcome]),
        DockDir::Left => tree.split_left(path.node, 0.5, vec![Tab::Welcome]),
        DockDir::Up => tree.split_above(path.node, 0.5, vec![Tab::Welcome]),
        DockDir::Down => tree.split_below(path.node, 0.5, vec![Tab::Welcome]),
    };
    app.dock.set_focused_node_and_surface(egui_dock::NodePath { surface: path.surface, node: new_node });
}

/// Collapse the target leaf into its neighbour on `dir`: move every
/// tab into the neighbour's leaf and let egui_dock drop the now-
/// empty leaf + collapse the parent split. No-op when there's no
/// neighbour on that side (e.g. merge-left from the leftmost pane).
pub fn dock_merge_focused(app: &mut HxyApp, dir: DockDir) {
    let Some(path) = resolve_target_leaf(app) else { return };
    let tree = &app.dock[path.surface];
    let Some(neighbor_node) = find_neighbor_leaf(tree, path.node, dir) else { return };
    let target = egui_dock::NodePath { surface: path.surface, node: neighbor_node };
    dock_merge_to(app, path, target);
}

/// Pour every tab from `source` into `target`, then remove `source`
/// so the parent split collapses. Operations across surfaces are
/// supported -- each surface's tree is mutated independently. No-op
/// when source equals target or source has no tabs.
pub fn dock_merge_to(app: &mut HxyApp, source: egui_dock::NodePath, target: egui_dock::NodePath) {
    if source == target {
        return;
    }
    let tabs: Vec<_> = match &mut app.dock[source.surface][source.node] {
        egui_dock::Node::Leaf(leaf) => std::mem::take(&mut leaf.tabs),
        _ => return,
    };
    if tabs.is_empty() {
        return;
    }
    let moved_real_tab = tabs.iter().any(|t| !matches!(t, Tab::Welcome));
    let refocus_tab = tabs[0];
    for tab in tabs {
        app.dock[target.surface][target.node].append_tab(tab);
    }
    if moved_real_tab {
        remove_welcome_from_leaf(&mut app.dock, target.surface, target.node);
    }
    app.dock[source.surface].remove_leaf(source.node);
    if let Some(found) = app.dock.find_tab(&refocus_tab) {
        app.dock.set_focused_node_and_surface(egui_dock::NodePath { surface: found.surface, node: found.node });
    }
}

/// Pop just the focused tab from its leaf and append it to the
/// neighbour leaf in `dir`. Sibling tabs stay where they are -- this
/// is the single-tab counterpart to [`dock_merge_focused`]. If the
/// source leaf ends up empty after the move it gets removed the same
/// way merge does so the parent split collapses.
pub fn dock_move_focused_tab(app: &mut HxyApp, dir: DockDir) {
    let Some(path) = resolve_target_leaf(app) else { return };
    let tree = &app.dock[path.surface];
    let Some(neighbor_node) = find_neighbor_leaf(tree, path.node, dir) else { return };
    let target = egui_dock::NodePath { surface: path.surface, node: neighbor_node };
    dock_move_tab_to(app, path, target);
}

/// Move just the source leaf's active tab into `target`. Sibling
/// tabs stay put. If the source leaf ends up empty it's removed so
/// the parent split collapses, the same way [`dock_merge_to`] does
/// when the merge drains the leaf. Cross-surface moves are supported.
pub fn dock_move_tab_to(app: &mut HxyApp, source: egui_dock::NodePath, target: egui_dock::NodePath) {
    if source == target {
        return;
    }
    let moved_tab = match &mut app.dock[source.surface][source.node] {
        egui_dock::Node::Leaf(leaf) => {
            if leaf.tabs.is_empty() {
                return;
            }
            let idx = leaf.active.0.min(leaf.tabs.len().saturating_sub(1));
            let tab = leaf.tabs.remove(idx);
            if !leaf.tabs.is_empty() {
                let new_active = idx.min(leaf.tabs.len() - 1);
                leaf.active = egui_dock::TabIndex(new_active);
            }
            tab
        }
        _ => return,
    };
    let refocus_tab = moved_tab;
    let moved_real_tab = !matches!(moved_tab, Tab::Welcome);
    app.dock[target.surface][target.node].append_tab(moved_tab);
    if moved_real_tab {
        remove_welcome_from_leaf(&mut app.dock, target.surface, target.node);
    }
    let source_empty =
        matches!(&app.dock[source.surface][source.node], egui_dock::Node::Leaf(leaf) if leaf.tabs.is_empty());
    if source_empty {
        app.dock[source.surface].remove_leaf(source.node);
    }
    if let Some(found) = app.dock.find_tab(&refocus_tab) {
        app.dock.set_focused_node_and_surface(egui_dock::NodePath { surface: found.surface, node: found.node });
    }
}

/// Toggle visibility of the right-hand tool panel (the Plugins
/// manager and any plugin mount tabs). When visible, drains every
/// tool-class tab out of the dock into `hidden_tool_tabs`. The
/// now-empty leaf is removed by egui_dock and adjacent panes reflow
/// to take the space. When hidden, recreates the right-split leaf
/// at the standard 28% width and refills it from the stash.
pub fn toggle_tool_panel(app: &mut HxyApp) {
    if !app.hidden_tool_tabs.is_empty() {
        let to_restore = std::mem::take(&mut app.hidden_tool_tabs);
        let mut iter = to_restore.into_iter();
        let Some(first) = iter.next() else { return };
        app.dock.main_surface_mut().split_right(egui_dock::NodeIndex::root(), 0.72, vec![first]);
        if let Some(path) = app.dock.find_tab(&first) {
            for tab in iter {
                if let Ok(leaf) = app.dock.leaf_mut(path.node_path()) {
                    leaf.append_tab(tab);
                }
            }
        }
        return;
    }
    let mut to_hide: Vec<(egui_dock::TabPath, Tab)> = Vec::new();
    for (path, tab) in app.dock.iter_all_tabs() {
        if is_tool_tab(tab) {
            to_hide.push((path, *tab));
        }
    }
    if to_hide.is_empty() {
        return;
    }
    to_hide.sort_by(|a, b| b.0.tab.0.cmp(&a.0.tab.0));
    let stash: Vec<Tab> = to_hide.iter().rev().map(|(_, t)| *t).collect();
    for (path, _) in to_hide {
        let _ = app.dock.remove_tab(path);
    }
    app.hidden_tool_tabs = stash;
}

/// Toggle visibility of the workspace VFS tree sub-tab. Hide just
/// removes `WorkspaceTab::VfsTree` from the workspace's inner dock
/// (the leaf that hosted it auto-cleans if it was the only tab,
/// returning that horizontal slice to the editor + entries leaf).
/// Show re-adds the tree as a fresh left split at the same default
/// fraction we use for new workspaces.
pub fn toggle_workspace_vfs(app: &mut HxyApp) {
    let Some(workspace_id) = crate::app::active_workspace_id(app) else { return };
    let Some(workspace) = app.workspaces.get_mut(&workspace_id) else { return };
    if let Some(path) = workspace.dock.find_tab(&crate::files::WorkspaceTab::VfsTree) {
        let _ = workspace.dock.remove_tab(path);
    } else {
        workspace.dock.main_surface_mut().split_left(
            egui_dock::NodeIndex::root(),
            0.3,
            vec![crate::files::WorkspaceTab::VfsTree],
        );
    }
}

/// Push a freshly-opened VFS entry into the leaf that holds the
/// workspace's `Editor` sub-tab so the entry stacks alongside the
/// parent file rather than landing wherever the user was last
/// clicking (typically the VFS-tree leaf, since the click that
/// triggered the open came from there). The tree stays in its own
/// dedicated leaf as a tool panel.
pub fn push_workspace_entry(workspace: &mut crate::files::Workspace, file_id: FileId) {
    let entry = crate::files::WorkspaceTab::Entry(file_id);
    if let Some(editor_path) = workspace.dock.find_tab(&crate::files::WorkspaceTab::Editor)
        && let Ok(leaf) = workspace.dock.leaf_mut(editor_path.node_path())
    {
        leaf.append_tab(entry);
        return;
    }
    workspace.dock.push_to_focused_leaf(entry);
}

/// Tabs that belong in the right-hand "tool" panel: the plugin
/// manager, live plugin VFS browsers, and the secondary
/// inspector / console / entropy / search panels. The shared
/// trait is "this tab augments the user's main editing area
/// rather than being a primary editing surface" -- a leaf
/// holding only these should never receive a fresh file open.
pub fn is_tool_tab(t: &Tab) -> bool {
    match t {
        Tab::Plugins | Tab::Inspector | Tab::Console => true,
        #[cfg(not(target_arch = "wasm32"))]
        Tab::PluginMount(_) | Tab::SearchResults | Tab::Entropy(_) => true,
        _ => false,
    }
}

/// Tabs that hold the user's main editing surface -- File buffers and
/// the two static placeholders (Welcome, Settings) that share the same
/// leaf with them.
pub fn is_content_tab(t: &Tab) -> bool {
    matches!(t, Tab::File(_) | Tab::Welcome | Tab::Settings)
}

/// First leaf in the dock whose tabs are all tool-class. Used as the
/// destination for plugin tab opens; if no such leaf exists, the
/// caller splits a new one off the right side.
pub fn find_tool_leaf(dock: &egui_dock::DockState<Tab>) -> Option<egui_dock::NodePath> {
    for (path, _tab) in dock.iter_all_tabs() {
        let node_path = path.node_path();
        let Ok(leaf) = dock.leaf(node_path) else { continue };
        if !leaf.tabs.is_empty() && leaf.tabs.iter().all(is_tool_tab) {
            return Some(node_path);
        }
    }
    None
}

/// First leaf whose tabs include any content-class entry. Used as the
/// fallback target for File opens originating from inside a tool
/// panel when `HxyApp::last_content_leaf` is stale or unset.
pub fn find_content_leaf(dock: &egui_dock::DockState<Tab>) -> Option<egui_dock::NodePath> {
    for (path, _tab) in dock.iter_all_tabs() {
        let node_path = path.node_path();
        let Ok(leaf) = dock.leaf(node_path) else { continue };
        if leaf.tabs.iter().any(is_content_tab) {
            return Some(node_path);
        }
    }
    None
}

/// Append `tab` to the dock's tool leaf, creating one with a right
/// split off the main surface root if none exists yet. Activates the
/// new tab in its leaf but does not move keyboard focus -- callers
/// that want focus follow this with `set_focused_node_and_surface`.
pub fn push_tool_tab(dock: &mut egui_dock::DockState<Tab>, tab: Tab) -> egui_dock::NodePath {
    if let Some(node_path) = find_tool_leaf(dock)
        && let Ok(leaf) = dock.leaf_mut(node_path)
    {
        leaf.append_tab(tab);
        return node_path;
    }
    dock.main_surface_mut().split_right(egui_dock::NodeIndex::root(), 0.72, vec![tab]);
    find_tool_leaf(dock).expect("split_right just created a tool leaf")
}

/// Snapshot the currently-focused leaf if it counts as a content
/// leaf. Called after each dock pass so file opens routed via
/// `last_content_leaf` land where the user was last editing.
pub fn track_content_leaf(app: &mut HxyApp) {
    let Some(node_path) = app.dock.focused_leaf() else { return };
    let Ok(leaf) = app.dock.leaf(node_path) else { return };
    if leaf.tabs.iter().any(is_content_tab) {
        app.last_content_leaf = Some(node_path);
    }
}

/// True when the dock's currently-focused leaf holds only
/// tool-class tabs (Inspector, Console, Plugins, Entropy,
/// SearchResults, plugin mounts) -- nothing the user would
/// expect a freshly opened file to land in. The host
/// consults this before pushing a new `Tab::File` so opens
/// invoked while a tool panel is focused get rerouted to
/// the editing area.
pub fn focused_leaf_is_all_tool(app: &HxyApp) -> bool {
    let Some(node_path) = app.dock.focused_leaf() else { return false };
    let Ok(leaf) = app.dock.leaf(node_path) else { return false };
    !leaf.tabs.is_empty() && leaf.tabs.iter().all(is_tool_tab)
}

/// Move dock focus onto the saved `last_content_leaf`, falling back
/// to the first content-bearing leaf in the dock. Used right before
/// `app.open()` from a plugin VFS click so `push_to_focused_leaf`
/// inside `open` lands the new File tab in the editing area.
pub fn focus_content_leaf(app: &mut HxyApp) {
    if let Some(node_path) = app.last_content_leaf
        && app.dock.leaf(node_path).is_ok()
    {
        app.dock.set_focused_node_and_surface(node_path);
        return;
    }
    if let Some(node_path) = find_content_leaf(&app.dock) {
        app.last_content_leaf = Some(node_path);
        app.dock.set_focused_node_and_surface(node_path);
    }
}

pub fn remove_welcome_from_leaf(
    dock: &mut egui_dock::DockState<Tab>,
    surface: egui_dock::SurfaceIndex,
    node: egui_dock::NodeIndex,
) {
    let welcome_idx = match &dock[surface][node] {
        egui_dock::Node::Leaf(leaf) => leaf.tabs.iter().position(|t| matches!(t, Tab::Welcome)),
        _ => None,
    };
    if let Some(idx) = welcome_idx {
        let _ = dock.remove_tab(egui_dock::TabPath { surface, node, tab: egui_dock::TabIndex(idx) });
    }
}

/// Walk up the tree from `current` looking for the nearest ancestor
/// split oriented so `dir` steps across it; then descend the sibling
/// subtree to find a concrete leaf. Returns `None` when no such
/// neighbour exists (current is on the outer edge in `dir`).
pub fn find_neighbor_leaf(
    tree: &egui_dock::Tree<Tab>,
    current: egui_dock::NodeIndex,
    dir: DockDir,
) -> Option<egui_dock::NodeIndex> {
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

pub fn first_leaf_in(tree: &egui_dock::Tree<Tab>, start: egui_dock::NodeIndex) -> Option<egui_dock::NodeIndex> {
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
