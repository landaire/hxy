//! [`ComparePanel`]: side-by-side byte diff of two sources.
//!
//! Two independent [`HexPane`] editors (each over its own in-memory
//! source), a toolbar (sync-scroll + diff-colors toggles, recompute
//! status), and a bottom hunk table. All diff logic -- hunk
//! computation, gap-aligned row maps, the recompute debounce -- lives
//! in `hxy_panels::diff` (shared with the egui front end); this module
//! only owns the two panes, the reactive recompute plumbing, the
//! per-byte diff coloring, and the table wiring. It mirrors the egui
//! app's `CompareSession` / `render_compare_tab`
//! (`crates/hxy/src/compare/`), reworked onto gpui's entity/observer
//! model instead of egui's per-frame poll.
//!
//! Unlike the per-file panels (strings, entropy, checksums), a compare
//! tab is not owned by a single file: it holds its own two editors, so
//! it never binds to a sibling `FilePanel`. Persistence is scoped to
//! disk-vs-disk compares (both sides re-read from their paths on
//! restore); a side sourced from an open file's in-memory buffer is
//! dropped on restore (see `dump` / `crate::persist`).

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::App;
use gpui::AppContext;
use gpui::Context;
use gpui::Div;
use gpui::Entity;
use gpui::EventEmitter;
use gpui::FocusHandle;
use gpui::Focusable;
use gpui::Hsla;
use gpui::InteractiveElement;
use gpui::IntoElement;
use gpui::ParentElement;
use gpui::Render;
use gpui::SharedString;
use gpui::Stateful;
use gpui::StatefulInteractiveElement;
use gpui::Styled;
use gpui::Subscription;
use gpui::Task;
use gpui::WeakEntity;
use gpui::Window;
use gpui::div;
use gpui::px;
use gpui::component::ActiveTheme;
use gpui::component::Disableable;
use gpui::component::Selectable;
use gpui::component::button::Button;
use gpui::component::dock::BasePanel;
use gpui::component::dock::Panel;
use gpui::component::dock::PanelEvent;
use gpui::component::dock::PanelInfo;
use gpui::component::dock::PanelState;
use gpui::component::h_flex;
use gpui::component::label::Label;
use gpui::component::table::Column;
use gpui::component::table::DataTable;
use gpui::component::table::TableDelegate;
use gpui::component::table::TableEvent;
use gpui::component::table::TableState;
use gpui::component::v_flex;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_core::Selection;
use hxy_core::byte_palette::ValueHighlight;
use hxy_panels::diff::DiffHunk;
use hxy_panels::diff::DiffResult;
use hxy_panels::diff::HunkKind;
use hxy_panels::diff::PaneFingerprint;
use hxy_panels::diff::RECOMPUTE_DEBOUNCE;
use hxy_panels::diff::build_row_maps;
use hxy_view_gpui::ByteStyleOverride;
use hxy_view_gpui::HexPane;

/// Stable identifier for layout (de)serialization; must never change.
pub const COMPARE_PANEL_NAME: &str = "ComparePanel";

/// Diff colors, mirroring the egui compare view exactly
/// (`crates/hxy/src/compare/pane.rs`): green added, red removed, orange
/// changed. gpui-component's theme exposes no semantic add/remove/change
/// tokens, so the fixed RGB values keep visual parity across the two
/// front ends rather than guessing at theme colors.
const COLOR_ADDED: u32 = 0x3cc864;
const COLOR_REMOVED: u32 = 0xdc5a5a;
const COLOR_CHANGED: u32 = 0xdca03c;

/// Fixed height of the bottom hunk table.
const TABLE_HEIGHT: f32 = 180.0;

// The wall-clock safety net for the Myers diff comes from the user's
// `compare_recompute_deadline` setting (read per recompute in
// `Self::recompute`). egui's per-tab `recompute_deadline_override`
// has no gpui equivalent yet -- the global setting always applies.

/// Which side of the compare pair a range / styler belongs to. `A` is
/// the old side, `B` the new side (matching `similar`'s indices).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    A,
    B,
}

/// One side's construction inputs: its display name, initial bytes, and
/// the disk path to re-read on restore (`None` for an open-file-sourced
/// side, which is dropped on restore).
pub struct CompareSideInit {
    pub name: String,
    pub bytes: Vec<u8>,
    pub restore_path: Option<PathBuf>,
}

/// The persisted identity of one side: its display name and, when the
/// side is disk-restorable, its path.
struct SideMeta {
    name: String,
    restore_path: Option<PathBuf>,
}

pub struct ComparePanel {
    a: Entity<HexPane>,
    b: Entity<HexPane>,
    a_meta: SideMeta,
    b_meta: SideMeta,
    /// Most recent diff, or `None` until the first compute completes.
    diff: Option<DiffResult>,
    /// Fingerprints both sides were at when `diff` was computed; drives
    /// the "did an edit happen since?" check.
    last_fingerprint: Option<(PaneFingerprint, PaneFingerprint)>,
    /// A background diff is in flight; suppresses overlapping recomputes.
    recomputing: bool,
    /// Paint the per-byte diff colors on both panes.
    diff_colors: bool,
    /// The highlight mode the installed diff stylers were built for:
    /// `Text` puts the diff color on the glyph (the palette owns the
    /// fill there), `Background` on the cell fill. Mirrors egui's
    /// `compare_kind_style` channel pick.
    styler_mode: ValueHighlight,
    /// Mirror scroll between the two panes.
    sync_scroll: bool,
    /// Last vertical scroll (in rows) both panes agreed on, used to pick
    /// the leader when they diverge. Mirrors the egui session's
    /// `last_synced_scroll`.
    last_synced_scroll: f32,
    table: Entity<TableState<CompareTableDelegate>>,
    _table_sub: Subscription,
    _a_observe: Subscription,
    _b_observe: Subscription,
    /// Re-applies the view settings (columns, minimap flags) to both
    /// panes when the settings global changes; egui's compare pane
    /// reads them per frame instead.
    _settings_observe: Subscription,
    _debounce: Option<Task<()>>,
    _recompute: Option<Task<()>>,
}

