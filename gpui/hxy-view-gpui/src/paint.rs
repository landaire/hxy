//! Canvas paint pass for the hex grid. Given a snapshot of editor
//! state captured at render time plus the window text system, it
//! shapes one line per row-region and fills selection / cursor quads,
//! matching the egui hxy-view layout (address gutter, column header,
//! hex grid, ascii grid) against the [`GridGeometry`] from
//! [`crate::geometry`].

use std::sync::Arc;

use gpui::App;
use gpui::Bounds;
use gpui::ContentMask;
use gpui::Entity;
use gpui::Font;
use gpui::Hsla;
use gpui::Pixels;
use gpui::Point;
use gpui::TextRun;
use gpui::Window;
use gpui::bounds;
use gpui::canvas;
use gpui::fill;
use gpui::font;
use gpui::outline;
use gpui::point;
use gpui::px;
use gpui::size;
use hxy_core::ByteLen;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::ColumnCount;
use hxy_core::HexSource;
use hxy_core::Selection;
use hxy_editor::NibbleCursor;
use hxy_editor::Pane;

use crate::CellMetrics;
use crate::FrameInfo;
use crate::GridGeometry;
use crate::HexPane;

/// Left / top inset of the grid inside the pane bounds.
const PAD_X: f32 = 8.0;
const PAD_Y: f32 = 4.0;
/// Nibble-caret underline thickness.
const NIBBLE_UNDERLINE_H: f32 = 2.0;
/// Alpha applied to the accent color for the active-pane cursor fill so
/// the glyph painted on top stays legible.
const CURSOR_FILL_ALPHA: f32 = 0.4;
/// Advance-width fallback fraction of the font size, used only if the
/// text system cannot measure the mono font (it always can in practice;
/// paint has no channel to propagate an error, and a proportional guess
/// keeps the grid drawable rather than panicking).
const CHAR_W_FALLBACK_FRAC: f32 = 0.6;

/// Theme colors resolved once at render time and moved into the paint
/// closure (the closure runs with `&mut App` but reading the theme there
/// would re-borrow; snapshotting is cheaper and matches gpui's canvas
/// pattern of capturing cheap clones).
#[derive(Clone, Copy)]
pub(crate) struct PaintColors {
    pub foreground: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub selection: Hsla,
}

/// Snapshot of the editor state a single paint pass needs. Captured in
/// `HexPane::render` (which has `&mut self`) so the paint closure never
/// re-borrows the entity for reads; it only writes [`FrameInfo`] back.
pub(crate) struct GridSnapshot {
    pub source: Arc<dyn HexSource>,
    pub selection: Option<Selection>,
    pub active_pane: Pane,
    pub nibble: Option<NibbleCursor>,
    pub columns: ColumnCount,
    pub scroll_rows: f32,
    pub colors: PaintColors,
    pub mono_family: gpui::SharedString,
    pub mono_size: Pixels,
}

/// Build the canvas element that paints the grid. `entity` is used from
/// the paint closure to store this frame's [`FrameInfo`] for the input
/// handlers; capturing a strong handle is safe because the canvas (and
/// thus the closure) is dropped at the end of the frame.
pub(crate) fn hex_canvas(snap: GridSnapshot, entity: Entity<HexPane>) -> impl gpui::IntoElement {
    canvas(
        move |_bounds, _window, _app| {},
        move |bounds, _prepaint, window, app| paint_grid(&snap, &entity, bounds, window, app),
    )
}

