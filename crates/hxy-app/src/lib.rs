//! hxy hex editor application.

#![forbid(unsafe_code)]

pub mod app;
pub mod file;
pub mod settings;
pub mod tabs;
pub mod window;

pub use app::HxyApp;

pub const APP_NAME: &str = "hxy";
