//! Persistent editor state: patch overlay, undo/redo history,
//! coalescing bookkeeping. Compiled only when the `editor` feature
//! is enabled -- consumers who just want a read-only hex view can
//! strip `suture`, `thiserror`, and `tracing` by building with
//! `default-features = false`.

use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use std::time::Instant;

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_core::PatchedSource;
use suture::Patch;
use thiserror::Error;

/// Whether writes through [`crate::HexEditor::request_write`] are
/// accepted. New editors default to [`EditMode::Mutable`]; set
/// [`EditMode::Readonly`] to gate out all write-producing key
/// presses and API calls. Mirrors 010 Editor's edit-mode toggle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EditMode {
    Readonly,
    #[default]
    Mutable,
}

/// How [`crate::HexEditor::type_hex_digit`] /
/// [`crate::HexEditor::type_ascii_byte`] interpret each keystroke.
/// Default `Replace` is the hex-editor convention -- typing
/// overwrites in place and only grows the buffer past EOF, matching
/// vim's `R` mode. `Insert` is vim's `i`: every keystroke splices a
/// new byte at the cursor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TypingMode {
    #[default]
    Replace,
    Insert,
}

#[derive(Debug, Error)]
pub enum WriteError {
    #[error("editor is read-only; enable edit mode first")]
    Readonly,
    #[error("write at {offset} extends past source length {source_len}")]
    OutOfBounds { offset: u64, len: u64, source_len: u64 },
    #[error("write rejected: {0}")]
    Rejected(String),
}

/// Single reversible edit: the byte range `[offset, offset+old_len)`
/// in the patched view at the time of recording, the bytes that were
/// there, and the bytes that replaced them. `old_bytes.len() == 0`
/// encodes a pure insert; `new_bytes.is_empty()` would encode a pure
/// delete (not currently produced by the typing paths but handled by
/// rebuild).
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EditEntry {
    pub offset: u64,
    pub old_bytes: Vec<u8>,
    pub new_bytes: Vec<u8>,
}

impl EditEntry {
    pub(crate) fn end(&self) -> u64 {
        self.offset + self.new_bytes.len() as u64
    }
}

/// Cap on how many separate undo entries are retained. A hex editor
/// doesn't benefit from bottomless history and a million single-byte
/// edits would balloon memory.
const UNDO_HISTORY_CAP: usize = 1000;

/// Idle interval after which a new write stops coalescing into the
/// previous undo entry. Matches the "pause to think" cadence so a
/// short run of typing stays one undo unit but a deliberate second
/// edit made after a beat reads as a separate logical change.
const EDIT_COALESCE_IDLE: Duration = Duration::from_millis(800);

/// Internal state carried by [`crate::HexEditor`] when the `editor`
/// feature is on: the live patched byte view, the patch itself, and
/// the undo/redo stacks plus the bookkeeping that drives coalescing.
pub(crate) struct EditState {
    /// The immutable base source the editor was constructed from.
    /// Kept so [`Self::swap_base`] can rebuild the patched view
    /// against a fresh buffer after a save.
    pub(crate) base_source: Arc<dyn HexSource>,
    /// Patched view exposed to consumers through
    /// [`crate::HexEditor::source`]. Wraps `base_source` and `patch`.
    pub(crate) patched_source: Arc<dyn HexSource>,
    /// Shared handle to the patch inside `patched_source`. Exposed
    /// via [`crate::HexEditor::patch`] so consumers can persist it.
    /// Treated as *derived state*: it's always rebuilt from
    /// `undo_stack` after any change.
    pub(crate) patch: Arc<RwLock<Patch>>,
    pub(crate) mode: EditMode,
    /// How [`crate::HexEditor::type_hex_digit`] /
    /// [`crate::HexEditor::type_ascii_byte`] interpret each press.
    /// `Replace` is the hex-editor default; `Insert` is vim's `i`.
    pub(crate) typing_mode: TypingMode,
    /// Two-press hex-digit input state. `true` means the next typed
    /// digit overwrites the high nibble.
    pub(crate) edit_high_nibble: bool,
    pub(crate) undo_stack: Vec<EditEntry>,
    pub(crate) redo_stack: Vec<EditEntry>,
    /// `true` when the next write must start a fresh undo entry
    /// rather than coalesce into the previous one.
    pub(crate) history_break: bool,
    /// Monotonic instant of the most recent successful write.
    pub(crate) last_edit_at: Option<Instant>,
    /// Lazily-populated copy of `base_source`'s bytes. Read once on
    /// first mutation so `rebuild_patch_from_stack` doesn't re-read
    /// the base on every keystroke. Cleared by [`Self::swap_base`].
    base_cache: Option<Vec<u8>>,
}

