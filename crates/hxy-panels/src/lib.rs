//! Framework-agnostic panel logic shared between hxy's egui frontend
//! and the in-progress GPUI port: offset/range parsing, the search
//! engine, and the data inspector's decoders. Nothing here depends on
//! a UI toolkit or the app's file/tab types.

pub mod goto;
pub mod inspector;
pub mod search;
