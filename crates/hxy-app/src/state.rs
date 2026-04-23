//! Persisted state shared between the UI thread and (on desktop) the
//! synchronous save sink. The only mutable access path goes through
//! [`HxyApp::persist_mut`](crate::app::HxyApp::persist_mut) so every
//! change gets persisted unconditionally.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::settings::AppSettings;
use crate::window::WindowSettings;

#[derive(Clone, Default, PartialEq)]
pub struct PersistedState {
    pub window: WindowSettings,
    pub app: AppSettings,
}

pub type SharedPersistedState = Arc<RwLock<PersistedState>>;

pub fn shared(state: PersistedState) -> SharedPersistedState {
    Arc::new(RwLock::new(state))
}
