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
mod commands;
mod fs_impl;
mod grants;
mod handler;
mod host;
mod manifest;
mod registry;
mod state_store;
mod token;

pub mod template;

pub use commands::InvokeOutcome;
pub use commands::MountRequest;
pub use commands::PluginCommand;
pub use commands::PromptRequest;
pub use grants::PermissionGrants;
pub use grants::PluginGrants;
pub use grants::PluginKey;
pub use handler::MountByTokenError;
pub use handler::PluginHandler;
pub use manifest::ManifestError;
pub use manifest::Permissions;
pub use manifest::PluginManifest;
pub use manifest::PluginMeta;
pub use registry::PluginLoadError;
pub use registry::load_plugins_from_dir;
pub use registry::load_template_plugins_from_dir;
pub use registry::load_template_runtime_from_bytes;
pub use state_store::InMemoryStateStore;
pub use state_store::MAX_STATE_BYTES;
pub use state_store::StateError;
pub use state_store::StateStore;
pub use state_store::validate_plugin_name;
pub use template::BG_COLOR_ATTR;
pub use template::BITFIELD_BITS_ATTR;
pub use template::COLOR_ATTR;
pub use template::COMMENT_ATTR;
pub use template::ENDIAN_ATTR;
pub use template::FORMAT_ATTR;
pub use template::ParsedTemplate;
pub use template::TemplateRuntime;
pub use template::WasmTemplateRuntime;
pub use template::node_display_type;
pub use template::node_type_label;
pub use template::scalar_kind_name;
pub use token::TokenError;
pub use token::fresh as fresh_token;
