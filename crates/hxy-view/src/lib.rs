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
    palette_override: Option<HighlightPalette>,
    context_menu: Option<ContextMenuFn<'s>>,
    minimap: bool,
    initial_scroll: Option<f32>,
}

impl<'s, S: HexSource + ?Sized> HexView<'s, S> {
    pub fn new(source: &'s S, selection: &'s mut Option<Selection>) -> Self {
        Self {
            source,
            columns: ColumnCount::DEFAULT,
            selection,
            value_highlight: None,
            palette_override: None,
            context_menu: None,
            minimap: false,
            initial_scroll: None,
        }
    }

    /// Scroll the view to `offset` (in pixels from the top of content)
    /// on this frame. Useful for restoring a saved scroll position on
    /// file reopen.
    pub fn scroll_to(mut self, offset: f32) -> Self {
        self.initial_scroll = Some(offset);
        self
    }

    /// Draw a narrow "minimap" strip on the right-hand side of the view
    /// that shows the full file colored by the current palette, with a
    /// viewport indicator, and supports click/drag to scroll.
    pub fn minimap(mut self, enabled: bool) -> Self {
        self.minimap = enabled;
        self
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

    /// Override the built-in theme-based palette. Use this to plug in a
    /// class palette, a value gradient, or (later) a custom colour
    /// scheme.
    pub fn palette(mut self, palette: HighlightPalette) -> Self {
        self.palette_override = Some(palette);
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
        let Self {
            source,
            columns,
            selection,
            value_highlight,
            palette_override,
            context_menu,
            minimap,
            initial_scroll,
        } = self;
        let palette = value_highlight.map(|mode| {
            let palette =
                palette_override.unwrap_or_else(|| HighlightPalette::for_theme_and_mode(ui.visuals().dark_mode, mode));
            (mode, palette)
        });
        let total_rows = row_count(source.len(), columns);
        let address_chars = address_hex_width(source.len());
        let font_id = TextStyle::Monospace.resolve(ui.style());
        let row_height = ui.text_style_height(&TextStyle::Monospace);
        let char_w = measure_char_width(ui, &font_id);
        let layout = RowLayout::compute(char_w, address_chars, columns);
        let source_len = source.len();

        let mut response = HexViewResponse::default();

        paint_column_header(ui, &layout, &font_id, row_height);

        let scroll_id = ui.id().with("hxy_scroll");
        // Minimap click, explicit `scroll_to`, or a stashed pending value
        // from a prior frame can all drive the next scroll position.
        let pending_offset = ui.ctx().data_mut(|d| d.remove_temp::<f32>(scroll_id)).or(initial_scroll);

        let minimap_width = if minimap { (char_w * 8.0).max(48.0) } else { 0.0 };
        let avail = ui.available_rect_before_wrap();
        let hex_rect = Rect::from_min_size(avail.min, Vec2::new(avail.width() - minimap_width, avail.height()));
        let minimap_rect =
            Rect::from_min_size(Pos2::new(hex_rect.right(), avail.top()), Vec2::new(minimap_width, avail.height()));

        let hex_out = ui
            .scope_builder(egui::UiBuilder::new().max_rect(hex_rect), |ui| {
                let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt(scroll_id);
                if let Some(target) = pending_offset {
                    area = area.vertical_scroll_offset(target);
                }
                area.show(ui, |ui| {
                    paint_and_interact(
                        ui,
                        &layout,
                        &font_id,
                        row_height,
                        total_rows,
                        source_len,
                        columns,
                        source,
                        selection,
                        palette,
                        context_menu,
                        &mut response,
                    );
                })
            })
            .inner;

        response.scroll_offset = hex_out.state.offset.y;
        response.viewport_height = hex_out.inner_rect.height();

        if minimap {
            draw_minimap(
                ui,
                scroll_id,
                minimap_rect,
                source,
                source_len,
                palette,
                row_height,
                hex_out.state.offset.y,
                hex_out.inner_rect.height(),
                total_rows,
            );
        }

        response
    }
}

#[derive(Default)]
pub struct HexViewResponse {
    pub hovered_offset: Option<ByteOffset>,
    pub error: Option<CoreError>,
    /// Scroll offset (in content pixels) at the end of this frame.
    pub scroll_offset: f32,
    /// Visible viewport height (in pixels).
    pub viewport_height: f32,
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

/// Everything the painter needs that stays constant for the whole frame.
struct PaintCtx<'a> {
    layout: &'a RowLayout,
    font_id: &'a FontId,
    row_height: f32,
    columns: ColumnCount,
    palette: Option<(ValueHighlight, HighlightPalette)>,
    colors: RowColors,
    selected_range: Option<ByteRange>,
    cursor_offset: Option<ByteOffset>,
    hover_offset: Option<ByteOffset>,
}

/// Geometry + source metadata used for pointer hit-testing against the
/// full (scrolled) content rect.
struct HitCtx<'a> {
    layout: &'a RowLayout,
    block_rect: Rect,
    row_height: f32,
    columns: ColumnCount,
    total_rows: usize,
    source_len: ByteLen,
}