fn paint_grid(snap: &GridSnapshot, entity: &Entity<HexPane>, bounds: Bounds<Pixels>, window: &mut Window, app: &mut App) {
    let mono = font(snap.mono_family.clone());
    let metrics = cell_metrics(&mono, snap.mono_size, window);
    let source_len = snap.source.len();
    let geometry = GridGeometry::new(metrics, snap.columns, source_len);
    let cols = u64::from(snap.columns.get());

    let first_visible_row = snap.scroll_rows.floor().max(0.0) as u64;
    let frac = snap.scroll_rows - first_visible_row as f32;
    let grid_top = bounds.origin.y + px(PAD_Y) + metrics.line_h;
    let content_origin = point(bounds.origin.x + px(PAD_X), grid_top - metrics.line_h * frac);
    let grid_area_h = (bounds.size.height - px(PAD_Y) - metrics.line_h).max(px(0.0));
    let rows_visible = f32::from(grid_area_h) / f32::from(metrics.line_h);

    let row_count = geometry.row_count(source_len);
    let last_visible_row = (first_visible_row + rows_visible.ceil() as u64 + 1).min(row_count.saturating_sub(1));

    // Grid content width shrinks by the strip width + gap: the strip
    // claims the right edge of the content area (below the header,
    // same height as the scrollable rows) and nothing else paints
    // there.
    let minimap_bounds = crate::minimap::strip_bounds(
        gpui::bounds(point(bounds.origin.x, grid_top), size(bounds.size.width, grid_area_h)),
        metrics.char_w,
    );

    // Feed this frame's viewport back into the editor so its own
    // scrolloff/visibility bookkeeping (`ensure_cursor_visible_with_scrolloff`,
    // `is_offset_visible`) has real data; without this, keyboard
    // navigation can never trigger an auto-scroll. `scroll_offset` is
    // pixels (`scroll_rows * line_h`), matching egui's unit; it only
    // needs to round-trip consistently through `on_frame` /
    // `pending_scroll`, which it does since the pane converts back
    // through the same `line_h`. `interacted_pane` stays `None`: mouse
    // handlers call `set_active_pane` directly rather than routing
    // through this frame-latch.
    let visible_start = first_visible_row.saturating_mul(cols).min(source_len.get());
    let visible_end = last_visible_row.saturating_add(1).saturating_mul(cols).min(source_len.get());
    let visible_range = ByteRange::new(ByteOffset::new(visible_start), ByteOffset::new(visible_end)).ok();
    let scroll_offset_px = snap.scroll_rows * f32::from(metrics.line_h);

    entity.update(app, |pane, _cx| {
        pane.last_frame = Some(FrameInfo { geometry, content_origin, rows_visible, first_visible_row, minimap_bounds });
        pane.editor_mut().on_frame(scroll_offset_px, snap.columns, visible_range, None);
    });

    crate::minimap::paint_minimap(
        snap.source.as_ref(),
        source_len,
        snap.columns,
        &snap.colors,
        minimap_bounds,
        row_count,
        first_visible_row,
        rows_visible,
        window,
    );

    let bytes = read_visible(snap.source.as_ref(), first_visible_row, last_visible_row, cols, source_len);
    let selected = snap.selection.map(|s| s.range());
    let cursor = snap.selection.map(|s| s.cursor.get());
    let block_start = first_visible_row.saturating_mul(cols);

    // Clip the header and rows to the width left after the strip and
    // its gap, so the grid can never paint over the minimap.
    let grid_w = (bounds.size.width - minimap_bounds.size.width - px(crate::minimap::STRIP_GAP)).max(px(0.0));
    let grid_clip = gpui::bounds(bounds.origin, size(grid_w, bounds.size.height));
    window.with_content_mask(Some(ContentMask { bounds: grid_clip }), |window| {
        paint_header(snap, &geometry, &mono, content_origin, bounds.origin.y + px(PAD_Y), window, app);

        for row in first_visible_row..=last_visible_row {
            let ctx = RowCtx {
                geometry: &geometry,
                origin_x: content_origin.x,
                row_y: content_origin.y + metrics.line_h * (row - first_visible_row) as f32,
                row_start: row.saturating_mul(cols),
                cols,
                block_start,
                source_len,
            };
            paint_selection_bands(&ctx, selected, snap.colors.selection, window);
            paint_cursor_cell(&ctx, snap, cursor, window);
            paint_row_text(&ctx, snap, &mono, &bytes, window, app);
            paint_nibble_caret(&ctx, snap, cursor, window);
        }
    });
}

/// Per-row invariants shared by the paint helpers: the row's screen
/// origin, its byte span, and the frame's read-block offset for indexing
/// into the bytes buffer.
struct RowCtx<'a> {
    geometry: &'a GridGeometry,
    origin_x: Pixels,
    row_y: Pixels,
    row_start: u64,
    cols: u64,
    block_start: u64,
    source_len: ByteLen,
}

impl RowCtx<'_> {
    fn line_h(&self) -> Pixels {
        self.geometry.metrics.line_h
    }

    /// Column of `cursor` within this row, or `None` if it lies outside.
    fn cursor_col(&self, cursor: u64) -> Option<u16> {
        (cursor >= self.row_start && cursor < self.row_start.saturating_add(self.cols))
            .then(|| (cursor - self.row_start) as u16)
    }
}

