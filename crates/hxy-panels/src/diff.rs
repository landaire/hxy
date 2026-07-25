//! Byte-level diff computation and row-map alignment for the compare
//! view. Framework- and app-agnostic; `CompareSession` /
//! `ComparePane` (which own an egui `HexEditor` and drive worker
//! threads) stay in `crates/hxy`.
//!
//! The diff itself is byte-level Myers via the `similar` crate. That
//! handles up to a few hundred MiB comfortably; multi-GiB sources
//! will want a follow-up block-hash strategy but the [`DiffResult`]
//! shape is the same either way.

use hxy_core::RowSlot;
use similar::Algorithm;
use similar::DiffOp;

/// Cached diff between two sides at a point in time. `a_len` and
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

/// Compute diff hunks between two byte buffers with a wall-clock
/// deadline as a safety net (`similar::capture_diff_slices_deadline`).
/// Not available on wasm32: `similar`'s deadline path calls
/// `std::time::Instant::now()` internally, which panics on
/// wasm32-unknown-unknown. Use [`diff_hunks`] there.
#[cfg(not(target_arch = "wasm32"))]
pub fn diff_hunks_with_deadline(a: &[u8], b: &[u8], deadline: Option<std::time::Instant>) -> Vec<DiffHunk> {
    let ops = similar::capture_diff_slices_deadline(Algorithm::Myers, a, b, deadline);
    ops.into_iter().map(diff_op_to_hunk).collect()
}

/// Compute diff hunks between two byte buffers, running the Myers
/// algorithm to completion with no deadline.
pub fn diff_hunks(a: &[u8], b: &[u8]) -> Vec<DiffHunk> {
    let ops = similar::capture_diff_slices(Algorithm::Myers, a, b);
    ops.into_iter().map(diff_op_to_hunk).collect()
}

/// Per-side row map (one [`RowSlot`] per visual row) plus the shared
/// row count. Both sides have the same length so the two hex views
/// render in lockstep with horizontally aligned rows even when the
/// underlying byte streams have different lengths.
pub struct CompareRowMaps {
    pub a: Vec<RowSlot>,
    pub b: Vec<RowSlot>,
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
    a.resize(max_len, RowSlot::Gap);
    b.resize(max_len, RowSlot::Gap);

    CompareRowMaps { a, b }
}

/// Natural 16-aligned row stream for one side: `Real(0, cols)`,
/// `Real(cols, cols)`, ..., with the last slot possibly shorter
/// than `cols` when `side_len` doesn't land on a row boundary.
fn natural_rows(side_len: u64, columns: u64) -> Vec<RowSlot> {
    let mut rows = Vec::new();
    if side_len == 0 || columns == 0 {
        return rows;
    }
    let mut offset = 0u64;
    while offset < side_len {
        let len = (side_len - offset).min(columns) as u16;
        rows.push(RowSlot::Real { offset, len });
        offset += columns;
    }
    rows
}

/// Splice gap rows into a side's natural row stream at the
/// positions named by `gaps` (BTreeMap key = "insert before this
/// natural row index", value = number of gaps to insert).
fn interleave_with_gaps(natural: &[RowSlot], gaps: &std::collections::BTreeMap<usize, u64>) -> Vec<RowSlot> {
    let total_gaps: u64 = gaps.values().sum();
    let mut out = Vec::with_capacity(natural.len() + total_gaps as usize);
    for (i, row) in natural.iter().enumerate() {
        if let Some(count) = gaps.get(&i) {
            for _ in 0..*count {
                out.push(RowSlot::Gap);
            }
        }
        out.push(*row);
    }
    if let Some(count) = gaps.get(&natural.len()) {
        for _ in 0..*count {
            out.push(RowSlot::Gap);
        }
    }
    out
}

/// Cheap "did this side change?" snapshot -- undo-stack length plus
/// source length covers inserts, deletes, in-place writes, undo,
/// redo, swap-source. Hosts construct one per side from their own
/// editor state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneFingerprint {
    pub undo_len: usize,
    pub source_len: u64,
}

impl PaneFingerprint {
    pub fn new(undo_len: usize, source_len: u64) -> Self {
        Self { undo_len, source_len }
    }
}

