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

/// State for a single tab's open file — enough to reopen it on launch
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
}

#[derive(Clone, Default, PartialEq)]
pub struct PersistedState {
    pub window: WindowSettings,
    pub app: AppSettings,
    pub open_tabs: Vec<OpenTabState>,
}

pub type SharedPersistedState = Arc<RwLock<PersistedState>>;

pub fn shared(state: PersistedState) -> SharedPersistedState {
    Arc::new(RwLock::new(state))
}