/// One monospace character advance and the row height.
fn cell_metrics(mono: &Font, mono_size: Pixels, window: &mut Window) -> CellMetrics {
    let font_id = window.text_system().resolve_font(mono);
    // Fallback: the text system can always measure a resolved mono
    // font; a proportional guess only guards a paint pass that has no
    // way to surface an error.
    let char_w = window.text_system().em_advance(font_id, mono_size).unwrap_or(mono_size * CHAR_W_FALLBACK_FRAC);
    CellMetrics { char_w, line_h: window.line_height() }
}

/// Column-index header over the hex and ascii panes plus a faint rule
/// under it, mirroring egui's top offset row.
fn paint_header(
    snap: &GridSnapshot,
    geometry: &GridGeometry,
    mono: &Font,
    content_origin: Point<Pixels>,
    header_y: Pixels,
    window: &mut Window,
    app: &mut App,
) {
    for col in 0..snap.columns.get() {
        let hex = format!("{:02X}", col & 0xFF);
        let x = content_origin.x + geometry.hex_x(col);
        paint_line(mono, &hex, snap.mono_size, snap.colors.muted, point(x, header_y), window, app);

        let ascii = format!("{:X}", col & 0x0F);
        let ax = content_origin.x + geometry.ascii_x(col);
        paint_line(mono, &ascii, snap.mono_size, snap.colors.muted, point(ax, header_y), window, app);
    }
}

fn read_visible(source: &dyn HexSource, first_row: u64, last_row: u64, cols: u64, source_len: ByteLen) -> Vec<u8> {
    let len = source_len.get();
    let start = first_row.saturating_mul(cols).min(len);
    let end = last_row.saturating_add(1).saturating_mul(cols).min(len);
    if start >= end {
        return Vec::new();
    }
    match ByteRange::new(ByteOffset::new(start), ByteOffset::new(end)) {
        Ok(range) => match source.read(range) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(?range, %err, "hex grid read failed; painting empty rows");
                Vec::new()
            }
        },
        Err(err) => {
            tracing::warn!(%err, "hex grid range invalid; painting empty rows");
            Vec::new()
        }
    }
}

/// Continuous selection band behind the selected columns of one row, in
/// both the hex and ascii panes. A single quad per pane spans the whole
/// column run (gaps between cells included).
fn paint_selection_bands(ctx: &RowCtx, selected: Option<ByteRange>, color: Hsla, window: &mut Window) {
    let Some(sel) = selected else { return };
    let row_end = ctx.row_start.saturating_add(ctx.cols);
    let lo = sel.start().get().max(ctx.row_start);
    let hi = sel.end().get().min(row_end);
    if lo >= hi {
        return;
    }
    let first = (lo - ctx.row_start) as u16;
    let last = (hi - 1 - ctx.row_start) as u16;
    let g = ctx.geometry;

    let hx0 = ctx.origin_x + g.hex_x(first);
    let hx1 = ctx.origin_x + g.hex_x(last) + g.hex_cell_w();
    window.paint_quad(fill(band_bounds(hx0, hx1, ctx.row_y, ctx.line_h()), color));

    let ax0 = ctx.origin_x + g.ascii_x(first);
    let ax1 = ctx.origin_x + g.ascii_x(last + 1);
    window.paint_quad(fill(band_bounds(ax0, ax1, ctx.row_y, ctx.line_h()), color));
}

fn band_bounds(x0: Pixels, x1: Pixels, y: Pixels, h: Pixels) -> Bounds<Pixels> {
    bounds(point(x0, y), size(x1 - x0, h))
}

/// Cursor cell emphasis: a stronger fill on the active pane's copy of
/// the cursor byte, an outline on the inactive pane's copy.
fn paint_cursor_cell(ctx: &RowCtx, snap: &GridSnapshot, cursor: Option<u64>, window: &mut Window) {
    let Some(col) = cursor.and_then(|c| ctx.cursor_col(c)) else { return };
    let g = ctx.geometry;
    let line_h = ctx.line_h();
    let fill_color = snap.colors.accent.opacity(CURSOR_FILL_ALPHA);

    let hex_b = band_bounds(ctx.origin_x + g.hex_x(col), ctx.origin_x + g.hex_x(col) + g.hex_cell_w(), ctx.row_y, line_h);
    let ascii_b = band_bounds(ctx.origin_x + g.ascii_x(col), ctx.origin_x + g.ascii_x(col + 1), ctx.row_y, line_h);

    let (active, inactive) = match snap.active_pane {
        Pane::Hex => (hex_b, ascii_b),
        Pane::Ascii => (ascii_b, hex_b),
    };
    window.paint_quad(fill(active, fill_color));
    window.paint_quad(outline(inactive, snap.colors.accent, gpui::BorderStyle::Solid));
}

