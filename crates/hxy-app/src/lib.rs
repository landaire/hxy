//! hxy hex editor application.

#![forbid(unsafe_code)]

pub mod app;
pub mod commands;
pub mod file;
pub mod settings;
pub mod state;
pub mod tabs;
pub mod vfs_panel;
pub mod window;

#[cfg(not(target_arch = "wasm32"))]
pub mod template_panel;

#[cfg(not(target_arch = "wasm32"))]
pub mod persist;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub use app::HxyApp;

pub const APP_NAME: &str = "hxy";
