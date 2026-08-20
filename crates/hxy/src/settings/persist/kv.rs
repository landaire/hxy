//! egui-app-specific typed keys over the shared `settings` table.
//! The generic JSON helpers live in `hxy_settings::persist::kv`;
//! these wrappers pin the keys whose value types are app-side
//! (window geometry, open tabs).

use hxy_settings::persist::PersistResult;
use hxy_settings::persist::kv::load;
use hxy_settings::persist::kv::store;
use sqlx::SqlitePool;

use crate::state::OpenTabState;
use crate::window::WindowSettings;

const KEY_WINDOW: &str = "window";
const KEY_OPEN_TABS: &str = "open_tabs";

pub async fn load_window_settings(pool: &SqlitePool) -> PersistResult<Option<WindowSettings>> {
    load(pool, KEY_WINDOW).await
}

pub async fn store_window_settings(pool: &SqlitePool, ws: &WindowSettings) -> PersistResult<()> {
    store(pool, KEY_WINDOW, ws).await
}

pub async fn load_open_tabs(pool: &SqlitePool) -> PersistResult<Option<Vec<OpenTabState>>> {
    load(pool, KEY_OPEN_TABS).await
}

pub async fn store_open_tabs(pool: &SqlitePool, tabs: &[OpenTabState]) -> PersistResult<()> {
    store(pool, KEY_OPEN_TABS, &tabs).await
}