impl EditState {
    pub(crate) fn new(base: Arc<dyn HexSource>) -> Self {
        let patched = PatchedSource::new(base.clone());
        let patch = patched.patch();
        let patched_source: Arc<dyn HexSource> = Arc::new(patched);
        Self {
            base_source: base,
            patched_source,
            patch,
            mode: EditMode::Mutable,
            typing_mode: TypingMode::Replace,
            edit_high_nibble: true,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            history_break: false,
            last_edit_at: None,
            base_cache: None,
        }
    }

    /// Replace the base source and clear editor history. Used after a
    /// successful save to re-anchor against the just-written bytes
    /// so subsequent reads reflect on-disk state instead of the
    /// stale pre-save buffer.
    pub(crate) fn swap_base(&mut self, base: Arc<dyn HexSource>) {
        let patched = PatchedSource::new(base.clone());
        self.patch = patched.patch();
        self.patched_source = Arc::new(patched);
        self.base_source = base;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.history_break = true;
        self.last_edit_at = None;
        self.edit_high_nibble = true;
        self.base_cache = None;
    }

    pub(crate) fn is_dirty(&self) -> bool {
        !self.patch.read().expect("patch lock poisoned").is_empty()
    }

    pub(crate) fn modified_ranges(&self) -> Vec<(u64, u64)> {
        self.patch
            .read()
            .expect("patch lock poisoned")
            .ops()
            .iter()
            .map(|op| (op.offset, op.offset + op.new_bytes.len() as u64))
            .collect()
    }

    pub(crate) fn request_write(&mut self, offset: u64, bytes: Vec<u8>) -> Result<(), WriteError> {
        if self.mode != EditMode::Mutable {
            return Err(WriteError::Readonly);
        }
        let source_len = self.patched_source.len().get();
        let end = offset + bytes.len() as u64;
        if end > source_len {
            return Err(WriteError::OutOfBounds { offset, len: bytes.len() as u64, source_len });
        }
        let mut old_bytes = Vec::with_capacity(bytes.len());
        for i in 0..bytes.len() {
            old_bytes.push(self.read_byte_at(offset + i as u64)?);
        }
        self.record_entry(EditEntry { offset, old_bytes, new_bytes: bytes });
        self.rebuild_patch_from_stack();
        Ok(())
    }

    /// Replace the byte range `[offset, offset + remove)` with
    /// `insert`. Generalises [`Self::request_write`] (in-place,
    /// `remove == insert.len()`) and [`Self::insert_at`]
    /// (`remove == 0`); also covers pure delete (`insert.is_empty()`)
    /// and arbitrary splices (vim's `p` / `d` over a visual range).
    pub(crate) fn splice(
        &mut self,
        offset: u64,
        remove: u64,
        insert: Vec<u8>,
    ) -> Result<(), WriteError> {
        if self.mode != EditMode::Mutable {
            return Err(WriteError::Readonly);
        }
        let source_len = self.patched_source.len().get();
        if offset > source_len || offset.saturating_add(remove) > source_len {
            return Err(WriteError::OutOfBounds { offset, len: remove, source_len });
        }
        let mut old_bytes = Vec::with_capacity(remove as usize);
        for i in 0..remove {
            old_bytes.push(self.read_byte_at(offset + i)?);
        }
        self.record_entry(EditEntry { offset, old_bytes, new_bytes: insert });
        self.rebuild_patch_from_stack();
        Ok(())
    }

