//! hxy hex editor application.

#![deny(unsafe_code)]

pub mod app;
pub mod commands;
#[cfg(not(target_arch = "wasm32"))]
pub mod compare;
pub mod file;
#[cfg(not(target_arch = "wasm32"))]
pub mod global_search;
pub mod search;
pub mod search_bar;
pub mod settings;
pub mod shortcuts;
pub mod state;
pub mod tabs;
pub mod vfs_panel;
pub mod window;

#[cfg(not(target_arch = "wasm32"))]
pub mod builtin_runtimes;
#[cfg(not(target_arch = "wasm32"))]
pub mod cli;
#[cfg(not(target_arch = "wasm32"))]
pub mod command_palette;
#[cfg(not(target_arch = "wasm32"))]
pub mod copy_format;
#[cfg(not(target_arch = "wasm32"))]
pub mod goto;
#[cfg(not(target_arch = "wasm32"))]
pub mod inspector;
#[cfg(not(target_arch = "wasm32"))]
pub mod ipc;
#[cfg(not(target_arch = "wasm32"))]
pub mod pane_pick;
#[cfg(not(target_arch = "wasm32"))]
pub mod paste;
#[cfg(not(target_arch = "wasm32"))]
pub mod plugin_runner;
#[cfg(not(target_arch = "wasm32"))]
pub mod plugins_tab;
#[cfg(not(target_arch = "wasm32"))]
pub mod template_library;
#[cfg(not(target_arch = "wasm32"))]
pub mod template_panel;

#[cfg(not(target_arch = "wasm32"))]
pub mod patch_persist;

#[cfg(not(target_arch = "wasm32"))]
pub mod persist;
#[cfg(not(target_arch = "wasm32"))]
pub mod persisted_dock;

#[cfg(target_os = "macos")]
pub mod menu;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub use app::HxyApp;

pub const APP_NAME: &str = "hxy";
