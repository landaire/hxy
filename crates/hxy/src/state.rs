//! Persisted state shared between the UI thread and (on desktop) the
//! synchronous save sink. The only mutable access path goes through
//! [`HxyApp::persist_mut`](crate::app::HxyApp::persist_mut) so every
//! change gets persisted unconditionally.

use std::sync::Arc;

use hxy_core::Selection;
use hxy_vfs::TabSource;
use parking_lot::RwLock;
use serde::Deserialize;
use serde::Serialize;

use crate::settings::AppSettings;
use crate::window::WindowSettings;

/// State for a single tab's open file -- enough to reopen it on launch
/// with the same selection and scroll position. `source` may refer to a
/// plain filesystem file or an entry inside a parent tab's mounted VFS;
/// restore logic topologically sorts parents before children.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpenTabState {
    pub source: TabSource,
    #[serde(default)]
    pub selection: Option<Selection>,
    #[serde(default)]
    pub scroll_offset: f32,
    /// Whether this tab was wrapped in a `Tab::Workspace` (file plus
    /// mounted VFS, nested dock area) instead of a plain `Tab::File`.
    /// Only meaningful for filesystem-rooted tabs whose detected
    /// handler can mount the source. `VfsEntry` children always
    /// restore inside their parent's workspace and ignore this flag.
    #[serde(default)]
    pub as_workspace: bool,
}

#[derive(Clone, Default)]
pub struct PersistedState {
    pub window: WindowSettings,
    pub app: AppSettings,
    pub open_tabs: Vec<OpenTabState>,
    /// User consent decisions for each loaded plugin. Mirrored
    /// into `HxyApp` is unnecessary -- the rest of the app reads /
    /// writes it through the same `state.read()` / `state.write()`
    /// path the rest of [`PersistedState`] uses.
    pub plugin_grants: hxy_plugin_host::PluginGrants,
    /// Cached JSON of the most recent dock-layout snapshot.
    /// Stored as a string rather than the typed
    /// [`crate::persisted_dock::PersistedDock`] so the per-frame
    /// dirty check stays a cheap byte comparison and so the field
    /// carries the wasm-stripped feature gate without
    /// ricocheting through every consumer of [`PersistedState`].
    /// `None` until the host snapshots the dock at least once.
    #[cfg(not(target_arch = "wasm32"))]
    pub dock_layout_json: Option<String>,
    /// Expanded directory paths in each VFS panel, keyed by the
    /// parent source the panel renders. Cleared per-source if the
    /// panel re-opens with no surviving entries -- the format only
    /// records dirs that exist as of the most recent render, so
    /// stale entries fall out automatically when a tree is rebuilt.
    /// Stored as a Vec of pairs rather than a [`HashMap`] because
    /// JSON object keys must be strings and [`TabSource`] is an enum;
    /// the Vec form round-trips through serde without bespoke
    /// serialise / deserialise.
    pub vfs_tree_expanded: Vec<(TabSource, Vec<String>)>,
}

impl PartialEq for PersistedState {
    fn eq(&self, other: &Self) -> bool {
        let base = self.window == other.window
            && self.app == other.app
            && self.open_tabs == other.open_tabs
            && self.plugin_grants == other.plugin_grants
            && self.vfs_tree_expanded == other.vfs_tree_expanded;
        #[cfg(not(target_arch = "wasm32"))]
        {
            base && self.dock_layout_json == other.dock_layout_json
        }
        #[cfg(target_arch = "wasm32")]
        {
            base
        }
    }
}

pub type SharedPersistedState = Arc<RwLock<PersistedState>>;

pub fn shared(state: PersistedState) -> SharedPersistedState {
    Arc::new(RwLock::new(state))
}
