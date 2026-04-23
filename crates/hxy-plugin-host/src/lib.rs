//! Host for `hxy:vfs` WASM component plugins.
//!
//! Each plugin is a WebAssembly component exporting the `handler`
//! interface defined in `wit/world.wit`. The host imports the plugin,
//! wraps it as an [`hxy_vfs::VfsHandler`], and exposes a bidirectional
//! `source` interface so plugins can stream bytes from the underlying
//! [`HexSource`] without loading the whole file into memory.

#![forbid(unsafe_code)]
#![cfg(not(target_arch = "wasm32"))]

mod bindings;
mod fs_impl;
mod handler;
mod host;
mod registry;

pub use handler::PluginHandler;
pub use registry::PluginLoadError;
pub use registry::load_plugins_from_dir;
