//! Per-tab open-file state.

use std::path::PathBuf;
use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;
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

/// A loaded file in a tab. The source is kept behind `Arc<dyn HexSource>` so
/// a future async reader can hand the same instance to the UI thread.
pub struct OpenFile {
    pub id: FileId,
    pub display_name: String,
    pub path: Option<PathBuf>,
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
}

impl OpenFile {
    /// Construct from an in-memory buffer — used for initial load of small
    /// files before we have a streaming reader.
    pub fn from_bytes(id: FileId, display_name: impl Into<String>, path: Option<PathBuf>, bytes: Vec<u8>) -> Self {
        Self {
            id,
            display_name: display_name.into(),
            path,
            source: Arc::new(MemorySource::new(bytes)),
            selection: None,
            hovered: None,
            scroll_offset: 0.0,
            pending_scroll: None,
        }
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
