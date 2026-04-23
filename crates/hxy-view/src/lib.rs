//! Reusable egui hex-view widget.
//!
//! Renders bytes from a [`HexSource`] in a virtualised scroll view with a
//! configurable number of hex columns (16 by default), an address column,
//! and an ASCII sidebar. Supports click, shift-click, and drag across
//! arbitrary row boundaries to extend the selection. Highlights are
//! painted as contiguous bars across each row for readability.

#![forbid(unsafe_code)]

use egui::Align2;
use egui::Color32;
use egui::FontId;
use egui::Pos2;
use egui::Rect;
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

/// Where the byte-value palette should be applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueHighlight {
    /// Paint the palette as a background fill per byte. Text gets a
    /// contrast-adjusted color so it stays readable over the tint.
    Background,
    /// Tint the hex/ascii glyphs themselves; leave the background alone.
    Text,
}

/// Callback type for context menu rendering — invoked on right-click
/// anywhere in the hex or ASCII pane.
pub type ContextMenuFn<'s> = Box<dyn FnOnce(&mut egui::Ui) + 's>;

pub struct HexView<'s, S: HexSource + ?Sized> {
    source: &'s S,
    columns: ColumnCount,
    selection: &'s mut Option<Selection>,
    value_highlight: Option<ValueHighlight>,
    context_menu: Option<ContextMenuFn<'s>>,
}

impl<'s, S: HexSource + ?Sized> HexView<'s, S> {
    pub fn new(source: &'s S, selection: &'s mut Option<Selection>) -> Self {
        Self { source, columns: ColumnCount::DEFAULT, selection, value_highlight: None, context_menu: None }
    }

    pub fn columns(mut self, cols: ColumnCount) -> Self {
        self.columns = cols;
        self
    }

    /// Toggle value-class highlighting. `None` disables, `Some(mode)`
    /// enables with either a background fill or text recoloring.
    pub fn value_highlight(mut self, mode: Option<ValueHighlight>) -> Self {
        self.value_highlight = mode;
        self
    }

    /// Install a context-menu callback rendered when the user
    /// right-clicks anywhere in the hex or ASCII pane. Callers use this
    /// to add per-app commands like Copy.
    pub fn context_menu(mut self, add_contents: impl FnOnce(&mut egui::Ui) + 's) -> Self {
        self.context_menu = Some(Box::new(add_contents));
        self
    }

