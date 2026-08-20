//! Shim preserving the egui app's module path; the implementation
//! lives in the shared `hxy-templates` crate.

#![cfg(not(target_arch = "wasm32"))]

pub use hxy_templates::builtin::*;
