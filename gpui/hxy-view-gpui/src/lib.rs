//! GPUI hex-view widget driven by hxy-editor.

#![forbid(unsafe_code)]

mod geometry;
mod input;
mod paint;
mod pane;

pub use geometry::*;
pub use pane::FrameInfo;
pub use pane::HexPane;
