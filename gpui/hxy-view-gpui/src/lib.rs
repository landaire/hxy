//! GPUI hex-view widget driven by hxy-editor.

#![forbid(unsafe_code)]

mod geometry;
mod input;
mod minimap;
mod paint;
mod pane;

pub use geometry::*;
pub use minimap::MinimapBounds;
pub use pane::ByteStyleOverride;
pub use pane::ByteStyler;
pub use pane::FrameInfo;
pub use pane::HexPane;
pub use pane::PaneHighlight;
