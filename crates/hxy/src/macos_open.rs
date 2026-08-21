//! egui adapter over the framework-neutral macOS open-with handlers
//! in `hxy-ipc`.
//!
//! The Apple Event / NSServices registration, the pending-path buffer,
//! and the install-before-window contract live in
//! [`hxy_ipc::macos_open`]; this module only supplies the egui wake
//! seam so a forwarded batch schedules a repaint.
//!
//! [`install`] MUST still run before `eframe::run_native` so the
//! handlers are in place before `NSApplication::run` dispatches any
//! cold-start "Open With" Apple Events; registering them from inside
//! the eframe creator would replace AppKit's in-flight
//! `_handleAEOpenEvent:` handler mid-dispatch and crash winit's
//! ApplicationDelegate state machine.

use std::sync::Arc;

pub use hxy_ipc::macos_open::drain_pending_paths;
pub use hxy_ipc::macos_open::install;
pub use hxy_ipc::macos_open::push_paths;

/// Plumb the egui [`Context`] in once it's available so subsequent
/// handler firings can request a repaint. Safe to call multiple times
/// -- only the first call takes effect.
pub fn wire_repaint_ctx(ctx: &egui::Context) {
    let ctx = ctx.clone();
    hxy_ipc::macos_open::set_wake(Arc::new(move || ctx.request_repaint()));
}