impl ComparePanel {
    /// Build a compare tab over two byte sources, kicking off the first
    /// diff immediately (no debounce -- the tab has nothing to show yet).
    pub fn from_sources(a: CompareSideInit, b: CompareSideInit, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let a_src: Arc<dyn HexSource> = Arc::new(MemorySource::new(a.bytes));
        let b_src: Arc<dyn HexSource> = Arc::new(MemorySource::new(b.bytes));
        let a_pane = cx.new(|cx| HexPane::new(a_src, cx));
        let b_pane = cx.new(|cx| HexPane::new(b_src, cx));

        let weak = cx.entity().downgrade();
        let table = cx.new(|cx| TableState::new(CompareTableDelegate::new(weak), window, cx));
        let table_sub = cx.subscribe(&table, Self::on_table_event);

        let a_observe = cx.observe(&a_pane, Self::on_pane_changed);
        let b_observe = cx.observe(&b_pane, Self::on_pane_changed);
        let settings_observe =
            cx.observe_global::<crate::settings::SettingsGlobal>(|this, cx| this.apply_view_settings(cx));

        let mut this = Self {
            a: a_pane,
            b: b_pane,
            a_meta: SideMeta { name: a.name, restore_path: a.restore_path },
            b_meta: SideMeta { name: b.name, restore_path: b.restore_path },
            diff: None,
            last_fingerprint: None,
            recomputing: false,
            diff_colors: true,
            styler_mode: crate::settings::view_mode(crate::settings::settings(cx).byte_highlight_mode),
            sync_scroll: true,
            last_synced_scroll: 0.0,
            table,
            _table_sub: table_sub,
            _a_observe: a_observe,
            _b_observe: b_observe,
            _settings_observe: settings_observe,
            _debounce: None,
            _recompute: None,
        };
        this.apply_view_settings(cx);
        this.recompute(cx);
        this
    }

    /// Push the user's hex-view settings onto both panes (egui's
    /// compare pane reads `state.app` per frame). Guarded per field so
    /// unrelated settings mutations (e.g. a recents bump) do not
    /// trigger spurious repaints; a columns change also rebuilds the
    /// aligned row maps, which are laid out per column count.
    fn apply_view_settings(&mut self, cx: &mut Context<Self>) {
        let s = crate::settings::settings(cx);
        let columns_changed = self.a.read(cx).columns() != s.hex_columns;
        // The egui compare pane turns the byte-value highlight on/off
        // and picks the mode, but never installs a palette override,
        // so hxy-view always falls back to the Class tables there --
        // the scheme setting is ignored (compare/pane.rs:60-75 vs
        // hxy-view's for_theme_and_mode fallback). Mirrored here via
        // `compare_highlight_palette`. Derived per settings change, so
        // a theme flip re-lands on the next settings pass rather than
        // immediately (accepted lag for compare tabs).
        let highlight = crate::settings::compare_highlight_palette(&s, cx.theme().mode.is_dark());
        let mode = crate::settings::view_mode(s.byte_highlight_mode);
        for pane in [self.a.clone(), self.b.clone()] {
            pane.update(cx, |pane, cx| {
                if pane.columns() != s.hex_columns {
                    pane.set_columns(s.hex_columns, cx);
                }
                if pane.show_minimap() != s.show_minimap {
                    pane.set_show_minimap(s.show_minimap, cx);
                }
                if pane.minimap_colored() != s.minimap_colored {
                    pane.set_minimap_colored(s.minimap_colored, cx);
                }
                if pane.highlight() != highlight.as_ref() {
                    pane.set_highlight(highlight.clone(), cx);
                }
            });
        }
        if columns_changed {
            self.rebuild_row_maps(cx);
        }
        if mode != self.styler_mode {
            self.styler_mode = mode;
            self.apply_stylers(cx);
        }
    }

    /// Recompute both panes' aligned row maps from the current diff at
    /// the current column count. No-op until the first diff lands.
    fn rebuild_row_maps(&mut self, cx: &mut Context<Self>) {
        let columns = self.a.read(cx).columns().as_u64();
        let maps = match &self.diff {
            Some(diff) => build_row_maps(diff, columns),
            None => return,
        };
        self.a.update(cx, |p, cx| p.set_row_map(Some(maps.a), cx));
        self.b.update(cx, |p, cx| p.set_row_map(Some(maps.b), cx));
        cx.notify();
    }

    /// Rebuild a disk-vs-disk compare from persisted [`PanelInfo`]. Both
    /// paths are re-read; callers prune non-restorable payloads before
    /// restore (see `crate::persist`), so a missing path or read failure
    /// here is only a defensive fallback to an empty buffer.
    pub fn restore(info: &PanelInfo, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let a = restore_side(info, "a_path", "a_name", window);
        let b = restore_side(info, "b_path", "b_name", window);
        Self::from_sources(a, b, window, cx)
    }

