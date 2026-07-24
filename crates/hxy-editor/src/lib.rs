//! Framework-agnostic hex-editor model. UI layers (egui: hxy-view,
//! gpui: hxy-view-gpui) translate native events into [`InputEvent`]s
//! and render from [`HexEditor`] state.

#![forbid(unsafe_code)]

pub mod events;
