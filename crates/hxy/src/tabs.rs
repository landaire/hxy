//! Dock tab identifiers and rendering.

use serde::Deserialize;
use serde::Serialize;

use crate::file::FileId;
#[cfg(not(target_arch = "wasm32"))]
use crate::file::MountId;

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
    /// A live plugin VFS mount. Renders only the VFS tree; clicking an
    /// entry opens a regular `File` tab. The `MountId` indexes into
    /// `HxyApp::mounts`.
    #[cfg(not(target_arch = "wasm32"))]
    PluginMount(MountId),
    /// Cross-file search results. Lists every match across every open
    /// file. Clicking jumps to the file + offset; the active match is
    /// highlighted in the corresponding hex view via its selection.
    #[cfg(not(target_arch = "wasm32"))]
    SearchResults,
}

impl Tab {
    pub fn is_file(&self, id: FileId) -> bool {
        matches!(self, Tab::File(fid) if *fid == id)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Serialize for MountId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.get())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<'de> Deserialize<'de> for MountId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        u64::deserialize(d).map(MountId::new)
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