    #[cfg(test)]
    pub(crate) fn pane_a(&self) -> &Entity<HexPane> {
        &self.a
    }

    #[cfg(test)]
    pub(crate) fn pane_b(&self) -> &Entity<HexPane> {
        &self.b
    }

    #[cfg(test)]
    pub(crate) fn diff(&self) -> Option<&DiffResult> {
        self.diff.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn is_recomputing(&self) -> bool {
        self.recomputing
    }

    #[cfg(test)]
    pub(crate) fn sync_scroll_enabled(&self) -> bool {
        self.sync_scroll
    }

    /// Non-equal hunks in diff order -- the rows the table shows and the
    /// indices `jump_to_hunk` / `set_hover` resolve against.
    fn visible_hunks(&self) -> Vec<DiffHunk> {
        match &self.diff {
            Some(diff) => diff.changes().copied().collect(),
            None => Vec::new(),
        }
    }

    /// Runs on any change to either pane's editor (edit or scroll):
    /// mirror scroll, then re-arm the recompute debounce if an edit
    /// moved a fingerprint.
    fn on_pane_changed(&mut self, _pane: Entity<HexPane>, cx: &mut Context<Self>) {
        self.mirror_scroll(cx);
        self.maybe_schedule_recompute(cx);
    }

    /// Mirror whichever pane the user just scrolled onto the other, so
    /// the two aligned row maps stay in lockstep. Mirrors the egui
    /// session's `sync_scroll` (`crates/hxy/src/compare/mod.rs:300`):
    /// pick the leader by which side moved from the last agreed scroll,
    /// then snap both to it. No-op when disabled or already in sync.
    fn mirror_scroll(&mut self, cx: &mut Context<Self>) {
        let a = self.a.read(cx).scroll_rows();
        if !self.sync_scroll {
            self.last_synced_scroll = a;
            return;
        }
        let b = self.b.read(cx).scroll_rows();
        let eps = 1e-3_f32;
        if (a - b).abs() <= eps {
            self.last_synced_scroll = a;
            return;
        }
        let a_moved = (a - self.last_synced_scroll).abs() > eps;
        let b_moved = (b - self.last_synced_scroll).abs() > eps;
        let leader = match (a_moved, b_moved) {
            (true, false) => a,
            (false, true) => b,
            // Both moved this frame: take the side that moved further,
            // the one the user is most likely dragging.
            (true, true) => {
                if (a - self.last_synced_scroll).abs() >= (b - self.last_synced_scroll).abs() {
                    a
                } else {
                    b
                }
            }
            // Residual mismatch from a prior frame; snap to A.
            (false, false) => a,
        };
        if (a - leader).abs() > eps {
            self.a.update(cx, |p, cx| p.set_scroll_rows(leader, cx));
        }
        if (b - leader).abs() > eps {
            self.b.update(cx, |p, cx| p.set_scroll_rows(leader, cx));
        }
        self.last_synced_scroll = leader;
    }

    /// (Re)arm the 300ms recompute debounce if either side has mutated
    /// since the last diff. Replacing the stored task cancels the prior
    /// timer, so a burst of edits pushes the deadline out until they
    /// settle -- the reactive equivalent of the egui app's
    /// `needs_recompute_debounced` poll. Scroll-only changes don't move
    /// a fingerprint, so they never schedule a recompute.
    fn maybe_schedule_recompute(&mut self, cx: &mut Context<Self>) {
        if self.recomputing {
            return;
        }
        let current = (self.fingerprint(Side::A, cx), self.fingerprint(Side::B, cx));
        let changed = match self.last_fingerprint {
            Some(last) => last != current,
            None => self.diff.is_none(),
        };
        if !changed {
            return;
        }
        self._debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RECOMPUTE_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.recompute(cx));
        }));
    }

    fn fingerprint(&self, side: Side, cx: &App) -> PaneFingerprint {
        let pane = self.pane(side).read(cx);
        PaneFingerprint::new(pane.editor().undo_stack().len(), pane.editor().source().len().get())
    }

    fn pane(&self, side: Side) -> &Entity<HexPane> {
        match side {
            Side::A => &self.a,
            Side::B => &self.b,
        }
    }

    /// Snapshot both sides' bytes and diff them off the UI thread.
    /// Mirrors the egui session's `request_recompute`: the worker owns
    /// `Vec<u8>` snapshots so it never aliases live editor state.
    /// Force a fresh diff regardless of the fingerprint gate -- the
    /// toolbar Recompute button, egui parity
    /// (`crates/hxy/src/compare/tab.rs`). Lets the user re-run a diff the
    /// recompute deadline truncated on a prior pass. Disabled in the
    /// UI while a run is in flight, matching egui's `add_enabled`.
    pub(crate) fn recompute_now(&mut self, cx: &mut Context<Self>) {
        self.recompute(cx);
    }

    fn recompute(&mut self, cx: &mut Context<Self>) {
        if self.recomputing {
            return;
        }
        let a_bytes = read_all(&self.a, cx);
        let b_bytes = read_all(&self.b, cx);
        let a_len = a_bytes.len() as u64;
        let b_len = b_bytes.len() as u64;
        let fingerprint = (self.fingerprint(Side::A, cx), self.fingerprint(Side::B, cx));
        let deadline = crate::settings::settings(cx).compare_recompute_deadline.as_duration();
        self.recomputing = true;
        cx.notify();
        self._recompute = Some(cx.spawn(async move |this, cx| {
            let hunks = cx.background_spawn(async move { diff_with_deadline(&a_bytes, &b_bytes, deadline) }).await;
            let diff = DiffResult { hunks, a_len, b_len };
            let _ = this.update(cx, |this, cx| this.apply_diff(diff, fingerprint, cx));
        }));
    }

    /// Install a freshly computed diff: push aligned row maps and the
    /// per-byte color stylers onto both panes, refresh the table, then
    /// re-check for edits that landed while the worker ran.
    fn apply_diff(
        &mut self,
        diff: DiffResult,
        fingerprint: (PaneFingerprint, PaneFingerprint),
        cx: &mut Context<Self>,
    ) {
        self.recomputing = false;
        self.last_fingerprint = Some(fingerprint);
        let columns = self.a.read(cx).columns().as_u64();
        let maps = build_row_maps(&diff, columns);
        self.a.update(cx, |p, cx| p.set_row_map(Some(maps.a), cx));
        self.b.update(cx, |p, cx| p.set_row_map(Some(maps.b), cx));
        self.diff = Some(diff);
        self.apply_stylers(cx);
        self.table.update(cx, |t, cx| t.refresh(cx));
        cx.notify();
        // An edit during the worker run leaves the fingerprint moved;
        // this re-arms the debounce to catch it (the egui app relies on
        // its next poll for the same).
        self.maybe_schedule_recompute(cx);
    }

    /// Push the current diff's per-byte color stylers onto both panes,
    /// or clear them when diff-colors is toggled off.
    fn apply_stylers(&mut self, cx: &mut Context<Self>) {
        let (a_ranges, b_ranges) = match &self.diff {
            Some(diff) => (side_ranges(diff, Side::A), side_ranges(diff, Side::B)),
            None => (Vec::new(), Vec::new()),
        };
        let on = self.diff_colors;
        let mode = self.styler_mode;
        self.a.update(cx, |p, cx| p.set_byte_styler(on.then(|| make_styler(a_ranges, mode)), cx));
        self.b.update(cx, |p, cx| p.set_byte_styler(on.then(|| make_styler(b_ranges, mode)), cx));
    }

    fn toggle_diff_colors(&mut self, cx: &mut Context<Self>) {
        self.diff_colors = !self.diff_colors;
        self.apply_stylers(cx);
        cx.notify();
    }

    fn toggle_sync_scroll(&mut self, cx: &mut Context<Self>) {
        self.sync_scroll = !self.sync_scroll;
        if self.sync_scroll {
            // Re-sync immediately so re-enabling snaps the panes back
            // together instead of waiting for the next scroll.
            self.last_synced_scroll = self.a.read(cx).scroll_rows();
            self.b.update(cx, |p, cx| p.set_scroll_rows(self.last_synced_scroll, cx));
        }
        cx.notify();
    }

    /// Select and scroll both panes to the hunk's byte ranges. Mirrors
    /// the egui table's click handling (`crates/hxy/src/compare/tab.rs`):
    /// scroll each side only when it has bytes there (an Added hunk has
    /// nothing on A, a Removed hunk nothing on B).
    pub(crate) fn jump_to_hunk(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(hunk) = self.visible_hunks().get(row).copied() else { return };
        if hunk.a_len > 0 {
            scroll_pane_to(&self.a, hunk.a_offset, hunk.a_len, cx);
        }
        if hunk.b_len > 0 {
            scroll_pane_to(&self.b, hunk.b_offset, hunk.b_len, cx);
        }
    }

    /// Preview a hunk on both panes via their secondary hover band, or
    /// clear it. Mirrors the egui session's `hovered_hunk` -> `hover_span`
    /// wiring.
    pub(crate) fn set_hover(&mut self, row: Option<usize>, cx: &mut Context<Self>) {
        let (a_span, b_span) = match row.and_then(|r| self.visible_hunks().get(r).copied()) {
            Some(hunk) => (hover_span(hunk.a_offset, hunk.a_len), hover_span(hunk.b_offset, hunk.b_len)),
            None => (None, None),
        };
        self.a.update(cx, |p, cx| p.set_hover_span(a_span, cx));
        self.b.update(cx, |p, cx| p.set_hover_span(b_span, cx));
    }

    fn on_table_event(
        &mut self,
        _table: Entity<TableState<CompareTableDelegate>>,
        event: &TableEvent,
        cx: &mut Context<Self>,
    ) {
        if let TableEvent::SelectRow(row) = event {
            self.jump_to_hunk(*row, cx);
        }
    }

    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let status = if self.recomputing {
            hxy_i18n::t("compare-status-recomputing")
        } else if let Some(diff) = &self.diff {
            hxy_i18n::t_args(
                "compare-status",
                &[("a", &self.a_meta.name), ("b", &self.b_meta.name), ("changes", &diff.change_count().to_string())],
            )
        } else {
            hxy_i18n::t("compare-status-pending")
        };
        h_flex()
            .gap_2()
            .items_center()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("compare-recompute")
                    .label(hxy_i18n::t("compare-recompute"))
                    .compact()
                    .disabled(self.recomputing)
                    .on_click(cx.listener(|this, _, _, cx| this.recompute_now(cx))),
            )
            .child(
                Button::new("compare-sync-scroll")
                    .label(hxy_i18n::t("compare-sync-scroll"))
                    .compact()
                    .selected(self.sync_scroll)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sync_scroll(cx))),
            )
            .child(
                Button::new("compare-diff-colors")
                    .label(hxy_i18n::t("compare-diff-colors-toggle"))
                    .compact()
                    .selected(self.diff_colors)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_diff_colors(cx))),
            )
            .child(Label::new(status).text_color(cx.theme().muted_foreground))
    }

    fn render_panes(&self, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .flex_1()
            .min_h_0()
            .child(div().flex_1().min_w_0().h_full().child(self.a.clone()))
            .child(div().w(px(1.0)).h_full().bg(cx.theme().border))
            .child(div().flex_1().min_w_0().h_full().child(self.b.clone()))
    }
}

