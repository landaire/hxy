//! Typed key/value helpers over the single `settings` table. Values are
//! JSON-encoded so individual fields can evolve with serde defaults.

use rootcause::Report;
use rootcause::prelude::ResultExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::SqlitePool;

use crate::settings::AppSettings;
use crate::window::WindowSettings;

const KEY_WINDOW: &str = "window";
const KEY_APP: &str = "app_settings";

async fn fetch(pool: &SqlitePool, key: &str) -> Result<Option<String>, Report> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
        .context("fetch setting")?;
    Ok(row.map(|(v,)| v))
}

async fn store<V: Serialize>(pool: &SqlitePool, key: &str, value: &V) -> Result<(), Report> {
    let json = serde_json::to_string(value).context("serialize setting")?;
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(json)
    .execute(pool)
    .await
    .context("store setting")?;
    Ok(())
}

async fn load<V: DeserializeOwned>(pool: &SqlitePool, key: &str) -> Result<Option<V>, Report> {
    match fetch(pool, key).await? {
        Some(json) => Ok(Some(serde_json::from_str(&json).context("deserialize setting")?)),
        None => Ok(None),
    }
}

pub async fn load_window_settings(pool: &SqlitePool) -> Result<Option<WindowSettings>, Report> {
    load(pool, KEY_WINDOW).await
}

pub async fn store_window_settings(pool: &SqlitePool, ws: &WindowSettings) -> Result<(), Report> {
    store(pool, KEY_WINDOW, ws).await
}

pub async fn load_app_settings(pool: &SqlitePool) -> Result<Option<AppSettings>, Report> {
    load(pool, KEY_APP).await
}

pub async fn store_app_settings(pool: &SqlitePool, s: &AppSettings) -> Result<(), Report> {
    store(pool, KEY_APP, s).await
}