    /// Insert `bytes` at `offset`, growing the source by `bytes.len()`.
    /// Used by the editor when the cursor sits at EOF so anonymous
    /// buffers can start at 0 bytes and grow with typing.
    pub(crate) fn insert_at(&mut self, offset: u64, bytes: Vec<u8>) -> Result<(), WriteError> {
        if self.mode != EditMode::Mutable {
            return Err(WriteError::Readonly);
        }
        let source_len = self.patched_source.len().get();
        if offset > source_len {
            return Err(WriteError::OutOfBounds { offset, len: bytes.len() as u64, source_len });
        }
        self.record_entry(EditEntry { offset, old_bytes: Vec::new(), new_bytes: bytes });
        self.rebuild_patch_from_stack();
        Ok(())
    }

    /// Common bookkeeping after capturing a new edit: clear redo,
    /// coalesce into the previous entry when still on the same
    /// logical change, otherwise push fresh and enforce the cap.
    ///
    /// Three coalescing cases are recognised, all gated on no
    /// history break and no idle gap:
    ///   1. Both entries length-preserving and their patched ranges
    ///      touch -- classic "typing runs into one undo".
    ///   2. The new entry is length-preserving and sits entirely
    ///      inside the previous entry's `new_bytes` region. Covers
    ///      the "insert a zeroed byte at EOF, then overwrite its
    ///      nibbles" path that anonymous buffers rely on.
    ///   3. Both entries are pure inserts and adjacent in patched
    ///      coords. Covers typing past EOF: every new byte extends
    ///      the single tail insert instead of spawning its own
    ///      entry.
    fn record_entry(&mut self, entry: EditEntry) {
        self.redo_stack.clear();
        let now = Instant::now();
        let idle_break = self.last_edit_at.is_some_and(|last| now.duration_since(last) >= EDIT_COALESCE_IDLE);

        if !self.history_break
            && !idle_break
            && let Some(last) = self.undo_stack.last_mut()
        {
            let last_lp = last.old_bytes.len() == last.new_bytes.len();
            let entry_lp = entry.old_bytes.len() == entry.new_bytes.len();

            // Case 1: LP-LP touching -- classic coalesce.
            if last_lp && entry_lp && ranges_touch(last.offset, last.end(), entry.offset, entry.end()) {
                merge_entry(last, &entry);
                self.history_break = false;
                self.last_edit_at = Some(now);
                return;
            }

            // Case 2: LP entry contained in previous entry's
            // new_bytes. In-place overwrite; does not change the
            // previous entry's patched-coord footprint or old_bytes.
            let last_new_end = last.offset + last.new_bytes.len() as u64;
            if entry_lp
                && entry.offset >= last.offset
                && entry.offset + entry.new_bytes.len() as u64 <= last_new_end
            {
                let rel = (entry.offset - last.offset) as usize;
                last.new_bytes[rel..rel + entry.new_bytes.len()].copy_from_slice(&entry.new_bytes);
                self.history_break = false;
                self.last_edit_at = Some(now);
                return;
            }

            // Case 3: adjacent pure inserts -- concatenate.
            if last.old_bytes.is_empty()
                && entry.old_bytes.is_empty()
                && entry.offset == last_new_end
            {
                last.new_bytes.extend_from_slice(&entry.new_bytes);
                self.history_break = false;
                self.last_edit_at = Some(now);
                return;
            }
        }

        self.undo_stack.push(entry);
        if self.undo_stack.len() > UNDO_HISTORY_CAP {
            self.undo_stack.remove(0);
        }
        self.history_break = false;
        self.last_edit_at = Some(now);
    }

