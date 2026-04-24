//! Typed key/value helpers over the single `settings` table. Values are
//! JSON-encoded so individual fields can evolve with serde defaults.

use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::SqlitePool;

use crate::persist::PersistError;
use crate::persist::PersistResult;
use crate::settings::AppSettings;
use crate::state::OpenTabState;
use crate::window::WindowSettings;

const KEY_WINDOW: &str = "window";
const KEY_APP: &str = "app_settings";
const KEY_OPEN_TABS: &str = "open_tabs";

async fn fetch(pool: &SqlitePool, key: &str) -> PersistResult<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
        .map_err(PersistError::Query)?;
    Ok(row.map(|(v,)| v))
}

async fn store<V: Serialize>(pool: &SqlitePool, key: &'static str, value: &V) -> PersistResult<()> {
    let json = serde_json::to_string(value).map_err(|source| PersistError::Serialize { key, source })?;
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(json)
    .execute(pool)
    .await
    .map_err(PersistError::Query)?;
    Ok(())
}

async fn load<V: DeserializeOwned>(pool: &SqlitePool, key: &'static str) -> PersistResult<Option<V>> {
    match fetch(pool, key).await? {
        Some(json) => serde_json::from_str(&json).map(Some).map_err(|source| PersistError::Deserialize { key, source }),
        None => Ok(None),
    }
}

pub async fn load_window_settings(pool: &SqlitePool) -> PersistResult<Option<WindowSettings>> {
    load(pool, KEY_WINDOW).await
}

pub async fn store_window_settings(pool: &SqlitePool, ws: &WindowSettings) -> PersistResult<()> {
    store(pool, KEY_WINDOW, ws).await
}

pub async fn load_app_settings(pool: &SqlitePool) -> PersistResult<Option<AppSettings>> {
    load(pool, KEY_APP).await
}

pub async fn store_app_settings(pool: &SqlitePool, s: &AppSettings) -> PersistResult<()> {
    store(pool, KEY_APP, s).await
}

pub async fn load_open_tabs(pool: &SqlitePool) -> PersistResult<Option<Vec<OpenTabState>>> {
    load(pool, KEY_OPEN_TABS).await
}

pub async fn store_open_tabs(pool: &SqlitePool, tabs: &[OpenTabState]) -> PersistResult<()> {
    store(pool, KEY_OPEN_TABS, &tabs).await
}
