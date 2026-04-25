//! VFS layer for the hxy hex editor.
//!
//! Exposes a [`VfsHandler`] trait that plugins (native or wasm) implement
//! to turn a byte source into something browsable as a virtual filesystem.
//! The [`VfsRegistry`] holds the active set of handlers and picks the
//! right one based on the source's first few bytes.

#![forbid(unsafe_code)]

mod capabilities;
mod error;
mod handler;
mod registry;
mod tab_source;

pub mod handlers;

pub use capabilities::VfsCapabilities;
pub use error::HandlerError;
pub use handler::MountedVfs;
pub use handler::VfsHandler;
pub use handler::VfsWriter;
pub use registry::VfsRegistry;
pub use tab_source::AnonymousId;
pub use tab_source::TabSource;
pub use vfs;
