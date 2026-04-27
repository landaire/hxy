//! `vfs::FileSystem` implementation that routes calls through a
//! plugin's `mount` resource. Read-only -- writes return
//! `NotSupported`.
//!
//! `open_file` returns a [`RangedReader`] that satisfies the
//! `Read + Seek` contract by pulling block-aligned ranges from the
//! plugin's `read-range` method and caching them in a shared
//! [`FileBlockCache`]. The cache is keyed by `(path, block_offset)`
//! and capped by total bytes; least-recently-used blocks evict
//! when the budget is exceeded. This means scrolling the hex view
//! (which re-reads the same neighborhood every paint) doesn't
//! round-trip the kit on every frame.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use hxy_vfs::VfsWriter;
use vfs::FileSystem;
use vfs::SeekAndRead;
use vfs::SeekAndWrite;
use vfs::VfsError;
use vfs::VfsFileType;
use vfs::VfsMetadata;
use vfs::VfsResult;
use vfs::error::VfsErrorKind;

use crate::bindings::handler_world::exports::hxy::vfs::handler::FileType as WitFileType;
use crate::handler::PluginFileSystem;
use crate::handler::PluginFsInner;

/// Block size for the file-content LRU. Big enough to amortise the
/// per-call overhead of crossing the wasm boundary + a network
/// round trip on remote VFSes; small enough that random-access
/// reads in a multi-megabyte file don't pull more than a couple of
/// blocks at a time.
pub(crate) const FILE_BLOCK_SIZE: u64 = 64 * 1024;

/// Total bytes of file data the per-mount LRU keeps in memory before
/// evicting least-recently-used blocks. Tuned for browsing a few
/// large files at a time without runaway memory.
pub(crate) const FILE_BLOCK_CACHE_BUDGET_BYTES: usize = 64 * 1024 * 1024;

impl fmt::Debug for PluginFileSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginFileSystem").field("plugin", &self.plugin_name).finish()
    }
}

impl FileSystem for PluginFileSystem {
    fn read_dir(&self, path: &str) -> VfsResult<Box<dyn Iterator<Item = String> + Send>> {
        // Cache hit short-circuits the wasm + wire round trip. The
        // VFS panel re-walks the expanded tree on every frame, so
        // without this a remote-VFS plugin (xbox-neighborhood) gets
        // hammered for each render.
        if let Ok(cache) = self.dir_cache.lock()
            && let Some(entries) = cache.get(path).cloned()
        {
            return Ok(Box::new(entries.into_iter()));
        }
        let started = std::time::Instant::now();
        let mut g = self.inner.lock().map_err(poisoned)?;
        let g = &mut *g;
        let mount_guest = g.plugin.hxy_vfs_handler().mount();
        let result = mount_guest
            .call_read_dir(&mut g.store, g.mount, path)
            .map_err(|e| other(format!("plugin read-dir call trap: {e}")))?
            .map_err(|e| other(format!("plugin read-dir: {e}")));
        match &result {
            Ok(entries) => tracing::debug!(
                target: "hxy_plugin_host::mount",
                plugin = %self.plugin_name,
                path = %path,
                count = entries.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "read_dir ok"
            ),
            Err(e) => tracing::warn!(
                target: "hxy_plugin_host::mount",
                plugin = %self.plugin_name,
                path = %path,
                error = %e,
                "read_dir err"
            ),
        }
        let entries = result?;
        if let Ok(mut cache) = self.dir_cache.lock() {
            cache.insert(path.to_owned(), entries.clone());
        }
        Ok(Box::new(entries.into_iter()))
    }

    fn create_dir(&self, _path: &str) -> VfsResult<()> {
        Err(VfsError::from(VfsErrorKind::NotSupported))
    }

    fn open_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndRead + Send>> {
        // Need the file size up front for `Seek` semantics
        // (`SeekFrom::End` resolves against it). Walking through
        // our own metadata() picks up the cached value when the
        // VFS panel just listed the parent.
        let meta = self.metadata(path)?;
        if meta.file_type != VfsFileType::File {
            return Err(VfsError::from(VfsErrorKind::Other(format!("open_file on non-file: {path}"))));
        }
        Ok(Box::new(RangedReader {
            inner: Arc::clone(&self.inner),
            block_cache: Arc::clone(&self.block_cache),
            plugin_name: self.plugin_name.clone(),
            path: path.to_owned(),
            total_size: meta.len,
            pos: 0,
        }))
    }

