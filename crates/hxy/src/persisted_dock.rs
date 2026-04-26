//! Save/restore wire format for the dock layout.
//!
//! [`crate::tabs::Tab`] and [`crate::file::WorkspaceTab`] reference
//! ephemeral [`FileId`] / [`WorkspaceId`] / [`MountId`] values that are
//! re-allocated each launch, so serialising them directly produces
//! gibberish on restore. This module mirrors them with stable
//! identifiers (a [`TabSource`] for files / workspaces, the plugin
//! name + token for mounts) and provides translation helpers in both
//! directions.
//!
//! The wrapper [`PersistedDock`] also carries a schema version so
//! incompatible saves are rejected and the host falls back to its
//! default layout instead of panicking on a stale blob.
//!
//! Translation uses [`egui_dock::DockState::filter_map_tabs`], which
//! preserves splits / fractions / focus / window state and only
//! visits the tab leaves -- so layout fidelity is bounded only by
//! whether each leaf maps to something we can resolve at restore.
//! Tabs that don't (file deleted, plugin uninstalled) are silently
//! dropped from the layout but the surrounding structure survives.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;

use egui_dock::DockState;
use hxy_vfs::TabSource;
use serde::Deserialize;
use serde::Serialize;

use crate::compare::CompareId;
use crate::file::FileId;
use crate::file::MountId;
use crate::file::WorkspaceId;
use crate::file::WorkspaceTab;
use crate::tabs::Tab;

/// Bumped whenever the on-disk shape of [`PersistedDock`] changes in a
/// way that older code can't deserialise. Restoring a snapshot whose
/// version doesn't match this constant is treated the same as having
/// no saved layout -- the host builds a default dock from the
/// per-tab list in [`crate::state::PersistedState::open_tabs`].
pub const SCHEMA_VERSION: u32 = 1;

/// Stable, restartable counterpart to [`Tab`]. Variants without a
/// dynamic identifier (Welcome, Settings, ...) translate verbatim;
/// variants that reference a runtime-allocated id swap it for the
/// underlying source identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PersistedTab {
    Welcome,
    Settings,
    Console,
    Inspector,
    Plugins,
    SearchResults,
    /// File tab, keyed by its [`TabSource`] -- the same identity used
    /// in [`crate::state::OpenTabState`], so we can look up whichever
    /// [`FileId`] was allocated for it during the open-tabs restore.
    File(TabSource),
    /// Workspace tab, keyed by the parent file's [`TabSource`]. The
    /// inner dock layout is stored separately on
    /// [`PersistedDock::workspace_layouts`] under the same key.
    Workspace(TabSource),
    /// Plugin VFS mount tab. Carries the plugin name + token used to
    /// remount, plus a display title for the placeholder if the
    /// remount fails on restart.
    PluginMount {
        plugin_name: String,
        token: String,
        title: String,
    },
    /// Side-by-side byte-diff tab. Both sides are keyed by their
    /// originating [`TabSource`] so the host can re-read fresh
    /// bytes from each on restart and spawn a new
    /// [`crate::compare::CompareSession`].
    Compare {
        a: TabSource,
        b: TabSource,
    },
}

/// Stable counterpart to [`WorkspaceTab`]. Same idea: keep the
/// singletons literal, swap the runtime [`FileId`] in `Entry` for a
/// [`TabSource`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PersistedWorkspaceTab {
    Editor,
    VfsTree,
    Entry(TabSource),
}

/// One full snapshot of every dock the host owns: the outer tab tree
/// and one inner tree per live workspace. Versioned so a future
/// shape change can be rejected cleanly.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedDock {
    pub schema_version: u32,
    pub outer: DockState<PersistedTab>,
    /// Inner-dock layout per workspace, keyed by the workspace's
    /// parent file [`TabSource`]. Stored as a `Vec` of pairs (not
    /// a `HashMap`) because JSON object keys must be strings and
    /// [`TabSource`] is an enum -- the round-trip would fail at
    /// serialise time. Entries are dropped on restore if no live
    /// workspace ends up matching the key.
    pub workspace_layouts: Vec<(TabSource, DockState<PersistedWorkspaceTab>)>,
}

/// Reverse maps the host needs while restoring: stable identifier ->
/// freshly-allocated runtime id. Built up by the open-tabs restore
/// pass *before* the dock is rebuilt; passed into
/// [`persisted_to_live`] so each [`PersistedTab`] leaf can resolve.
pub struct RestoreMaps<'a> {
    pub files_by_source: &'a HashMap<TabSource, FileId>,
    pub workspaces_by_parent: &'a HashMap<TabSource, WorkspaceId>,
    pub mounts_by_token: &'a HashMap<(String, String), MountId>,
    pub compares_by_sources: &'a HashMap<(TabSource, TabSource), CompareId>,
}

/// Snapshot the live outer dock plus every workspace's inner dock
/// into the persisted form. Tabs that can't be resolved to a stable
/// identifier (e.g. a [`Tab::File`] whose [`crate::file::OpenFile`]
/// has no [`TabSource`] -- transient anonymous buffers we don't want
/// to persist) are filtered out; everything else round-trips.
pub fn live_to_persisted(
    outer: &DockState<Tab>,
    workspaces: &std::collections::BTreeMap<WorkspaceId, crate::file::Workspace>,
    files: &HashMap<FileId, crate::file::OpenFile>,
    mounts: &std::collections::BTreeMap<MountId, crate::file::MountedPlugin>,
    compares: &std::collections::BTreeMap<CompareId, crate::compare::CompareSession>,
) -> PersistedDock {
    let outer_persisted = outer.filter_map_tabs(|tab| live_to_persisted_tab(tab, workspaces, files, mounts, compares));
    let mut workspace_layouts: Vec<(TabSource, DockState<PersistedWorkspaceTab>)> = Vec::new();
    for ws in workspaces.values() {
        let Some(parent_source) = files.get(&ws.editor_id).and_then(|f| f.source_kind.clone()) else {
            // Workspace whose parent file is anonymous / sourceless --
            // not persistable, skip.
            continue;
        };
        let inner = ws.dock.filter_map_tabs(|t| live_to_persisted_workspace_tab(t, files));
        workspace_layouts.push((parent_source, inner));
    }
    PersistedDock { schema_version: SCHEMA_VERSION, outer: outer_persisted, workspace_layouts }
}

