//! hxy hex editor application.

#![forbid(unsafe_code)]

pub mod app;
pub mod file;
pub mod settings;
pub mod state;
pub mod tabs;
pub mod window;

#[cfg(not(target_arch = "wasm32"))]
pub mod persist;

pub use app::HxyApp;

pub const APP_NAME: &str = "hxy";
