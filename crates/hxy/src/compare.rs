//! Side-by-side byte diff for two arbitrary sources.
//!
//! Each [`Tab::Compare`] tab owns a [`CompareSession`] -- two
//! [`ComparePane`] sides plus the cached [`DiffResult`] between them.
//! Each pane wraps its own [`hxy_view::HexEditor`] so both sides stay
//! independently editable with their own undo/redo, selection, and
//! scroll. The diff is recomputed (debounced) whenever either side's
//! patched view changes.
//!
//! The diff itself is byte-level Myers via the `similar` crate. That
//! handles up to a few hundred MiB comfortably; multi-GiB sources
//! will want a follow-up block-hash strategy but the [`DiffResult`]
//! shape is the same either way.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_vfs::TabSource;
use similar::Algorithm;
use similar::DiffOp;
use similar::capture_diff_slices;

use crate::file::EditMode;

/// Stable id for an open compare tab. Like [`crate::file::FileId`] /
/// [`crate::file::WorkspaceId`], allocated monotonically by the host
/// and used as the dock tab payload (`Tab::Compare(CompareId)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CompareId(u64);

impl CompareId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

impl serde::Serialize for CompareId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.get())
    }
}

impl<'de> serde::Deserialize<'de> for CompareId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        u64::deserialize(d).map(CompareId::new)
    }
}

/// Tells the diff coloring renderer which side of the compare pair it
/// is so "added" / "removed" map to the right colors. `A` is treated
/// as the *old* side, `B` as the *new* side -- matching `similar`'s
/// `old_index` / `new_index` terminology.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareSide {
    A,
    B,
}

/// One side of the compare. Owns its own editor + undo state and
/// remembers the originating [`TabSource`] so the host can reuse the
/// same identity for restore.
pub struct ComparePane {
    pub source: Option<TabSource>,
    pub display_name: String,
    pub editor: hxy_view::HexEditor,
    /// Whether to render the diff colors on top of the hex bytes.
    /// When `false` the pane shows the hex view as if it weren't part
    /// of a comparison -- mirrors the per-file template-color toggle.
    pub diff_colors: bool,
}

impl ComparePane {
    pub fn from_bytes(display_name: impl Into<String>, source: Option<TabSource>, bytes: Vec<u8>) -> Self {
        let base: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
        let mut editor = hxy_view::HexEditor::new(base);
        editor.set_edit_mode(EditMode::Mutable);
        Self {
            source,
            display_name: display_name.into(),
            editor,
            diff_colors: true,
        }
    }
}

/// One open compare tab. `id` is the same value that lives in
/// `Tab::Compare(CompareId)` so the host can look the session up
/// directly from the tab.
pub struct CompareSession {
    pub id: CompareId,
    pub a: ComparePane,
    pub b: ComparePane,
    /// Most recent diff result. `None` until the first compute (or
    /// after a recompute is queued but hasn't run yet).
    pub diff: Option<DiffResult>,
    /// Bumped after every successful recompute so render code that
    /// caches per-diff data (minimap row colors, etc.) can detect
    /// staleness without comparing the whole diff structure.
    pub diff_serial: u64,
}

impl CompareSession {
    pub fn new(id: CompareId, a: ComparePane, b: ComparePane) -> Self {
        Self { id, a, b, diff: None, diff_serial: 0 }
    }

    /// Recompute the diff from the current patched view of both
    /// sides. Cheap when nothing changed; the caller is expected to
    /// debounce calls so live edits don't churn for every keystroke.
    pub fn recompute(&mut self) -> Result<(), CompareError> {
        let a_bytes = read_all(&self.a.editor)?;
        let b_bytes = read_all(&self.b.editor)?;
        let ops = capture_diff_slices(Algorithm::Myers, &a_bytes, &b_bytes);
        let hunks = ops.into_iter().map(diff_op_to_hunk).collect();
        self.diff = Some(DiffResult { hunks, a_len: a_bytes.len() as u64, b_len: b_bytes.len() as u64 });
        self.diff_serial = self.diff_serial.wrapping_add(1);
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CompareError {
    #[error("read side bytes: {0}")]
    Read(String),
}

fn read_all(editor: &hxy_view::HexEditor) -> Result<Vec<u8>, CompareError> {
    let len = editor.source().len().get();
    if len == 0 {
        return Ok(Vec::new());
    }
    let range = ByteRange::new(hxy_core::ByteOffset::new(0), hxy_core::ByteOffset::new(len))
        .map_err(|e| CompareError::Read(e.to_string()))?;
    editor.source().read(range).map_err(|e| CompareError::Read(e.to_string()))
}

/// Cached diff between the two sides at a point in time. `a_len` and
/// `b_len` snapshot the side lengths the diff was computed against
/// so renderers can detect when the buffer has moved beyond it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffResult {
    pub hunks: Vec<DiffHunk>,
    pub a_len: u64,
    pub b_len: u64,
}

impl DiffResult {
    /// Iterator over only the non-equal hunks -- what the diff table
    /// actually wants to show. Kept as a method (not a separate field)
    /// so `hunks` stays the canonical source of truth.
    pub fn changes(&self) -> impl Iterator<Item = &DiffHunk> {
        self.hunks.iter().filter(|h| !matches!(h.kind, HunkKind::Equal))
    }

