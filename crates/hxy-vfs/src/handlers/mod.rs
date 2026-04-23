//! Built-in VFS handlers. These are plain Rust implementations of
//! [`VfsHandler`](super::VfsHandler) used until the wasm plugin pipeline
//! lands; they double as a test oracle once plugins exist.

mod zip;

pub use zip::ZipHandler;
