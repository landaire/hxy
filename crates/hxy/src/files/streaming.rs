//! Streaming [`HexSource`] implementations.
//!
//! Filesystem files and VFS entries used to be slurped into a
//! `Vec<u8>` at open time, which kept opens O(file_size) in
//! both wall-clock and RAM. These wrappers replace that with
//! lazy byte-range reads on demand: the open path captures the
//! length and a backing handle, the hex view's per-row reads
//! pull through a bounded byte cache, and the polling worker
//! gets a cheap way to re-check arbitrary ranges without
//! holding a full copy in memory.

#![cfg(not(target_arch = "wasm32"))]

use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use fskit::ReadAt;
use fskit::cache::CachingReader;
use hxy_core::ByteLen;
use hxy_core::HexSource;
use hxy_core::ReadAtSource;
use hxy_vfs::MountedVfs;
use hxy_vfs::vfs::SeekAndRead;

/// Per-source memory budget for the byte-range cache. Sized so
/// 100 open tabs stay well under a GiB in aggregate while still
/// keeping the hex view's per-row reads (16-byte chunks) hot
/// for the visible window.
pub const STREAM_CACHE_BUDGET: usize = 4 * 1024 * 1024;

/// Disk-backed [`ReadAt`] over a single open `File`. Holds the
/// FD inside a `Mutex` so concurrent `read_at` calls serialise
/// on a `seek + read_exact` pair without re-opening the file
/// every time. The path is retained for diagnostic logging.
pub struct FilePread {
    path: PathBuf,
    file: Mutex<File>,
}

impl FilePread {
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let file = File::open(&path)?;
        Ok(Self { path, file: Mutex::new(file) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl ReadAt for FilePread {
    fn read_at(&self, _ctx: &(), range: Range<u64>) -> std::io::Result<impl AsRef<[u8]>> {
        let mut file = self.file.lock().expect("file mutex poisoned");
        let want = range.end.saturating_sub(range.start) as usize;
        file.seek(SeekFrom::Start(range.start))?;
        let mut buf = vec![0u8; want];
        file.read_exact(&mut buf)?;
        Ok(buf)
    }
}

/// Build a streaming [`HexSource`] over `path` plus the
/// captured length. Returns the length separately so callers
/// can stash it in tab-restoration state without paying for
/// another `metadata()` round-trip.
pub fn open_filesystem(path: &Path) -> std::io::Result<(Arc<dyn HexSource>, u64)> {
    let len = std::fs::metadata(path)?.len();
    let pread = FilePread::open(path)?;
    let cached = CachingReader::with_mem_limit(pread, STREAM_CACHE_BUDGET);
    let source: Arc<dyn HexSource> = Arc::new(ReadAtSource::new(cached, ByteLen::new(len)));
    Ok((source, len))
}

/// VFS-backed [`ReadAt`] over a single entry. Holds one
/// `SeekAndRead` handle in a `Mutex` so each read is a
/// `seek + read_exact` rather than a fresh `open_file` call --
/// some handlers (zip, plugin streams) charge real CPU for
/// each open. The mount Arc keeps the underlying handler
/// alive for the source's lifetime regardless of whether the
/// host workspace closes first.
pub struct VfsPread {
    mount: Arc<MountedVfs>,
    entry_path: String,
    /// Cached open handle. Lazily populated on the first
    /// `read_at`; some VFS impls open eagerly even when no
    /// bytes are requested, so we delay that work until the
    /// hex view actually queries a range.
    handle: Mutex<Option<Box<dyn SeekAndRead + Send>>>,
}

impl VfsPread {
    pub fn open(mount: Arc<MountedVfs>, entry_path: String) -> Self {
        Self { mount, entry_path, handle: Mutex::new(None) }
    }

    pub fn entry_path(&self) -> &str {
        &self.entry_path
    }

    pub fn mount(&self) -> &Arc<MountedVfs> {
        &self.mount
    }
}

impl ReadAt for VfsPread {
    fn read_at(&self, _ctx: &(), range: Range<u64>) -> std::io::Result<impl AsRef<[u8]>> {
        let mut guard = self.handle.lock().expect("vfs handle mutex poisoned");
        if guard.is_none() {
            let stream = self
                .mount
                .fs
                .open_file(&self.entry_path)
                .map_err(|e| std::io::Error::other(format!("open vfs entry {}: {e}", self.entry_path)))?;
            *guard = Some(stream);
        }
        let stream = guard.as_mut().expect("just populated");
        let want = range.end.saturating_sub(range.start) as usize;
        stream.seek(SeekFrom::Start(range.start))?;
        let mut buf = vec![0u8; want];
        stream.read_exact(&mut buf)?;
        Ok(buf)
    }
}

/// Build a streaming [`HexSource`] over the VFS entry at
/// `entry_path` inside `mount`. Returns the resolved length
/// separately so callers don't need a follow-up `metadata`
/// call.
pub fn open_vfs(mount: Arc<MountedVfs>, entry_path: String) -> std::io::Result<(Arc<dyn HexSource>, u64)> {
    let metadata = mount
        .fs
        .metadata(&entry_path)
        .map_err(|e| std::io::Error::other(format!("vfs metadata for {entry_path}: {e}")))?;
    let len = metadata.len;
    let pread = VfsPread::open(mount, entry_path);
    let cached = CachingReader::with_mem_limit(pread, STREAM_CACHE_BUDGET);
    let source: Arc<dyn HexSource> = Arc::new(ReadAtSource::new(cached, ByteLen::new(len)));
    Ok((source, len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hxy_core::ByteOffset;
    use hxy_core::ByteRange;

    #[test]
    fn file_source_reads_byte_ranges_lazily() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp, b"abcdefghij").unwrap();
        let path = tmp.path().to_path_buf();
        let (source, len) = open_filesystem(&path).unwrap();
        assert_eq!(len, 10);
        assert_eq!(source.len().get(), 10);
        let range = ByteRange::new(ByteOffset::new(2), ByteOffset::new(6)).unwrap();
        assert_eq!(source.read(range).unwrap(), b"cdef");
    }

    #[test]
    fn file_source_caches_repeated_reads() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp, b"hello world").unwrap();
        let (source, _) = open_filesystem(tmp.path()).unwrap();
        let range = ByteRange::new(ByteOffset::new(0), ByteOffset::new(5)).unwrap();
        let first = source.read(range).unwrap();
        let second = source.read(range).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, b"hello");
    }
}
