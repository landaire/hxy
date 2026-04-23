//! Persisted state shared between the UI thread and (on desktop) the
//! background save task. Held behind an `Arc<RwLock<>>` so the save task
//! can read without blocking the UI.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::settings::AppSettings;
use crate::window::WindowSettings;

#[derive(Default)]
pub struct PersistedState {
    pub window: WindowSettings,
    pub app: AppSettings,
}

pub type SharedPersistedState = Arc<RwLock<PersistedState>>;

pub fn shared(state: PersistedState) -> SharedPersistedState {
    Arc::new(RwLock::new(state))
}
