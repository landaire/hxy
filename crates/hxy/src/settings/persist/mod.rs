//! Desktop persistence. The SQLite store lives in
//! `hxy_settings::persist`; this module re-exports it at the old
//! paths and adds the egui-app-specific typed keys (window geometry,
//! open tabs) plus the whole-state [`SaveSink`].

mod kv;
mod save;

pub use hxy_settings::persist::PersistError;
pub use hxy_settings::persist::PersistResult;
pub use hxy_settings::persist::SqliteStateStore;
pub use hxy_settings::persist::load_app_settings;
pub use hxy_settings::persist::load_dock_layout;
pub use hxy_settings::persist::load_plugin_grants;
pub use hxy_settings::persist::load_vfs_tree_expanded;
pub use hxy_settings::persist::open_db;
pub use hxy_settings::persist::storage_dir;
pub use hxy_settings::persist::store_app_settings;
pub use hxy_settings::persist::store_dock_layout;
pub use hxy_settings::persist::store_plugin_grants;
pub use hxy_settings::persist::store_vfs_tree_expanded;
pub use kv::load_open_tabs;
pub use kv::load_window_settings;
pub use kv::store_open_tabs;
pub use kv::store_window_settings;
pub use save::SaveSink;

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
