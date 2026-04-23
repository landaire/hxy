//! Reusable egui hex-view widget.
//!
//! Renders bytes from a [`HexSource`] in a virtualised scroll view with a
//! configurable number of hex columns (16 by default), an address column,
//! and an ASCII sidebar. Supports click-to-select, drag-to-select, and
//! shift-extend, with mirrored bounding-box highlights in both panes.

#![forbid(unsafe_code)]

use egui::Align2;
use egui::Color32;
use egui::FontId;
use egui::Pos2;
use egui::Rect;
use egui::Response;
use egui::Sense;
use egui::Stroke;
use egui::StrokeKind;
use egui::TextStyle;
use egui::Ui;
use egui::Vec2;
use hxy_core::ByteLen;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::ColumnCount;
use hxy_core::Error as CoreError;
use hxy_core::HexSource;
use hxy_core::RowIndex;
use hxy_core::Selection;

pub struct HexView<'s, S: HexSource + ?Sized> {
    source: &'s S,
    columns: ColumnCount,
    selection: &'s mut Option<Selection>,
}

impl<'s, S: HexSource + ?Sized> HexView<'s, S> {
    pub fn new(source: &'s S, selection: &'s mut Option<Selection>) -> Self {
        Self { source, columns: ColumnCount::DEFAULT, selection }
    }

    pub fn columns(mut self, cols: ColumnCount) -> Self {
        self.columns = cols;
        self
    }

    pub fn show(self, ui: &mut Ui) -> HexViewResponse {
        let Self { source, columns, selection } = self;
        let total_rows = row_count(source.len(), columns);
        let address_chars = address_hex_width(source.len());
        let font_id = TextStyle::Monospace.resolve(ui.style());
        let row_height = ui.text_style_height(&TextStyle::Monospace);
        let char_w = measure_char_width(ui, &font_id);
        let layout = RowLayout::compute(char_w, address_chars, columns);
        let source_len = source.len();

        let mut response = HexViewResponse::default();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(
            ui,
            row_height,
            total_rows,
            |ui, visible| {
                let range = match visible_byte_range(visible, columns, source_len) {
                    Some(r) => r,
                    None => return,
                };
                let bytes = match source.read(range) {
                    Ok(b) => b,
                    Err(e) => {
                        ui.colored_label(Color32::RED, format!("read error: {e}"));
                        response.error = Some(e);
                        return;
                    }
                };
                render_rows(ui, &layout, &font_id, row_height, range.start(), &bytes, selection);
            },
        );

        response
    }
}

#[derive(Default)]
pub struct HexViewResponse {
    pub error: Option<CoreError>,
}

fn row_count(len: ByteLen, columns: ColumnCount) -> usize {
    let len = len.get();
    if len == 0 {
        return 0;
    }
    let rows = len.div_ceil(columns.as_u64());
    usize::try_from(rows).unwrap_or(usize::MAX)
}

fn address_hex_width(len: ByteLen) -> usize {
    let bits_needed = 64 - len.get().saturating_sub(1).leading_zeros() as usize;
    bits_needed.div_ceil(4).max(8)
}

fn visible_byte_range(visible: std::ops::Range<usize>, columns: ColumnCount, len: ByteLen) -> Option<ByteRange> {
    let first = RowIndex::new(visible.start as u64);
    let last = RowIndex::new(visible.end as u64);
    let start = first.start_offset(columns);
    let end = ByteOffset::new(last.start_offset(columns).get().min(len.get()));
    let range = ByteRange::new(start, end).ok()?;
    if range.is_empty() { None } else { Some(range) }
}

fn measure_char_width(ui: &Ui, font_id: &FontId) -> f32 {
    let painter = ui.painter();
    let galley = painter.layout_no_wrap("0".to_string(), font_id.clone(), Color32::WHITE);
    galley.size().x
}

/// Precomputed x-offsets for every slot in a row. Addressed in "points".
struct RowLayout {
    address_w: f32,
    hex_start_x: f32,
    hex_cell_w: f32,
    hex_gap: f32,
    ascii_start_x: f32,
    ascii_cell_w: f32,
    total_width: f32,
    columns: ColumnCount,
}