    pub fn show(self, ui: &mut Ui) -> HexViewResponse {
        let Self { source, columns, selection, value_highlight, context_menu } = self;
        let palette = value_highlight.map(|mode| (mode, BytePalette::for_theme_and_mode(ui.visuals().dark_mode, mode)));
        let mut context_menu_slot = context_menu;
        let total_rows = row_count(source.len(), columns);
        let address_chars = address_hex_width(source.len());
        let font_id = TextStyle::Monospace.resolve(ui.style());
        let row_height = ui.text_style_height(&TextStyle::Monospace);
        let char_w = measure_char_width(ui, &font_id);
        let layout = RowLayout::compute(char_w, address_chars, columns);
        let source_len = source.len();

        let mut response = HexViewResponse::default();

        paint_column_header(ui, &layout, &font_id, row_height);

        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(
            ui,
            row_height,
            total_rows,
            |ui, visible| {
                let first_row = RowIndex::new(visible.start as u64);
                let range = match visible_byte_range(visible.clone(), columns, source_len) {
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
                paint_and_interact(
                    ui,
                    &layout,
                    &font_id,
                    row_height,
                    first_row,
                    range.start(),
                    source_len,
                    columns,
                    &bytes,
                    selection,
                    palette,
                    context_menu_slot.take(),
                    &mut response,
                );
            },
        );

        response
    }
}

#[derive(Default)]
pub struct HexViewResponse {
    pub hovered_offset: Option<ByteOffset>,
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
    address_chars: usize,
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
            address_chars,
        }
    }

    /// Background-tint rect for hex cell `col` that bleeds halfway into
    /// each side-gap so adjacent tints touch with no visible seam. The
    /// first/last columns clamp to the pane edges.
    fn hex_tint_rect(&self, row_origin: Pos2, col: usize, total_cols: usize, row_height: f32) -> Rect {
        let cell = self.hex_cell_rect(row_origin, col, row_height);
        let left = if col == 0 { cell.left() } else { cell.left() - self.hex_gap / 2.0 };
        let right = if col + 1 >= total_cols { cell.right() } else { cell.right() + self.hex_gap / 2.0 };
        Rect::from_min_max(Pos2::new(left, cell.top()), Pos2::new(right, cell.bottom()))
    }

    /// ASCII tint rect: ASCII cells already sit edge-to-edge, so this is
    /// just the cell rect with no rounding.
    fn ascii_tint_rect(&self, row_origin: Pos2, col: usize, _total_cols: usize, row_height: f32) -> Rect {
        self.ascii_cell_rect(row_origin, col, row_height)
    }

    fn hex_cell_rect(&self, row_origin: Pos2, col: usize, row_height: f32) -> Rect {
        let left = row_origin.x + self.hex_start_x + (col as f32) * (self.hex_cell_w + self.hex_gap);
        Rect::from_min_size(Pos2::new(left, row_origin.y), Vec2::new(self.hex_cell_w, row_height))
    }

    /// Rect spanning hex columns `from..=to` contiguously (no internal
    /// gaps between cells in the span).
    fn hex_span_rect(&self, row_origin: Pos2, from: usize, to: usize, row_height: f32) -> Rect {
        let start = self.hex_cell_rect(row_origin, from, row_height).left();
        let end = self.hex_cell_rect(row_origin, to, row_height).right();
        Rect::from_min_max(Pos2::new(start, row_origin.y), Pos2::new(end, row_origin.y + row_height))
    }

    fn ascii_cell_rect(&self, row_origin: Pos2, col: usize, row_height: f32) -> Rect {
        let left = row_origin.x + self.ascii_start_x + (col as f32) * self.ascii_cell_w;
        Rect::from_min_size(Pos2::new(left, row_origin.y), Vec2::new(self.ascii_cell_w, row_height))
    }

    fn ascii_span_rect(&self, row_origin: Pos2, from: usize, to: usize, row_height: f32) -> Rect {
        let start = self.ascii_cell_rect(row_origin, from, row_height).left();
        let end = self.ascii_cell_rect(row_origin, to, row_height).right();
        Rect::from_min_max(Pos2::new(start, row_origin.y), Pos2::new(end, row_origin.y + row_height))
    }

    fn address_rect(&self, row_origin: Pos2, row_height: f32) -> Rect {
        Rect::from_min_size(row_origin, Vec2::new(self.address_w, row_height))
    }

    /// Map a pointer position within the rendered block to `(row_in_block,
    /// column)`. `row_in_block` is clamped to `0..num_rows`; `column` is
    /// clamped to `0..columns`. Returns `None` only if the pointer's x is
    /// before the hex pane or past the ASCII pane.
    fn hit_test(&self, block_rect: Rect, pos: Pos2, row_height: f32, num_rows: usize) -> Option<HitRowCol> {
        let x = (pos.x - block_rect.left()).max(0.0);
        let y = (pos.y - block_rect.top()).clamp(0.0, num_rows.saturating_sub(1) as f32 * row_height);
        let row = ((y / row_height) as usize).min(num_rows.saturating_sub(1));

        let cols = usize::from(self.columns.get());
        if x >= self.hex_start_x && x < self.ascii_start_x {
            let local = x - self.hex_start_x;
            let stride = self.hex_cell_w + self.hex_gap;
            let col = ((local / stride) as usize).min(cols - 1);
            return Some(HitRowCol { row, col });
        }
        let ascii_end = self.ascii_start_x + (cols as f32) * self.ascii_cell_w;
        if x >= self.ascii_start_x && x < ascii_end {
            let local = x - self.ascii_start_x;
            let col = ((local / self.ascii_cell_w) as usize).min(cols - 1);
            return Some(HitRowCol { row, col });
        }
        None
    }
}

