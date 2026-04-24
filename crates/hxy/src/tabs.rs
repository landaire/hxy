//! Dock tab identifiers and rendering.

use serde::Deserialize;
use serde::Serialize;

use crate::file::FileId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tab {
    Welcome,
    File(FileId),
    Settings,
    /// Append-only log of plugin / template output. Opened from the
    /// View menu; closeable and persists across sessions via the
    /// dock state (but the entries themselves are in-memory only).
    Console,
    /// Datatype inspector: decodes the bytes at the active file tab's
    /// caret into integers / floats / time / color rows. Opened from
    /// the View menu; closeable.
    Inspector,
    /// Plugin manager: browse VFS handlers and template runtimes
    /// installed in the user plugin directories, install new ones
    /// from disk, and delete / rescan.
    Plugins,
}

impl Tab {
    pub fn is_file(&self, id: FileId) -> bool {
        matches!(self, Tab::File(fid) if *fid == id)
    }
}

impl Serialize for FileId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.get())
    }
}

impl<'de> Deserialize<'de> for FileId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        u64::deserialize(d).map(FileId::new)
    }
}
