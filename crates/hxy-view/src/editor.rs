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

#[derive(Debug, Error)]
pub enum WriteError {
    #[error("editor is read-only; enable edit mode first")]
    Readonly,
    #[error("write at {offset} extends past source length {source_len}")]
    OutOfBounds { offset: u64, len: u64, source_len: u64 },
    #[error("write rejected: {0}")]
    Rejected(String),
}

/// Single reversible edit: the byte range `[offset, offset+len)`, the
/// bytes that were there before the edit, and the bytes that replaced
/// them. Length-preserving, so `old_bytes.len() == new_bytes.len()`.
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
    pub(crate) patch: Arc<RwLock<Patch>>,
    pub(crate) mode: EditMode,
    /// Two-press hex-digit input state. `true` means the next typed
    /// digit overwrites the high nibble.
    pub(crate) edit_high_nibble: bool,
    pub(crate) undo_stack: Vec<EditEntry>,
    pub(crate) redo_stack: Vec<EditEntry>,
    /// `true` when the next `request_write` must start a fresh undo
    /// entry rather than coalesce into the previous one.
    pub(crate) history_break: bool,
    /// Monotonic instant of the most recent successful write.
    pub(crate) last_edit_at: Option<Instant>,
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
            edit_high_nibble: true,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            history_break: false,
            last_edit_at: None,
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
        self.patch
            .write()
            .expect("patch lock poisoned")
            .write(offset, bytes.clone())
            .map_err(|e| WriteError::Rejected(e.to_string()))?;
        self.redo_stack.clear();
        let now = Instant::now();
        let idle_break = self.last_edit_at.is_some_and(|last| now.duration_since(last) >= EDIT_COALESCE_IDLE);
        let entry = EditEntry { offset, old_bytes, new_bytes: bytes };
        if !self.history_break
            && !idle_break
            && let Some(last) = self.undo_stack.last_mut()
            && ranges_touch(last.offset, last.end(), entry.offset, entry.end())
        {
            merge_entry(last, &entry);
        } else {
            self.undo_stack.push(entry);
            if self.undo_stack.len() > UNDO_HISTORY_CAP {
                self.undo_stack.remove(0);
            }
        }
        self.history_break = false;
        self.last_edit_at = Some(now);
        Ok(())
    }

    pub(crate) fn revert(&mut self) {
        *self.patch.write().expect("patch lock poisoned") = Patch::new();
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.history_break = true;
        self.last_edit_at = None;
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
        if let Err(e) = self.patch.write().expect("patch lock poisoned").write(entry.offset, entry.new_bytes.clone()) {
            tracing::warn!(error = %e, "redo write rejected; restoring redo stack");
            self.redo_stack.push(entry);
            return None;
        }
        self.undo_stack.push(entry.clone());
        self.history_break = true;
        self.edit_high_nibble = true;
        Some(entry)
    }

    fn rebuild_patch_from_stack(&self) {
        let mut patch = self.patch.write().expect("patch lock poisoned");
        let metadata = patch.metadata().cloned();
        *patch = match metadata {
            Some(m) => Patch::with_metadata(m),
            None => Patch::new(),
        };
        for entry in &self.undo_stack {
            if let Err(e) = patch.write(entry.offset, entry.new_bytes.clone()) {
                tracing::warn!(error = %e, "rebuild_patch_from_stack: entry rejected");
            }
        }
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
}