#[derive(Clone, Copy)]
struct HitRowCol {
    row: usize,
    col: usize,
}

#[allow(clippy::too_many_arguments)]
fn paint_and_interact(
    ui: &mut Ui,
    layout: &RowLayout,
    font_id: &FontId,
    row_height: f32,
    first_row: RowIndex,
    start: ByteOffset,
    source_len: ByteLen,
    columns: ColumnCount,
    bytes: &[u8],
    selection: &mut Option<Selection>,
    palette: Option<(ValueHighlight, BytePalette)>,
    context_menu: Option<ContextMenuFn<'_>>,
    response_out: &mut HexViewResponse,
) {
    let cols = usize::from(columns.get());
    let num_rows = bytes.len().div_ceil(cols);
    let block_size = Vec2::new(layout.total_width, num_rows as f32 * row_height);
    let (block_rect, response) = ui.allocate_exact_size(block_size, Sense::click_and_drag());
    let painter = ui.painter_at(block_rect);

    let text_color = ui.visuals().text_color();
    let weak_color = ui.visuals().weak_text_color();
    let selection_bg = ui.visuals().selection.bg_fill;
    let selection_fg = ui.visuals().selection.stroke.color;
    let cursor_stroke = Stroke::new(1.5, ui.visuals().strong_text_color());

    let selected_range = selection.and_then(|s| {
        let r = s.range();
        if r.is_empty() { None } else { Some(r) }
    });
    let cursor_offset = selection.map(|s| s.cursor);

    for (chunk_idx, chunk) in bytes.chunks(cols).enumerate() {
        let row_top = block_rect.top() + (chunk_idx as f32) * row_height;
        let row_origin = Pos2::new(block_rect.left(), row_top);
        let row_first_offset = ByteOffset::new(start.get() + (chunk_idx * cols) as u64);

        painter.text(
            layout.address_rect(row_origin, row_height).left_center(),
            Align2::LEFT_CENTER,
            format_address(row_first_offset, layout.address_chars),
            font_id.clone(),
            weak_color,
        );

        if let Some(range) = selected_range {
            paint_row_selection(
                &painter,
                layout,
                row_origin,
                row_height,
                row_first_offset,
                chunk.len(),
                range,
                selection_bg,
            );
        }

        let cols = usize::from(layout.columns.get());
        for (i, byte) in chunk.iter().enumerate() {
            let byte_offset = ByteOffset::new(row_first_offset.get() + i as u64);
            let hex_rect = layout.hex_cell_rect(row_origin, i, row_height);
            let ascii_rect = layout.ascii_cell_rect(row_origin, i, row_height);
            let is_sel = selected_range.is_some_and(|r| r.contains(byte_offset));

            let class_color = palette.map(|(_, p)| p.color_for(*byte));
            if let (Some((ValueHighlight::Background, _)), Some(color)) = (palette, class_color)
                && !is_sel
            {
                let hex_tint = layout.hex_tint_rect(row_origin, i, cols, row_height);
                let ascii_tint = layout.ascii_tint_rect(row_origin, i, cols, row_height);
                painter.rect_filled(hex_tint, 0.0, color);
                painter.rect_filled(ascii_tint, 0.0, color);
            }

            let fg = if is_sel {
                selection_fg
            } else {
                match palette {
                    Some((ValueHighlight::Text, _)) => class_color.unwrap_or(text_color),
                    Some((ValueHighlight::Background, _)) => {
                        contrast_text_color(class_color.unwrap_or(text_color), text_color)
                    }
                    None => text_color,
                }
            };

            painter.text(hex_rect.center(), Align2::CENTER_CENTER, format!("{byte:02X}"), font_id.clone(), fg);
            let ch = if (0x20..0x7f).contains(byte) { *byte as char } else { '.' };
            painter.text(ascii_rect.center(), Align2::CENTER_CENTER, ch.to_string(), font_id.clone(), fg);

            if cursor_offset == Some(byte_offset) {
                painter.rect_stroke(hex_rect, 2.0, cursor_stroke, StrokeKind::Inside);
                painter.rect_stroke(ascii_rect, 2.0, cursor_stroke, StrokeKind::Inside);
            }
        }
    }

    apply_interaction(
        ui, &response, layout, block_rect, row_height, first_row, columns, num_rows, source_len, selection,
    );

    response_out.hovered_offset =
        hovered_byte(ui, &response, layout, block_rect, row_height, first_row, columns, num_rows, source_len);

    if let Some(add) = context_menu {
        response.context_menu(add);
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_row_selection(
    painter: &egui::Painter,
    layout: &RowLayout,
    row_origin: Pos2,
    row_height: f32,
    row_first_offset: ByteOffset,
    chunk_len: usize,
    selection: ByteRange,
    bg: Color32,
) {
    let cols = usize::from(layout.columns.get());
    let row_start = row_first_offset.get();
    let row_end = row_start + chunk_len as u64;

    let sel_start = selection.start().get();
    let sel_end = selection.end().get();
    if sel_end <= row_start || sel_start >= row_end {
        return;
    }

    let local_from = (sel_start.saturating_sub(row_start)) as usize;
    let local_to_exclusive = (sel_end.min(row_end).saturating_sub(row_start)) as usize;
    if local_to_exclusive == 0 || local_from >= cols {
        return;
    }
    let local_to = local_to_exclusive.saturating_sub(1);

    let hex_bar = layout.hex_span_rect(row_origin, local_from, local_to, row_height);
    let ascii_bar = layout.ascii_span_rect(row_origin, local_from, local_to, row_height);
    painter.rect_filled(hex_bar, 2.0, bg);
    painter.rect_filled(ascii_bar, 2.0, bg);
}

#[allow(clippy::too_many_arguments)]
fn apply_interaction(
    ui: &Ui,
    response: &egui::Response,
    layout: &RowLayout,
    block_rect: Rect,
    row_height: f32,
    first_row: RowIndex,
    columns: ColumnCount,
    num_rows: usize,
    source_len: ByteLen,
    selection: &mut Option<Selection>,
) {
    let cols = usize::from(columns.get());
    let shift = ui.input(|i| i.modifiers.shift);
    let active = response.dragged() || response.drag_started() || response.clicked();
    if !active {
        return;
    }

    let pos = response.interact_pointer_pos().or_else(|| ui.ctx().input(|i| i.pointer.interact_pos()));
    let Some(pos) = pos else { return };
    let Some(hit) = layout.hit_test(block_rect, pos, row_height, num_rows) else {
        return;
    };
    let Some(hit_offset) = hit_to_offset(hit, first_row, cols, source_len) else { return };

    if response.drag_started() {
        *selection = Some(match (shift, *selection) {
            (true, Some(existing)) => Selection { anchor: existing.anchor, cursor: hit_offset },
            _ => Selection::caret(hit_offset),
        });
    } else if response.dragged() {
        match selection.as_mut() {
            Some(s) => s.cursor = hit_offset,
            None => *selection = Some(Selection::caret(hit_offset)),
        }
    } else if response.clicked() {
        *selection = Some(match (shift, *selection) {
            (true, Some(existing)) => Selection { anchor: existing.anchor, cursor: hit_offset },
            _ => Selection::caret(hit_offset),
        });
    }
}

fn hit_to_offset(hit: HitRowCol, first_row: RowIndex, cols: usize, source_len: ByteLen) -> Option<ByteOffset> {
    let abs_row = first_row.get().saturating_add(hit.row as u64);
    let offset = abs_row.checked_mul(cols as u64)?.checked_add(hit.col as u64)?;
    if offset >= source_len.get() {
        if source_len.get() == 0 {
            return None;
        }
        return Some(ByteOffset::new(source_len.get() - 1));
    }
    Some(ByteOffset::new(offset))
}

#[allow(clippy::too_many_arguments)]
fn hovered_byte(
    ui: &Ui,
    response: &egui::Response,
    layout: &RowLayout,
    block_rect: Rect,
    row_height: f32,
    first_row: RowIndex,
    columns: ColumnCount,
    num_rows: usize,
    source_len: ByteLen,
) -> Option<ByteOffset> {
    let pos = response.hover_pos().or_else(|| ui.ctx().input(|i| i.pointer.latest_pos()))?;
    if !block_rect.contains(pos) {
        return None;
    }
    let hit = layout.hit_test(block_rect, pos, row_height, num_rows)?;
    hit_to_offset(hit, first_row, usize::from(columns.get()), source_len)
}

/// Palette for byte-value tinting. Each variant of [`ByteClass`] maps to
/// one background color. Alpha is kept low so rendered text remains
/// readable against the theme foreground.
#[derive(Clone, Copy, Debug)]
pub struct BytePalette {
    pub null: Color32,
    pub all_bits: Color32,
    pub whitespace: Color32,
    pub printable: Color32,
    pub control: Color32,
    pub extended: Color32,
}

impl BytePalette {
    /// Pick a palette variant appropriate for the theme and highlight
    /// mode. Background mode uses muted semi-transparent tints;
    /// text mode uses saturated opaque colors readable against the theme
    /// background.
    pub fn for_theme_and_mode(dark: bool, mode: ValueHighlight) -> Self {
        match (dark, mode) {
            (true, ValueHighlight::Background) => Self::BG_DARK,
            (false, ValueHighlight::Background) => Self::BG_LIGHT,
            (true, ValueHighlight::Text) => Self::TEXT_DARK,
            (false, ValueHighlight::Text) => Self::TEXT_LIGHT,
        }
    }

    pub const BG_DARK: Self = Self {
        null: Color32::from_rgb(60, 60, 64),
        all_bits: Color32::from_rgb(200, 150, 40),
        whitespace: Color32::from_rgb(50, 90, 140),
        printable: Color32::from_rgb(40, 120, 60),
        control: Color32::from_rgb(150, 60, 60),
        extended: Color32::from_rgb(120, 60, 140),
    };

    pub const BG_LIGHT: Self = Self {
        null: Color32::from_rgb(220, 220, 220),
        all_bits: Color32::from_rgb(245, 215, 110),
        whitespace: Color32::from_rgb(180, 210, 240),
        printable: Color32::from_rgb(190, 235, 200),
        control: Color32::from_rgb(240, 190, 190),
        extended: Color32::from_rgb(225, 195, 240),
    };

    pub const TEXT_DARK: Self = Self {
        null: Color32::from_rgb(140, 140, 140),
        all_bits: Color32::from_rgb(255, 200, 80),
        whitespace: Color32::from_rgb(120, 180, 240),
        printable: Color32::from_rgb(120, 220, 140),
        control: Color32::from_rgb(240, 130, 130),
        extended: Color32::from_rgb(210, 140, 230),
    };

    pub const TEXT_LIGHT: Self = Self {
        null: Color32::from_rgb(120, 120, 120),
        all_bits: Color32::from_rgb(180, 120, 20),
        whitespace: Color32::from_rgb(30, 90, 180),
        printable: Color32::from_rgb(30, 130, 60),
        control: Color32::from_rgb(180, 50, 50),
        extended: Color32::from_rgb(130, 40, 170),
    };

    pub fn color_for(&self, byte: u8) -> Color32 {
        match ByteClass::of(byte) {
            ByteClass::Null => self.null,
            ByteClass::AllBits => self.all_bits,
            ByteClass::Whitespace => self.whitespace,
            ByteClass::Printable => self.printable,
            ByteClass::Control => self.control,
            ByteClass::Extended => self.extended,
        }
    }
}

/// Pick a glyph color for text painted on top of `bg`. Brighter
/// backgrounds get a darker grey; darker backgrounds get near-white. The
/// `default_fg` is returned unchanged when `bg` is transparent.
fn contrast_text_color(bg: Color32, default_fg: Color32) -> Color32 {
    if bg.a() == 0 {
        return default_fg;
    }
    let luminance = 0.299 * f32::from(bg.r()) + 0.587 * f32::from(bg.g()) + 0.114 * f32::from(bg.b());
    let t = (luminance / 255.0).clamp(0.0, 1.0);
    let white = 240.0_f32;
    let gray = 30.0_f32;
    let v = (white * (1.0 - t) + gray * t).round() as u8;
    Color32::from_rgb(v, v, v)
}

/// Coarse categorization of a byte value for palette lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteClass {
    Null,
    AllBits,
    Whitespace,
    Printable,
    Control,
    Extended,
}

impl ByteClass {
    pub fn of(byte: u8) -> Self {
        match byte {
            0x00 => Self::Null,
            0xFF => Self::AllBits,
            b'\t' | b'\n' | b'\r' => Self::Whitespace,
            0x01..=0x1F | 0x7F => Self::Control,
            0x20..=0x7E => Self::Printable,
            0x80..=0xFE => Self::Extended,
        }
    }
}

/// Paint a one-row header with column indices ("0" through "f" in a 16-
/// column view) aligned with each hex cell. Rendered outside the scroll
/// area so it stays in view while scrolling.
fn paint_column_header(ui: &mut Ui, layout: &RowLayout, font_id: &FontId, row_height: f32) {
    let cols = usize::from(layout.columns.get());
    let (header_rect, _) = ui.allocate_exact_size(Vec2::new(layout.total_width, row_height), Sense::empty());
    let painter = ui.painter_at(header_rect);
    let color = ui.visuals().weak_text_color();
    let origin = header_rect.min;
    for col in 0..cols {
        let cell = layout.hex_cell_rect(origin, col, row_height);
        painter.text(cell.center(), Align2::CENTER_CENTER, format!("{col:X}"), font_id.clone(), color);
        let ascii_cell = layout.ascii_cell_rect(origin, col, row_height);
        painter.text(ascii_cell.center(), Align2::CENTER_CENTER, format!("{col:X}"), font_id.clone(), color);
    }
    ui.separator();
}

fn format_address(offset: ByteOffset, width: usize) -> String {
    format!("{:0width$X}", offset.get(), width = width)
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
        assert_eq!(format_address(ByteOffset::new(0x1a), 8), "0000001A");
    }

    #[test]
    fn visible_range_clamped_to_source_len() {
        let cols = ColumnCount::new(16).unwrap();
        let r = visible_byte_range(0..10, cols, ByteLen::new(100)).unwrap();
        assert_eq!(r.start().get(), 0);
        assert_eq!(r.end().get(), 100);
    }

    #[test]
    fn hex_span_covers_contiguous_columns() {
        let cols = ColumnCount::new(16).unwrap();
        let layout = RowLayout::compute(10.0, 8, cols);
        let origin = Pos2::ZERO;
        let span = layout.hex_span_rect(origin, 3, 7, 20.0);
        let c3 = layout.hex_cell_rect(origin, 3, 20.0);
        let c7 = layout.hex_cell_rect(origin, 7, 20.0);
        assert_eq!(span.left(), c3.left());
        assert_eq!(span.right(), c7.right());
    }

    #[test]
    fn hit_test_clamps_row_past_last_visible() {
        let cols = ColumnCount::new(16).unwrap();
        let layout = RowLayout::compute(10.0, 8, cols);
        let block = Rect::from_min_size(Pos2::ZERO, Vec2::new(layout.total_width, 60.0));
        // Pointer below the last row should clamp to last row (num_rows=3).
        let below = Pos2::new(layout.hex_start_x + 5.0, 1000.0);
        let hit = layout.hit_test(block, below, 20.0, 3).unwrap();
        assert_eq!(hit.row, 2);
    }
}