    pub(crate) fn revert(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.history_break = true;
        self.last_edit_at = None;
        self.rebuild_patch_from_stack();
    }

    pub(crate) fn undo(&mut self) -> Option<EditEntry> {
        if self.mode != EditMode::Mutable {
            return None;
        }
        let entry = self.undo_stack.pop()?;
        self.rebuild_patch_from_stack();
        self.redo_stack.push(entry.clone());
        self.history_break = true;
        self.edit_high_nibble = true;
        Some(entry)
    }

    pub(crate) fn redo(&mut self) -> Option<EditEntry> {
        if self.mode != EditMode::Mutable {
            return None;
        }
        let entry = self.redo_stack.pop()?;
        self.undo_stack.push(entry.clone());
        self.rebuild_patch_from_stack();
        self.history_break = true;
        self.edit_high_nibble = true;
        Some(entry)
    }

    /// Materialise the final patched view by replaying `undo_stack`
    /// over a mutable copy of the base, then diff against base to
    /// emit a fresh [`Patch`]. Preserves any existing metadata.
    ///
    /// Replaces the previous implementation which appended each
    /// entry directly to the patch via [`Patch::write`] /
    /// [`Patch::insert`]. That approach couldn't compose an
    /// overwrite with a prior insert op (suture rejects overlap
    /// with non-length-preserving ops), so editing a just-inserted
    /// byte in an anonymous buffer failed.
    fn rebuild_patch_from_stack(&mut self) {
        let base = self.cached_base_bytes().clone();
        let mut buffer = base.clone();

        for entry in &self.undo_stack {
            let off = entry.offset as usize;
            let old_end = off.saturating_add(entry.old_bytes.len());
            if off > buffer.len() || old_end > buffer.len() {
                tracing::warn!(
                    offset = entry.offset,
                    old_len = entry.old_bytes.len(),
                    buffer_len = buffer.len(),
                    "rebuild: entry out of buffer range; skipping"
                );
                continue;
            }
            buffer.splice(off..old_end, entry.new_bytes.iter().copied());
        }

        let mut patch = self.patch.write().expect("patch lock poisoned");
        let metadata = patch.metadata().cloned();
        *patch = match metadata {
            Some(m) => Patch::with_metadata(m),
            None => Patch::new(),
        };

        let base_len = base.len();
        let compare_end = base_len.min(buffer.len());
        let mut i = 0;
        while i < compare_end {
            if base[i] != buffer[i] {
                let mut j = i + 1;
                while j < compare_end && base[j] != buffer[j] {
                    j += 1;
                }
                if let Err(e) = patch.splice(i as u64, (j - i) as u64, buffer[i..j].to_vec()) {
                    tracing::warn!(error = %e, "rebuild: splice rejected");
                }
                i = j;
            } else {
                i += 1;
            }
        }

        if buffer.len() < base_len {
            let del_start = buffer.len() as u64;
            let del_len = (base_len - buffer.len()) as u64;
            if let Err(e) = patch.delete(del_start, del_len) {
                tracing::warn!(error = %e, "rebuild: trailing delete rejected");
            }
        } else if buffer.len() > base_len {
            let tail = buffer[base_len..].to_vec();
            if let Err(e) = patch.insert(base_len as u64, tail) {
                tracing::warn!(error = %e, "rebuild: tail insert rejected");
            }
        }
    }

    /// Lazily read the base source into memory on first use so
    /// diffing against it in `rebuild_patch_from_stack` doesn't hit
    /// storage on every keystroke. Cleared by [`Self::swap_base`].
    fn cached_base_bytes(&mut self) -> &Vec<u8> {
        if self.base_cache.is_none() {
            let base_len = self.base_source.len().get();
            let buf = if base_len == 0 {
                Vec::new()
            } else {
                let range = ByteRange::new(ByteOffset::new(0), ByteOffset::new(base_len))
                    .expect("0..base_len is a valid range");
                match self.base_source.read(range) {
                    Ok(b) => b.to_vec(),
                    Err(e) => {
                        tracing::warn!(error = %e, "rebuild: base read failed; treating as empty");
                        Vec::new()
                    }
                }
            };
            self.base_cache = Some(buf);
        }
        self.base_cache.as_ref().expect("just populated")
    }

