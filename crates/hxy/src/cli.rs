//! CLI parsing, re-exported from the framework-neutral `hxy-ipc`
//! crate. See [`hxy_ipc::cli`] for the parser itself.

#![cfg(not(target_arch = "wasm32"))]

pub use hxy_ipc::cli::*;