/// Translate a single live [`Tab`] leaf into its persisted form.
/// Returns `None` for tabs we don't want to restore (e.g. anonymous
/// buffers without a [`TabSource`]).
fn live_to_persisted_tab(
    tab: &Tab,
    workspaces: &std::collections::BTreeMap<WorkspaceId, crate::file::Workspace>,
    files: &HashMap<FileId, crate::file::OpenFile>,
    mounts: &std::collections::BTreeMap<MountId, crate::file::MountedPlugin>,
    compares: &std::collections::BTreeMap<CompareId, crate::compare::CompareSession>,
) -> Option<PersistedTab> {
    Some(match tab {
        Tab::Welcome => PersistedTab::Welcome,
        Tab::Settings => PersistedTab::Settings,
        Tab::Console => PersistedTab::Console,
        Tab::Inspector => PersistedTab::Inspector,
        Tab::Plugins => PersistedTab::Plugins,
        Tab::SearchResults => PersistedTab::SearchResults,
        Tab::File(id) => PersistedTab::File(files.get(id)?.source_kind.clone()?),
        Tab::Workspace(id) => {
            let ws = workspaces.get(id)?;
            let parent = files.get(&ws.editor_id)?.source_kind.clone()?;
            PersistedTab::Workspace(parent)
        }
        Tab::PluginMount(id) => {
            let m = mounts.get(id)?;
            PersistedTab::PluginMount {
                plugin_name: m.plugin_name.clone(),
                token: m.token.clone(),
                title: m.display_name.clone(),
            }
        }
        Tab::Compare(id) => {
            // Pin the compare to its two source identities so the
            // host can reread bytes from disk / VFS on restart and
            // respawn a fresh session. Anonymous-buffer panes
            // (no TabSource) drop the whole tab from the layout.
            let session = compares.get(id)?;
            let a = session.a.source.clone()?;
            let b = session.b.source.clone()?;
            PersistedTab::Compare { a, b }
        }
    })
}

fn live_to_persisted_workspace_tab(
    tab: &WorkspaceTab,
    files: &HashMap<FileId, crate::file::OpenFile>,
) -> Option<PersistedWorkspaceTab> {
    Some(match tab {
        WorkspaceTab::Editor => PersistedWorkspaceTab::Editor,
        WorkspaceTab::VfsTree => PersistedWorkspaceTab::VfsTree,
        WorkspaceTab::Entry(id) => PersistedWorkspaceTab::Entry(files.get(id)?.source_kind.clone()?),
    })
}

/// Translate the persisted snapshot back into live dock state. Tabs
/// whose stable identifier no longer maps (file deleted, plugin
/// uninstalled, mount failed to remount) are dropped -- the
/// surrounding splits / sizes / focus survive.
pub fn persisted_to_live(
    snapshot: &PersistedDock,
    maps: &RestoreMaps<'_>,
) -> (DockState<Tab>, HashMap<WorkspaceId, DockState<WorkspaceTab>>) {
    let outer = snapshot.outer.filter_map_tabs(|t| persisted_to_live_tab(t, maps));
    let mut inner_by_id: HashMap<WorkspaceId, DockState<WorkspaceTab>> = HashMap::new();
    for (parent, layout) in &snapshot.workspace_layouts {
        let Some(ws_id) = maps.workspaces_by_parent.get(parent) else {
            continue;
        };
        let live = layout.filter_map_tabs(|t| persisted_to_live_workspace_tab(t, maps));
        inner_by_id.insert(*ws_id, live);
    }
    (outer, inner_by_id)
}

fn persisted_to_live_tab(tab: &PersistedTab, maps: &RestoreMaps<'_>) -> Option<Tab> {
    Some(match tab {
        PersistedTab::Welcome => Tab::Welcome,
        PersistedTab::Settings => Tab::Settings,
        PersistedTab::Console => Tab::Console,
        PersistedTab::Inspector => Tab::Inspector,
        PersistedTab::Plugins => Tab::Plugins,
        PersistedTab::SearchResults => Tab::SearchResults,
        PersistedTab::File(source) => Tab::File(*maps.files_by_source.get(source)?),
        PersistedTab::Workspace(parent) => Tab::Workspace(*maps.workspaces_by_parent.get(parent)?),
        PersistedTab::PluginMount { plugin_name, token, .. } => {
            let key = (plugin_name.clone(), token.clone());
            Tab::PluginMount(*maps.mounts_by_token.get(&key)?)
        }
        PersistedTab::Compare { a, b } => {
            let key = (a.clone(), b.clone());
            Tab::Compare(*maps.compares_by_sources.get(&key)?)
        }
    })
}

fn persisted_to_live_workspace_tab(tab: &PersistedWorkspaceTab, maps: &RestoreMaps<'_>) -> Option<WorkspaceTab> {
    Some(match tab {
        PersistedWorkspaceTab::Editor => WorkspaceTab::Editor,
        PersistedWorkspaceTab::VfsTree => WorkspaceTab::VfsTree,
        PersistedWorkspaceTab::Entry(source) => WorkspaceTab::Entry(*maps.files_by_source.get(source)?),
    })
}
