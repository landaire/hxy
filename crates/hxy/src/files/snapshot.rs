//! egui-side alias for the per-file snapshot store. The store logic
//! is framework-agnostic and lives in
//! [`hxy_panels::files::snapshot`]; this app supplies its snapshot
//! root under `$DATA_DIR/hxy/snapshots` via [`snapshots_base`].

#![cfg(not(target_arch = "wasm32"))]

use std::path::PathBuf;

use crate::APP_NAME;

pub use hxy_panels::files::snapshot::{IN_MEMORY_CACHE_MAX, Snapshot, SnapshotId, SnapshotStore};

/// This app's snapshot sidecar root. `None` when the platform
/// doesn't expose a data dir, in which case captures fall back to a
/// tempfile sidecar that doesn't survive the session.
pub fn snapshots_base() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join(APP_NAME).join("snapshots"))
}
