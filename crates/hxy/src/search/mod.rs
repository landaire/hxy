//! Search bar.
//!
//! Per-file `SearchState` lives on `OpenFile`. The bar renders at the
//! bottom of a file tab when open; cross-file search runs through a
//! shared `GlobalSearchState` whose results are listed in a dedicated
//! `Tab::SearchResults`.
//!
//! The query encoding / state / chunked scanner moved to
//! `hxy_panels::search` (framework-agnostic, shared with the GPUI
//! port); re-exported here under the original path. `bar`, `global`,
//! `modal`, and `replace` stay -- they render with egui and touch
//! `OpenFile` / `HxyApp` / toasts.

pub mod bar;
pub mod global;
pub mod modal;
pub mod replace;

pub use hxy_panels::search::*;