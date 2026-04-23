use std::sync::Arc;

use hxy_core::HexSource;
use vfs::FileSystem;

use crate::capabilities::VfsCapabilities;
use crate::error::HandlerError;

/// Something that turns a byte source into a browsable VFS. Implemented
/// by native handlers today and by wasm plugins once that machinery is
/// in place.
pub trait VfsHandler: Send + Sync {
    /// Stable short name for the plugin — used in logs / UI badges.
    fn name(&self) -> &str;

    /// Fast format check against the source's first few bytes. Called
    /// once per file open to decide whether the "Browse" command should
    /// be enabled. Returning true does not mean `mount` will succeed;
    /// the full mount may still reject malformed input.
    fn matches(&self, head: &[u8]) -> bool;

    /// Mount the source and return a `MountedVfs`. Plugins are expected
    /// to do the minimum work here (read a central directory, open a
    /// handle) rather than prefetching entries.
    fn mount(&self, source: Arc<dyn HexSource>) -> Result<MountedVfs, HandlerError>;
}

/// A mounted VFS plus the capability flags the handler reports. Held
/// behind an `Arc` on the file tab so the tree panel and any open
/// child-tabs can share it.
pub struct MountedVfs {
    pub fs: Box<dyn FileSystem>,
    pub capabilities: VfsCapabilities,
}