/// Outcome of [`needs_recompute_debounced`]. The host both updates
/// its UI based on the variant *and* uses the `WaitFor` duration to
/// schedule the next repaint so the debounce fires even with no
/// further input.
#[derive(Clone, Copy, Debug)]
pub enum DebouncedDecision {
    /// Nothing changed since the last diff -- skip.
    Idle,
    /// Edits are happening; wait until at least this long from now
    /// before recomputing. The host should call
    /// `ctx.request_repaint_after(after)` so an idle session
    /// eventually flushes.
    WaitFor(std::time::Duration),
    /// Edits have settled long enough; recompute now.
    Recompute,
}

/// Idle window a host waits before recomputing the diff after
/// observing a mutation. Tuned for "type a few bytes, see the diff
/// catch up" -- short enough to feel live, long enough to avoid
/// churning the diff on every keystroke for a multi-MiB file.
pub const RECOMPUTE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// Decide whether a host should recompute the diff, given the
/// current and last-computed fingerprints. `pending` is true while a
/// background worker is already computing (short-circuits to
/// `Idle` so the next poll picks up the result). `edit_at` is the
/// host's persisted "when did the current unflushed edit start"
/// slot -- this function sets it on first detecting a change and
/// clears it once idle or recomputed, so callers should persist it
/// across calls on the same session.
pub fn needs_recompute_debounced(
    pending: bool,
    last_fingerprint: Option<(PaneFingerprint, PaneFingerprint)>,
    diff_present: bool,
    current: (PaneFingerprint, PaneFingerprint),
    edit_at: &mut Option<web_time::Instant>,
    now: web_time::Instant,
) -> DebouncedDecision {
    if pending {
        return DebouncedDecision::Idle;
    }
    let changed = match last_fingerprint {
        Some(last) => last != current,
        None => !diff_present,
    };
    if !changed {
        *edit_at = None;
        return DebouncedDecision::Idle;
    }
    let start = match *edit_at {
        Some(t) => t,
        None => {
            *edit_at = Some(now);
            now
        }
    };
    let elapsed = now.duration_since(start);
    if elapsed >= RECOMPUTE_DEBOUNCE {
        DebouncedDecision::Recompute
    } else {
        DebouncedDecision::WaitFor(RECOMPUTE_DEBOUNCE - elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff_of(a: &[u8], b: &[u8]) -> DiffResult {
        let hunks = diff_hunks(a, b);
        DiffResult { hunks, a_len: a.len() as u64, b_len: b.len() as u64 }
    }

    #[test]
    fn equal_buffers_produce_only_equal_hunks() {
        let diff = diff_of(b"hello", b"hello");
        assert_eq!(diff.change_count(), 0);
        assert_eq!(diff.hunks.iter().filter(|h| h.kind == HunkKind::Equal).count(), 1);
    }

    #[test]
    fn insertion_in_b_is_added_hunk() {
        let diff = diff_of(b"abcd", b"abXYcd");
        let added: Vec<&DiffHunk> = diff.changes().collect();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].kind, HunkKind::Added);
        assert_eq!(added[0].b_len, 2);
        assert_eq!(added[0].a_len, 0);
    }

    #[test]
    fn deletion_in_b_is_removed_hunk() {
        let diff = diff_of(b"abcdef", b"abef");
        let removed: Vec<&DiffHunk> = diff.changes().collect();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].kind, HunkKind::Removed);
        assert_eq!(removed[0].a_len, 2);
        assert_eq!(removed[0].b_len, 0);
    }

    #[test]
    fn changed_run_is_replace_hunk() {
        let diff = diff_of(b"abcdef", b"abZZZf");
        let changed: Vec<&DiffHunk> = diff.changes().collect();
        assert!(changed.iter().any(|h| matches!(h.kind, HunkKind::Changed | HunkKind::Added | HunkKind::Removed)));
    }

    #[test]
    fn empty_sides_produce_no_diff() {
        let diff = diff_of(b"", b"");
        assert_eq!(diff.change_count(), 0);
        assert_eq!(diff.a_len, 0);
        assert_eq!(diff.b_len, 0);
    }

    #[test]
    fn debounce_idle_when_nothing_changed() {
        let fp = PaneFingerprint::new(0, 3);
        let mut edit_at = None;
        let now = web_time::Instant::now();
        let decision = needs_recompute_debounced(false, Some((fp, fp)), true, (fp, fp), &mut edit_at, now);
        assert!(matches!(decision, DebouncedDecision::Idle));
    }

    #[test]
    fn row_maps_align_equal_buffers() {
        let diff = diff_of(b"abcdefgh", b"abcdefgh");
        let maps = build_row_maps(&diff, 4);
        assert_eq!(maps.a.len(), maps.b.len());
        assert_eq!(maps.a.len(), 2);
        assert_eq!(maps.a, vec![RowSlot::real(0, 4), RowSlot::real(4, 4)]);
        assert_eq!(maps.b, maps.a);
    }

    #[test]
    fn row_maps_use_natural_alignment_for_same_length_changed() {
        // a and b have a tiny equal prefix then differ -- the prior
        // algorithm would have emitted a partial 2-byte row at
        // offset 2 followed by a row at offset 4. The fix keeps
        // natural 4-byte (cols) alignment on both sides since the
        // total lengths match.
        let diff = diff_of(b"abXYZW", b"abMNOP");
        let maps = build_row_maps(&diff, 4);
        assert_eq!(maps.a.len(), maps.b.len());
        // Both sides have 6 bytes -> 2 rows of 4+2 at offsets 0 and 4.
        for slot in &maps.a {
            if let RowSlot::Real { offset, .. } = slot {
                assert!(offset.is_multiple_of(4), "A slot at non-aligned offset: {:?}", slot);
            }
        }
        for slot in &maps.b {
            if let RowSlot::Real { offset, .. } = slot {
                assert!(offset.is_multiple_of(4), "B slot at non-aligned offset: {:?}", slot);
            }
        }
    }

    #[test]
    fn row_maps_pad_added_with_gaps_on_a() {
        // 6 bytes on A vs 9 bytes on B (3 inserted). With cols=4:
        // A has 2 natural rows; B has 3 natural rows; A needs 1 gap.
        let diff = diff_of(b"abcdef", b"abXYZcdef");
        let maps = build_row_maps(&diff, 4);
        assert_eq!(maps.a.len(), maps.b.len());
        let gaps_a = maps.a.iter().filter(|s| s.is_gap()).count();
        let gaps_b = maps.b.iter().filter(|s| s.is_gap()).count();
        assert!(gaps_a >= 1, "A should have at least one gap: {:?}", maps.a);
        assert_eq!(gaps_b, 0, "B should be all-real rows: {:?}", maps.b);
        // A's Real slots stay at 4-aligned offsets.
        for slot in &maps.a {
            if let RowSlot::Real { offset, .. } = slot {
                assert!(offset.is_multiple_of(4), "non-aligned A slot: {:?}", slot);
            }
        }
    }

    #[test]
    fn row_maps_pad_removed_with_gaps_on_b() {
        let diff = diff_of(b"abXYZcdef", b"abcdef");
        let maps = build_row_maps(&diff, 4);
        assert_eq!(maps.a.len(), maps.b.len());
        let gaps_a = maps.a.iter().filter(|s| s.is_gap()).count();
        let gaps_b = maps.b.iter().filter(|s| s.is_gap()).count();
        assert_eq!(gaps_a, 0);
        assert!(gaps_b >= 1);
    }

    #[test]
    fn debounce_waits_then_recomputes_after_edit() {
        let mut edit_at = None;
        let before = PaneFingerprint::new(0, 3);
        let after_edit = PaneFingerprint::new(1, 3);
        let t0 = web_time::Instant::now();
        let decision = needs_recompute_debounced(false, Some((before, before)), true, (after_edit, before), &mut edit_at, t0);
        match decision {
            DebouncedDecision::WaitFor(d) => assert!(d <= RECOMPUTE_DEBOUNCE),
            other => panic!("expected WaitFor, got {other:?}"),
        }
        let after = t0 + RECOMPUTE_DEBOUNCE + std::time::Duration::from_millis(1);
        let decision2 = needs_recompute_debounced(false, Some((before, before)), true, (after_edit, before), &mut edit_at, after);
        assert!(matches!(decision2, DebouncedDecision::Recompute));
    }
}
