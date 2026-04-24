//! [`PatchedSource`]: a [`HexSource`] that overlays a [`suture::Patch`]
//! on top of a base source.
//!
//! Readers see splice-modified bytes through the same `HexSource`
//! API the base exposes, so the hex view, inspector, template
//! runner, and exporter all transparently pick up edits without
//! needing a separate "modified" code path.

use std::sync::Arc;
use std::sync::RwLock;

use suture::Patch;

use crate::error::Error;
use crate::error::Result;
use crate::geometry::ByteLen;
use crate::geometry::ByteOffset;
use crate::geometry::ByteRange;
use crate::source::HexSource;

/// A [`HexSource`] that returns `base`'s bytes with `patch` applied.
///
/// The patch is held behind an `RwLock` so the UI can mutate it
/// (recording a write) while readers concurrently issue
/// [`HexSource::read`]. Reads take a read-lock on the patch and
/// release it before returning, so the lock is never held across UI
/// frames.
pub struct PatchedSource {
    base: Arc<dyn HexSource>,
    patch: Arc<RwLock<Patch>>,
}

impl PatchedSource {
    pub fn new(base: Arc<dyn HexSource>) -> Self {
        Self::with_patch(base, Patch::new())
    }

    pub fn with_patch(base: Arc<dyn HexSource>, patch: Patch) -> Self {
        Self { base, patch: Arc::new(RwLock::new(patch)) }
    }

    /// Shared handle to the underlying patch. The same `Arc` is
    /// returned on every call so multiple consumers (the editor, the
    /// auto-save sidecar, the dirty-indicator) observe the same
    /// state.
    pub fn patch(&self) -> Arc<RwLock<Patch>> {
        Arc::clone(&self.patch)
    }

    pub fn base(&self) -> &Arc<dyn HexSource> {
        &self.base
    }

    pub fn is_dirty(&self) -> bool {
        !self.patch.read().expect("patch lock poisoned").is_empty()
    }
}

impl HexSource for PatchedSource {
    fn len(&self) -> ByteLen {
        let patch = self.patch.read().expect("patch lock poisoned");
        ByteLen(patch.output_len(self.base.len().get()))
    }

    fn read(&self, range: ByteRange) -> Result<Vec<u8>> {
        let patch = self.patch.read().expect("patch lock poisoned");
        if patch.is_empty() {
            return self.base.read(range);
        }
        let want_start = range.start().get();
        let want_end = range.end().get();
        let total_len = patch.output_len(self.base.len().get());
        if want_end > total_len {
            return Err(Error::OutOfBounds { range, len: ByteOffset(total_len) });
        }
        let mut out = Vec::with_capacity((want_end - want_start) as usize);

        let source_len = self.base.len().get();
        let mut output_cursor: u64 = 0;
        let mut source_cursor: u64 = 0;

        for op in patch.ops() {
            // Pre-splice base segment: source[source_cursor..op.offset]
            let pre_len = op.offset.saturating_sub(source_cursor);
            if pre_len > 0 {
                self.copy_segment_from_base(
                    source_cursor,
                    pre_len,
                    output_cursor,
                    want_start,
                    want_end,
                    &mut out,
                )?;
                output_cursor += pre_len;
            }
            // New bytes from the splice
            let new_len = op.new_bytes.len() as u64;
            if new_len > 0 {
                copy_segment_from_slice(&op.new_bytes, output_cursor, want_start, want_end, &mut out);
                output_cursor += new_len;
            }
            source_cursor = op.offset + op.old_len;
        }
        // Trailing base segment after the last splice.
        if source_cursor < source_len {
            let trail_len = source_len - source_cursor;
            self.copy_segment_from_base(source_cursor, trail_len, output_cursor, want_start, want_end, &mut out)?;
        }
        Ok(out)
    }
}

impl PatchedSource {
    fn copy_segment_from_base(
        &self,
        source_offset: u64,
        len: u64,
        output_offset: u64,
        want_start: u64,
        want_end: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let seg_end = output_offset + len;
        if seg_end <= want_start || output_offset >= want_end {
            return Ok(());
        }
        let local_start = want_start.saturating_sub(output_offset);
        let local_end = (want_end - output_offset).min(len);
        let src_start = source_offset + local_start;
        let src_end = source_offset + local_end;
        let bytes = self.base.read(ByteRange::new(ByteOffset(src_start), ByteOffset(src_end)).expect("valid range"))?;
        out.extend_from_slice(&bytes);
        Ok(())
    }
}

fn copy_segment_from_slice(bytes: &[u8], output_offset: u64, want_start: u64, want_end: u64, out: &mut Vec<u8>) {
    let len = bytes.len() as u64;
    let seg_end = output_offset + len;
    if seg_end <= want_start || output_offset >= want_end {
        return;
    }
    let local_start = want_start.saturating_sub(output_offset) as usize;
    let local_end = ((want_end - output_offset).min(len)) as usize;
    out.extend_from_slice(&bytes[local_start..local_end]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemorySource;

    fn range(start: u64, end: u64) -> ByteRange {
        ByteRange::new(ByteOffset(start), ByteOffset(end)).unwrap()
    }

    #[test]
    fn empty_patch_passes_through() {
        let base: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![1, 2, 3, 4, 5]));
        let p = PatchedSource::new(base);
        assert_eq!(p.len().get(), 5);
        assert_eq!(p.read(range(1, 4)).unwrap(), vec![2, 3, 4]);
        assert!(!p.is_dirty());
    }

    #[test]
    fn write_replaces_in_place() {
        let base: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![1, 2, 3, 4, 5]));
        let p = PatchedSource::new(base);
        p.patch().write().unwrap().write(2, vec![0xAA, 0xBB]).unwrap();
        assert_eq!(p.read(range(0, 5)).unwrap(), vec![1, 2, 0xAA, 0xBB, 5]);
        assert_eq!(p.len().get(), 5);
        assert!(p.is_dirty());
    }

    #[test]
    fn insert_grows_output() {
        let base: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![1, 2, 3, 4]));
        let p = PatchedSource::new(base);
        p.patch().write().unwrap().insert(2, vec![0xAA, 0xBB]).unwrap();
        assert_eq!(p.len().get(), 6);
        assert_eq!(p.read(range(0, 6)).unwrap(), vec![1, 2, 0xAA, 0xBB, 3, 4]);
    }

    #[test]
    fn delete_shrinks_output() {
        let base: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![1, 2, 3, 4, 5]));
        let p = PatchedSource::new(base);
        p.patch().write().unwrap().delete(1, 2).unwrap();
        assert_eq!(p.len().get(), 3);
        assert_eq!(p.read(range(0, 3)).unwrap(), vec![1, 4, 5]);
    }

    #[test]
    fn read_inside_a_splice() {
        let base: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![1, 2, 3, 4, 5, 6, 7, 8]));
        let p = PatchedSource::new(base);
        p.patch().write().unwrap().write(2, vec![0xAA, 0xBB, 0xCC]).unwrap();
        assert_eq!(p.read(range(3, 5)).unwrap(), vec![0xBB, 0xCC]);
    }

    #[test]
    fn out_of_bounds_read_errors() {
        let base: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![1, 2, 3]));
        let p = PatchedSource::new(base);
        assert!(p.read(range(0, 10)).is_err());
    }
}