/// Sorted-by-start `(start, end_exclusive, kind)` ranges for one side,
/// skipping hunks with no bytes on that side. Mirrors egui's
/// `compare_pane_ranges` (`crates/hxy/src/compare/pane.rs`).
fn side_ranges(diff: &DiffResult, side: Side) -> Vec<(u64, u64, HunkKind)> {
    diff.changes()
        .filter_map(|h| {
            let (offset, len) = match side {
                Side::A => (h.a_offset, h.a_len),
                Side::B => (h.b_offset, h.b_len),
            };
            if len == 0 {
                return None;
            }
            Some((offset, offset + len, h.kind))
        })
        .collect()
}

/// Per-byte styler over the side's sorted ranges, tinting each byte by
/// hunk kind. The diff color lands on the glyph in `Text` mode (where
/// the highlight palette owns the cell fill) and on the cell fill
/// otherwise, mirroring egui's `compare_kind_style`
/// (`crates/hxy/src/compare/pane.rs`). `partition_point` finds the
/// last range that starts at or before the offset, mirroring egui's
/// byte_styler there.
fn make_styler(
    ranges: Vec<(u64, u64, HunkKind)>,
    mode: ValueHighlight,
) -> Box<dyn Fn(u8, ByteOffset) -> ByteStyleOverride + Send> {
    Box::new(move |_byte, offset| {
        let off = offset.get();
        let idx = ranges.partition_point(|(start, _, _)| *start <= off);
        if idx == 0 {
            return ByteStyleOverride::default();
        }
        let (_, end_exclusive, kind) = ranges[idx - 1];
        if off >= end_exclusive {
            return ByteStyleOverride::default();
        }
        match (kind_color(kind), mode) {
            (Some(color), ValueHighlight::Text) => ByteStyleOverride { bg: None, fg: Some(color) },
            (Some(color), ValueHighlight::Background) => ByteStyleOverride { bg: Some(color), fg: None },
            (None, _) => ByteStyleOverride::default(),
        }
    })
}

