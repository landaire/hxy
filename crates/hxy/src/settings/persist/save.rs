//! Whole-state save sink: thin wrapper over the shared
//! [`hxy_settings::persist::SaveSink`] that persists every
//! [`PersistedState`] piece in one blocking call. Callers invoke
//! [`SaveSink::save`] after any mutation; there is no timer, no
//! debounce, no background task.

use std::sync::Arc;

use hxy_settings::persist::PersistResult;
use sqlx::SqlitePool;
use tokio::runtime::Runtime;

use crate::settings::persist::store_app_settings;
use crate::settings::persist::store_dock_layout;
use crate::settings::persist::store_open_tabs;
use crate::settings::persist::store_plugin_grants;
use crate::settings::persist::store_vfs_tree_expanded;
use crate::settings::persist::store_window_settings;
use crate::state::PersistedState;

pub struct SaveSink {
    inner: hxy_settings::persist::SaveSink,
}

impl SaveSink {
    pub fn new(pool: SqlitePool, runtime: Arc<Runtime>) -> Self {
        Self { inner: hxy_settings::persist::SaveSink::new(pool, runtime) }
    }

    /// Persist the whole state. Blocks the calling thread on the tokio
    /// runtime's executor until the writes complete (SQLite WAL writes
    /// are sub-millisecond for our tiny key/value payloads).
    pub fn save(&self, state: &PersistedState) -> PersistResult<()> {
        let pool = self.inner.pool().clone();
        let window = state.window;
        let app = state.app.clone();
        let tabs = state.open_tabs.clone();
        let plugin_grants = state.plugin_grants.clone();
        let dock_layout = state.dock_layout_json.clone();
        let vfs_tree_expanded = state.vfs_tree_expanded.clone();
        self.inner.block_on(async move {
            store_window_settings(&pool, &window).await?;
            store_app_settings(&pool, &app).await?;
            store_open_tabs(&pool, &tabs).await?;
            store_plugin_grants(&pool, &plugin_grants).await?;
            if let Some(json) = dock_layout.as_deref() {
                store_dock_layout(&pool, json).await?;
            }
            store_vfs_tree_expanded(&pool, &vfs_tree_expanded).await?;
            Ok(())
        })
    }

    /// Close the pool on shutdown. Safe to call even if already closed.
    pub fn close(self) {
        self.inner.close()
    }
}