impl RowLayout {
    fn compute(char_w: f32, address_chars: usize, columns: ColumnCount) -> Self {
        let address_w = char_w * address_chars as f32;
        let hex_gap = char_w * 0.5;
        let hex_cell_w = char_w * 2.0;
        let cols_f = f32::from(columns.get());
        let hex_total = cols_f * hex_cell_w + (cols_f - 1.0) * hex_gap;
        let section_gap = char_w * 2.0;
        let hex_start_x = address_w + section_gap;
        let ascii_cell_w = char_w;
        let ascii_start_x = hex_start_x + hex_total + section_gap;
        let ascii_total = cols_f * ascii_cell_w;
        Self {
            address_w,
            hex_start_x,
            hex_cell_w,
            hex_gap,
            ascii_start_x,
            ascii_cell_w,
            total_width: ascii_start_x + ascii_total,
            columns,
        }
    }

    fn hex_cell_rect(&self, row_rect: Rect, col: usize, row_height: f32) -> Rect {
        let left = row_rect.left() + self.hex_start_x + (col as f32) * (self.hex_cell_w + self.hex_gap);
        Rect::from_min_size(Pos2::new(left, row_rect.top()), Vec2::new(self.hex_cell_w, row_height))
    }

    fn ascii_cell_rect(&self, row_rect: Rect, col: usize, row_height: f32) -> Rect {
        let left = row_rect.left() + self.ascii_start_x + (col as f32) * self.ascii_cell_w;
        Rect::from_min_size(Pos2::new(left, row_rect.top()), Vec2::new(self.ascii_cell_w, row_height))
    }

    fn address_rect(&self, row_rect: Rect, row_height: f32) -> Rect {
        Rect::from_min_size(row_rect.min, Vec2::new(self.address_w, row_height))
    }

    /// Map a pointer position within `row_rect` to a column index in
    /// `0..chunk_len`. Returns `None` if the pointer is outside either pane.
    fn hit_test(&self, row_rect: Rect, pos: Pos2, chunk_len: usize) -> Option<usize> {
        let cols = usize::from(self.columns.get());
        let chunk = chunk_len.min(cols);
        let x = pos.x - row_rect.left();

        if x >= self.hex_start_x && x < self.ascii_start_x {
            let local = x - self.hex_start_x;
            let stride = self.hex_cell_w + self.hex_gap;
            let idx = (local / stride) as usize;
            if idx < chunk {
                return Some(idx);
            }
        }
        if x >= self.ascii_start_x && x < self.ascii_start_x + (cols as f32) * self.ascii_cell_w {
            let local = x - self.ascii_start_x;
            let idx = (local / self.ascii_cell_w) as usize;
            if idx < chunk {
                return Some(idx);
            }
        }
        None
    }
}

fn render_rows(
    ui: &mut Ui,
    layout: &RowLayout,
    font_id: &FontId,
    row_height: f32,
    start: ByteOffset,
    bytes: &[u8],
    selection: &mut Option<Selection>,
) {
    let cols = usize::from(layout.columns.get());
    let selected_range = selection.map(|s| s.range()).filter(|r| !r.is_empty());
    let cursor_offset = selection.map(|s| s.cursor);
    let text_color = ui.visuals().text_color();
    let weak_color = ui.visuals().weak_text_color();
    let selection_bg = ui.visuals().selection.bg_fill;
    let selection_fg = ui.visuals().selection.stroke.color;
    let cursor_stroke = Stroke::new(1.5, ui.visuals().strong_text_color());
    let shift_held = ui.input(|i| i.modifiers.shift);

    for (chunk_idx, chunk) in bytes.chunks(cols).enumerate() {
        let row_start_offset = ByteOffset::new(start.get() + (chunk_idx * cols) as u64);
        let (row_rect, response) =
            ui.allocate_exact_size(Vec2::new(layout.total_width, row_height), Sense::click_and_drag());
        let painter = ui.painter_at(row_rect);

        let address_rect = layout.address_rect(row_rect, row_height);
        painter.text(
            address_rect.left_center(),
            Align2::LEFT_CENTER,
            format_address(row_start_offset, (layout.address_w / layout.hex_cell_w * 2.0).round() as usize),
            font_id.clone(),
            weak_color,
        );

        for (i, byte) in chunk.iter().enumerate() {
            let byte_offset = ByteOffset::new(row_start_offset.get() + i as u64);
            let hex_rect = layout.hex_cell_rect(row_rect, i, row_height);
            let ascii_rect = layout.ascii_cell_rect(row_rect, i, row_height);

            let is_sel = selected_range.is_some_and(|r| r.contains(byte_offset));
            let is_cursor = cursor_offset == Some(byte_offset);

            let (hex_fg, ascii_fg) = if is_sel {
                painter.rect_filled(hex_rect, 2.0, selection_bg);
                painter.rect_filled(ascii_rect, 2.0, selection_bg);
                (selection_fg, selection_fg)
            } else {
                (text_color, text_color)
            };

            painter.text(hex_rect.center(), Align2::CENTER_CENTER, format!("{byte:02x}"), font_id.clone(), hex_fg);

            let ch = if (0x20..0x7f).contains(byte) { *byte as char } else { '.' };
            painter.text(ascii_rect.center(), Align2::CENTER_CENTER, ch.to_string(), font_id.clone(), ascii_fg);

            if is_cursor {
                painter.rect_stroke(hex_rect, 2.0, cursor_stroke, StrokeKind::Inside);
                painter.rect_stroke(ascii_rect, 2.0, cursor_stroke, StrokeKind::Inside);
            }
        }

        apply_interaction(&response, shift_held, layout, row_rect, row_start_offset, chunk.len(), selection);
    }
}