    fn create_file(&self, _path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
        Err(VfsError::from(VfsErrorKind::NotSupported))
    }

    fn append_file(&self, _path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
        Err(VfsError::from(VfsErrorKind::NotSupported))
    }

    fn metadata(&self, path: &str) -> VfsResult<VfsMetadata> {
        // Cache hit. The tree-walk hits this once per child per
        // frame; even though the plugin probably has its own per-
        // mount cache, every miss still crosses the wasm boundary,
        // which adds up at 60 fps.
        if let Ok(cache) = self.meta_cache.lock()
            && let Some(&(file_type, len)) = cache.get(path)
        {
            return Ok(VfsMetadata { file_type, len, created: None, modified: None, accessed: None });
        }
        let started = std::time::Instant::now();
        let mut g = self.inner.lock().map_err(poisoned)?;
        let g = &mut *g;
        let mount_guest = g.plugin.hxy_vfs_handler().mount();
        let result = mount_guest
            .call_metadata(&mut g.store, g.mount, path)
            .map_err(|e| other(format!("plugin metadata call trap: {e}")))?
            .map_err(|e| other(format!("plugin metadata: {e}")));
        match &result {
            Ok(meta) => tracing::debug!(
                target: "hxy_plugin_host::mount",
                plugin = %self.plugin_name,
                path = %path,
                len = meta.length,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "metadata ok"
            ),
            Err(e) => tracing::warn!(
                target: "hxy_plugin_host::mount",
                plugin = %self.plugin_name,
                path = %path,
                error = %e,
                "metadata err"
            ),
        }
        let meta = result?;
        let file_type = match meta.file_type {
            WitFileType::RegularFile => VfsFileType::File,
            WitFileType::Directory => VfsFileType::Directory,
        };
        let len = meta.length;
        if let Ok(mut cache) = self.meta_cache.lock() {
            cache.insert(path.to_owned(), (file_type, len));
        }
        Ok(VfsMetadata { file_type, len, created: None as Option<SystemTime>, modified: None, accessed: None })
    }

    fn exists(&self, path: &str) -> VfsResult<bool> {
        match self.metadata(path) {
            Ok(_) => Ok(true),
            Err(e) => match e.kind() {
                VfsErrorKind::FileNotFound => Ok(false),
                _ => Ok(false),
            },
        }
    }

    fn remove_file(&self, _path: &str) -> VfsResult<()> {
        Err(VfsError::from(VfsErrorKind::NotSupported))
    }

    fn remove_dir(&self, _path: &str) -> VfsResult<()> {
        Err(VfsError::from(VfsErrorKind::NotSupported))
    }
}

/// Byte-bounded LRU of fixed-size file blocks. Most-recent at the
/// front; eviction pops from the back when total size exceeds
/// `max_bytes`. Linear-time `get` / `put` (entry list typically
/// holds a few hundred blocks across a session, well within the
/// budget for a O(N) scan).
pub(crate) struct FileBlockCache {
    /// `((path, block_offset), bytes)` ordered most-recent-first.
    entries: VecDeque<((String, u64), Vec<u8>)>,
    size_bytes: usize,
    max_bytes: usize,
}

