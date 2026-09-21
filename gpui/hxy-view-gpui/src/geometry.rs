//! Pure hex/ascii grid layout math: cell positions and hit-testing.
//! No rendering; consumers pair this with the window's text system to
//! get [`CellMetrics`] and then paint against the positions below.

use gpui::Pixels;
use gpui::Point;
use gpui::px;
use hxy_core::ByteLen;
use hxy_core::ByteOffset;
use hxy_core::ColumnCount;
use hxy_core::RowSlot;
use hxy_editor::NibbleCursor;
use hxy_editor::Pane;

/// Font-derived cell dimensions, computed once per paint from the
/// window text system.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    /// Advance width of one monospace character.
    pub char_w: Pixels,
    pub line_h: Pixels,
}

/// Horizontal layout of one row: x origins (relative to the grid's
/// content origin) for the address gutter and each hex / ascii cell.
/// All math in character units times char_w, matching egui's
/// RowLayout proportions exactly (hxy-view/src/lib.rs,
/// RowLayout::compute): address gutter = address_chars + 2 chars
/// gap; each hex cell is 2 chars wide with a 0.5-char gap between
/// cells (stride 2.5 chars, no trailing gap after the last column);
/// 2 chars gap before the ascii pane; ascii cells 1 char wide.
///
/// `address_chars` is a snapshot of [`Self::address_chars_for`] taken
/// at construction time, not recomputed from the `source_len` passed
/// to [`Self::row_count`] / [`Self::hit_test`]. Rebuild the geometry
/// (call [`Self::new`] again) after the source grows or shrinks past
/// an address-width boundary, or x positions will be sized for the
/// stale width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridGeometry {
    pub metrics: CellMetrics,
    pub columns: u16,
    pub address_chars: usize,
}

/// Chars of glyph pair only, per hex cell.
const HEX_CELL_GLYPH_CHARS: f32 = 2.0;
/// Chars of gap between adjacent hex cells (egui's `hex_gap`).
const HEX_CELL_GAP_CHARS: f32 = 0.5;
/// Left-edge-to-left-edge distance between adjacent hex cells:
/// glyph pair + inter-cell gap. There is no trailing gap after the
/// last column (egui's `hex_total = cols*hex_cell_w + (cols-1)*hex_gap`).
const HEX_CELL_STRIDE_CHARS: f32 = HEX_CELL_GLYPH_CHARS + HEX_CELL_GAP_CHARS;
/// Chars of gap between the address gutter and the hex pane, and
/// between the hex pane and the ascii pane.
const SECTION_GAP_CHARS: f32 = 2.0;

impl GridGeometry {
    pub fn new(metrics: CellMetrics, columns: ColumnCount, source_len: ByteLen, virtual_base: u64) -> Self {
        // The gutter must fit the highest virtual address (base + last byte).
        let max_address = ByteLen::new(virtual_base.saturating_add(source_len.get()));
        Self { metrics, columns: columns.get(), address_chars: Self::address_chars_for(max_address) }
    }

    /// Minimum hex digits to address the source, min 8 (same rule as
    /// egui's address_hex_width).
    pub fn address_chars_for(source_len: ByteLen) -> usize {
        let bits_needed = 64 - source_len.get().saturating_sub(1).leading_zeros() as usize;
        bits_needed.div_ceil(4).max(8)
    }

    pub fn address_x(&self) -> Pixels {
        px(0.0)
    }

    /// x origin of the start of the hex pane (before any columns).
    fn hex_pane_start(&self) -> Pixels {
        self.metrics.char_w * (self.address_chars as f32 + SECTION_GAP_CHARS)
    }

    /// x origin one past the last hex cell's glyph pair (no trailing
    /// gap), i.e. the start of the section gap before the ascii pane.
    /// `columns * stride - gap`: `stride` includes one inter-cell gap
    /// per column, but the last column has no gap after it.
    fn hex_pane_end(&self) -> Pixels {
        self.hex_pane_start()
            + self.metrics.char_w * (f32::from(self.columns) * HEX_CELL_STRIDE_CHARS - HEX_CELL_GAP_CHARS)
    }

    pub fn hex_x(&self, col: u16) -> Pixels {
        self.hex_pane_start() + self.metrics.char_w * (f32::from(col) * HEX_CELL_STRIDE_CHARS)
    }

    fn ascii_pane_start(&self) -> Pixels {
        self.hex_pane_end() + self.metrics.char_w * SECTION_GAP_CHARS
    }

