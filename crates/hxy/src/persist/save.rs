//! Synchronous save sink. Callers invoke [`SaveSink::save`] after any
//! mutation; there is no timer, no debounce, no background task.

use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::runtime::Runtime;

use crate::persist::PersistResult;
use crate::persist::store_app_settings;
use crate::persist::store_dock_layout;
use crate::persist::store_open_tabs;
use crate::persist::store_plugin_grants;
use crate::persist::store_vfs_tree_expanded;
use crate::persist::store_window_settings;
use crate::state::PersistedState;

pub struct SaveSink {
    pool: SqlitePool,
    runtime: Arc<Runtime>,
}

impl SaveSink {
    pub fn new(pool: SqlitePool, runtime: Arc<Runtime>) -> Self {
        Self { pool, runtime }
    }

    /// Persist the whole state. Blocks the calling thread on the tokio
    /// runtime's executor until the writes complete (SQLite WAL writes
    /// are sub-millisecond for our tiny key/value payloads).
    pub fn save(&self, state: &PersistedState) -> PersistResult<()> {
        let pool = self.pool.clone();
        let window = state.window;
        let app = state.app.clone();
        let tabs = state.open_tabs.clone();
        let plugin_grants = state.plugin_grants.clone();
        let dock_layout = state.dock_layout_json.clone();
        let vfs_tree_expanded = state.vfs_tree_expanded.clone();
        self.runtime.block_on(async move {
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
        let pool = self.pool;
        self.runtime.block_on(async move {
            pool.close().await;
        });
    }
}
