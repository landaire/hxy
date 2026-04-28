use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// Stable identifier for an anonymous (scratch) tab. Monotonic
/// within an app install; paired with a persisted byte file on
/// disk so the tab survives restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AnonymousId(pub u64);

impl AnonymousId {
    pub fn get(self) -> u64 {
        self.0
    }
}

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
    /// A scratch / untitled buffer not tied to any on-disk file.
    /// Bytes are persisted under the app's data dir keyed by `id`;
    /// `title` keeps the user-visible name (e.g. `Untitled 3`)
    /// stable across restarts.
    Anonymous { id: AnonymousId, title: String },
    /// A tab whose VFS comes from a plugin's `mount-by-token`. The
    /// `plugin_name` matches what the plugin's WIT `name()` returns;
    /// `token` is whatever opaque value the plugin handed back via
    /// its `Mount` invoke outcome. Restoration is best-effort: if
    /// the plugin is no longer installed or its `mount_by_token`
    /// rejects the saved token, the host drops the tab from
    /// `open_tabs` instead of leaving an orphaned shell.
    PluginMount { plugin_name: String, token: String, title: String },
}

impl TabSource {
    /// Depth of the nesting chain. `Filesystem`, `Anonymous`, and
    /// `PluginMount` are depth 0; each nested `VfsEntry` adds one.
    pub fn depth(&self) -> usize {
        match self {
            Self::Filesystem(_) | Self::Anonymous { .. } | Self::PluginMount { .. } => 0,
            Self::VfsEntry { parent, .. } => parent.depth() + 1,
        }
    }

    /// The root filesystem path at the bottom of any nesting.
    /// `None` for `Anonymous` and `PluginMount` tabs (no on-disk
    /// origin).
    pub fn root_path(&self) -> Option<&PathBuf> {
        match self {
            Self::Filesystem(p) => Some(p),
            Self::VfsEntry { parent, .. } => parent.root_path(),
            Self::Anonymous { .. } | Self::PluginMount { .. } => None,
        }
    }

    /// Lowercased extension of the leaf entry the tab actually shows.
    /// Differs from `root_path().extension()` for VFS-nested entries:
    /// opening `image.png` from inside `assets.zip` reports `png`
    /// here, but the root path's extension is `zip`. Used by the
    /// template suggester so an extension-matched `.bt` / `.hexpat`
    /// fires for the inner format, not the container.
    pub fn leaf_extension(&self) -> Option<String> {
        match self {
            Self::Filesystem(p) => p.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase()),
            Self::VfsEntry { entry_path, .. } => {
                let leaf = entry_path.rsplit('/').find(|s| !s.is_empty())?;
                let dot = leaf.rfind('.')?;
                Some(leaf[dot + 1..].to_ascii_lowercase())
            }
            Self::Anonymous { title, .. } | Self::PluginMount { title, .. } => {
                let dot = title.rfind('.')?;
                Some(title[dot + 1..].to_ascii_lowercase())
            }
        }
    }

    /// A short human label: for `Filesystem` it's the file name; for
    /// `VfsEntry` it's the last segment of `entry_path`; for
    /// `Anonymous` and `PluginMount` it's the stored title.
    pub fn display_name(&self) -> String {
        match self {
            Self::Filesystem(p) => {
                p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
            }
            Self::VfsEntry { entry_path, .. } => {
                entry_path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(entry_path).to_owned()
            }
            Self::Anonymous { title, .. } => title.clone(),
            Self::PluginMount { title, .. } => title.clone(),
        }
    }
}