    pub fn ascii_x(&self, col: u16) -> Pixels {
        self.ascii_pane_start() + self.metrics.char_w * f32::from(col)
    }

    /// Total content width.
    pub fn row_width(&self) -> Pixels {
        self.ascii_x(self.columns)
    }

    /// 2 chars (glyph pair, excl. spacing).
    pub fn hex_cell_w(&self) -> Pixels {
        self.metrics.char_w * HEX_CELL_GLYPH_CHARS
    }

    /// Total rows incl. the EOF-cursor row (same rule as egui's
    /// row_count: empty source renders 1 row; len == k*cols renders
    /// k+1 rows).
    pub fn row_count(&self, source_len: ByteLen) -> u64 {
        let cols = u64::from(self.columns);
        source_len.get().saturating_add(1).div_ceil(cols).max(1)
    }

    /// Hit-test a point (relative to content origin, y already
    /// adjusted for scroll) to a pane/byte/nibble.
    ///
    /// Follows egui's `RowLayout::hit_test` (hxy-view/src/lib.rs) for
    /// the x axis: x left of the hex pane (the address gutter) returns
    /// `None`. Any x in `[hex_pane_start, ascii_pane_start)` -- including
    /// the section gap trailing the last hex column -- belongs to the
    /// hex pane, with the column clamped to the last one; the gap does
    /// not read as a no-hit. x past the ascii pane's last column returns
    /// `None`. A `y` past the last row clamps to the last row (the
    /// EOF-cursor row) rather than returning `None`.
    ///
    /// One intentional divergence from egui: a click on the EOF row
    /// lands on the one-past-end insertion slot (offset clamped to
    /// `source_len`), whereas egui clamps the same click to the last
    /// byte (`len - 1`). Reaching the EOF insertion caret by mouse is
    /// strictly more capable and matches how the keyboard handles the
    /// EOF position.
    pub fn hit_test(&self, pos: Point<Pixels>, source_len: ByteLen) -> Option<GridHit> {
        let max_row = self.row_count(source_len).saturating_sub(1);
        let row = self.row_at(pos.y).min(max_row);
        let (pane, col, nibble) = self.column_at(pos.x)?;
        let raw_offset = row.saturating_mul(u64::from(self.columns)).saturating_add(u64::from(col));
        let offset = raw_offset.min(source_len.get());
        Some(GridHit { pane, offset: ByteOffset::new(offset), nibble })
    }

    /// Slot-aware hit-test: the vertical row indexes into `slots`
    /// (its length is the row count), and the byte offset comes from
    /// that row's [`RowSlot`] rather than the linear `row * cols + col`.
    /// Mirrors egui hxy-view's `HitCtx::byte_offset_at`: a [`RowSlot::Gap`]
    /// row returns `None`, and a column past a [`RowSlot::Real`] row's
    /// `len` (a click over the empty tail of a partial row) returns `None`
    /// too. A `y` past the last slot clamps to it, matching the linear
    /// [`Self::hit_test`]'s bottom-row clamp.
    pub fn hit_test_slots(&self, pos: Point<Pixels>, slots: &[RowSlot]) -> Option<GridHit> {
        let max_row = (slots.len() as u64).saturating_sub(1);
        let row = self.row_at(pos.y).min(max_row);
        let (pane, col, nibble) = self.column_at(pos.x)?;
        match *slots.get(row as usize)? {
            RowSlot::Real { offset, len } => {
                if u64::from(col) >= u64::from(len) {
                    return None;
                }
                Some(GridHit { pane, offset: ByteOffset::new(offset + u64::from(col)), nibble })
            }
            RowSlot::Gap => None,
        }
    }

    /// Visual row under `y` (relative to content origin, scroll already
    /// applied), floored and clamped to non-negative. Callers clamp the
    /// upper bound against the row count / slot count themselves.
    fn row_at(&self, y: Pixels) -> u64 {
        (y / self.metrics.line_h).max(0.0).floor() as u64
    }