fn paint_row_text(ctx: &RowCtx, snap: &GridSnapshot, mono: &Font, bytes: &[u8], window: &mut Window, app: &mut App) {
    let g = ctx.geometry;
    let addr = format!("{:0width$X}", ctx.row_start, width = g.address_chars);
    paint_line(mono, &addr, snap.mono_size, snap.colors.muted, point(ctx.origin_x + g.address_x(), ctx.row_y), window, app);

    let len = ctx.source_len.get();
    let mut ascii = String::with_capacity(ctx.cols as usize);
    let mut ascii_runs: Vec<TextRun> = Vec::with_capacity(ctx.cols as usize);

    for c in 0..ctx.cols {
        let offset = ctx.row_start + c;
        if offset >= len {
            break;
        }
        let idx = (offset - ctx.block_start) as usize;
        let Some(&byte) = bytes.get(idx) else { break };
        let color = byte_color(byte, &snap.colors);
        let col = c as u16;

        let hex = format!("{byte:02X}");
        paint_line(mono, &hex, snap.mono_size, color, point(ctx.origin_x + g.hex_x(col), ctx.row_y), window, app);

        let ch = if is_printable(byte) { byte as char } else { '.' };
        ascii.push(ch);
        ascii_runs.push(run(ch.len_utf8(), color, mono));
    }

    if !ascii.is_empty() {
        let shaped = window.text_system().shape_line(ascii.into(), snap.mono_size, &ascii_runs, None);
        let _ = shaped.paint(point(ctx.origin_x + g.ascii_x(0), ctx.row_y), ctx.line_h(), window, app);
    }
}

/// Two-pixel underline under the active nibble's glyph, only in the hex
/// pane. `High` underlines the left glyph, `Low` the right.
fn paint_nibble_caret(ctx: &RowCtx, snap: &GridSnapshot, cursor: Option<u64>, window: &mut Window) {
    if snap.active_pane != Pane::Hex {
        return;
    }
    let Some(nibble) = snap.nibble else { return };
    let Some(col) = cursor.and_then(|c| ctx.cursor_col(c)) else { return };
    let g = ctx.geometry;
    let glyph_w = g.metrics.char_w;
    let x0 = ctx.origin_x
        + g.hex_x(col)
        + match nibble {
            NibbleCursor::High => px(0.0),
            NibbleCursor::Low => glyph_w,
        };
    let y = ctx.row_y + ctx.line_h() - px(NIBBLE_UNDERLINE_H);
    let b = bounds(point(x0, y), size(glyph_w, px(NIBBLE_UNDERLINE_H)));
    window.paint_quad(fill(b, snap.colors.accent));
}

fn paint_line(mono: &Font, text: &str, size: Pixels, color: Hsla, origin: Point<Pixels>, window: &mut Window, app: &mut App) {
    let runs = [run(text.len(), color, mono)];
    let shaped = window.text_system().shape_line(text.to_string().into(), size, &runs, None);
    let _ = shaped.paint(origin, window.line_height(), window, app);
}

fn run(len: usize, color: Hsla, mono: &Font) -> TextRun {
    TextRun { len, font: mono.clone(), color, background_color: None, underline: None, strikethrough: None }
}

fn byte_color(byte: u8, colors: &PaintColors) -> Hsla {
    if byte == 0 {
        colors.muted
    } else if is_printable(byte) {
        colors.foreground
    } else {
        colors.accent
    }
}

/// Shared with [`crate::minimap`]'s byte-class averaging so the strip
/// and the grid draw the same printable-ascii boundary.
pub(crate) fn is_printable(byte: u8) -> bool {
    (0x20..=0x7e).contains(&byte)
}

impl PaintColors {
    pub(crate) fn from_theme(theme: &gpui_component::Theme) -> Self {
        Self {
            foreground: theme.foreground,
            muted: theme.muted_foreground,
            accent: theme.accent_foreground,
            selection: theme.selection,
        }
    }
}
