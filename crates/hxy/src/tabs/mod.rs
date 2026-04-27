//! Dock tab identifiers and rendering.

#[cfg(not(target_arch = "wasm32"))]
pub mod pane_pick;
#[cfg(not(target_arch = "wasm32"))]
pub mod persisted_dock;

use serde::Deserialize;
use serde::Serialize;

#[cfg(not(target_arch = "wasm32"))]
use crate::compare::CompareId;
use crate::files::FileId;
#[cfg(not(target_arch = "wasm32"))]
use crate::files::MountId;
use crate::files::WorkspaceId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tab {
    Welcome,
    /// A plain file with no VFS handler -- the dock tab is the hex
    /// view itself, no nested layout. Files that grow VFS structure
    /// (zip, minidump, etc.) are wrapped in `Workspace` instead.
    File(FileId),
    /// A file plus its mounted VFS, rendered as a nested dock area
    /// inside this tab. Indexes into `HxyApp::workspaces`.
    Workspace(WorkspaceId),
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
    /// Side-by-side diff between two byte sources. Indexes into
    /// `HxyApp::compares` for the [`crate::compare::CompareSession`].
    #[cfg(not(target_arch = "wasm32"))]
    Compare(CompareId),
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

impl Serialize for WorkspaceId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.get())
    }
}

impl<'de> Deserialize<'de> for WorkspaceId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        u64::deserialize(d).map(WorkspaceId::new)
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
