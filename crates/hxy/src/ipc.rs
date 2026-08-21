//! egui adapter over the framework-neutral IPC socket in `hxy-ipc`.
//!
//! The socket listener, wire format, and forwarding client live in
//! [`hxy_ipc::socket`]; this module only bridges the neutral
//! [`std::sync::mpsc::Receiver`] into an [`egui_inbox::UiInbox`] so
//! the egui app can schedule a repaint when a forwarded batch lands.

#![cfg(not(target_arch = "wasm32"))]

use std::path::PathBuf;
use std::thread;

pub use hxy_ipc::socket::IpcMessage;
pub use hxy_ipc::socket::MAX_MESSAGE_BYTES;
pub use hxy_ipc::socket::SOCKET_NAME;
pub use hxy_ipc::socket::try_send_to_running_instance;

/// Bind the IPC socket and hand back a `UiInbox` the egui app drains
/// each frame. `None` when the socket can't be bound (stale lock,
/// permissions, etc.): the GUI still runs but won't accept forwarded
/// opens until the next launch.
///
/// A small forwarding thread reads the neutral receiver and re-sends
/// each batch through the `UiInbox` sender, which carries `ctx` so
/// the arrival schedules a repaint.
pub fn start_server(ctx: &egui::Context) -> Option<egui_inbox::UiInbox<Vec<PathBuf>>> {
    let receiver = match hxy_ipc::socket::start_server() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "ipc: start server; CLI forwarding disabled");
            return None;
        }
    };
    let (sender, inbox) = egui_inbox::UiInbox::channel_with_ctx(ctx);
    thread::spawn(move || {
        while let Ok(paths) = receiver.recv() {
            if sender.send(paths).is_err() {
                // UI dropped the inbox -- the app is shutting down.
                return;
            }
        }
    });
    Some(inbox)
}