#[derive(Clone, Copy)]
struct RowColors {
    text: Color32,
    weak: Color32,
    selection_bg: Color32,
    selection_fg: Color32,
    cursor_stroke: Stroke,
    hover_stroke: Stroke,
}

#[allow(clippy::too_many_arguments)]
fn paint_and_interact<S: HexSource + ?Sized>(
    ui: &mut Ui,
    layout: &RowLayout,
    font_id: &FontId,
    row_height: f32,
    total_rows: usize,
    source_len: ByteLen,
    columns: ColumnCount,
    source: &S,
    selection: &mut Option<Selection>,
    palette: Option<(ValueHighlight, HighlightPalette)>,
    context_menu: Option<ContextMenuFn<'_>>,
    response_out: &mut HexViewResponse,
) {
    let cols = usize::from(columns.get());
    let total_height = total_rows as f32 * row_height;
    let block_size = Vec2::new(layout.total_width, total_height);
    let (block_rect, response) = ui.allocate_exact_size(block_size, Sense::click_and_drag());

    let hit = HitCtx { layout, block_rect, row_height, columns, total_rows, source_len };

    // Clip-driven visibility: paint only rows that intersect the scroll
    // area's clip rect, letting the area scroll with pixel granularity
    // rather than snapping to whole rows.
    let Some((first_visible, last_visible_exclusive)) =
        visible_rows(&block_rect, row_height, total_rows, ui.clip_rect())
    else {
        response_out.hovered_offset = None;
        if let Some(add) = context_menu {
            response.context_menu(add);
        }
        return;
    };

    let range_start = (first_visible as u64).saturating_mul(cols as u64).min(source_len.get());
    let range_end = (last_visible_exclusive as u64).saturating_mul(cols as u64).min(source_len.get());
    let Ok(read_range) = ByteRange::new(ByteOffset::new(range_start), ByteOffset::new(range_end)) else {
        return;
    };
    let bytes = match source.read(read_range) {
        Ok(b) => b,
        Err(e) => {
            response_out.error = Some(e);
            return;
        }
    };

    let weak = ui.visuals().weak_text_color();
    let colors = RowColors {
        text: ui.visuals().text_color(),
        weak,
        selection_bg: ui.visuals().selection.bg_fill,
        selection_fg: ui.visuals().selection.stroke.color,
        cursor_stroke: Stroke::new(1.5, ui.visuals().strong_text_color()),
        hover_stroke: Stroke::new(1.0, weak.gamma_multiply(0.9)),
    };

    let selected_range = selection.and_then(|s| {
        let r = s.range();
        if r.is_empty() { None } else { Some(r) }
    });
    let cursor_offset = selection.map(|s| s.cursor);
    let hover_offset = hovered_byte(ui, &response, &hit);

    let ctx =
        PaintCtx { layout, font_id, row_height, columns, palette, colors, selected_range, cursor_offset, hover_offset };
    let painter = ui.painter_at(block_rect);
    paint_rows(&painter, &ctx, block_rect, first_visible, &bytes);

    apply_interaction(ui, &response, &hit, selection);

    response_out.hovered_offset = hover_offset;

    if let Some(add) = context_menu {
        response.context_menu(add);
    }
}

