//! Read-only ZIP handler. Parses the central directory on mount and
//! serves entries by decompressing on demand into an in-memory cursor.
//!
//! This is the reference native handler. Once the wasm plugin pipeline
//! is in place a wasm-hosted zip plugin will take over and this will
//! stay around as a test oracle.

use std::io::Cursor;
use std::sync::Arc;

use fskit::FileOpener;
use fskit::Metadata;
use fskit::ReadOnlyVfs;
use fskit::VfsTreeBuilder;
use hxy_core::ByteLen;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;

use crate::capabilities::VfsCapabilities;
use crate::error::HandlerError;
use crate::handler::MountedVfs;
use crate::handler::VfsHandler;

pub struct ZipHandler;

impl ZipHandler {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ZipHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl VfsHandler for ZipHandler {
    fn name(&self) -> &str {
        "zip"
    }

    fn matches(&self, head: &[u8]) -> bool {
        // Zip local-file and end-of-central-dir signatures.
        head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") || head.starts_with(b"PK\x07\x08")
    }

    fn mount(&self, source: Arc<dyn HexSource>) -> Result<MountedVfs, HandlerError> {
        // v1 reads the whole archive into memory. Streaming support
        // lands when we grow past the "small archive" target.
        let bytes = load_all(&*source)?;
        let bytes: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
        let archive = zip::ZipArchive::new(Cursor::new(bytes.clone()))
            .map_err(|e| HandlerError::Malformed(format!("open archive: {e}")))?;

        let mut builder = VfsTreeBuilder::<ZipEntryMeta>::new();
        for i in 0..archive.len() {
            // Re-borrow via an immutable clone so we can build the tree
            // without holding a mutable archive borrow across iterations.
            let mut scratch = archive.clone();
            let entry = scratch.by_index(i).map_err(|e| HandlerError::Malformed(format!("zip entry {i}: {e}")))?;
            let name = entry.name().to_string();
            if entry.is_dir() {
                builder = builder.insert_dir(strip_trailing_slash(&name), None);
            } else {
                let meta = ZipEntryMeta { index: i, size: entry.size() };
                builder = builder.insert(name, meta);
            }
        }
        let tree = builder.build();
        let opener = ZipOpener { bytes };
        let fs: Box<dyn vfs::FileSystem> = Box::new(ReadOnlyVfs::new(tree, opener));
        Ok(MountedVfs { fs, capabilities: VfsCapabilities::READ_ONLY, writer: None })
    }
}

#[derive(Debug, Clone)]
struct ZipEntryMeta {
    index: usize,
    size: u64,
}

impl Metadata for ZipEntryMeta {
    fn len(&self) -> u64 {
        self.size
    }
}

#[derive(Debug)]
struct ZipOpener {
    bytes: Arc<[u8]>,
}

impl FileOpener<ZipEntryMeta> for ZipOpener {
    fn open(&self, meta: &ZipEntryMeta) -> vfs::VfsResult<Box<dyn vfs::SeekAndRead + Send>> {
        use std::io::Read;
        let mut archive = zip::ZipArchive::new(Cursor::new(self.bytes.clone()))
            .map_err(|e| vfs::VfsError::from(vfs::error::VfsErrorKind::Other(format!("reopen archive: {e}"))))?;
        let mut entry = archive
            .by_index(meta.index)
            .map_err(|e| vfs::VfsError::from(vfs::error::VfsErrorKind::Other(format!("by_index: {e}"))))?;
        let mut buf = Vec::with_capacity(meta.size as usize);
        entry
            .read_to_end(&mut buf)
            .map_err(|e| vfs::VfsError::from(vfs::error::VfsErrorKind::Other(format!("decompress: {e}"))))?;
        Ok(Box::new(Cursor::new(buf)))
    }
}

fn strip_trailing_slash(path: &str) -> &str {
    path.strip_suffix('/').unwrap_or(path)
}

fn load_all(source: &dyn HexSource) -> Result<Vec<u8>, HandlerError> {
    let len = source.len();
    if len.get() == 0 {
        return Ok(Vec::new());
    }
    let range = ByteRange::new(ByteOffset::new(0), ByteOffset::new(len.get()))
        .map_err(|e| HandlerError::Internal(e.to_string()))?;
    let _ = ByteLen::ZERO; // silence unused import warning on some builds
    source.read(range).map_err(|e| HandlerError::SourceIo(std::io::Error::other(e.to_string())))
}
