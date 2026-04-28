//! Dockable side panels: inspector, VFS tree, template panel,
//! plugin manager. Each one is a self-contained `egui::Ui` renderer
//! plus the small amount of state it owns.

#![cfg(not(target_arch = "wasm32"))]

pub mod entropy;
pub mod inspector;
pub mod plugins;
pub mod template;
pub mod vfs;
