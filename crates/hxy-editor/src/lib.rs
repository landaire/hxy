//! Framework-agnostic hex-editor model. UI layers (egui: hxy-view,
//! gpui: hxy-view-gpui) translate native events into [`InputEvent`]s
//! and render from [`HexEditor`] state.

#![forbid(unsafe_code)]

pub mod events;

pub use events::Disposition;
pub use events::Effect;
pub use events::InputEvent;
pub use events::Key;
pub use events::Modifiers;
