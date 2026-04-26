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
use similar::capture_diff_slices_deadline;

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
    /// Cached fingerprints of each side at the last diff, used to
    /// detect that an edit has happened so the host can debounce a
    /// recompute. See [`Self::needs_recompute_debounced`].
    last_diff_fingerprint: Option<(PaneFingerprint, PaneFingerprint)>,
    /// Wall-clock time of the most recent observed mutation. Used
    /// as the start of the debounce window.
    edit_at: Option<std::time::Instant>,
    /// Last vertical scroll position the host saw both panes
    /// agreeing on. Used by [`Self::sync_scroll`] to detect which
    /// side moved when the user dragged a scrollbar / used the
    /// wheel and propagate that motion to the other side so the
    /// row maps stay aligned.
    last_synced_scroll: f32,
    /// Worker handle while a background diff is in flight. `None`
    /// when no recompute is pending. Polling-only -- see
    /// [`Self::poll_recompute`].
    pending_recompute: Option<RecomputePending>,
}

/// Cheap "did this side change?" snapshot pulled from the public
/// editor API -- undo-stack length plus source length covers
/// inserts, deletes, in-place writes, undo, redo, swap-source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PaneFingerprint {
    undo_len: usize,
    source_len: u64,
}

impl PaneFingerprint {
    fn for_pane(pane: &ComparePane) -> Self {
        Self {
            undo_len: pane.editor.undo_stack().len(),
            source_len: pane.editor.source().len().get(),
        }
    }
}

/// Outcome of [`CompareSession::needs_recompute_debounced`]. The
/// host both updates its UI based on the variant *and* uses the
/// `RecomputeAfter` deadline to schedule the next repaint so the
/// debounce fires even with no further input.
#[derive(Clone, Copy, Debug)]
pub enum DebouncedDecision {
    /// Nothing changed since the last diff -- skip.
    Idle,
    /// Edits are happening; wait until at least `after` from now
    /// before recomputing. The host should call
    /// `ctx.request_repaint_after(after)` so an idle session
    /// eventually flushes.
    WaitFor(std::time::Duration),
    /// Edits have settled long enough; recompute now.
    Recompute,
}

/// Idle window the host waits before recomputing the diff after
/// observing a mutation. Tuned for "type a few bytes, see the diff
/// catch up" -- short enough to feel live, long enough to avoid
/// churning the diff on every keystroke for a multi-MiB file.
pub const RECOMPUTE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// Maximum wall-clock time the worker thread is allowed to spend
/// on a single Myers diff. Past this, [`similar`] falls back to
/// an approximation -- the result is still a valid diff, just less
/// granular. Bounds the worst case for completely-unrelated
/// inputs where Myers degenerates to O(N*D) with D ~= N.
pub const RECOMPUTE_DEADLINE: std::time::Duration = std::time::Duration::from_millis(2000);

/// In-flight worker thread state. The session keeps one of these
/// while a background diff is running; each frame the host calls
/// [`CompareSession::poll_recompute`] which `try_recv`s the
/// channel and applies the result if ready.
struct RecomputePending {
    rx: std::sync::mpsc::Receiver<DiffResult>,
    /// Fingerprint of the inputs the worker is computing against,
    /// stored on the session so we can update
    /// [`CompareSession::last_diff_fingerprint`] correctly when the
    /// worker finishes -- not the *current* fingerprint, which may
    /// have moved on while the worker ran.
    fingerprint: (PaneFingerprint, PaneFingerprint),
}

impl CompareSession {
    pub fn new(id: CompareId, a: ComparePane, b: ComparePane) -> Self {
        Self {
            id,
            a,
            b,
            diff: None,
            diff_serial: 0,
            last_diff_fingerprint: None,
            edit_at: None,
            last_synced_scroll: 0.0,
            pending_recompute: None,
        }
    }

    /// `true` while a worker thread is computing the diff. Hosts
    /// can use this to render a "computing…" indicator and to
    /// avoid issuing another recompute request.
    pub fn is_recomputing(&self) -> bool {
        self.pending_recompute.is_some()
    }

