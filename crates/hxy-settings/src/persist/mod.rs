//! Desktop persistence: SQLite-backed key/value settings store.

use std::path::Path;
use std::path::PathBuf;

use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqliteJournalMode;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::SqliteSynchronous;
use thiserror::Error;

pub mod kv;
mod plugin_state;
mod save;

pub use kv::load_app_settings;
pub use kv::load_dock_layout;
pub use kv::load_plugin_grants;
pub use kv::load_vfs_tree_expanded;
pub use kv::store_app_settings;
pub use kv::store_dock_layout;
pub use kv::store_plugin_grants;
pub use kv::store_vfs_tree_expanded;
pub use plugin_state::SqliteStateStore;
pub use save::SaveSink;

#[derive(Debug, Error)]
pub enum PersistError {
    #[error("cannot resolve storage directory for this platform")]
    StorageDirMissing,
    #[error("create storage directory {path}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("open sqlite connection pool at {path}")]
    OpenPool {
        path: PathBuf,
        #[source]
        source: sqlx::Error,
    },
    #[error("run sqlite migrations")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("query settings table")]
    Query(#[source] sqlx::Error),
    #[error("serialize setting {key}")]
    Serialize {
        key: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("deserialize setting {key}")]
    Deserialize {
        key: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("build tokio runtime")]
    Runtime(#[source] std::io::Error),
}

pub type PersistResult<T> = Result<T, PersistError>;

pub fn storage_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join("Library/Application Support/hxy"))
    }
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var_os("APPDATA")?;
        Some(PathBuf::from(appdata).join("hxy"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            return Some(PathBuf::from(xdg).join("hxy"));
        }
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".local/share/hxy"))
    }
}

pub async fn open_db() -> PersistResult<SqlitePool> {
    let dir = storage_dir().ok_or(PersistError::StorageDirMissing)?;
    open_db_in(&dir).await
}

/// Open (creating if needed) the settings database inside `dir`
/// instead of the platform [`storage_dir`]. Lets tests point
/// persistence at a temp directory without a global override.
pub async fn open_db_in(dir: &Path) -> PersistResult<SqlitePool> {
    let path = dir.join("hxy.db");
    std::fs::create_dir_all(dir).map_err(|source| PersistError::CreateDir { path: dir.to_path_buf(), source })?;
    let opts = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .map_err(|source| PersistError::OpenPool { path: path.clone(), source })?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::AppSettings;

    #[test]
    fn app_settings_round_trip_through_temp_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("build runtime");
        rt.block_on(async {
            let pool = open_db_in(dir.path()).await.expect("open db");
            assert_eq!(load_app_settings(&pool).await.expect("load empty"), None);
            let settings =
                AppSettings { byte_value_highlight: false, file_poll_interval_ms: 1234, ..AppSettings::default() };
            store_app_settings(&pool, &settings).await.expect("store");
            let loaded = load_app_settings(&pool).await.expect("reload");
            assert_eq!(loaded, Some(settings));
            pool.close().await;
        });
    }
}
