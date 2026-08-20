//! Synchronous save sink. Callers invoke a save method after any
//! mutation; there is no timer, no debounce, no background task.

use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::runtime::Runtime;

use crate::AppSettings;
use crate::persist::PersistResult;
use crate::persist::store_app_settings;

pub struct SaveSink {
    pool: SqlitePool,
    runtime: Arc<Runtime>,
}

impl SaveSink {
    pub fn new(pool: SqlitePool, runtime: Arc<Runtime>) -> Self {
        Self { pool, runtime }
    }

    /// Pool handle for callers composing multi-key saves through
    /// [`Self::block_on`].
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Run `fut` to completion on the sink's runtime, blocking the
    /// calling thread until it finishes (SQLite WAL writes are
    /// sub-millisecond for our tiny key/value payloads).
    pub fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.runtime.block_on(fut)
    }

    /// Persist the app-settings blob. Blocks like [`Self::block_on`].
    pub fn save_app_settings(&self, settings: &AppSettings) -> PersistResult<()> {
        self.block_on(store_app_settings(&self.pool, settings))
    }

    /// Close the pool on shutdown. Safe to call even if already closed.
    pub fn close(self) {
        let pool = self.pool;
        self.runtime.block_on(async move {
            pool.close().await;
        });
    }
}
