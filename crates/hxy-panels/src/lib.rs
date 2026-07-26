//! Framework-agnostic panel logic shared between hxy's egui frontend
//! and the in-progress GPUI port: offset/range parsing, the search
//! engine, the data inspector's decoders, strings/entropy/checksum
//! analysis, and compare-view diffing. Nothing here depends on a UI
//! toolkit or the app's file/tab types.

pub mod checksums;
pub mod diff;
pub mod entropy;
pub mod goto;
pub mod inspector;
pub mod search;
pub mod strings;
#[cfg(not(target_arch = "wasm32"))]
pub mod watch;
