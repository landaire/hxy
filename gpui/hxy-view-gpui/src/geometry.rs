//! Pure hex/ascii grid layout math: cell positions and hit-testing.
//! No rendering; consumers pair this with the window's text system to
//! get [`CellMetrics`] and then paint against the positions below.

use gpui::Pixels;
use gpui::Point;
use gpui::px;
use hxy_core::ByteLen;
use hxy_core::ByteOffset;
use hxy_core::ColumnCount;
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
/// All math in character units times char_w, mirroring the egui
/// RowLayout proportions (hxy-view/src/lib.rs, RowLayout::compute):
/// address gutter = address_chars + 2 chars gap; each hex cell is
/// 3 chars wide (2 glyphs + 1 space); 2 chars gap before the ascii
/// pane; ascii cells 1 char wide.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridGeometry {
    pub metrics: CellMetrics,
    pub columns: u16,
    pub address_chars: usize,
}

/// Chars of glyph pair + trailing space per hex cell.
const HEX_CELL_STRIDE_CHARS: f32 = 3.0;
/// Chars of glyph pair only (excludes the trailing space).
const HEX_CELL_GLYPH_CHARS: f32 = 2.0;
/// Chars of gap between the address gutter and the hex pane, and
/// between the hex pane and the ascii pane.
const SECTION_GAP_CHARS: f32 = 2.0;

impl GridGeometry {
    pub fn new(metrics: CellMetrics, columns: ColumnCount, source_len: ByteLen) -> Self {
        Self { metrics, columns: columns.get(), address_chars: Self::address_chars_for(source_len) }
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

    /// x origin one past the last hex cell's trailing space, i.e. the
    /// start of the gap before the ascii pane.
    fn hex_pane_end(&self) -> Pixels {
        self.hex_pane_start() + self.metrics.char_w * (f32::from(self.columns) * HEX_CELL_STRIDE_CHARS)
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
    /// Points left of the hex pane (the address gutter), in the gap
    /// between the hex and ascii panes, or right of the ascii pane's
    /// last column return `None` -- those spans belong to no pane.
    /// A `y` past the last row clamps to the last row (the EOF-cursor
    /// row) rather than returning `None`. The resulting byte offset
    /// is clamped to `source_len`, so the EOF row's cursor position
    /// can land exactly on `source_len` (one past the last byte).
    pub fn hit_test(&self, pos: Point<Pixels>, source_len: ByteLen) -> Option<GridHit> {
        let max_row = self.row_count(source_len).saturating_sub(1);
        let row_raw = (pos.y / self.metrics.line_h).max(0.0).floor() as u64;
        let row = row_raw.min(max_row);

        let hex_start = self.hex_pane_start();
        let hex_end = self.hex_pane_end();
        let ascii_start = self.ascii_pane_start();
        let ascii_end = self.ascii_x(self.columns);

        let (pane, col, nibble) = if pos.x >= hex_start && pos.x < hex_end {
            let local_chars = (pos.x - hex_start) / self.metrics.char_w;
            let col = (local_chars / HEX_CELL_STRIDE_CHARS).floor() as u16;
            let cell_local_chars = local_chars - f32::from(col) * HEX_CELL_STRIDE_CHARS;
            let nibble =
                if cell_local_chars < HEX_CELL_GLYPH_CHARS / 2.0 { NibbleCursor::High } else { NibbleCursor::Low };
            (Pane::Hex, col, nibble)
        } else if pos.x >= ascii_start && pos.x < ascii_end {
            let col = ((pos.x - ascii_start) / self.metrics.char_w).floor() as u16;
            (Pane::Ascii, col, NibbleCursor::High)
        } else {
            return None;
        };

        let raw_offset = row.saturating_mul(u64::from(self.columns)).saturating_add(u64::from(col));
        let offset = raw_offset.min(source_len.get());
        Some(GridHit { pane, offset: ByteOffset::new(offset), nibble })
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

    fn geo() -> GridGeometry {
        let metrics = CellMetrics { char_w: px(8.0), line_h: px(16.0) };
        GridGeometry::new(metrics, ColumnCount::new(16).unwrap(), ByteLen::new(256))
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
    fn hex_cells_are_three_chars_apart() {
        let g = geo();
        assert_eq!(g.hex_x(1) - g.hex_x(0), px(24.0));
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
    /// hovered_byte, which treats out-of-bounds x the same as an
    /// inter-pane gap: no hit.
    #[test]
    fn hit_test_clamps_past_last_column() {
        let g = geo();
        let x = g.ascii_x(15) + px(100.0);
        let hit = g.hit_test(point(x, px(0.0)), ByteLen::new(256));
        assert!(hit.is_none());
    }

    /// The gap between the address gutter and the hex pane, and the
    /// gap between the hex pane and the ascii pane, are dead space:
    /// no pane owns them, so hit-testing there returns `None`.
    #[test]
    fn hit_test_gap_between_panes_is_none() {
        let g = geo();
        let gap_x = g.hex_x(0) - px(1.0);
        assert!(g.hit_test(point(gap_x, px(0.0)), ByteLen::new(256)).is_none());

        let inter_pane_gap_x = g.ascii_x(0) - px(1.0);
        assert!(g.hit_test(point(inter_pane_gap_x, px(0.0)), ByteLen::new(256)).is_none());
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
}