    pub(crate) fn read_byte_at(&self, offset: u64) -> Result<u8, WriteError> {
        let range = ByteRange::new(ByteOffset::new(offset), ByteOffset::new(offset + 1))
            .map_err(|e| WriteError::Rejected(format!("invalid range: {e}")))?;
        self.patched_source
            .read(range)
            .map(|b| b[0])
            .map_err(|e| WriteError::Rejected(format!("read: {e}")))
    }
}

fn ranges_touch(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start <= b_end && b_start <= a_end
}

fn merge_entry(dst: &mut EditEntry, next: &EditEntry) {
    let start = dst.offset.min(next.offset);
    let end = dst.end().max(next.end());
    let len = (end - start) as usize;
    let mut new_bytes = vec![0u8; len];
    let mut old_bytes = vec![0u8; len];
    let dst_off = (dst.offset - start) as usize;
    new_bytes[dst_off..dst_off + dst.new_bytes.len()].copy_from_slice(&dst.new_bytes);
    old_bytes[dst_off..dst_off + dst.old_bytes.len()].copy_from_slice(&dst.old_bytes);
    let next_off = (next.offset - start) as usize;
    new_bytes[next_off..next_off + next.new_bytes.len()].copy_from_slice(&next.new_bytes);
    if next.offset < dst.offset {
        let prefix_len = (dst.offset - next.offset) as usize;
        old_bytes[0..prefix_len].copy_from_slice(&next.old_bytes[..prefix_len]);
    }
    if next.end() > dst.end() {
        let overlap = (dst.end().saturating_sub(next.offset)) as usize;
        let tail = &next.old_bytes[overlap..];
        let tail_off = (dst.end() - start) as usize;
        old_bytes[tail_off..tail_off + tail.len()].copy_from_slice(tail);
    }
    dst.offset = start;
    dst.new_bytes = new_bytes;
    dst.old_bytes = old_bytes;
}

#[cfg(test)]
mod tests {
    use super::*;
    use hxy_core::MemorySource;

