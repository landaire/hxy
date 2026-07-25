//! Side-by-side byte diff for two arbitrary sources.
//!
//! Each [`Tab::Compare`] tab owns a [`CompareSession`] -- two
//! [`ComparePane`] sides plus the cached [`DiffResult`] between them.
//! Each pane wraps its own [`hxy_view::HexEditor`] so both sides stay
//! independently editable with their own undo/redo, selection, and
//! scroll. The diff is recomputed (debounced) whenever either side's
//! patched view changes.
//!
//! Diff hunk computation, row-map alignment, and the recompute
//! debounce moved to `hxy_panels::diff` (framework-agnostic, shared
//! with the GPUI port); re-exported here under the original path.
//! `CompareSession` / `ComparePane` own the egui `HexEditor` and
//! worker plumbing and stay here.

pub mod pane;
// Picker uses sync `rfd::FileDialog` + `std::fs::read` and the
// command-palette compare-flow types -- both desktop-only. The
// wasm equivalent goes through `app::HxyApp`'s
// `rfd::AsyncFileDialog` plumbing.
#[cfg(not(target_arch = "wasm32"))]
pub mod picker;
pub mod tab;

use std::sync::Arc;

use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_vfs::TabSource;
pub use hxy_panels::diff::CompareRowMaps;
pub use hxy_panels::diff::DebouncedDecision;
pub use hxy_panels::diff::DiffHunk;
pub use hxy_panels::diff::DiffResult;
pub use hxy_panels::diff::HunkKind;
pub use hxy_panels::diff::RECOMPUTE_DEBOUNCE;
pub use hxy_panels::diff::build_row_maps;

use crate::files::EditMode;

/// Stable id for an open compare tab. Like [`crate::files::FileId`] /
/// [`crate::files::WorkspaceId`], allocated monotonically by the host
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
        Self { source, display_name: display_name.into(), editor, diff_colors: true }
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
    last_diff_fingerprint: Option<(hxy_panels::diff::PaneFingerprint, hxy_panels::diff::PaneFingerprint)>,
    /// Wall-clock time of the most recent observed mutation. Used
    /// as the start of the debounce window.
    edit_at: Option<web_time::Instant>,
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
    /// Index into [`Self::diff`]'s `hunks` of the row the diff
    /// table currently has hovered. Drives the secondary fill the
    /// hex panes paint via `hover_span` so hovering a row in the
    /// table previews the affected bytes in both views without
    /// committing a selection. Cleared when the table loses
    /// hover.
    pub hovered_hunk: Option<usize>,
    /// User-toggleable scroll mirroring between the two panes.
    /// On (default) -- whichever pane the user just scrolled
    /// drives the other; off -- panes scroll independently. Lives
    /// on the session because it's per-tab UX, not a global app
    /// setting.
    pub sync_scroll_enabled: bool,
    /// Per-tab override for the Myers diff deadline. When `Some`,
    /// the worker spawned by [`Self::request_recompute`] runs with
    /// this budget instead of the global
    /// [`crate::settings::AppSettings::compare_recompute_deadline`].
    /// `None` (default) tracks the global setting so changes there
    /// affect every session that hasn't opted out.
    pub recompute_deadline_override: Option<crate::settings::RecomputeDeadline>,
}

