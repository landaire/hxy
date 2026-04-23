use std::io;
use std::sync::Arc;

use fskit::ReadAt;

use crate::error::Error;
use crate::error::Result;
use crate::geometry::ByteLen;
use crate::geometry::ByteOffset;
use crate::geometry::ByteRange;

/// Read-only access to a byte source, indexed by [`ByteRange`].
///
/// All implementations are required to return exactly `range.len()` bytes on
/// success, or an [`Error::OutOfBounds`] if the range extends past the source.
pub trait HexSource: Send + Sync {
    /// Total length of the source.
    fn len(&self) -> ByteLen;

    /// Return whether the source is empty.
    fn is_empty(&self) -> bool {
        self.len().is_zero()
    }

    /// Read `range` from the source.
    ///
    /// Returns an owned `Vec<u8>` rather than a borrowed slice so callers
    /// don't need to hold a lock across UI frames. Implementations backed by
    /// a contiguous buffer can allocate once per call; file-backed sources
    /// typically buffer each read anyway.
    fn read(&self, range: ByteRange) -> Result<Vec<u8>>;
}

impl<T: HexSource + ?Sized> HexSource for Arc<T> {
    fn len(&self) -> ByteLen {
        (**self).len()
    }
    fn read(&self, range: ByteRange) -> Result<Vec<u8>> {
        (**self).read(range)
    }
}

/// In-memory source backed by a `Vec<u8>`.
#[derive(Debug, Clone)]
pub struct MemorySource {
    bytes: Arc<[u8]>,
}

impl MemorySource {
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self { bytes: bytes.into() }
    }
}

impl HexSource for MemorySource {
    fn len(&self) -> ByteLen {
        ByteLen(self.bytes.len() as u64)
    }

    fn read(&self, range: ByteRange) -> Result<Vec<u8>> {
        let source_len = ByteOffset(self.bytes.len() as u64);
        if range.end() > source_len {
            return Err(Error::OutOfBounds { range, len: source_len });
        }
        let start = range.start().get() as usize;
        let end = range.end().get() as usize;
        Ok(self.bytes[start..end].to_vec())
    }
}

/// Adapter wrapping any [`fskit::ReadAt`] source with a known length.
///
/// Caller supplies the length because `ReadAt` is a pure byte-range read
/// trait and doesn't expose size. Typical users construct this once when
/// opening a file (where the length comes from `std::fs::Metadata` or a
/// `vfs::VfsMetadata`).
pub struct ReadAtSource<R: ReadAt<()> + Send + Sync> {
    inner: R,
    len: ByteLen,
}

impl<R: ReadAt<()> + Send + Sync> ReadAtSource<R> {
    pub fn new(inner: R, len: ByteLen) -> Self {
        Self { inner, len }
    }
}

impl<R: ReadAt<()> + Send + Sync> HexSource for ReadAtSource<R> {
    fn len(&self) -> ByteLen {
        self.len
    }

    fn read(&self, range: ByteRange) -> Result<Vec<u8>> {
        let source_end = ByteOffset(self.len.get());
        if range.end() > source_end {
            return Err(Error::OutOfBounds { range, len: source_end });
        }
        let bytes =
            self.inner.read_at(&(), range.as_u64_range()).map_err(|source: io::Error| Error::Io { range, source })?;
        Ok(bytes.as_ref().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::ByteOffset;

    #[test]
    fn memory_source_reads() {
        let src = MemorySource::new(vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(src.len().get(), 6);
        let range = ByteRange::new(ByteOffset(1), ByteOffset(4)).unwrap();
        assert_eq!(src.read(range).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn memory_source_out_of_bounds() {
        let src = MemorySource::new(vec![0, 1, 2]);
        let range = ByteRange::new(ByteOffset(0), ByteOffset(10)).unwrap();
        match src.read(range) {
            Err(Error::OutOfBounds { .. }) => {}
            other => panic!("expected OutOfBounds, got {other:?}"),
        }
    }
}