fn visible_rows(block_rect: &Rect, row_height: f32, total_rows: usize, clip: Rect) -> Option<(usize, usize)> {
    if total_rows == 0 {
        return None;
    }
    let total_height = total_rows as f32 * row_height;
    let visible_top = (clip.top() - block_rect.top()).max(0.0);
    let visible_bottom = (clip.bottom() - block_rect.top()).clamp(0.0, total_height);
    if visible_bottom <= visible_top {
        return None;
    }
    let first = (visible_top / row_height).floor() as usize;
    let last = ((visible_bottom / row_height).ceil() as usize).min(total_rows);
    if first >= last { None } else { Some((first, last)) }
}

/// Paint in two passes so marker strokes (cursor/hover) end up on top of
/// every neighboring row's tint — otherwise the cell to the right of the
/// cursor can paint its tint *over* the cursor stroke.
fn paint_rows(painter: &egui::Painter, ctx: &PaintCtx<'_>, block_rect: Rect, first_visible: usize, bytes: &[u8]) {
    let cols = usize::from(ctx.columns.get());

    for (chunk_idx, chunk) in bytes.chunks(cols).enumerate() {
        let row_idx = first_visible + chunk_idx;
        let row_origin = row_origin_for(block_rect, row_idx, ctx.row_height);
        let row_first_offset = ByteOffset::new((row_idx as u64) * (cols as u64));
        paint_row_backs_and_glyphs(painter, ctx, row_origin, row_first_offset, chunk);
    }
    for (chunk_idx, chunk) in bytes.chunks(cols).enumerate() {
        let row_idx = first_visible + chunk_idx;
        let row_origin = row_origin_for(block_rect, row_idx, ctx.row_height);
        let row_first_offset = ByteOffset::new((row_idx as u64) * (cols as u64));
        paint_row_marks(painter, ctx, row_origin, row_first_offset, chunk.len());
    }
}

fn row_origin_for(block_rect: Rect, row_idx: usize, row_height: f32) -> Pos2 {
    Pos2::new(block_rect.left(), block_rect.top() + (row_idx as f32) * row_height)
}

fn paint_row_backs_and_glyphs(
    painter: &egui::Painter,
    ctx: &PaintCtx<'_>,
    row_origin: Pos2,
    row_first_offset: ByteOffset,
    chunk: &[u8],
) {
    let cols = usize::from(ctx.columns.get());

    painter.text(
        ctx.layout.address_rect(row_origin, ctx.row_height).left_center(),
        Align2::LEFT_CENTER,
        format_address(row_first_offset, ctx.layout.address_chars),
        ctx.font_id.clone(),
        ctx.colors.weak,
    );

    if let Some(range) = ctx.selected_range {
        paint_row_selection(
            painter,
            ctx.layout,
            row_origin,
            ctx.row_height,
            row_first_offset,
            chunk.len(),
            range,
            ctx.colors.selection_bg,
        );
    }

    for (i, byte) in chunk.iter().enumerate() {
        let byte_offset = ByteOffset::new(row_first_offset.get() + i as u64);
        let hex_rect = ctx.layout.hex_cell_rect(row_origin, i, ctx.row_height);
        let ascii_rect = ctx.layout.ascii_cell_rect(row_origin, i, ctx.row_height);
        let is_sel = ctx.selected_range.is_some_and(|r| r.contains(byte_offset));

        let class_color = ctx.palette.map(|(_, p)| p.color_for(*byte));
        if let (Some((ValueHighlight::Background, _)), Some(color)) = (ctx.palette, class_color)
            && !is_sel
        {
            let hex_tint = ctx.layout.hex_tint_rect(row_origin, i, cols, ctx.row_height);
            let ascii_tint = ctx.layout.ascii_tint_rect(row_origin, i, cols, ctx.row_height);
            painter.rect_filled(hex_tint, 0.0, color);
            painter.rect_filled(ascii_tint, 0.0, color);
        }

        let fg = if is_sel {
            ctx.colors.selection_fg
        } else {
            match ctx.palette {
                Some((ValueHighlight::Text, _)) => class_color.unwrap_or(ctx.colors.text),
                Some((ValueHighlight::Background, _)) => {
                    contrast_text_color(class_color.unwrap_or(ctx.colors.text), ctx.colors.text)
                }
                None => ctx.colors.text,
            }
        };

        painter.text(hex_rect.center(), Align2::CENTER_CENTER, format!("{byte:02X}"), ctx.font_id.clone(), fg);
        let ch = if (0x20..0x7f).contains(byte) { *byte as char } else { '.' };
        painter.text(ascii_rect.center(), Align2::CENTER_CENTER, ch.to_string(), ctx.font_id.clone(), fg);
    }
}