/// Cheap "did this side change?" snapshot pulled from the public
/// editor API -- undo-stack length plus source length covers
/// inserts, deletes, in-place writes, undo, redo, swap-source.
fn pane_fingerprint(pane: &ComparePane) -> hxy_panels::diff::PaneFingerprint {
    hxy_panels::diff::PaneFingerprint::new(pane.editor.undo_stack().len(), pane.editor.source().len().get())
}

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
    fingerprint: (hxy_panels::diff::PaneFingerprint, hxy_panels::diff::PaneFingerprint),
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
            hovered_hunk: None,
            sync_scroll_enabled: true,
            recompute_deadline_override: None,
        }
    }

    /// The deadline this session would use right now: its override
    /// when set, otherwise the supplied global default. Hosts
    /// resolve this once per recompute so toggling the override
    /// or editing the global setting and pressing the Recompute
    /// button immediately picks up the new value.
    pub fn effective_deadline(&self, global: crate::settings::RecomputeDeadline) -> crate::settings::RecomputeDeadline {
        match self.recompute_deadline_override {
            Some(d) => d,
            None => global,
        }
    }

    /// `true` while a worker thread is computing the diff. Hosts
    /// can use this to render a "computing..." indicator and to
    /// avoid issuing another recompute request.
    pub fn is_recomputing(&self) -> bool {
        self.pending_recompute.is_some()
    }

    /// Spawn a worker thread that computes the diff with the given
    /// deadline as a safety net. The thread reads from owned
    /// `Vec<u8>` snapshots (taken synchronously here) so it can
    /// outlive the editor without aliasing patched-source state.
    /// `ctx` is cloned into the worker so completing the diff
    /// requests an immediate UI repaint instead of waiting on the
    /// next ambient repaint. `deadline` is resolved by the caller
    /// (typically via [`Self::effective_deadline`]) so changes to
    /// the global setting or per-tab override are picked up at
    /// every recompute.
    pub fn request_recompute(&mut self, ctx: &egui::Context, deadline: crate::settings::RecomputeDeadline) {
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
        let fingerprint = (pane_fingerprint(&self.a), pane_fingerprint(&self.b));
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx_clone = ctx.clone();
        let deadline_dur = deadline.as_duration();
        crate::background::submit(move || {
            // `similar::capture_diff_slices_deadline` calls
            // `std::time::Instant::now()` internally, which panics
            // on wasm32-unknown-unknown. Skip the deadline on wasm
            // and run the diff to completion. The web build deals
            // with smaller inputs so the bound rarely matters; if a
            // genuine cap is ever needed, we can split-budget by
            // ops processed instead of wall-clock.
            #[cfg(not(target_arch = "wasm32"))]
            let hunks = {
                let deadline_at = std::time::Instant::now() + deadline_dur;
                hxy_panels::diff::diff_hunks_with_deadline(&a_bytes, &b_bytes, Some(deadline_at))
            };
            #[cfg(target_arch = "wasm32")]
            let hunks = {
                let _ = deadline_dur;
                hxy_panels::diff::diff_hunks(&a_bytes, &b_bytes)
            };
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
    /// pixel when content height changes underfoot. No-op when
    /// `sync_scroll_enabled` is off -- the user has opted out of
    /// the lockstep behaviour for this tab.
    pub fn sync_scroll(&mut self) {
        if !self.sync_scroll_enabled {
            self.last_synced_scroll = self.a.editor.scroll_offset();
            return;
        }
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
        let hunks = hxy_panels::diff::diff_hunks(&a_bytes, &b_bytes);
        self.diff = Some(DiffResult { hunks, a_len: a_bytes.len() as u64, b_len: b_bytes.len() as u64 });
        self.diff_serial = self.diff_serial.wrapping_add(1);
        self.last_diff_fingerprint = Some((pane_fingerprint(&self.a), pane_fingerprint(&self.b)));
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
    pub fn needs_recompute_debounced(&mut self, now: web_time::Instant) -> DebouncedDecision {
        let current = (pane_fingerprint(&self.a), pane_fingerprint(&self.b));
        hxy_panels::diff::needs_recompute_debounced(
            self.pending_recompute.is_some(),
            self.last_diff_fingerprint,
            self.diff.is_some(),
            current,
            &mut self.edit_at,
            now,
        )
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

// Diff-hunk and row-map logic is unit-tested directly in
// `hxy_panels::diff` now that it's pure. `CompareSession`'s debounce
// / recompute wiring around that logic is exercised the same way it
// always was here, but the pure-math cases moved with the code.
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
    fn debounce_idle_when_nothing_changed() {
        let mut s = session(b"abc", b"abc");
        let now = web_time::Instant::now();
        assert!(matches!(s.needs_recompute_debounced(now), DebouncedDecision::Idle));
    }

    #[test]
    fn debounce_waits_then_recomputes_after_edit() {
        let mut s = session(b"abc", b"abc");
        s.a.editor.request_write(0, vec![b'X']).unwrap();
        let t0 = web_time::Instant::now();
        match s.needs_recompute_debounced(t0) {
            DebouncedDecision::WaitFor(d) => assert!(d <= RECOMPUTE_DEBOUNCE),
            other => panic!("expected WaitFor, got {other:?}"),
        }
        let after = t0 + RECOMPUTE_DEBOUNCE + std::time::Duration::from_millis(1);
        assert!(matches!(s.needs_recompute_debounced(after), DebouncedDecision::Recompute));
    }
}
