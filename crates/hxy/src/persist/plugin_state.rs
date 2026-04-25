//! SQLite-backed [`hxy_plugin_host::StateStore`] implementation.
//! Lives in the app crate so the host crate stays independent of
//! sqlx; the bridge from sync (the trait contract, driven by WIT
//! calls) to async (sqlx) goes through the same `Arc<Runtime>` the
//! [`crate::persist::SaveSink`] uses.

use std::sync::Arc;

use hxy_plugin_host::MAX_STATE_BYTES;
use hxy_plugin_host::StateError;
use hxy_plugin_host::StateStore;
use hxy_plugin_host::validate_plugin_name;
use sqlx::SqlitePool;
use tokio::runtime::Runtime;

pub struct SqliteStateStore {
    pool: SqlitePool,
    runtime: Arc<Runtime>,
}

impl SqliteStateStore {
    pub fn new(pool: SqlitePool, runtime: Arc<Runtime>) -> Self {
        Self { pool, runtime }
    }
}

impl StateStore for SqliteStateStore {
    fn load(&self, plugin_name: &str) -> Result<Option<Vec<u8>>, StateError> {
        validate_plugin_name(plugin_name)?;
        let pool = self.pool.clone();
        let name = plugin_name.to_owned();
        self.runtime.block_on(async move {
            let row: Option<(Vec<u8>,)> = sqlx::query_as("SELECT blob FROM plugin_state WHERE plugin_name = ?")
                .bind(&name)
                .fetch_optional(&pool)
                .await
                .map_err(into_backend_error)?;
            Ok(row.map(|(b,)| b))
        })
    }

    fn save(&self, plugin_name: &str, blob: &[u8]) -> Result<(), StateError> {
        validate_plugin_name(plugin_name)?;
        let actual = blob.len() as u64;
        if actual > MAX_STATE_BYTES {
            return Err(StateError::QuotaExceeded { actual, limit: MAX_STATE_BYTES });
        }
        let pool = self.pool.clone();
        let name = plugin_name.to_owned();
        let blob = blob.to_vec();
        self.runtime.block_on(async move {
            sqlx::query(
                "INSERT INTO plugin_state (plugin_name, blob) VALUES (?, ?) \
                 ON CONFLICT(plugin_name) DO UPDATE SET blob = excluded.blob",
            )
            .bind(&name)
            .bind(&blob)
            .execute(&pool)
            .await
            .map_err(into_backend_error)?;
            Ok(())
        })
    }

    fn clear(&self, plugin_name: &str) -> Result<(), StateError> {
        validate_plugin_name(plugin_name)?;
        let pool = self.pool.clone();
        let name = plugin_name.to_owned();
        self.runtime.block_on(async move {
            sqlx::query("DELETE FROM plugin_state WHERE plugin_name = ?")
                .bind(&name)
                .execute(&pool)
                .await
                .map_err(into_backend_error)?;
            Ok(())
        })
    }
}

fn into_backend_error(e: sqlx::Error) -> StateError {
    StateError::Backend(Box::new(e))
}
