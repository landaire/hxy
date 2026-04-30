//! Dockable side panels: inspector, VFS tree, template panel,
//! plugin manager. Each one is a self-contained `egui::Ui`
//! renderer plus the small amount of state it owns.
//!
//! Most panels are pure compute or pure rendering and compile on
//! every target. The `plugins` (dynamic plugin manager) and
//! `template` (per-file template tree) panels stay desktop-only
//! because they need wasmtime through `hxy-plugin-host`.

pub mod checksums;
pub mod entropy;
pub mod inspector;
pub mod memory;
pub mod strings;
pub mod vfs;

#[cfg(not(target_arch = "wasm32"))]
pub mod plugins;
#[cfg(not(target_arch = "wasm32"))]
pub mod template;
