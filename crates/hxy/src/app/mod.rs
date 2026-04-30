//! Application type. One symbolic `HxyApp` re-exported for both
//! the native desktop build and the browser wasm build, dispatched
//! at compile time to the corresponding sub-module.
//!
//! The two impls share types lower in the dependency graph
//! (`hxy_view::HexEditor`, `crate::files::OpenFile`,
//! `crate::tabs::Tab`, `crate::panels::*`, ...) and structurally
//! converge as wasm-incompatible bits get pushed inward and the
//! shared UI moves into common helpers. See
//! [`crate::app::desktop`] for the full HxyApp and
//! [`crate::app::wasm`] for the slimmer browser one.

#[cfg(not(target_arch = "wasm32"))]
pub mod desktop;
#[cfg(target_arch = "wasm32")]
pub mod wasm;

#[cfg(not(target_arch = "wasm32"))]
pub use desktop::*;
#[cfg(target_arch = "wasm32")]
pub use wasm::HxyApp;