/// Run the byte diff with the user-configured deadline safety net.
/// The deadline path is unavailable on wasm (`similar` calls
/// `Instant::now()` internally, which panics there), so wasm runs to
/// completion -- matching the egui app's own cfg split.
fn diff_with_deadline(a: &[u8], b: &[u8], budget: Duration) -> Vec<DiffHunk> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let deadline = std::time::Instant::now() + budget;
        hxy_panels::diff::diff_hunks_with_deadline(a, b, Some(deadline))
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = budget;
        hxy_panels::diff::diff_hunks(a, b)
    }
}

fn kind_color(kind: HunkKind) -> Option<Hsla> {
    let rgb = match kind {
        HunkKind::Added => COLOR_ADDED,
        HunkKind::Removed => COLOR_REMOVED,
        HunkKind::Changed => COLOR_CHANGED,
        HunkKind::Equal => return None,
    };
    Some(gpui::rgb(rgb).into())
}

/// Byte range for one side of a hunk, or `None` when that side is empty
/// (Added on A / Removed on B). Mirrors egui's `pane_hover_span`.
fn hover_span(offset: u64, len: u64) -> Option<ByteRange> {
    if len == 0 {
        return None;
    }
    ByteRange::new(ByteOffset::new(offset), ByteOffset::new(offset + len)).ok()
}

/// Select the pane's `[offset, offset+len)` and scroll it into view.
/// Mirrors egui's `scroll_pane_to`.
fn scroll_pane_to(pane: &Entity<HexPane>, offset: u64, len: u64, cx: &mut App) {
    pane.update(cx, |pane, cx| {
        let end_inclusive = offset.saturating_add(len.max(1)).saturating_sub(1);
        pane.editor_mut()
            .set_selection(Some(Selection { anchor: ByteOffset::new(offset), cursor: ByteOffset::new(end_inclusive) }));
        pane.editor_mut().set_scroll_to_byte(ByteOffset::new(offset));
        pane.sync_pending_scroll(cx);
    });
}

/// Read a pane's whole source into an owned buffer, or an empty buffer
/// on read failure (logged) -- the diff of an unreadable side against
/// its peer is still meaningful (all removed / all added).
fn read_all(pane: &Entity<HexPane>, cx: &App) -> Vec<u8> {
    let source = pane.read(cx).editor().source().clone();
    let len = source.len().get();
    if len == 0 {
        return Vec::new();
    }
    let range = match ByteRange::new(ByteOffset::new(0), ByteOffset::new(len)) {
        Ok(range) => range,
        Err(err) => {
            tracing::warn!(%err, "compare: side byte range");
            return Vec::new();
        }
    };
    match source.read(range) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(%err, "compare: read side bytes");
            Vec::new()
        }
    }
}

