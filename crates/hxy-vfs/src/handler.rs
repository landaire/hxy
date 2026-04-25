use std::sync::Arc;

use hxy_core::HexSource;
use vfs::FileSystem;

use crate::capabilities::VfsCapabilities;
use crate::error::HandlerError;

/// Something that turns a byte source into a browsable VFS. Implemented
/// by native handlers today and by wasm plugins once that machinery is
/// in place.
pub trait VfsHandler: Send + Sync {
    /// Stable short name for the plugin -- used in logs / UI badges.
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
    /// Optional in-place writeback handle. `Some` for handlers that
    /// support `write_range` (e.g. xbox-neighborhood's
    /// `/modules/...` and `/memory/...` synthetic dirs); `None` for
    /// read-only mounts. The save flow uses this to push patched
    /// bytes back through the plugin via xbdm `setmem`.
    pub writer: Option<Arc<dyn VfsWriter>>,
}

/// In-place ranged writeback. Sits alongside [`vfs::FileSystem`]
/// rather than being part of it because the upstream trait only
/// has `create_file` / `append_file` and we want to overwrite
/// bytes inside an existing entry without truncating or extending.
///
/// Returns the number of bytes the underlying medium actually
/// wrote. May be less than `data.len()` (e.g. xbdm's `setmem`
/// stops at the first unmapped page); the editor surfaces a
/// partial-write to the user as a warning.
pub trait VfsWriter: Send + Sync {
    fn write_range(&self, path: &str, offset: u64, data: &[u8]) -> Result<u64, String>;
}
