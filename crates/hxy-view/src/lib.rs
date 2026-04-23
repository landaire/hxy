//! Reusable egui hex-view widget.
//!
//! Renders bytes from a [`HexSource`] in a virtualised scroll view with a
//! configurable number of hex columns (16 by default), an address column,
//! and an ASCII sidebar.

#![forbid(unsafe_code)]

use egui::Color32;
use egui::FontId;
use egui::Label;
use egui::RichText;
use egui::TextStyle;
use egui::Ui;
use hxy_core::ByteLen;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::ColumnCount;
use hxy_core::Error as CoreError;
use hxy_core::HexSource;
use hxy_core::RowIndex;
use hxy_core::Selection;

/// Widget entry point. Borrow-only by design: nothing persists between
/// frames, so callers own the [`HexSource`] and (optionally) a
/// [`Selection`] displayed by the view.
pub struct HexView<'s, S: HexSource + ?Sized> {
    source: &'s S,
    columns: ColumnCount,
    selection: Option<&'s Selection>,
}

impl<'s, S: HexSource + ?Sized> HexView<'s, S> {
    pub fn new(source: &'s S) -> Self {
        Self { source, columns: ColumnCount::DEFAULT, selection: None }
    }

    pub fn columns(mut self, cols: ColumnCount) -> Self {
        self.columns = cols;
        self
    }

    pub fn selection(mut self, selection: &'s Selection) -> Self {
        self.selection = Some(selection);
        self
    }

    /// Render the hex view. The returned [`HexViewResponse`] reports any
    /// read error from the visible range so callers can log or surface it.
    pub fn show(self, ui: &mut Ui) -> HexViewResponse {
        let Self { source, columns, selection } = self;
        let total_rows = row_count(source.len(), columns);
        let address_chars = address_hex_width(source.len());
        let font_id = TextStyle::Monospace.resolve(ui.style());
        let row_height = ui.text_style_height(&TextStyle::Monospace);

        let mut response = HexViewResponse::default();
        let selected_range = selection.map(|s| s.range()).filter(|r| !r.is_empty());

        let scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
        scroll.show_rows(ui, row_height, total_rows, |ui, visible| {
            let range = match visible_byte_range(visible, columns, source.len()) {
                Some(r) => r,
                None => return,
            };
            match source.read(range) {
                Ok(bytes) => {
                    render_rows(ui, &font_id, columns, address_chars, range.start(), &bytes, selected_range);
                }
                Err(e) => {
                    ui.colored_label(Color32::RED, format!("read error: {e}"));
                    response.error = Some(e);
                }
            }
        });

        response
    }
}

/// Result of rendering one frame of the hex view.
#[derive(Default)]
pub struct HexViewResponse {
    /// Set when reading the currently-visible range failed.
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

/// Width in hex chars of the address column: at least 8 (4 GB),
/// wider for files beyond that.
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

fn render_rows(
    ui: &mut Ui,
    font_id: &FontId,
    columns: ColumnCount,
    address_chars: usize,
    start: ByteOffset,
    bytes: &[u8],
    selected: Option<ByteRange>,
) {
    let cols = usize::from(columns.get());
    for (chunk_idx, chunk) in bytes.chunks(cols).enumerate() {
        let row_offset = ByteOffset::new(start.get() + (chunk_idx * cols) as u64);
        ui.horizontal(|ui| {
            ui.add(
                Label::new(RichText::new(format_address(row_offset, address_chars)).font(font_id.clone()))
                    .selectable(false),
            );
            ui.add_space(8.0);
            for (i, byte) in chunk.iter().enumerate() {
                let byte_offset = ByteOffset::new(row_offset.get() + i as u64);
                let mut text = RichText::new(format!("{byte:02x}")).font(font_id.clone());
                if selected.is_some_and(|r| r.contains(byte_offset)) {
                    text = text
                        .background_color(ui.visuals().selection.bg_fill)
                        .color(ui.visuals().selection.stroke.color);
                }
                ui.add(Label::new(text).selectable(false));
            }
            for _ in chunk.len()..cols {
                ui.add(Label::new(RichText::new("  ").font(font_id.clone())).selectable(false));
            }
            ui.add_space(8.0);
            let ascii: String =
                chunk.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
            ui.add(Label::new(RichText::new(ascii).font(font_id.clone())).selectable(false));
        });
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
        // A file of exactly 2^32 bytes has last offset 0xFFFF_FFFF — still 8 hex chars.
        assert_eq!(address_hex_width(ByteLen::new(1u64 << 32)), 8);
        // One byte beyond that pushes the last offset to 0x1_0000_0000 — now 9.
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
}