fn restore_side(info: &PanelInfo, path_key: &str, name_key: &str, _window: &mut Window) -> CompareSideInit {
    let path = string_field(info, path_key).map(PathBuf::from);
    let name = string_field(info, name_key)
        .or_else(|| path.as_deref().map(leaf_name))
        .unwrap_or_else(|| hxy_i18n::t("gpui-file-untitled"));
    let (bytes, restore_path) = match &path {
        Some(path) => match std::fs::read(path) {
            Ok(bytes) => (bytes, Some(path.clone())),
            Err(err) => {
                tracing::warn!(?path, %err, "compare restore: re-read failed; empty buffer");
                (Vec::new(), Some(path.clone()))
            }
        },
        None => (Vec::new(), None),
    };
    CompareSideInit { name, bytes, restore_path }
}

fn string_field(info: &PanelInfo, key: &str) -> Option<String> {
    let PanelInfo::Panel(value) = info else { return None };
    value.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

/// The leaf file name of a path, falling back to the full display form.
pub(crate) fn leaf_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
}

impl BasePanel for ComparePanel {
    fn panel_name(&self) -> &'static str {
        COMPARE_PANEL_NAME
    }

    /// Persist a disk-restorable compare (both sides have a path);
    /// otherwise write a payload with a missing side path so
    /// `crate::persist::prune_for_restore` drops the tab (mirrors the
    /// egui app dropping open-file-side compares on restore).
    fn dump(&self, _cx: &App) -> PanelState {
        let mut state = PanelState::new(self.panel_name());
        state.info = PanelInfo::panel(serde_json::json!({
            "a_path": self.a_meta.restore_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "a_name": self.a_meta.name,
            "b_path": self.b_meta.restore_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "b_name": self.b_meta.name,
        }));
        state
    }

    /// Clear both panes' hover bands on removal so a pointer resting on a
    /// hunk row when the tab closes leaves nothing stale behind.
    fn on_removed(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_hover(None, cx);
    }
}

impl Panel for ComparePanel {
    fn title(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        SharedString::from(compare_title(&self.a_meta.name, &self.b_meta.name))
    }

    fn tab_name(&self, _cx: &App) -> Option<SharedString> {
        Some(SharedString::from(compare_title(&self.a_meta.name, &self.b_meta.name)))
    }
}

fn compare_title(a: &str, b: &str) -> String {
    hxy_i18n::t_args("tab-compare-title", &[("a", a), ("b", b)])
}

impl Focusable for ComparePanel {
    /// Focus lands on the A pane so the tab is keyboard-ready.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.a.read(cx).focus_handle(cx)
    }
}

impl EventEmitter<PanelEvent> for ComparePanel {}

impl Render for ComparePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(self.render_toolbar(cx))
            .child(self.render_panes(cx))
            .child(
                div()
                    .h(px(TABLE_HEIGHT))
                    .min_h(px(TABLE_HEIGHT))
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(DataTable::new(&self.table)),
            )
    }
}

struct CompareTableDelegate {
    columns: [Column; 4],
    panel: WeakEntity<ComparePanel>,
}

impl CompareTableDelegate {
    fn new(panel: WeakEntity<ComparePanel>) -> Self {
        Self {
            columns: [
                Column::new("kind", hxy_i18n::t("compare-table-kind")).width(px(96.0)),
                Column::new("a", hxy_i18n::t("compare-table-a-range")).width(px(160.0)),
                Column::new("b", hxy_i18n::t("compare-table-b-range")).width(px(160.0)),
                Column::new("size", hxy_i18n::t("compare-table-size")).width(px(140.0)),
            ],
            panel,
        }
    }

    fn hunk(&self, row: usize, cx: &App) -> Option<DiffHunk> {
        let panel = self.panel.upgrade()?;
        panel.read(cx).visible_hunks().get(row).copied()
    }
}

impl TableDelegate for CompareTableDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, cx: &App) -> usize {
        self.panel.upgrade().map(|p| p.read(cx).visible_hunks().len()).unwrap_or(0)
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        self.columns[col_ix].clone()
    }

    /// Preview the hovered hunk on both panes.
    fn render_tr(&mut self, row_ix: usize, _window: &mut Window, _cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let panel = self.panel.clone();
        div().id(("compare-row", row_ix)).on_hover(move |hovered, _window, cx| {
            if let Some(panel) = panel.upgrade() {
                let row = hovered.then_some(row_ix);
                panel.update(cx, |panel, cx| panel.set_hover(row, cx));
            }
        })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(hunk) = self.hunk(row_ix, cx) else {
            return div().into_any_element();
        };
        match col_ix {
            0 => {
                let (key, color) = match hunk.kind {
                    HunkKind::Added => ("compare-kind-added", COLOR_ADDED),
                    HunkKind::Removed => ("compare-kind-removed", COLOR_REMOVED),
                    HunkKind::Changed => ("compare-kind-changed", COLOR_CHANGED),
                    HunkKind::Equal => return div().into_any_element(),
                };
                div().text_color(gpui::rgb(color)).child(hxy_i18n::t(key)).into_any_element()
            }
            1 => div().child(format_range(hunk.a_offset, hunk.a_len)).into_any_element(),
            2 => div().child(format_range(hunk.b_offset, hunk.b_len)).into_any_element(),
            3 => div()
                .child(hxy_i18n::t_args(
                    "compare-table-size-fmt",
                    &[("a", &hunk.a_len.to_string()), ("b", &hunk.b_len.to_string())],
                ))
                .into_any_element(),
            _ => div().into_any_element(),
        }
    }

    fn render_empty(&mut self, _window: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let text = match self.panel.upgrade() {
            Some(panel) if panel.read(cx).diff.is_some() => hxy_i18n::t("compare-no-differences"),
            Some(_) => hxy_i18n::t("compare-status-pending"),
            None => String::new(),
        };
        h_flex().size_full().justify_center().text_color(cx.theme().muted_foreground).child(text)
    }
}

