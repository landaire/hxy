//! hxy hex editor application.

#![deny(unsafe_code)]

pub mod app;
pub mod files;
pub mod search;
pub mod settings;
pub mod state;
pub mod style;
pub mod tabs;
pub mod window;

pub mod background;
pub mod commands;
pub mod compare;
pub mod panels;
#[cfg(not(target_arch = "wasm32"))]
pub mod plugins;
#[cfg(not(target_arch = "wasm32"))]
pub mod templates;
pub mod view;
#[cfg(not(target_arch = "wasm32"))]
pub mod visualizers;

#[cfg(not(target_arch = "wasm32"))]
pub mod cli;
#[cfg(not(target_arch = "wasm32"))]
pub mod ipc;
pub mod toasts;

#[cfg(target_os = "macos")]
pub mod menu;

#[cfg(target_arch = "wasm32")]
mod wasm;
#[cfg(target_arch = "wasm32")]
pub mod wasm_blob_source;

pub use app::HxyApp;

pub const APP_NAME: &str = "hxy";
