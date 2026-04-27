//! Template runtime registry, built-in interpreters, and the
//! ImHex pattern-corpus auto-download flow.

#![cfg(not(target_arch = "wasm32"))]

pub mod builtin;
pub mod library;
pub mod patterns_fetch;
