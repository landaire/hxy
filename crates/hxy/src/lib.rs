//! hxy hex editor application.

#![deny(unsafe_code)]

#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
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

// Browser build of `HxyApp`. Stepping stone -- see
// `crate::wasm_app` for why this exists. Each commit folds more
// of its content into `crate::app::HxyApp` (gated to non-wasm
// where appropriate); the file goes away once nothing
// wasm-specific remains here that isn't already in `app`.
#[cfg(target_arch = "wasm32")]
mod wasm_app;

#[cfg(not(target_arch = "wasm32"))]
pub use app::HxyApp;
#[cfg(target_arch = "wasm32")]
pub use wasm_app::HxyApp;

pub const APP_NAME: &str = "hxy";