    fn state() -> EditState {
        let base: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![0x00, 0x11, 0x22, 0x33, 0x44, 0x55]));
        EditState::new(base)
    }

    fn empty_state() -> EditState {
        let base: Arc<dyn HexSource> = Arc::new(MemorySource::new(Vec::new()));
        EditState::new(base)
    }

    fn read_all(s: &EditState) -> Vec<u8> {
        let len = s.patched_source.len().get();
        if len == 0 {
            return Vec::new();
        }
        let range = ByteRange::new(ByteOffset::new(0), ByteOffset::new(len)).unwrap();
        s.patched_source.read(range).unwrap().to_vec()
    }

    #[test]
    fn consecutive_writes_coalesce_into_one_undo_entry() {
        let mut s = state();
        s.request_write(1, vec![0xAA]).unwrap();
        s.request_write(1, vec![0xAB]).unwrap();
        s.request_write(2, vec![0xCC]).unwrap();
        assert_eq!(s.undo_stack.len(), 1);
        assert_eq!(s.undo_stack[0].new_bytes, vec![0xAB, 0xCC]);
        assert_eq!(s.undo_stack[0].old_bytes, vec![0x11, 0x22]);
    }

    #[test]
    fn history_boundary_starts_a_new_entry() {
        let mut s = state();
        s.request_write(1, vec![0xAA]).unwrap();
        s.history_break = true;
        s.request_write(2, vec![0xBB]).unwrap();
        assert_eq!(s.undo_stack.len(), 2);
    }

    #[test]
    fn idle_gap_starts_a_new_entry() {
        let mut s = state();
        s.request_write(1, vec![0xAA]).unwrap();
        let backdated = s.last_edit_at.unwrap() - Duration::from_secs(2);
        s.last_edit_at = Some(backdated);
        s.request_write(2, vec![0xBB]).unwrap();
        assert_eq!(s.undo_stack.len(), 2);
    }

    #[test]
    fn undo_returns_to_clean_state() {
        let mut s = state();
        s.request_write(1, vec![0xAA]).unwrap();
        s.request_write(2, vec![0xBB]).unwrap();
        assert!(s.is_dirty());
        assert!(s.undo().is_some());
        assert!(!s.is_dirty());
    }

    #[test]
    fn redo_reapplies_the_edit() {
        let mut s = state();
        s.request_write(1, vec![0xAA]).unwrap();
        s.undo().unwrap();
        assert!(!s.is_dirty());
        s.redo().unwrap();
        assert!(s.is_dirty());
    }

    #[test]
    fn readonly_blocks_writes() {
        let mut s = state();
        s.mode = EditMode::Readonly;
        assert!(matches!(s.request_write(0, vec![0xAA]), Err(WriteError::Readonly)));
    }

    #[test]
    fn undo_cap_drops_oldest() {
        let mut s = state();
        for _ in 0..(UNDO_HISTORY_CAP + 5) {
            s.history_break = true;
            s.request_write(0, vec![0x01]).unwrap();
        }
        assert_eq!(s.undo_stack.len(), UNDO_HISTORY_CAP);
    }

    #[test]
    fn insert_then_overwrite_in_anonymous_buffer() {
        // Reproduces the bug: typing two nibbles at EOF of an empty
        // source. First nibble inserts a zeroed byte; second nibble
        // overwrites its low half. The previous implementation
        // rejected the second write as an overlap with the insert
        // op.
        let mut s = empty_state();
        s.insert_at(0, vec![0xA0]).unwrap();
        s.request_write(0, vec![0xAB]).unwrap();
        assert_eq!(read_all(&s), vec![0xAB]);
        // Both edits coalesce into a single insert entry.
        assert_eq!(s.undo_stack.len(), 1);
        assert_eq!(s.undo_stack[0].new_bytes, vec![0xAB]);
        assert!(s.undo_stack[0].old_bytes.is_empty());
    }

    #[test]
    fn typing_past_eof_grows_buffer_across_multiple_bytes() {
        let mut s = empty_state();
        s.insert_at(0, vec![0xA0]).unwrap();
        s.request_write(0, vec![0xAB]).unwrap();
        s.insert_at(1, vec![0xC0]).unwrap();
        s.request_write(1, vec![0xCD]).unwrap();
        assert_eq!(read_all(&s), vec![0xAB, 0xCD]);
    }

    #[test]
    fn undo_one_step_reverts_anonymous_typing() {
        let mut s = empty_state();
        s.insert_at(0, vec![0xA0]).unwrap();
        s.request_write(0, vec![0xAB]).unwrap();
        assert!(s.undo().is_some());
        assert_eq!(read_all(&s), Vec::<u8>::new());
        assert!(!s.is_dirty());
    }

    #[test]
    fn middle_insert_and_overwrite_composes_correctly() {
        // base [X Y Z], insert A at patched offset 1, then write B
        // at patched offset 2 (which is the Y that moved right).
        let base: Arc<dyn HexSource> = Arc::new(MemorySource::new(vec![0x58, 0x59, 0x5A]));
        let mut s = EditState::new(base);
        s.insert_at(1, vec![0x41]).unwrap();
        s.request_write(2, vec![0x42]).unwrap();
        assert_eq!(read_all(&s), vec![0x58, 0x41, 0x42, 0x5A]);
    }
}
