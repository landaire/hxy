use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// Stable, serialisable identifier for an open tab's byte source.
/// Nesting is explicit so a file inside an archive inside an archive
/// still persists correctly and can be topologically restored on
/// startup (parent opens first, then children).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TabSource {
    /// A regular file on the host filesystem.
    Filesystem(PathBuf),
    /// An entry inside another tab's mounted VFS. `entry_path` is the
    /// VFS path (forward-slash separated) within the parent's mount.
    VfsEntry { parent: Box<TabSource>, entry_path: String },
}

impl TabSource {
    /// Depth of the nesting chain. `Filesystem` is depth 0; each nested
    /// `VfsEntry` adds one.
    pub fn depth(&self) -> usize {
        match self {
            Self::Filesystem(_) => 0,
            Self::VfsEntry { parent, .. } => parent.depth() + 1,
        }
    }

    /// The root filesystem path at the bottom of any nesting. Useful
    /// for display in recents and for grouping tabs by archive.
    pub fn root_path(&self) -> &PathBuf {
        match self {
            Self::Filesystem(p) => p,
            Self::VfsEntry { parent, .. } => parent.root_path(),
        }
    }

    /// A short human label: for `Filesystem` it's the file name; for
    /// `VfsEntry` it's the last segment of `entry_path`.
    pub fn display_name(&self) -> String {
        match self {
            Self::Filesystem(p) => {
                p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
            }
            Self::VfsEntry { entry_path, .. } => {
                entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(entry_path).to_owned()
            }
        }
    }
}
