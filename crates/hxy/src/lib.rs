//! hxy hex editor application.

#![deny(unsafe_code)]

pub mod app;
pub mod files;
pub mod search;
pub mod settings;
pub mod state;
pub mod tabs;
pub mod window;

pub mod commands;
#[cfg(not(target_arch = "wasm32"))]
pub mod compare;
#[cfg(not(target_arch = "wasm32"))]
pub mod panels;
#[cfg(not(target_arch = "wasm32"))]
pub mod plugins;
#[cfg(not(target_arch = "wasm32"))]
pub mod templates;

#[cfg(not(target_arch = "wasm32"))]
pub mod cli;
#[cfg(not(target_arch = "wasm32"))]
pub mod ipc;
#[cfg(not(target_arch = "wasm32"))]
pub mod toasts;

#[cfg(target_os = "macos")]
pub mod menu;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub use app::HxyApp;

pub const APP_NAME: &str = "hxy";
