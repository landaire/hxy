//! Desktop persistence: SQLite-backed key/value settings store.

use std::path::PathBuf;

use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqliteJournalMode;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::SqliteSynchronous;
use thiserror::Error;

mod kv;
mod plugin_state;
mod save;

pub use kv::load_app_settings;
pub use kv::load_dock_layout;
pub use kv::load_open_tabs;
pub use kv::load_plugin_grants;
pub use kv::load_vfs_tree_expanded;
pub use kv::load_window_settings;
pub use kv::store_app_settings;
pub use kv::store_dock_layout;
pub use kv::store_open_tabs;
pub use kv::store_plugin_grants;
pub use kv::store_vfs_tree_expanded;
pub use kv::store_window_settings;
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

fn db_path() -> Option<PathBuf> {
    storage_dir().map(|d| d.join("hxy.db"))
}

pub async fn open_db() -> PersistResult<SqlitePool> {
    let path = db_path().ok_or(PersistError::StorageDirMissing)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|source| PersistError::CreateDir { path: parent.to_path_buf(), source })?;
    }
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

/// Blocking variant of [`load_window_settings`] for pre-eframe startup.
pub fn load_window_settings_sync() -> PersistResult<Option<crate::window::WindowSettings>> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(PersistError::Runtime)?;
    rt.block_on(async {
        let pool = open_db().await?;
        let settings = load_window_settings(&pool).await?;
        pool.close().await;
        Ok(settings)
    })
}
