//! Desktop persistence: SQLite-backed key/value settings store.

use std::path::PathBuf;

use rootcause::Report;
use rootcause::prelude::ResultExt;
use rootcause::report;

use thiserror::Error;

#[derive(Debug, Error)]
#[error("cannot resolve storage directory")]
struct StorageDirMissing;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqliteJournalMode;
use sqlx::sqlite::SqliteSynchronous;

mod kv;
mod save;

pub use kv::load_app_settings;
pub use kv::load_window_settings;
pub use kv::store_app_settings;
pub use kv::store_window_settings;
pub use save::SaveHandle;
pub use save::spawn_save_task;

/// Per-platform data directory for hxy. Returns `None` only when the
/// platform has no obvious home/data path (very rare).
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

/// Open (creating if needed) the settings database and run migrations.
pub async fn open_db() -> Result<SqlitePool, Report> {
    let path = db_path().ok_or_else(|| report!(StorageDirMissing).into_dynamic())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create storage dir")?;
    }
    let opts = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .context("open sqlite pool")?;
    sqlx::migrate!("./migrations").run(&pool).await.context("run migrations")?;
    Ok(pool)
}

/// Blocking variant of [`load_window_settings`] so the pre-eframe startup
/// code can restore the window geometry without establishing the full
/// tokio runtime.
pub fn load_window_settings_sync() -> Option<crate::window::WindowSettings> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    rt.block_on(async {
        let pool = open_db().await.ok()?;
        let settings = load_window_settings(&pool).await.ok().flatten();
        pool.close().await;
        settings
    })
}
