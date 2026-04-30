//! Background-thread driver for VFS-entry tab opens.
//!
//! Plugin VFS calls (`metadata`, `open_file`, `read_range`) end up in
//! wasmtime which can block on TCP / disk I/O for arbitrarily long.
//! Doing those on the UI thread freezes the whole frame loop -- and
//! during session restore, every persisted xbox / zip / etc. entry
//! tab adds up. Instead, this module spawns a fresh OS thread per
//! VFS open, sends the resulting [`HexSource`] back through the
//! shared [`egui_inbox::UiInbox`] on `HxyApp`, and lets the egui app
//! drain completed opens once a frame.
//!
//! Modeled on [`crate::plugins::runner`]: per-op `thread::spawn`
//! rather than the shared CPU pool, because the work is I/O-bound
//! and shouldn't head-of-line block short jobs queued for
//! [`crate::background`].

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;
use std::thread;

use hxy_core::HexSource;
use hxy_vfs::MountedVfs;

use crate::files::FileId;

/// One completed VFS-entry open delivered back to the UI thread.
/// `outcome` is the freshly-built byte source (success) or a
/// human-readable error message (plugin / IO error). The host
/// matches `file_id` against `app.files` and either swaps the
/// source into the editor or stamps `LoadStatus::Failed` on the
/// placeholder.
pub struct VfsOpenResult {
    pub file_id: FileId,
    pub outcome: Result<Arc<dyn HexSource>, String>,
}

/// Spawn a worker thread that opens the entry at `entry_path`
/// inside `mount` and posts the result back through `sender`.
/// Returns immediately; the calling frame proceeds and the worker
/// delivers its result on a later frame via the inbox.
pub fn spawn(
    sender: egui_inbox::UiInboxSender<VfsOpenResult>,
    file_id: FileId,
    mount: Arc<MountedVfs>,
    entry_path: String,
) {
    thread::spawn(move || {
        let outcome = crate::files::streaming::open_vfs(mount, entry_path)
            .map(|(source, _len)| source)
            .map_err(|e| e.to_string());
        // Send failure means the UI dropped the inbox -- the app is
        // shutting down. Nothing to recover.
        let _ = sender.send(VfsOpenResult { file_id, outcome });
    });
}
