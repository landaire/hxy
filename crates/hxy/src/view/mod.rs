//! Hex-view rendering primitives shared across the file tab,
//! status bar, and compare panes.

#![cfg(not(target_arch = "wasm32"))]

pub mod format;
pub mod hex_body;
