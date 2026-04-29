//! Persisted state shared between the UI thread and (on desktop) the
//! synchronous save sink. The only mutable access path goes through
//! [`HxyApp::persist_mut`](crate::app::HxyApp::persist_mut) so every
//! change gets persisted unconditionally.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use hxy_core::ByteRange;
use hxy_core::Selection;
use hxy_vfs::TabSource;
use parking_lot::RwLock;
use serde::Deserialize;
use serde::Serialize;

use crate::settings::AppSettings;
use crate::window::WindowSettings;

/// One template run we want to re-establish on the next app launch.
/// Auto-rerun matches by `source_path` + `range`; `source_fingerprint`
/// is the BLAKE3 of the expanded template source the previous run
/// saw and gates whether `node_color_overrides` get re-applied
/// (mismatched fingerprint = template author edited the file, node
/// indices may have shifted, drop the overrides).
///
/// Defined on every target so [`OpenTabState`] has a stable shape,
/// even though templates only actually execute on non-wasm builds.
/// On wasm the persisted list is just round-tripped untouched.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedTemplateInstance {
    pub source_path: PathBuf,
    pub display_name: String,
    pub range: ByteRange,
    /// `None` for instances persisted before fingerprinting existed,
    /// or for error-only entries that never produced a node tree.
    /// In both cases we restore the run but discard overrides.
    #[serde(default)]
    pub source_fingerprint: Option<[u8; 32]>,
    /// `BTreeMap` for stable JSON key ordering across saves; the keys
    /// are tree-node indices (`TemplateNodeIdx::0`).
    #[serde(default)]
    pub node_color_overrides: BTreeMap<u32, egui::Color32>,
}

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
    /// Templates the user had running on this tab when the session
    /// last saved. Auto-replayed on restore; see
    /// [`PersistedTemplateInstance`] for fingerprint semantics. The
    /// vec is always present in the schema so wasm and desktop tabs
    /// round-trip the same JSON shape; wasm just leaves it empty.
    #[serde(default)]
    pub templates: Vec<PersistedTemplateInstance>,
    /// Index into `templates` of the tab that was active in the
    /// template panel, or `None` if no tab was selected (or no
    /// templates were running). Restored after auto-rerun completes.
    #[serde(default)]
    pub active_template_idx: Option<usize>,
    /// Whether the user explicitly opened this tab's visualizer
    /// panel. Mirrors `OpenFile::visualizer_panel.open`. Defaults
    /// to `false` so the panel only ever appears in response to a
    /// deliberate user action (or a restored "true" from a previous
    /// session); a template emitting visualizer attributes does not,
    /// on its own, pop the panel.
    #[serde(default)]
    pub visualizer_open: bool,
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
    /// [`crate::tabs::persisted_dock::PersistedDock`] so the per-frame
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