fn format_range(offset: u64, len: u64) -> String {
    if len == 0 { format!("0x{offset:08X} {}", hxy_i18n::t("compare-gap")) } else { format!("0x{offset:08X} +{len}") }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::Entity;
    use gpui::TestAppContext;

    use super::*;

    fn setup(cx: &mut TestAppContext) {
        cx.update(gpui::component::init);
    }

    fn side(name: &str, bytes: Vec<u8>) -> CompareSideInit {
        CompareSideInit { name: name.to_string(), bytes, restore_path: None }
    }

    /// The diff color lands on the channel egui's `compare_kind_style`
    /// picks for the highlight mode: cell fill in `Background`, glyph
    /// in `Text` (where the palette owns the fill); equal spans and
    /// uncovered offsets style nothing.
    #[test]
    fn diff_styler_channel_follows_highlight_mode() {
        let ranges = vec![(0u64, 4u64, HunkKind::Added), (4, 8, HunkKind::Equal)];
        let added = kind_color(HunkKind::Added).unwrap();

        let bg_mode = make_styler(ranges.clone(), ValueHighlight::Background);
        assert_eq!(bg_mode(0, ByteOffset::new(1)), ByteStyleOverride { bg: Some(added), fg: None });
        assert_eq!(bg_mode(0, ByteOffset::new(5)), ByteStyleOverride::default(), "equal spans style nothing");
        assert_eq!(bg_mode(0, ByteOffset::new(9)), ByteStyleOverride::default(), "past the last range");

        let text_mode = make_styler(ranges, ValueHighlight::Text);
        assert_eq!(text_mode(0, ByteOffset::new(1)), ByteStyleOverride { bg: None, fg: Some(added) });
    }

    /// A gap row's range label is localized rather than the hard-coded
    /// "(gap)"; a sized range keeps its offset + length form.
    #[test]
    fn gap_range_label_is_localized() {
        let gap = format_range(0x10, 0);
        assert!(gap.contains(&hxy_i18n::t("compare-gap")), "gap label localized: {gap}");
        assert_eq!(format_range(0x10, 4), "0x00000010 +4");
    }

    /// Build a `ComparePanel` inside a real `gpui::component::Root` window
    /// (the `TableState` its toolbar hosts needs the Root layer, like the
    /// strings panel's harness), let the initial diff settle, and return
    /// the panel plus a driving context.
    fn build(cx: &mut TestAppContext, a: Vec<u8>, b: Vec<u8>) -> (Entity<ComparePanel>, &mut gpui::VisualTestContext) {
        let window = cx.add_window(|window, cx| {
            let panel = cx.new(|cx| ComparePanel::from_sources(side("a", a), side("b", b), window, cx));
            gpui::component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<ComparePanel>().unwrap());
        let vcx = gpui::VisualTestContext::from_window(*window, cx).into_mut();
        vcx.run_until_parked();
        (panel, vcx)
    }

    /// The initial diff produces hunks and installs gap-aligned row maps
    /// of equal length on both panes -- the alignment invariant the two
    /// hex views render in lockstep on. B has 16 bytes A lacks, so A's
    /// map carries the gap.
    #[gpui::test]
    fn initial_diff_aligns_both_panes(cx: &mut TestAppContext) {
        setup(cx);
        let a = vec![0xAAu8; 32];
        let mut b = vec![0xAAu8; 32];
        b.splice(16..16, vec![0xBBu8; 16]);
        let (panel, cx) = build(cx, a, b);

        panel.read_with(cx, |panel, cx| {
            let diff = panel.diff().expect("initial diff computed");
            assert!(diff.change_count() >= 1, "an inserted run is a change");
            let a_map = panel.pane_a().read(cx).row_map().expect("A row map").len();
            let b_map = panel.pane_b().read(cx).row_map().expect("B row map").len();
            assert_eq!(a_map, b_map, "both panes render the same number of rows");
            let a_gaps = panel.pane_a().read(cx).row_map().unwrap().iter().filter(|s| s.is_gap()).count();
            assert!(a_gaps >= 1, "A pads the inserted region with a gap row");
        });
    }

    /// The toolbar Recompute button forces a fresh diff even when no
    /// fingerprint moved: `recompute_now` re-enters the background worker
    /// (observable via the recomputing flag) regardless of the debounce
    /// gate, letting the user re-run a diff a deadline truncated.
    #[gpui::test]
    fn recompute_button_forces_a_fresh_diff(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, vec![0u8; 32], vec![0u8; 32]);
        assert!(!panel.read_with(cx, |p, _| p.is_recomputing()), "the initial diff settled");

        // No edit, so no fingerprint moved; the button must still re-run.
        panel.update(cx, |p, cx| p.recompute_now(cx));
        assert!(panel.read_with(cx, |p, _| p.is_recomputing()), "the button re-entered the worker");
        cx.run_until_parked();
        assert!(!panel.read_with(cx, |p, _| p.is_recomputing()), "the forced recompute finished");
    }

    /// Editing a side moves its fingerprint; after the debounce elapses a
    /// background recompute lands a fresh diff. Byte 0 was equal, so the
    /// overwrite introduces a change the first diff didn't have.
    #[gpui::test]
    fn edit_triggers_debounced_recompute(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, vec![0u8; 64], vec![0u8; 64]);
        let before = panel.read_with(cx, |p, _| p.diff().unwrap().change_count());
        assert_eq!(before, 0, "identical buffers start with no changes");

        panel.update(cx, |p, cx| {
            p.pane_a().update(cx, |pane, cx| {
                pane.editor_mut().request_write(0, vec![0xFF]).unwrap();
                // Production edits notify through the key handler; drive
                // the same notify so the panel's observer arms the
                // debounce.
                cx.notify();
            });
        });
        // The edit notifies the pane; the observer arms the debounce.
        cx.run_until_parked();
        cx.executor().advance_clock(RECOMPUTE_DEBOUNCE + Duration::from_millis(1));
        cx.run_until_parked();

        let after = panel.read_with(cx, |p, _| p.diff().unwrap().change_count());
        assert!(after >= 1, "the overwrite shows up as a change after the debounce");
    }

    /// Clicking a hunk row selects that hunk's byte range on both panes.
    #[gpui::test]
    fn hunk_click_selects_both_panes(cx: &mut TestAppContext) {
        setup(cx);
        // A single changed byte at offset 8 on both sides.
        let a = vec![0u8; 32];
        let mut b = vec![0u8; 32];
        b[8] = 0xFF;
        let (panel, cx) = build(cx, a, b);

        panel.update(cx, |p, cx| p.jump_to_hunk(0, cx));

        let (a_sel, b_sel) = panel.read_with(cx, |p, cx| {
            (p.pane_a().read(cx).editor().selection(), p.pane_b().read(cx).editor().selection())
        });
        let a_sel = a_sel.expect("A selection set");
        let b_sel = b_sel.expect("B selection set");
        assert_eq!(a_sel.range().start().get(), 8);
        assert_eq!(b_sel.range().start().get(), 8);
    }

    /// With sync-scroll on (the default), scrolling one pane mirrors onto
    /// the other via the observer-driven `mirror_scroll`.
    #[gpui::test]
    fn sync_scroll_mirrors_between_panes(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, vec![0u8; 256 * 16], vec![0u8; 256 * 16]);
        assert!(panel.read_with(cx, |p, _| p.sync_scroll_enabled()));

        panel.update(cx, |p, cx| p.pane_a().update(cx, |pane, cx| pane.set_scroll_rows(3.0, cx)));
        cx.run_until_parked();

        let (a, b) =
            panel.read_with(cx, |p, cx| (p.pane_a().read(cx).scroll_rows(), p.pane_b().read(cx).scroll_rows()));
        assert_eq!(a, 3.0, "A moved to row 3");
        assert_eq!(b, a, "B mirrored A");
    }

    /// Disabling sync-scroll leaves the panes independent.
    #[gpui::test]
    fn disabled_sync_scroll_keeps_panes_independent(cx: &mut TestAppContext) {
        setup(cx);
        let (panel, cx) = build(cx, vec![0u8; 256 * 16], vec![0u8; 256 * 16]);
        panel.update(cx, |p, cx| p.toggle_sync_scroll(cx));
        assert!(!panel.read_with(cx, |p, _| p.sync_scroll_enabled()));

        panel.update(cx, |p, cx| p.pane_a().update(cx, |pane, cx| pane.set_scroll_rows(5.0, cx)));
        cx.run_until_parked();

        let b = panel.read_with(cx, |p, cx| p.pane_b().read(cx).scroll_rows());
        assert_eq!(b, 0.0, "B stays put when sync is off");
    }

    /// A disk-restorable compare (both sides carry a path) dumps both
    /// paths; a side without a path dumps a null so pruning drops the tab.
    #[gpui::test]
    fn dump_records_restore_paths(cx: &mut TestAppContext) {
        setup(cx);
        let window = cx.add_window(|window, cx| {
            let a = CompareSideInit {
                name: "a".into(),
                bytes: vec![1, 2, 3],
                restore_path: Some(PathBuf::from("/tmp/a.bin")),
            };
            let b = CompareSideInit {
                name: "b".into(),
                bytes: vec![4, 5, 6],
                restore_path: Some(PathBuf::from("/tmp/b.bin")),
            };
            let panel = cx.new(|cx| ComparePanel::from_sources(a, b, window, cx));
            gpui::component::Root::new(panel, window, cx)
        });
        let root = window.root(cx).unwrap();
        let panel = root.read_with(cx, |root, _| root.view().clone().downcast::<ComparePanel>().unwrap());
        let cx = gpui::VisualTestContext::from_window(*window, cx).into_mut();

        let dumped = panel.read_with(cx, |p, cx| p.dump(cx));
        assert_eq!(dumped.panel_name, COMPARE_PANEL_NAME);
        let PanelInfo::Panel(value) = &dumped.info else { panic!("panel info") };
        assert_eq!(value.get("a_path").and_then(|v| v.as_str()), Some("/tmp/a.bin"));
        assert_eq!(value.get("b_path").and_then(|v| v.as_str()), Some("/tmp/b.bin"));
    }
}