fn apply_interaction(
    response: &Response,
    shift_held: bool,
    layout: &RowLayout,
    row_rect: Rect,
    row_start_offset: ByteOffset,
    chunk_len: usize,
    selection: &mut Option<Selection>,
) {
    let Some(hover_pos) = response.interact_pointer_pos().or_else(|| response.hover_pos()) else {
        return;
    };
    let Some(col) = layout.hit_test(row_rect, hover_pos, chunk_len) else {
        return;
    };
    let hit_offset = ByteOffset::new(row_start_offset.get() + col as u64);

    if response.drag_started() {
        match (shift_held, *selection) {
            (true, Some(existing)) => {
                *selection = Some(Selection { anchor: existing.anchor, cursor: hit_offset });
            }
            _ => {
                *selection = Some(Selection::caret(hit_offset));
            }
        }
    } else if response.dragged() {
        if let Some(sel) = selection.as_mut() {
            sel.cursor = hit_offset;
        } else {
            *selection = Some(Selection::caret(hit_offset));
        }
    } else if response.clicked() {
        match (shift_held, *selection) {
            (true, Some(existing)) => {
                *selection = Some(Selection { anchor: existing.anchor, cursor: hit_offset });
            }
            _ => {
                *selection = Some(Selection::caret(hit_offset));
            }
        }
    }
}

fn format_address(offset: ByteOffset, width: usize) -> String {
    format!("{:0width$x}", offset.get(), width = width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_width_scales_with_length() {
        assert_eq!(address_hex_width(ByteLen::new(0)), 8);
        assert_eq!(address_hex_width(ByteLen::new(256)), 8);
        assert_eq!(address_hex_width(ByteLen::new(1u64 << 32)), 8);
        assert_eq!(address_hex_width(ByteLen::new((1u64 << 32) + 1)), 9);
    }

    #[test]
    fn row_count_handles_partial_row() {
        let cols = ColumnCount::new(16).unwrap();
        assert_eq!(row_count(ByteLen::new(0), cols), 0);
        assert_eq!(row_count(ByteLen::new(1), cols), 1);
        assert_eq!(row_count(ByteLen::new(16), cols), 1);
        assert_eq!(row_count(ByteLen::new(17), cols), 2);
    }

    #[test]
    fn format_address_zero_pads() {
        assert_eq!(format_address(ByteOffset::new(0x1a), 8), "0000001a");
    }

    #[test]
    fn visible_range_clamped_to_source_len() {
        let cols = ColumnCount::new(16).unwrap();
        let r = visible_byte_range(0..10, cols, ByteLen::new(100)).unwrap();
        assert_eq!(r.start().get(), 0);
        assert_eq!(r.end().get(), 100);
    }

    #[test]
    fn hit_test_maps_x_to_column_in_hex_pane() {
        let cols = ColumnCount::new(16).unwrap();
        let layout = RowLayout::compute(10.0, 8, cols);
        let row_rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(layout.total_width, 20.0));
        let hex_cell_1 = layout.hex_cell_rect(row_rect, 1, 20.0);
        assert_eq!(layout.hit_test(row_rect, hex_cell_1.center(), 16), Some(1));
        let ascii_cell_5 = layout.ascii_cell_rect(row_rect, 5, 20.0);
        assert_eq!(layout.hit_test(row_rect, ascii_cell_5.center(), 16), Some(5));
    }

    #[test]
    fn hit_test_rejects_points_past_short_row() {
        let cols = ColumnCount::new(16).unwrap();
        let layout = RowLayout::compute(10.0, 8, cols);
        let row_rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(layout.total_width, 20.0));
        // Short row of 3 bytes. Clicking at column 10 should miss.
        let hex_cell_10 = layout.hex_cell_rect(row_rect, 10, 20.0);
        assert_eq!(layout.hit_test(row_rect, hex_cell_10.center(), 3), None);
    }
}