fn paint_row_marks(
    painter: &egui::Painter,
    ctx: &PaintCtx<'_>,
    row_origin: Pos2,
    row_first_offset: ByteOffset,
    chunk_len: usize,
) {
    let cols = usize::from(ctx.columns.get());
    for i in 0..chunk_len.min(cols) {
        let byte_offset = ByteOffset::new(row_first_offset.get() + i as u64);
        let hex_rect = ctx.layout.hex_cell_rect(row_origin, i, ctx.row_height);
        let ascii_rect = ctx.layout.ascii_cell_rect(row_origin, i, ctx.row_height);
        let hex_mark = hex_rect.expand2(Vec2::new(ctx.layout.hex_gap * 0.35, 2.0));
        let ascii_mark = ascii_rect.expand2(Vec2::new(0.5, 2.0));
        if ctx.cursor_offset == Some(byte_offset) {
            painter.rect_stroke(hex_mark, 2.0, ctx.colors.cursor_stroke, StrokeKind::Middle);
            painter.rect_stroke(ascii_mark, 2.0, ctx.colors.cursor_stroke, StrokeKind::Middle);
        } else if ctx.hover_offset == Some(byte_offset) {
            painter.rect_stroke(hex_mark, 2.0, ctx.colors.hover_stroke, StrokeKind::Middle);
            painter.rect_stroke(ascii_mark, 2.0, ctx.colors.hover_stroke, StrokeKind::Middle);
        }
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

fn apply_interaction(ui: &Ui, response: &egui::Response, hit: &HitCtx<'_>, selection: &mut Option<Selection>) {
    let cols = usize::from(hit.columns.get());
    let shift = ui.input(|i| i.modifiers.shift);
    let active = response.dragged() || response.drag_started() || response.clicked();
    if !active {
        return;
    }

    let pos = response.interact_pointer_pos().or_else(|| ui.ctx().input(|i| i.pointer.interact_pos()));
    let Some(pos) = pos else { return };
    let Some(rc) = hit.layout.hit_test(hit.block_rect, pos, hit.row_height, hit.total_rows) else {
        return;
    };
    let Some(hit_offset) = hit_to_offset(rc, cols, hit.source_len) else { return };

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

fn hit_to_offset(hit: HitRowCol, cols: usize, source_len: ByteLen) -> Option<ByteOffset> {
    let offset = (hit.row as u64).checked_mul(cols as u64)?.checked_add(hit.col as u64)?;
    if offset >= source_len.get() {
        if source_len.get() == 0 {
            return None;
        }
        return Some(ByteOffset::new(source_len.get() - 1));
    }
    Some(ByteOffset::new(offset))
}

#[allow(clippy::too_many_arguments)]
fn hovered_byte(ui: &Ui, response: &egui::Response, hit: &HitCtx<'_>) -> Option<ByteOffset> {
    let pos = response.hover_pos().or_else(|| ui.ctx().input(|i| i.pointer.latest_pos()))?;
    if !hit.block_rect.contains(pos) {
        return None;
    }
    let rc = hit.layout.hit_test(hit.block_rect, pos, hit.row_height, hit.total_rows)?;
    hit_to_offset(rc, usize::from(hit.columns.get()), hit.source_len)
}

/// Colour source for byte-value tinting. Either coarse class-based
/// (null / whitespace / printable / ...) or a per-value gradient that
/// gives every byte its own colour.
#[derive(Clone, Copy, Debug)]
pub enum HighlightPalette {
    Class(BytePalette),
    Value(ValueGradient),
}

impl HighlightPalette {
    pub fn for_theme_and_mode(dark: bool, mode: ValueHighlight) -> Self {
        Self::Class(BytePalette::for_theme_and_mode(dark, mode))
    }

    pub fn color_for(&self, byte: u8) -> Color32 {
        match self {
            Self::Class(p) => p.color_for(byte),
            Self::Value(g) => g.color_for(byte),
        }
    }
}

/// Every byte value gets a unique colour from a fixed HSL hue wheel.
/// Saturation and lightness are tuned per theme/mode so the resulting
/// colours stay readable under the view's fixed text contrast rules.
#[derive(Clone, Copy, Debug)]
pub struct ValueGradient {
    pub saturation: f32,
    pub lightness: f32,
}

impl ValueGradient {
    pub const BG_DARK: Self = Self { saturation: 0.55, lightness: 0.32 };
    pub const BG_LIGHT: Self = Self { saturation: 0.5, lightness: 0.78 };
    pub const TEXT_DARK: Self = Self { saturation: 0.75, lightness: 0.68 };
    pub const TEXT_LIGHT: Self = Self { saturation: 0.7, lightness: 0.4 };

    pub fn for_theme_and_mode(dark: bool, mode: ValueHighlight) -> Self {
        match (dark, mode) {
            (true, ValueHighlight::Background) => Self::BG_DARK,
            (false, ValueHighlight::Background) => Self::BG_LIGHT,
            (true, ValueHighlight::Text) => Self::TEXT_DARK,
            (false, ValueHighlight::Text) => Self::TEXT_LIGHT,
        }
    }

    pub fn color_for(&self, byte: u8) -> Color32 {
        let hue = (f32::from(byte) / 256.0) * 360.0;
        hsl_to_rgb(hue, self.saturation, self.lightness)
    }
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Color32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_norm = h / 60.0;
    let x = c * (1.0 - (h_norm.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match h_norm as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let cv = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgb(cv(r1), cv(g1), cv(b1))
}

/// Palette for byte-class tinting. Each variant of [`ByteClass`] maps
/// to one colour. Rendered text remains readable via the view's
/// contrast-adjustment.
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

#[allow(clippy::too_many_arguments)]
fn draw_minimap<S: HexSource + ?Sized>(
    ui: &mut Ui,
    scroll_id: egui::Id,
    minimap_rect: Rect,
    source: &S,
    source_len: ByteLen,
    palette: Option<(ValueHighlight, HighlightPalette)>,
    row_height: f32,
    current_offset: f32,
    viewport_height: f32,
    total_rows: usize,
) {
    if minimap_rect.width() < 1.0 || minimap_rect.height() < 1.0 || source_len.get() == 0 {
        return;
    }
    let response = ui.allocate_rect(minimap_rect, Sense::click_and_drag());
    let painter = ui.painter_at(minimap_rect);

    painter.rect_filled(minimap_rect, 0.0, ui.visuals().extreme_bg_color);

    let rows_per_pixel = (total_rows as f32 / minimap_rect.height()).max(1.0);
    let len = source_len.get();

    // Sample one byte per minimap pixel, taking it from the start of the
    // representative row bucket. For small files (rows_per_pixel < 1) we
    // instead spread available rows across the top of the minimap.
    let usable_height =
        if total_rows as f32 >= minimap_rect.height() { minimap_rect.height() } else { total_rows as f32 };
    let effective_pixels = usable_height.floor() as usize;

    let samples = collect_minimap_samples(source, len, total_rows, rows_per_pixel, effective_pixels);
    let fallback = ui.visuals().text_color();
    for (i, byte) in samples.iter().enumerate() {
        let y = minimap_rect.top() + i as f32;
        let color = palette.map(|(_, p)| p.color_for(*byte)).unwrap_or(fallback);
        painter.hline(minimap_rect.x_range(), y, Stroke::new(1.0, color));
    }

    // Viewport indicator.
    let content_height = total_rows as f32 * row_height;
    if content_height > 0.0 {
        let v_top = (current_offset / content_height) * minimap_rect.height();
        let v_bot = ((current_offset + viewport_height) / content_height) * minimap_rect.height();
        let indicator = Rect::from_min_max(
            Pos2::new(minimap_rect.left(), minimap_rect.top() + v_top.clamp(0.0, minimap_rect.height())),
            Pos2::new(minimap_rect.right(), minimap_rect.top() + v_bot.clamp(0.0, minimap_rect.height())),
        );
        let indicator_fill = Color32::from_white_alpha(if ui.visuals().dark_mode { 30 } else { 60 });
        painter.rect_filled(indicator, 0.0, indicator_fill);
        painter.rect_stroke(indicator, 0.0, Stroke::new(1.0, ui.visuals().weak_text_color()), StrokeKind::Inside);
    }

    // Interaction: clicking or dragging sets a target scroll offset
    // that centers the pointed-to byte in the hex viewport next frame.
    let pointer = response
        .interact_pointer_pos()
        .or_else(|| response.hover_pos().filter(|_| response.is_pointer_button_down_on()));
    if let Some(pos) = pointer.filter(|_| response.dragged() || response.clicked() || response.drag_started()) {
        let y = (pos.y - minimap_rect.top()).clamp(0.0, minimap_rect.height());
        let frac = y / minimap_rect.height();
        let target_center = frac * content_height;
        let target = (target_center - viewport_height / 2.0).max(0.0);
        ui.ctx().data_mut(|d| d.insert_temp(scroll_id, target));
        ui.ctx().request_repaint();
    }
}

/// Sample one byte per minimap pixel row. Reads are batched: we fetch
/// one byte at each sample point, which is `effective_pixels` reads.
fn collect_minimap_samples<S: HexSource + ?Sized>(
    source: &S,
    len: u64,
    total_rows: usize,
    rows_per_pixel: f32,
    effective_pixels: usize,
) -> Vec<u8> {
    let cols = 16u64;
    let mut out = Vec::with_capacity(effective_pixels);
    for i in 0..effective_pixels {
        let row_idx = (i as f32 * rows_per_pixel) as u64;
        let row_idx = row_idx.min(total_rows.saturating_sub(1) as u64);
        let byte_offset = row_idx.saturating_mul(cols).min(len.saturating_sub(1));
        let Ok(r) = ByteRange::new(ByteOffset::new(byte_offset), ByteOffset::new(byte_offset + 1)) else {
            continue;
        };
        match source.read(r) {
            Ok(bytes) => out.push(*bytes.first().unwrap_or(&0)),
            Err(_) => out.push(0),
        }
    }
    out
}

/// Paint a one-row header with column indices ("0" through "f" in a 16-
/// column view) aligned with each hex cell. Rendered outside the scroll
/// area so it stays in view while scrolling.
fn paint_column_header(ui: &mut Ui, layout: &RowLayout, font_id: &FontId, row_height: f32) {
    let cols = usize::from(layout.columns.get());
    let header_height = row_height * 0.75;
    let (header_rect, _) = ui.allocate_exact_size(Vec2::new(layout.total_width, header_height), Sense::empty());
    let painter = ui.painter_at(header_rect);
    let color = ui.visuals().weak_text_color();
    let origin = header_rect.min;
    for col in 0..cols {
        let cell = layout.hex_cell_rect(origin, col, header_height);
        painter.text(cell.center(), Align2::CENTER_CENTER, format!("{col:X}"), font_id.clone(), color);
        let ascii_cell = layout.ascii_cell_rect(origin, col, header_height);
        painter.text(ascii_cell.center(), Align2::CENTER_CENTER, format!("{col:X}"), font_id.clone(), color);
    }
    let divider_y = header_rect.bottom();
    painter.line_segment(
        [Pos2::new(header_rect.left(), divider_y), Pos2::new(header_rect.right(), divider_y)],
        Stroke::new(1.0, ui.visuals().weak_text_color().gamma_multiply(0.5)),
    );
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
