//! User-supplied WASM plugin runtime + mount integration.

#![cfg(not(target_arch = "wasm32"))]

pub mod mount;
pub mod runner;
