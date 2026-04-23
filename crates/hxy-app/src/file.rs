//! Per-tab open-file state.

use std::path::PathBuf;
use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;
use hxy_vfs::MountedVfs;
use hxy_vfs::TabSource;
use hxy_vfs::VfsHandler;
use thiserror::Error;

/// Identifier for an open-file tab. Stable across the tab's lifetime so
/// egui_dock can refer to it even as the tab moves around the dock tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileId(u64);

impl FileId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

pub struct OpenFile {
    pub id: FileId,
    pub display_name: String,
    /// Persistent identity of the tab's byte source. `None` for
    /// temporary in-memory buffers that shouldn't survive a restart.
    pub source_kind: Option<TabSource>,
    pub source: Arc<dyn HexSource>,
    pub selection: Option<Selection>,
    /// Last-hovered byte offset reported by the hex view — surfaced in
    /// the status bar. Cleared each frame (set from `HexViewResponse`).
    pub hovered: Option<ByteOffset>,
    /// Most recent scroll offset reported by the hex view.
    pub scroll_offset: f32,
    /// When `Some`, the widget should scroll to this offset on its next
    /// frame. Used to restore saved scroll position on reopen. Cleared
    /// after one frame so the user can scroll freely afterward.
    pub pending_scroll: Option<f32>,
    /// VFS handler detected for this file's byte source, if any. Cached
    /// from the first-frame detection so the toolbar command can check
    /// availability without re-scanning on each frame.
    pub detected_handler: Option<Arc<dyn VfsHandler>>,
    /// Mounted VFS, if the user has opened the archive via the
    /// "Browse archive" command. Shared so descendant tabs can open
    /// entries against the same mount.
    pub mount: Option<Arc<MountedVfs>>,
}

impl OpenFile {
    /// Construct from an in-memory buffer — used for initial load of
    /// small files before we have a streaming reader.
    pub fn from_bytes(
        id: FileId,
        display_name: impl Into<String>,
        source_kind: Option<TabSource>,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            id,
            display_name: display_name.into(),
            source_kind,
            source: Arc::new(MemorySource::new(bytes)),
            selection: None,
            hovered: None,
            scroll_offset: 0.0,
            pending_scroll: None,
            detected_handler: None,
            mount: None,
        }
    }

    /// Convenience: the filesystem path this tab (or any ancestor tab)
    /// ultimately originates from. `None` only for purely in-memory
    /// tabs with no path backing (e.g. placeholder buffers).
    pub fn root_path(&self) -> Option<&PathBuf> {
        self.source_kind.as_ref().map(|s| s.root_path())
    }
}

#[derive(Debug, Error)]
pub enum FileOpenError {
    #[error("user cancelled the file picker")]
    Cancelled,
    #[error("read file {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