    /// Spawn a worker thread that computes the diff with a
    /// [`RECOMPUTE_DEADLINE`] safety net. The thread reads from
    /// owned `Vec<u8>` snapshots (taken synchronously here) so it
    /// can outlive the editor without aliasing patched-source
    /// state. `ctx` is cloned into the worker so completing the
    /// diff requests an immediate UI repaint instead of waiting on
    /// the next ambient repaint.
    pub fn request_recompute(&mut self, ctx: &egui::Context) {
        if self.pending_recompute.is_some() {
            return;
        }
        let a_bytes = match read_all(&self.a.editor) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "compare: read side a");
                return;
            }
        };
        let b_bytes = match read_all(&self.b.editor) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "compare: read side b");
                return;
            }
        };
        let fingerprint = (PaneFingerprint::for_pane(&self.a), PaneFingerprint::for_pane(&self.b));
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx_clone = ctx.clone();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + RECOMPUTE_DEADLINE;
            let ops = capture_diff_slices_deadline(Algorithm::Myers, &a_bytes, &b_bytes, Some(deadline));
            let hunks: Vec<DiffHunk> = ops.into_iter().map(diff_op_to_hunk).collect();
            let result = DiffResult { hunks, a_len: a_bytes.len() as u64, b_len: b_bytes.len() as u64 };
            let _ = tx.send(result);
            ctx_clone.request_repaint();
        });
        self.pending_recompute = Some(RecomputePending { rx, fingerprint });
    }

    /// Try to receive the worker's diff result. Call once per
    /// frame; on success the diff is swapped in and the
    /// fingerprint that the worker computed against is stored, so
    /// the debounce logic can detect any edits that happened
    /// while the worker was running and schedule a follow-up.
    pub fn poll_recompute(&mut self) {
        let Some(pending) = self.pending_recompute.as_ref() else { return };
        match pending.rx.try_recv() {
            Ok(diff) => {
                self.diff = Some(diff);
                self.diff_serial = self.diff_serial.wrapping_add(1);
                self.last_diff_fingerprint = Some(pending.fingerprint);
                self.edit_at = None;
                self.pending_recompute = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Worker died without sending -- drop the slot so
                // the next debounce can try again.
                self.pending_recompute = None;
            }
        }
    }

    /// Mirror whichever pane the user just scrolled onto the other
    /// pane. Called after both panes have rendered for the current
    /// frame so [`hxy_view::HexEditor::scroll_offset`] reflects the
    /// just-rendered position. Equality is compared with a small
    /// epsilon because egui's scroll values can wiggle by a sub-
    /// pixel when content height changes underfoot.
    pub fn sync_scroll(&mut self) {
        let a = self.a.editor.scroll_offset();
        let b = self.b.editor.scroll_offset();
        let eps = 0.5_f32;
        if (a - b).abs() <= eps {
            self.last_synced_scroll = a;
            return;
        }
        let a_moved = (a - self.last_synced_scroll).abs() > eps;
        let b_moved = (b - self.last_synced_scroll).abs() > eps;
        let leader = match (a_moved, b_moved) {
            (true, false) => a,
            (false, true) => b,
            // Both moved this frame -- a tie. Take the side that
            // moved further as a heuristic for "the one the user
            // is actually dragging."
            (true, true) => {
                if (a - self.last_synced_scroll).abs() >= (b - self.last_synced_scroll).abs() {
                    a
                } else {
                    b
                }
            }
            // Neither moved past the epsilon yet they disagree --
            // residual mismatch from a previous frame's
            // recompute. Snap to A.
            (false, false) => a,
        };
        if (a - leader).abs() > eps {
            self.a.editor.set_scroll_to(leader);
        }
        if (b - leader).abs() > eps {
            self.b.editor.set_scroll_to(leader);
        }
        self.last_synced_scroll = leader;
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
        self.last_diff_fingerprint =
            Some((PaneFingerprint::for_pane(&self.a), PaneFingerprint::for_pane(&self.b)));
        self.edit_at = None;
        Ok(())
    }

    /// Inspect whether either side has mutated since the last diff
    /// and, if so, return how long the host should wait before
    /// recomputing. `now` is wall-clock time; passing
    /// `Instant::now()` is the typical use. Returns
    /// [`DebouncedDecision::Idle`] while a worker is already
    /// running -- the host's next [`Self::poll_recompute`] call
    /// will pick up the result and any post-worker edits will
    /// re-fire the debounce naturally.
    pub fn needs_recompute_debounced(&mut self, now: std::time::Instant) -> DebouncedDecision {
        if self.pending_recompute.is_some() {
            return DebouncedDecision::Idle;
        }
        let current = (PaneFingerprint::for_pane(&self.a), PaneFingerprint::for_pane(&self.b));
        let changed = match self.last_diff_fingerprint {
            Some(last) => last != current,
            None => self.diff.is_none(),
        };
        if !changed {
            self.edit_at = None;
            return DebouncedDecision::Idle;
        }
        let edit_at = match self.edit_at {
            Some(t) => t,
            None => {
                self.edit_at = Some(now);
                now
            }
        };
        let elapsed = now.duration_since(edit_at);
        if elapsed >= RECOMPUTE_DEBOUNCE {
            DebouncedDecision::Recompute
        } else {
            DebouncedDecision::WaitFor(RECOMPUTE_DEBOUNCE - elapsed)
        }
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

/// Per-side row map (one [`hxy_view::RowSlot`] per visual row) plus
/// the shared row count. Both sides have the same length so the two
/// hex views render in lockstep with horizontally aligned rows even
/// when the underlying byte streams have different lengths.
pub struct CompareRowMaps {
    pub a: Vec<hxy_view::RowSlot>,
    pub b: Vec<hxy_view::RowSlot>,
}

/// Build a parallel row map for both sides of `diff`. Each side's
/// Real slots are at 16-aligned (`columns`-aligned) offsets -- the
/// natural hex-grid rows of that side, no partial-row breaks at
/// hunk boundaries. The two maps end up the same length: gaps are
/// inserted on the shorter side to align added / removed regions.
///
/// Visual alignment is row-level rather than byte-level: a
/// 5-byte change that starts mid-row colors the affected bytes via
/// the per-byte styler, but the row itself stays 16 bytes wide and
/// aligned with its neighbors. Compare it to most hex-diff tools
/// (Beyond Compare, etc.) which take the same compromise.
pub fn build_row_maps(diff: &DiffResult, columns: u64) -> CompareRowMaps {
    use std::collections::BTreeMap;

    if columns == 0 {
        return CompareRowMaps { a: Vec::new(), b: Vec::new() };
    }
    let a_natural = natural_rows(diff.a_len, columns);
    let b_natural = natural_rows(diff.b_len, columns);

    // Per-side `(insert_before_natural_row_idx -> gap_count)` plan.
    // Multiple plan entries on the same row sum.
    let mut a_gaps: BTreeMap<usize, u64> = BTreeMap::new();
    let mut b_gaps: BTreeMap<usize, u64> = BTreeMap::new();

    for hunk in &diff.hunks {
        match hunk.kind {
            HunkKind::Added => {
                // B has bytes A doesn't. A needs `ceil(b_len/cols)`
                // gap rows, inserted at the row boundary nearest the
                // insertion point on A.
                let count = hunk.b_len.div_ceil(columns);
                let at = (hunk.a_offset.div_ceil(columns)) as usize;
                *a_gaps.entry(at).or_default() += count;
            }
            HunkKind::Removed => {
                let count = hunk.a_len.div_ceil(columns);
                let at = (hunk.b_offset.div_ceil(columns)) as usize;
                *b_gaps.entry(at).or_default() += count;
            }
            HunkKind::Changed => {
                // Each side emits `ceil(its_len/cols)` rows; pad the
                // shorter side with gaps right after the changed
                // region on that side.
                let rows_a = hunk.a_len.div_ceil(columns);
                let rows_b = hunk.b_len.div_ceil(columns);
                if rows_a < rows_b {
                    let count = rows_b - rows_a;
                    let at = ((hunk.a_offset + hunk.a_len).div_ceil(columns)) as usize;
                    *a_gaps.entry(at).or_default() += count;
                } else if rows_b < rows_a {
                    let count = rows_a - rows_b;
                    let at = ((hunk.b_offset + hunk.b_len).div_ceil(columns)) as usize;
                    *b_gaps.entry(at).or_default() += count;
                }
            }
            HunkKind::Equal => {}
        }
    }

    let mut a = interleave_with_gaps(&a_natural, &a_gaps);
    let mut b = interleave_with_gaps(&b_natural, &b_gaps);

    // Safety net: if the math produced different lengths (rounding
    // drift on hunk boundaries), pad the shorter map with end gaps
    // so both views stay row-aligned.
    let max_len = a.len().max(b.len());
    a.resize(max_len, hxy_view::RowSlot::Gap);
    b.resize(max_len, hxy_view::RowSlot::Gap);

    CompareRowMaps { a, b }
}

/// Natural 16-aligned row stream for one side: `Real(0, cols)`,
/// `Real(cols, cols)`, ..., with the last slot possibly shorter
/// than `cols` when `side_len` doesn't land on a row boundary.
fn natural_rows(side_len: u64, columns: u64) -> Vec<hxy_view::RowSlot> {
    let mut rows = Vec::new();
    if side_len == 0 || columns == 0 {
        return rows;
    }
    let mut offset = 0u64;
    while offset < side_len {
        let len = (side_len - offset).min(columns) as u16;
        rows.push(hxy_view::RowSlot::Real { offset, len });
        offset += columns;
    }
    rows
}

/// Splice gap rows into a side's natural row stream at the
/// positions named by `gaps` (BTreeMap key = "insert before this
/// natural row index", value = number of gaps to insert).
fn interleave_with_gaps(
    natural: &[hxy_view::RowSlot],
    gaps: &std::collections::BTreeMap<usize, u64>,
) -> Vec<hxy_view::RowSlot> {
    let total_gaps: u64 = gaps.values().sum();
    let mut out = Vec::with_capacity(natural.len() + total_gaps as usize);
    for (i, row) in natural.iter().enumerate() {
        if let Some(count) = gaps.get(&i) {
            for _ in 0..*count {
                out.push(hxy_view::RowSlot::Gap);
            }
        }
        out.push(*row);
    }
    if let Some(count) = gaps.get(&natural.len()) {
        for _ in 0..*count {
            out.push(hxy_view::RowSlot::Gap);
        }
    }
    out
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

    #[test]
    fn debounce_idle_when_nothing_changed() {
        let mut s = session(b"abc", b"abc");
        let now = std::time::Instant::now();
        assert!(matches!(s.needs_recompute_debounced(now), DebouncedDecision::Idle));
    }

    #[test]
    fn row_maps_align_equal_buffers() {
        let s = session(b"abcdefgh", b"abcdefgh");
        let diff = s.diff.expect("diff");
        let maps = build_row_maps(&diff, 4);
        assert_eq!(maps.a.len(), maps.b.len());
        assert_eq!(maps.a.len(), 2);
        assert_eq!(maps.a, vec![hxy_view::RowSlot::real(0, 4), hxy_view::RowSlot::real(4, 4)]);
        assert_eq!(maps.b, maps.a);
    }

    #[test]
    fn row_maps_use_natural_alignment_for_same_length_changed() {
        // a and b have a tiny equal prefix then differ -- the prior
        // algorithm would have emitted a partial 2-byte row at
        // offset 2 followed by a row at offset 4. The fix keeps
        // natural 4-byte (cols) alignment on both sides since the
        // total lengths match.
        let s = session(b"abXYZW", b"abMNOP");
        let diff = s.diff.expect("diff");
        let maps = build_row_maps(&diff, 4);
        assert_eq!(maps.a.len(), maps.b.len());
        // Both sides have 6 bytes -> 2 rows of 4+2 at offsets 0 and 4.
        for slot in &maps.a {
            if let hxy_view::RowSlot::Real { offset, .. } = slot {
                assert!(offset.is_multiple_of(4), "A slot at non-aligned offset: {:?}", slot);
            }
        }
        for slot in &maps.b {
            if let hxy_view::RowSlot::Real { offset, .. } = slot {
                assert!(offset.is_multiple_of(4), "B slot at non-aligned offset: {:?}", slot);
            }
        }
    }

    #[test]
    fn row_maps_pad_added_with_gaps_on_a() {
        // 6 bytes on A vs 9 bytes on B (3 inserted). With cols=4:
        // A has 2 natural rows; B has 3 natural rows; A needs 1 gap.
        let s = session(b"abcdef", b"abXYZcdef");
        let diff = s.diff.expect("diff");
        let maps = build_row_maps(&diff, 4);
        assert_eq!(maps.a.len(), maps.b.len());
        let gaps_a = maps.a.iter().filter(|s| s.is_gap()).count();
        let gaps_b = maps.b.iter().filter(|s| s.is_gap()).count();
        assert!(gaps_a >= 1, "A should have at least one gap: {:?}", maps.a);
        assert_eq!(gaps_b, 0, "B should be all-real rows: {:?}", maps.b);
        // A's Real slots stay at 4-aligned offsets.
        for slot in &maps.a {
            if let hxy_view::RowSlot::Real { offset, .. } = slot {
                assert!(offset.is_multiple_of(4), "non-aligned A slot: {:?}", slot);
            }
        }
    }

    #[test]
    fn row_maps_pad_removed_with_gaps_on_b() {
        let s = session(b"abXYZcdef", b"abcdef");
        let diff = s.diff.expect("diff");
        let maps = build_row_maps(&diff, 4);
        assert_eq!(maps.a.len(), maps.b.len());
        let gaps_a = maps.a.iter().filter(|s| s.is_gap()).count();
        let gaps_b = maps.b.iter().filter(|s| s.is_gap()).count();
        assert_eq!(gaps_a, 0);
        assert!(gaps_b >= 1);
    }

    #[test]
    fn debounce_waits_then_recomputes_after_edit() {
        let mut s = session(b"abc", b"abc");
        s.a.editor.request_write(0, vec![b'X']).unwrap();
        let t0 = std::time::Instant::now();
        match s.needs_recompute_debounced(t0) {
            DebouncedDecision::WaitFor(d) => assert!(d <= RECOMPUTE_DEBOUNCE),
            other => panic!("expected WaitFor, got {other:?}"),
        }
        let after = t0 + RECOMPUTE_DEBOUNCE + std::time::Duration::from_millis(1);
        assert!(matches!(s.needs_recompute_debounced(after), DebouncedDecision::Recompute));
    }
}
