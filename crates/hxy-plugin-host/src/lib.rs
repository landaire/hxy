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
mod template;

pub use handler::PluginHandler;
pub use registry::PluginLoadError;
pub use registry::load_plugins_from_dir;
pub use registry::load_template_runtimes_from_dir;
pub use template::Arg;
pub use template::ArgValue;
pub use template::DeferredArray;
pub use template::Diagnostic;
pub use template::DisplayHint;
pub use template::Node;
pub use template::ParsedTemplate;
pub use template::ResultTree;
pub use template::Severity;
pub use template::Span;
pub use template::TemplateRuntime;
pub use template::Value;
