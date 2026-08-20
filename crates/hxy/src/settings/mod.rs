//! User-visible application settings. The model lives in the shared
//! `hxy-settings` crate; this module re-exports it at the old paths
//! and adds the egui-side view conversions.

#[cfg(not(target_arch = "wasm32"))]
pub mod persist;

pub use hxy_settings::*;

/// Egui-side conversion for [`ByteHighlightMode`]. Lives here as an
/// extension trait because `hxy_view::ValueHighlight` is an egui-view
/// type and the shared settings crate stays egui-free.
pub trait ByteHighlightModeExt {
    fn as_view(&self) -> hxy_view::ValueHighlight;
}

impl ByteHighlightModeExt for ByteHighlightMode {
    fn as_view(&self) -> hxy_view::ValueHighlight {
        match self {
            Self::Background => hxy_view::ValueHighlight::Background,
            Self::Text => hxy_view::ValueHighlight::Text,
        }
    }
}