impl FileBlockCache {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self { entries: VecDeque::new(), size_bytes: 0, max_bytes }
    }

    /// Look up a block. Hit -> moves to front (marking just-used) +
    /// returns a clone of the bytes.
    fn get(&mut self, path: &str, block_offset: u64) -> Option<Vec<u8>> {
        let pos = self.entries.iter().position(|((p, off), _)| p == path && *off == block_offset)?;
        let entry = self.entries.remove(pos)?;
        let bytes = entry.1.clone();
        self.entries.push_front(entry);
        Some(bytes)
    }

    /// Drop every cached block for `path` whose byte range
    /// overlaps `[write_offset, write_offset + write_len)`. Called
    /// after a successful `write_range` so subsequent reads re-
    /// fetch the modified pages instead of serving stale bytes
    /// from before the write.
    pub(crate) fn invalidate_overlapping(&mut self, path: &str, write_offset: u64, write_len: u64) {
        if write_len == 0 {
            return;
        }
        let write_end = write_offset.saturating_add(write_len);
        self.entries.retain(|((p, block_off), bytes)| {
            if p != path {
                return true;
            }
            let block_end = block_off.saturating_add(bytes.len() as u64);
            // Keep the block iff it sits entirely outside the
            // written range.
            *block_off >= write_end || block_end <= write_offset
        });
        // Recompute size_bytes after retain rather than tracking
        // deltas inline; cheaper than the bookkeeping for the
        // expected small number of evictions.
        self.size_bytes = self.entries.iter().map(|(_, b)| b.len()).sum();
    }

    /// Insert a block, evicting from the back until the size budget
    /// fits. A single block larger than `max_bytes` still goes in
    /// (it'll evict on the next put); the alternative would be to
    /// silently drop, which surprises debuggers.
    fn put(&mut self, path: String, block_offset: u64, bytes: Vec<u8>) {
        if let Some(pos) = self.entries.iter().position(|((p, off), _)| p == &path && *off == block_offset)
            && let Some(old) = self.entries.remove(pos)
        {
            self.size_bytes = self.size_bytes.saturating_sub(old.1.len());
        }
        let n = bytes.len();
        self.entries.push_front(((path, block_offset), bytes));
        self.size_bytes = self.size_bytes.saturating_add(n);
        while self.size_bytes > self.max_bytes && self.entries.len() > 1 {
            if let Some(evicted) = self.entries.pop_back() {
                self.size_bytes = self.size_bytes.saturating_sub(evicted.1.len());
            }
        }
    }
}

/// `Read + Seek` adapter returned by `PluginFileSystem::open_file`.
/// Pulls block-aligned ranges via the plugin's `read-range`,
/// caches them in the shared `FileBlockCache`, and serves the
/// caller's read window from the cache. Cheap to drop; expensive
/// only on cache miss (one wasm + wire round trip per missed
/// block).
pub(crate) struct RangedReader {
    inner: Arc<Mutex<PluginFsInner>>,
    block_cache: Arc<Mutex<FileBlockCache>>,
    plugin_name: String,
    path: String,
    total_size: u64,
    pos: u64,
}

impl RangedReader {
    /// Fetch a block from the cache, or via `read-range` on miss.
    /// The block returned starts at `block_offset` and is at most
    /// `FILE_BLOCK_SIZE` bytes; the last block of a file is
    /// shorter.
    fn fetch_block(&self, block_offset: u64) -> io::Result<Vec<u8>> {
        if let Ok(mut cache) = self.block_cache.lock()
            && let Some(bytes) = cache.get(&self.path, block_offset)
        {
            return Ok(bytes);
        }
        let block_end = block_offset.saturating_add(FILE_BLOCK_SIZE).min(self.total_size);
        let length = block_end.saturating_sub(block_offset);
        if length == 0 {
            return Ok(Vec::new());
        }
        let started = std::time::Instant::now();
        let mut g = self.inner.lock().map_err(|_| io::Error::other("plugin filesystem mutex poisoned"))?;
        let g = &mut *g;
        let mount_guest = g.plugin.hxy_vfs_handler().mount();
        let result = mount_guest
            .call_read_range(&mut g.store, g.mount, &self.path, block_offset, length)
            .map_err(|e| io::Error::other(format!("read-range call trap: {e}")))?
            .map_err(|e| io::Error::other(format!("plugin read-range: {e}")));
        match &result {
            Ok(bytes) => tracing::debug!(
                target: "hxy_plugin_host::mount",
                plugin = %self.plugin_name,
                path = %self.path,
                offset = block_offset,
                requested = length,
                got = bytes.len(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "read_range ok"
            ),
            Err(e) => tracing::warn!(
                target: "hxy_plugin_host::mount",
                plugin = %self.plugin_name,
                path = %self.path,
                offset = block_offset,
                length = length,
                error = %e,
                "read_range err"
            ),
        }
        let bytes = result?;
        if let Ok(mut cache) = self.block_cache.lock() {
            cache.put(self.path.clone(), block_offset, bytes.clone());
        }
        Ok(bytes)
    }
}

impl Read for RangedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let want = buf.len() as u64;
        let remaining_in_file = self.total_size.saturating_sub(self.pos);
        let to_read = want.min(remaining_in_file);
        if to_read == 0 {
            return Ok(0);
        }
        // Pull from one or more blocks until we've filled the
        // caller's buffer. Each loop iteration consumes a single
        // block (at most), which keeps the cache LRU policy
        // accurate (most-recently-used = the block we just
        // touched).
        let mut written = 0usize;
        while (written as u64) < to_read {
            let target_offset = self.pos + written as u64;
            let block_offset = (target_offset / FILE_BLOCK_SIZE) * FILE_BLOCK_SIZE;
            let block_inner = (target_offset - block_offset) as usize;
            let block = self.fetch_block(block_offset)?;
            if block_inner >= block.len() {
                // Short block (EOF mid-block). Caller gets what we
                // had so far; subsequent reads return Ok(0).
                break;
            }
            let copy_len = (block.len() - block_inner).min((to_read as usize).saturating_sub(written));
            buf[written..written + copy_len].copy_from_slice(&block[block_inner..block_inner + copy_len]);
            written += copy_len;
        }
        self.pos += written as u64;
        Ok(written)
    }
}