    /// Resolve the pane, column, and hovered nibble for an `x` (relative
    /// to content origin). `None` for the address gutter or past the
    /// ascii pane's last column. Shared by the linear and slot-aware
    /// hit-tests so both honor egui's identical x-axis rules (see
    /// [`Self::hit_test`]'s doc).
    fn column_at(&self, x: Pixels) -> Option<(Pane, u16, NibbleCursor)> {
        let hex_start = self.hex_pane_start();
        let ascii_start = self.ascii_pane_start();
        let ascii_end = self.ascii_x(self.columns);
        let last_col = self.columns.saturating_sub(1);

        if x >= hex_start && x < ascii_start {
            let local_chars = (x - hex_start) / self.metrics.char_w;
            let col = ((local_chars / HEX_CELL_STRIDE_CHARS).floor() as u16).min(last_col);
            let cell_local_chars = local_chars - f32::from(col) * HEX_CELL_STRIDE_CHARS;
            let nibble =
                if cell_local_chars < HEX_CELL_GLYPH_CHARS / 2.0 { NibbleCursor::High } else { NibbleCursor::Low };
            Some((Pane::Hex, col, nibble))
        } else if x >= ascii_start && x < ascii_end {
            let col = ((x - ascii_start) / self.metrics.char_w).floor() as u16;
            Some((Pane::Ascii, col, NibbleCursor::High))
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridHit {
    pub pane: Pane,
    pub offset: ByteOffset,
    pub nibble: NibbleCursor,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;
    use gpui::px;
    use hxy_core::ByteLen;
    use hxy_core::ColumnCount;
    use hxy_core::RowSlot;

    fn geo() -> GridGeometry {
        let metrics = CellMetrics { char_w: px(8.0), line_h: px(16.0) };
        GridGeometry::new(metrics, ColumnCount::new(16).unwrap(), ByteLen::new(256), 0)
    }

    #[test]
    fn address_width_matches_egui_rule() {
        assert_eq!(GridGeometry::address_chars_for(ByteLen::new(0)), 8);
        assert_eq!(GridGeometry::address_chars_for(ByteLen::new(1u64 << 32)), 8);
        assert_eq!(GridGeometry::address_chars_for(ByteLen::new((1u64 << 32) + 1)), 9);
    }

    #[test]
    fn row_count_reserves_eof_row() {
        let g = geo();
        assert_eq!(g.row_count(ByteLen::new(0)), 1);
        assert_eq!(g.row_count(ByteLen::new(15)), 1);
        assert_eq!(g.row_count(ByteLen::new(16)), 2);
    }

    #[test]
    fn hex_cells_use_egui_stride() {
        let g = geo();
        assert_eq!(g.hex_x(1) - g.hex_x(0), px(20.0));
        assert_eq!(g.hex_cell_w(), px(16.0));
    }

    #[test]
    fn hit_test_left_half_of_hex_cell_is_high_nibble() {
        let g = geo();
        let x = g.hex_x(2) + px(3.0);
        let hit = g.hit_test(point(x, px(20.0)), ByteLen::new(256)).unwrap();
        assert_eq!(hit.pane, hxy_editor::Pane::Hex);
        assert_eq!(hit.offset.get(), 16 + 2);
        assert_eq!(hit.nibble, hxy_editor::NibbleCursor::High);
    }

    #[test]
    fn hit_test_ascii_pane() {
        let g = geo();
        let x = g.ascii_x(5) + px(1.0);
        let hit = g.hit_test(point(x, px(0.0)), ByteLen::new(256)).unwrap();
        assert_eq!(hit.pane, hxy_editor::Pane::Ascii);
        assert_eq!(hit.offset.get(), 5);
    }

    /// Points to the right of the ascii pane's last column are outside
    /// the grid entirely and return `None`, matching egui hxy-view's
    /// `hovered_byte`, which treats out-of-bounds x the same as an
    /// inter-pane gap: no hit.
    #[test]
    fn hit_test_x_past_last_column_is_none() {
        let g = geo();
        let x = g.ascii_x(15) + px(100.0);
        let hit = g.hit_test(point(x, px(0.0)), ByteLen::new(256));
        assert!(hit.is_none());
    }

    /// The address gutter (left of the hex pane) is dead space: no
    /// pane owns it, so hit-testing there returns `None`. The section
    /// gap trailing the last hex column, by contrast, is NOT dead
    /// space: it belongs to the hex pane's last column (clamped),
    /// matching egui's `RowLayout::hit_test`, which maps any x in
    /// `[hex_start_x, ascii_start_x)` to the hex pane. A click there
    /// also lands on the Low nibble, since it falls past the
    /// clamped cell's glyph-pair midpoint.
    #[test]
    fn hit_test_gutter_is_none_and_section_gap_clamps_to_last_hex_column() {
        let g = geo();
        let gutter_x = g.hex_x(0) - px(1.0);
        assert!(g.hit_test(point(gutter_x, px(0.0)), ByteLen::new(256)).is_none());

        let section_gap_x = g.ascii_x(0) - px(1.0);
        let hit = g.hit_test(point(section_gap_x, px(0.0)), ByteLen::new(256)).unwrap();
        assert_eq!(hit.pane, hxy_editor::Pane::Hex);
        assert_eq!(hit.offset.get(), 15);
        assert_eq!(hit.nibble, hxy_editor::NibbleCursor::Low);
    }

    /// A y past the last row clamps to the last row (the EOF-cursor
    /// row) instead of returning `None`, so dragging a selection below
    /// the visible content still lands on a byte.
    #[test]
    fn hit_test_y_past_last_row_clamps_to_last_row() {
        let g = geo();
        let x = g.hex_x(0) + px(3.0);
        let hit = g.hit_test(point(x, px(10_000.0)), ByteLen::new(256)).unwrap();
        assert_eq!(hit.pane, hxy_editor::Pane::Hex);
        // Last row is the EOF row (row_count - 1 == 16), byte offset 256.
        assert_eq!(hit.offset.get(), 256);
    }

    /// A hit on a [`RowSlot::Real`] row reads the byte offset from the
    /// slot, not from `row * cols`: slot 0 starts at an arbitrary
    /// offset (200 here), so column 3 of the first visual row lands on
    /// offset 203.
    #[test]
    fn hit_test_slots_reads_offset_from_slot() {
        let g = geo();
        let slots = [RowSlot::real(200, 16), RowSlot::real(216, 16)];
        let x = g.hex_x(3) + px(1.0);
        let hit = g.hit_test_slots(point(x, px(0.0)), &slots).unwrap();
        assert_eq!(hit.pane, hxy_editor::Pane::Hex);
        assert_eq!(hit.offset.get(), 203);

        // Second visual row indexes slot 1.
        let hit = g.hit_test_slots(point(x, g.metrics.line_h), &slots).unwrap();
        assert_eq!(hit.offset.get(), 219);
    }

    /// A click over the empty tail of a partial [`RowSlot::Real`] row
    /// (column at or past the slot's `len`) is a no-hit, matching egui
    /// `HitCtx::byte_offset_at`.
    #[test]
    fn hit_test_slots_partial_row_tail_is_none() {
        let g = geo();
        let slots = [RowSlot::real(0, 4)];
        // Column 3 is the last real byte.
        let x3 = g.hex_x(3) + px(1.0);
        assert_eq!(g.hit_test_slots(point(x3, px(0.0)), &slots).unwrap().offset.get(), 3);
        // Column 5 is past the 4-byte slot: no hit.
        let x5 = g.hex_x(5) + px(1.0);
        assert!(g.hit_test_slots(point(x5, px(0.0)), &slots).is_none());
    }

    /// Hit-testing a [`RowSlot::Gap`] row returns `None`: gap rows own
    /// no bytes even though they occupy a row's height.
    #[test]
    fn hit_test_slots_gap_row_is_none() {
        let g = geo();
        let slots = [RowSlot::real(0, 16), RowSlot::Gap, RowSlot::real(16, 16)];
        let x = g.hex_x(2) + px(1.0);
        // Row 1 is the gap.
        assert!(g.hit_test_slots(point(x, g.metrics.line_h), &slots).is_none());
        // Row 2 is real again.
        let hit = g.hit_test_slots(point(x, g.metrics.line_h * 2.0), &slots).unwrap();
        assert_eq!(hit.offset.get(), 18);
    }

    /// `hxy_core::row_for_byte` is the offset->row inverse the pane uses
    /// for scroll targets: it finds the slot covering a byte and skips
    /// gaps / off-the-end bytes.
    #[test]
    fn row_for_byte_maps_through_slots() {
        let slots = [RowSlot::real(200, 16), RowSlot::Gap, RowSlot::real(300, 8)];
        assert_eq!(hxy_core::row_for_byte(&slots, 205), Some(0));
        assert_eq!(hxy_core::row_for_byte(&slots, 300), Some(2));
        assert_eq!(hxy_core::row_for_byte(&slots, 307), Some(2));
        // Past the last slot's real bytes: no row.
        assert_eq!(hxy_core::row_for_byte(&slots, 308), None);
        // A byte between the two real slots that no slot owns.
        assert_eq!(hxy_core::row_for_byte(&slots, 250), None);
    }
}