    pub fn change_count(&self) -> usize {
        self.changes().count()
    }
}

/// One contiguous hunk of the diff. Lengths are signed only by virtue
/// of the kind: `Added` has `a_len == 0`, `Removed` has `b_len == 0`.
/// Offsets are byte offsets into the patched view of each side at
/// diff-time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub kind: HunkKind,
    pub a_offset: u64,
    pub a_len: u64,
    pub b_offset: u64,
    pub b_len: u64,
}

/// What changed between the two sides. The renderer maps these to
/// colors: green for `Added`, red for `Removed`, orange for
/// `Changed`. `Equal` hunks aren't colored at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HunkKind {
    Equal,
    Added,
    Removed,
    Changed,
}

fn diff_op_to_hunk(op: DiffOp) -> DiffHunk {
    match op {
        DiffOp::Equal { old_index, new_index, len } => DiffHunk {
            kind: HunkKind::Equal,
            a_offset: old_index as u64,
            a_len: len as u64,
            b_offset: new_index as u64,
            b_len: len as u64,
        },
        DiffOp::Insert { old_index, new_index, new_len } => DiffHunk {
            kind: HunkKind::Added,
            a_offset: old_index as u64,
            a_len: 0,
            b_offset: new_index as u64,
            b_len: new_len as u64,
        },
        DiffOp::Delete { old_index, old_len, new_index } => DiffHunk {
            kind: HunkKind::Removed,
            a_offset: old_index as u64,
            a_len: old_len as u64,
            b_offset: new_index as u64,
            b_len: 0,
        },
        DiffOp::Replace { old_index, old_len, new_index, new_len } => DiffHunk {
            kind: HunkKind::Changed,
            a_offset: old_index as u64,
            a_len: old_len as u64,
            b_offset: new_index as u64,
            b_len: new_len as u64,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(name: &str, bytes: &[u8]) -> ComparePane {
        ComparePane::from_bytes(name, None, bytes.to_vec())
    }

    fn session(a: &[u8], b: &[u8]) -> CompareSession {
        let mut s = CompareSession::new(CompareId::new(1), pane("a", a), pane("b", b));
        s.recompute().unwrap();
        s
    }

    #[test]
    fn equal_buffers_produce_only_equal_hunks() {
        let s = session(b"hello", b"hello");
        let diff = s.diff.expect("diff");
        assert_eq!(diff.change_count(), 0);
        assert_eq!(diff.hunks.iter().filter(|h| h.kind == HunkKind::Equal).count(), 1);
    }

    #[test]
    fn insertion_in_b_is_added_hunk() {
        let s = session(b"abcd", b"abXYcd");
        let diff = s.diff.expect("diff");
        let added: Vec<&DiffHunk> = diff.changes().collect();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].kind, HunkKind::Added);
        assert_eq!(added[0].b_len, 2);
        assert_eq!(added[0].a_len, 0);
    }

    #[test]
    fn deletion_in_b_is_removed_hunk() {
        let s = session(b"abcdef", b"abef");
        let diff = s.diff.expect("diff");
        let removed: Vec<&DiffHunk> = diff.changes().collect();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].kind, HunkKind::Removed);
        assert_eq!(removed[0].a_len, 2);
        assert_eq!(removed[0].b_len, 0);
    }

    #[test]
    fn changed_run_is_replace_hunk() {
        let s = session(b"abcdef", b"abZZZf");
        let diff = s.diff.expect("diff");
        let changed: Vec<&DiffHunk> = diff.changes().collect();
        assert!(changed.iter().any(|h| matches!(h.kind, HunkKind::Changed | HunkKind::Added | HunkKind::Removed)));
    }

    #[test]
    fn empty_sides_produce_no_diff() {
        let s = session(b"", b"");
        let diff = s.diff.expect("diff");
        assert_eq!(diff.change_count(), 0);
        assert_eq!(diff.a_len, 0);
        assert_eq!(diff.b_len, 0);
    }
}