impl Seek for RangedReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => (self.pos as i128).wrapping_add(n as i128),
            SeekFrom::End(n) => (self.total_size as i128).wrapping_add(n as i128),
        };
        if new_pos < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

fn other(msg: String) -> VfsError {
    VfsError::from(VfsErrorKind::Other(msg))
}

fn poisoned<T>(_e: std::sync::PoisonError<T>) -> VfsError {
    VfsError::from(VfsErrorKind::Other("plugin filesystem mutex poisoned".to_owned()))
}

/// In-place writeback handle. Holds the same `Arc<Mutex<PluginFsInner>>`
/// the [`PluginFileSystem`] does so writes can lock the wasmtime
/// `Store`, call into the plugin, and invalidate the matching read
/// blocks in the shared cache without going through the
/// `vfs::FileSystem` trait (which doesn't model in-place writes
/// natively -- only `create_file` and `append_file`).
///
/// Construction is paired with `PluginFileSystem` in
/// [`crate::handler::PluginHandler::mount_source`] /
/// [`crate::handler::PluginHandler::mount_by_token`].
pub(crate) struct PluginWriter {
    pub(crate) inner: Arc<Mutex<crate::handler::PluginFsInner>>,
    pub(crate) block_cache: Arc<Mutex<FileBlockCache>>,
    pub(crate) plugin_name: String,
}

impl VfsWriter for PluginWriter {
    fn write_range(&self, path: &str, offset: u64, data: &[u8]) -> Result<u64, String> {
        let started = std::time::Instant::now();
        let mut g = self.inner.lock().map_err(|_| "plugin filesystem mutex poisoned".to_owned())?;
        let g = &mut *g;
        let mount_guest = g.plugin.hxy_vfs_handler().mount();
        let result = mount_guest
            .call_write_range(&mut g.store, g.mount, path, offset, data)
            .map_err(|e| format!("plugin write-range trap: {e}"))?
            .map_err(|e| format!("plugin write-range: {e}"));
        match &result {
            Ok(written) => tracing::info!(
                target: "hxy_plugin_host::mount",
                plugin = %self.plugin_name,
                path = %path,
                offset,
                requested = data.len(),
                written,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "write_range ok"
            ),
            Err(e) => tracing::warn!(
                target: "hxy_plugin_host::mount",
                plugin = %self.plugin_name,
                path = %path,
                offset,
                length = data.len(),
                error = %e,
                "write_range err"
            ),
        }
        let written = result?;
        // Invalidate any cached read blocks that overlap the
        // written range so subsequent `read_range` calls don't
        // hand back stale pre-write bytes. The whole-file
        // metadata cache stays valid (size unchanged).
        if written > 0
            && let Ok(mut cache) = self.block_cache.lock()
        {
            cache.invalidate_overlapping(path, offset, written);
        }
        Ok(written)
    }
}
