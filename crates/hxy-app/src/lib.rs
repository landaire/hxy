//! hxy hex editor application.

#![deny(unsafe_code)]

pub mod app;
pub mod commands;
pub mod file;
pub mod settings;
pub mod state;
pub mod tabs;
pub mod vfs_panel;
pub mod window;

#[cfg(not(target_arch = "wasm32"))]
pub mod builtin_runtimes;
#[cfg(not(target_arch = "wasm32"))]
pub mod command_palette;
#[cfg(not(target_arch = "wasm32"))]
pub mod copy_format;
#[cfg(not(target_arch = "wasm32"))]
pub mod inspector;
#[cfg(not(target_arch = "wasm32"))]
pub mod plugins_tab;
#[cfg(not(target_arch = "wasm32"))]
pub mod template_library;
#[cfg(not(target_arch = "wasm32"))]
pub mod template_panel;

#[cfg(not(target_arch = "wasm32"))]
pub mod persist;

#[cfg(target_os = "macos")]
pub mod menu;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub use app::HxyApp;

pub const APP_NAME: &str = "hxy";
