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
use gpui::Styled;
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
use hxy_core::RowSlot;
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
    /// Softer, semi-transparent selection tint for the hover-span band.
    /// Distinct from `selection` so the two bands read apart when they
    /// overlap; mirrors egui hxy-view, which gamma-multiplies the
    /// selection background for its hover fill (lib.rs:1358).
    pub hover: Hsla,
    /// Opaque panel fill for the column-header band. The band occludes
    /// rows scrolled up under the header, so it must be fully opaque
    /// (egui renders the header outside the scroll area entirely).
    pub background: Hsla,
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
    /// Non-linear row stream; `None` keeps the linear fast path.
    pub row_map: Option<Vec<RowSlot>>,
    /// Secondary highlight band, painted under the selection band.
    pub hover_span: Option<ByteRange>,
    /// Per-byte color override consulted in the paint loop.
    pub byte_styler: Option<crate::ByteStyler>,
}

/// Build the canvas element that paints the grid. `entity` is used from
/// the paint closure to store this frame's [`FrameInfo`] for the input
/// handlers; capturing a strong handle is safe because the canvas (and
/// thus the closure) is dropped at the end of the frame.
pub(crate) fn hex_canvas(snap: GridSnapshot, entity: Entity<HexPane>) -> impl gpui::IntoElement {
    // A childless canvas lays out at auto height (0) and would paint
    // into a zero-height content mask; fill the parent so the grid and
    // minimap get the pane's real bounds.
    canvas(
        move |_bounds, _window, _app| {},
        move |bounds, _prepaint, window, app| paint_grid(&snap, &entity, bounds, window, app),
    )
    .size_full()
}

fn paint_grid(
    snap: &GridSnapshot,
    entity: &Entity<HexPane>,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    app: &mut App,
) {
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

    // A row map sets the row count to its slot count; otherwise the
    // linear rule (with its trailing EOF-cursor row) applies.
    let row_count = match snap.row_map.as_deref() {
        Some(slots) => slots.len() as u64,
        None => geometry.row_count(source_len),
    };
    // Paint one extra row past the clipped viewport so a partially
    // scrolled row at the bottom edge is never blank.
    let last_visible_row = (first_visible_row + rows_visible.ceil() as u64 + 1).min(row_count.saturating_sub(1));
    // The editor's scrolloff/visibility bookkeeping wants the range a
    // clipped egui viewport would report, not the paint overdraw range:
    // rows fully or partially inside the viewport (`ceil`), excluding
    // the extra overdraw row above. Exclusive end row, clamped to the
    // total row count, matching egui's `visible_row_range`.
    let on_frame_end_row = ((first_visible_row as f32 + rows_visible).ceil() as u64).min(row_count);

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
    // through this frame-latch. The byte span must be the one actually
    // on screen: with a row map that is the union of the visible slots'
    // ranges (mirroring egui's `read_visible_rows` aggregate), not the
    // linear `row * cols` span.
    let visible_range = match snap.row_map.as_deref() {
        Some(slots) => mapped_visible_range(slots, first_visible_row, on_frame_end_row),
        None => {
            let visible_start = first_visible_row.saturating_mul(cols).min(source_len.get());
            let visible_end = on_frame_end_row.saturating_mul(cols).min(source_len.get());
            ByteRange::new(ByteOffset::new(visible_start), ByteOffset::new(visible_end)).ok()
        }
    };
    let scroll_offset_px = snap.scroll_rows * f32::from(metrics.line_h);

    entity.update(app, |pane, _cx| {
        pane.last_frame = Some(FrameInfo { geometry, content_origin, rows_visible, first_visible_row, minimap_bounds });
        pane.editor_mut().on_frame(scroll_offset_px, snap.columns, visible_range, None);
    });

    // The minimap ignores the row map for its downsampling: it reads
    // the source linearly and scales to `row_count` (the slot count
    // when mapped), exactly as egui hxy-view does -- egui passes
    // `total_rows = slots.len()` to `draw_minimap` but its
    // `HexMinimapSource` still reads `row * cols` (lib.rs:2049-2057).
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

    let selected = snap.selection.map(|s| s.range());
    let cursor = snap.selection.map(|s| s.cursor.get());

    // Linear fast path reads one contiguous block and sub-slices each
    // row out of it (no per-row allocation). The row-map path issues a
    // read per visible slot so non-contiguous / partial offsets work
    // and gap rows contribute an empty entry; the allocation only
    // happens when a map is actually set.
    let block_start = first_visible_row.saturating_mul(cols);
    let block_bytes = match snap.row_map.as_deref() {
        Some(_) => Vec::new(),
        None => read_visible(snap.source.as_ref(), first_visible_row, last_visible_row, cols, source_len),
    };
    let mapped_rows = match snap.row_map.as_deref() {
        Some(slots) => read_mapped_rows(snap.source.as_ref(), slots, first_visible_row, last_visible_row),
        None => Vec::new(),
    };

    // Clip the header and rows to the width left after the strip and
    // its gap, so the grid can never paint over the minimap.
    let grid_w = (bounds.size.width - minimap_bounds.size.width - px(crate::minimap::STRIP_GAP)).max(px(0.0));
    let grid_clip = gpui::bounds(bounds.origin, size(grid_w, bounds.size.height));
    // Full-width opaque strip the header sits on, from the pane's top
    // edge down to the first content row.
    let header_band = header_band_bounds(bounds.origin.y, grid_top, bounds.origin.x, grid_w);
    window.with_content_mask(Some(ContentMask { bounds: grid_clip }), |window| {
        for row in first_visible_row..=last_visible_row {
            let row_y = content_origin.y + metrics.line_h * (row - first_visible_row) as f32;
            let (row_start, row_len, row_bytes) = match snap.row_map.as_deref() {
                Some(_) => {
                    // A row past the slot list (e.g. an empty map) has no
                    // read entry; skip it. Gap rows paint nothing but
                    // still occupy their height.
                    let Some(read) = mapped_rows.get((row - first_visible_row) as usize) else { continue };
                    if read.is_gap {
                        continue;
                    }
                    (read.offset.get(), read.bytes.len() as u64, read.bytes.as_slice())
                }
                None => {
                    let start = row.saturating_mul(cols);
                    // Clamp the slice window to the block: a short read
                    // (source returned fewer bytes than requested) must
                    // paint an empty row, not panic on an out-of-range
                    // slice.
                    let idx = ((start - block_start) as usize).min(block_bytes.len());
                    let n = (cols as usize).min(block_bytes.len() - idx);
                    (start, cols, &block_bytes[idx..idx + n])
                }
            };
            let ctx = RowCtx {
                geometry: &geometry,
                origin_x: content_origin.x,
                row_y,
                row_start,
                row_len,
                cols,
                bytes: row_bytes,
            };
            paint_hover_band(&ctx, snap.hover_span, snap.colors.hover, window);
            paint_selection_bands(&ctx, selected, snap.colors.selection, window);
            paint_styler_tints(&ctx, snap, window);
            paint_cursor_cell(&ctx, snap, cursor, window);
            paint_row_text(&ctx, snap, &mono, window, app);
            paint_nibble_caret(&ctx, snap, cursor, window);
        }

        // Painted last, over an opaque band, so rows scrolled up into
        // the header strip are occluded rather than showing through.
        paint_header(snap, &geometry, &mono, content_origin, header_band, window, app);
    });
}

/// Full-width band occupying the column-header strip: from the pane's
/// top edge (`top`) down to the first content row (`grid_top`). Painted
/// opaque and last so scrolled rows never show through the header.
fn header_band_bounds(top: Pixels, grid_top: Pixels, x: Pixels, width: Pixels) -> Bounds<Pixels> {
    bounds(point(x, top), size(width, grid_top - top))
}

/// Bytes + metadata for one rendered row when a row map is active.
/// `bytes` is empty for a [`RowSlot::Gap`] (the renderer skips it).
struct RowRead {
    offset: ByteOffset,
    bytes: Vec<u8>,
    is_gap: bool,
}

/// Read every visible slot's bytes, one read per [`RowSlot::Real`] row.
/// Mirrors egui hxy-view's `read_visible_rows` map branch: a gap row
/// yields an empty entry, a real row reads exactly its `[offset,
/// offset+len)` span so partial and non-contiguous rows both work. A
/// per-row read failure logs once and paints that row empty rather than
/// aborting the frame.
fn read_mapped_rows(source: &dyn HexSource, slots: &[RowSlot], first_row: u64, last_row: u64) -> Vec<RowRead> {
    let first = first_row as usize;
    if first >= slots.len() {
        return Vec::new();
    }
    let last = (last_row as usize).min(slots.len() - 1);
    let mut rows = Vec::with_capacity(last.saturating_sub(first) + 1);
    let mut warned = false;
    for slot in &slots[first..=last] {
        match *slot {
            RowSlot::Real { offset, len } => {
                let end = offset + u64::from(len);
                let bytes = match ByteRange::new(ByteOffset::new(offset), ByteOffset::new(end)) {
                    Ok(range) => match source.read(range) {
                        Ok(bytes) => bytes,
                        Err(err) => {
                            if !warned {
                                tracing::warn!(?range, %err, "mapped row read failed; painting empty rows");
                                warned = true;
                            }
                            Vec::new()
                        }
                    },
                    Err(err) => {
                        tracing::warn!(%err, offset, len, "mapped row range invalid; painting empty row");
                        Vec::new()
                    }
                };
                rows.push(RowRead { offset: ByteOffset::new(offset), bytes, is_gap: false });
            }
            RowSlot::Gap => rows.push(RowRead { offset: ByteOffset::new(0), bytes: Vec::new(), is_gap: true }),
        }
    }
    rows
}

/// Union of the [`RowSlot::Real`] byte ranges in `slots[first..end]`,
/// the mapped equivalent of the linear `row * cols` visible span fed to
/// the editor's scrolloff bookkeeping.
fn mapped_visible_range(slots: &[RowSlot], first_row: u64, end_row: u64) -> Option<ByteRange> {
    let first = (first_row as usize).min(slots.len());
    let end = (end_row as usize).min(slots.len());
    let mut lo: Option<u64> = None;
    let mut hi: Option<u64> = None;
    for slot in &slots[first..end] {
        if let RowSlot::Real { offset, len } = *slot {
            lo = Some(lo.map_or(offset, |v| v.min(offset)));
            hi = Some(hi.map_or(offset + u64::from(len), |v| v.max(offset + u64::from(len))));
        }
    }
    match (lo, hi) {
        (Some(lo), Some(hi)) => ByteRange::new(ByteOffset::new(lo), ByteOffset::new(hi)).ok(),
        _ => None,
    }
}

/// Per-row invariants shared by the paint helpers: the row's screen
/// origin, its byte span, and the row's own byte slice (already sliced
/// out of the read block or the per-slot read, indexed from column 0).
struct RowCtx<'a> {
    geometry: &'a GridGeometry,
    origin_x: Pixels,
    row_y: Pixels,
    /// First byte offset of this row (linear `row * cols`, or the slot
    /// offset under a row map).
    row_start: u64,
    /// Count of real byte columns this row spans, for selection / hover
    /// band and cursor-column math: `cols` linearly, the slot's `len`
    /// under a row map.
    row_len: u64,
    cols: u64,
    bytes: &'a [u8],
}

impl RowCtx<'_> {
    fn line_h(&self) -> Pixels {
        self.geometry.metrics.line_h
    }

    /// Column of `cursor` within this row, or `None` if it lies outside.
    fn cursor_col(&self, cursor: u64) -> Option<u16> {
        (cursor >= self.row_start && cursor < self.row_start.saturating_add(self.row_len))
            .then(|| (cursor - self.row_start) as u16)
    }

    /// Exclusive byte end of this row's real span.
    fn row_end(&self) -> u64 {
        self.row_start.saturating_add(self.row_len)
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
    band: Bounds<Pixels>,
    window: &mut Window,
    app: &mut App,
) {
    window.paint_quad(fill(band, snap.colors.background));

    // Glyph baseline sits a top pad below the band's top edge.
    let header_y = band.origin.y + px(PAD_Y);
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
    if let Some(sel) = selected {
        paint_range_band(ctx, sel, color, window);
    }
}

/// Secondary hover-highlight band. Painted before the selection band so
/// the user's explicit selection color stays authoritative where the
/// two overlap. Mirrors egui hxy-view's hover-under-selection intent
/// (lib.rs:1353-1358), which softens the selection color for the
/// secondary marker.
fn paint_hover_band(ctx: &RowCtx, hover: Option<ByteRange>, color: Hsla, window: &mut Window) {
    if let Some(span) = hover {
        paint_range_band(ctx, span, color, window);
    }
}

/// Fill the columns of `range` that fall in this row with `color`, one
/// quad in the hex pane and one in the ascii pane.
fn paint_range_band(ctx: &RowCtx, range: ByteRange, color: Hsla, window: &mut Window) {
    let lo = range.start().get().max(ctx.row_start);
    let hi = range.end().get().min(ctx.row_end());
    if lo >= hi {
        return;
    }
    let first = (lo - ctx.row_start) as u16;
    let last = (hi - 1 - ctx.row_start) as u16;
    paint_col_run(ctx, first, last, color, window);
}

/// Fill an inclusive `from..=to` column run with `color`: one quad in
/// the hex pane and one in the ascii pane. The hex quad spans from the
/// first cell's left edge to the last cell's right edge, bridging the
/// inter-cell gaps but stopping at the run's outer edges (no bleed into
/// the section gap after the last hex column).
fn paint_col_run(ctx: &RowCtx, from: u16, to: u16, color: Hsla, window: &mut Window) {
    let g = ctx.geometry;
    let hx0 = ctx.origin_x + g.hex_x(from);
    let hx1 = ctx.origin_x + g.hex_x(to) + g.hex_cell_w();
    window.paint_quad(fill(band_bounds(hx0, hx1, ctx.row_y, ctx.line_h()), color));

    let ax0 = ctx.origin_x + g.ascii_x(from);
    let ax1 = ctx.origin_x + g.ascii_x(to + 1);
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

    let hex_b =
        band_bounds(ctx.origin_x + g.hex_x(col), ctx.origin_x + g.hex_x(col) + g.hex_cell_w(), ctx.row_y, line_h);
    let ascii_b = band_bounds(ctx.origin_x + g.ascii_x(col), ctx.origin_x + g.ascii_x(col + 1), ctx.row_y, line_h);

    let (active, inactive) = match snap.active_pane {
        Pane::Hex => (hex_b, ascii_b),
        Pane::Ascii => (ascii_b, hex_b),
    };
    window.paint_quad(fill(active, fill_color));
    window.paint_quad(outline(inactive, snap.colors.accent, gpui::BorderStyle::Solid));
}

fn paint_row_text(ctx: &RowCtx, snap: &GridSnapshot, mono: &Font, window: &mut Window, app: &mut App) {
    let g = ctx.geometry;
    let addr = format!("{:0width$X}", ctx.row_start, width = g.address_chars);
    paint_line(
        mono,
        &addr,
        snap.mono_size,
        snap.colors.muted,
        point(ctx.origin_x + g.address_x(), ctx.row_y),
        window,
        app,
    );

    let mut ascii = String::with_capacity(ctx.cols as usize);
    let mut ascii_runs: Vec<TextRun> = Vec::with_capacity(ctx.cols as usize);

    for (c, &byte) in ctx.bytes.iter().enumerate().take(ctx.cols as usize) {
        let col = c as u16;
        let offset = ByteOffset::new(ctx.row_start + c as u64);
        // Styler foreground overrides the byte-class color (egui
        // lib.rs:1400-1418: `fg_override` wins over the palette).
        let fg_override = snap.byte_styler.as_ref().and_then(|f| f(byte, offset).fg);
        let color = fg_override.unwrap_or_else(|| byte_color(byte, &snap.colors));

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

/// Byte-styler background pass: fills each cell whose styler returns a
/// `bg`. Runs after the selection / hover bands but before the cursor
/// emphasis and glyphs, so the styler tint sits under the cursor fill
/// (brief: styler consulted before selection/cursor emphasis). Cells
/// the selection or hover band already covers are skipped so those
/// bands stay authoritative, matching egui hxy-view (lib.rs:1404-1409).
fn paint_styler_tints(ctx: &RowCtx, snap: &GridSnapshot, window: &mut Window) {
    let Some(styler) = snap.byte_styler.as_ref() else { return };
    let selected = snap.selection.map(|s| s.range());
    let mut cells: Vec<(u16, Hsla)> = Vec::with_capacity(ctx.bytes.len().min(ctx.cols as usize));
    for (c, &byte) in ctx.bytes.iter().enumerate().take(ctx.cols as usize) {
        let offset = ByteOffset::new(ctx.row_start + c as u64);
        let is_sel = selected.is_some_and(|r| r.contains(offset));
        let is_hovered = snap.hover_span.is_some_and(|r| r.contains(offset));
        if let Some(bg) = styler(byte, offset).bg.filter(|_| !is_sel && !is_hovered) {
            cells.push((c as u16, bg));
        }
    }
    for (from, to, color) in merge_tint_runs(&cells) {
        paint_col_run(ctx, from, to, color, window);
    }
}

/// Collapse a row's tinted cells into maximal same-color runs, the
/// quad-count optimization egui applies (lib.rs:2004): adjacent cells
/// sharing a `bg` become one span instead of one quad each. Input is
/// `(column, color)` in ascending column order; output is inclusive
/// `(first_col, last_col, color)` spans. A run breaks when the color
/// changes or the columns are non-adjacent, so a skipped cell (already
/// covered by a selection / hover band) never bridges a run across it.
fn merge_tint_runs(cells: &[(u16, Hsla)]) -> Vec<(u16, u16, Hsla)> {
    let mut runs: Vec<(u16, u16, Hsla)> = Vec::new();
    for &(col, color) in cells {
        match runs.last_mut() {
            Some((_, last, c)) if *c == color && *last + 1 == col => *last = col,
            _ => runs.push((col, col, color)),
        }
    }
    runs
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

fn paint_line(
    mono: &Font,
    text: &str,
    size: Pixels,
    color: Hsla,
    origin: Point<Pixels>,
    window: &mut Window,
    app: &mut App,
) {
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

/// Opacity applied to the selection color to derive the hover-span
/// tint: soft enough to read as a secondary marker under the primary
/// selection band. Mirrors egui hxy-view's `gamma_multiply(0.45)` on
/// its hover fill (lib.rs:1358).
const HOVER_TINT_ALPHA: f32 = 0.45;

impl PaintColors {
    pub(crate) fn from_theme(theme: &gpui_component::Theme) -> Self {
        Self {
            foreground: theme.foreground,
            muted: theme.muted_foreground,
            accent: theme.accent_foreground,
            selection: theme.selection,
            hover: theme.selection.opacity(HOVER_TINT_ALPHA),
            background: theme.background,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hxy_core::ColumnCount;

    fn color(h: f32) -> Hsla {
        Hsla { h, s: 0.5, l: 0.5, a: 1.0 }
    }

    fn geo() -> GridGeometry {
        let metrics = CellMetrics { char_w: px(8.0), line_h: px(16.0) };
        GridGeometry::new(metrics, ColumnCount::new(16).unwrap(), ByteLen::new(256))
    }

    #[test]
    fn contiguous_same_color_merges_to_one_span() {
        let a = color(0.1);
        let runs = merge_tint_runs(&[(2, a), (3, a), (4, a)]);
        assert_eq!(runs, vec![(2, 4, a)]);
    }

    #[test]
    fn color_change_splits_runs() {
        let a = color(0.1);
        let b = color(0.7);
        let runs = merge_tint_runs(&[(0, a), (1, a), (2, b), (3, b)]);
        assert_eq!(runs, vec![(0, 1, a), (2, 3, b)]);
    }

    #[test]
    fn isolated_cell_is_a_single_column_span() {
        let a = color(0.1);
        let runs = merge_tint_runs(&[(5, a)]);
        assert_eq!(runs, vec![(5, 5, a)]);
    }

    #[test]
    fn non_adjacent_same_color_does_not_bridge_skipped_cell() {
        // Column 3 skipped (e.g. covered by a selection band): the run
        // must break rather than paint tint over the gap cell.
        let a = color(0.1);
        let runs = merge_tint_runs(&[(2, a), (4, a)]);
        assert_eq!(runs, vec![(2, 2, a), (4, 4, a)]);
    }

    #[test]
    fn empty_input_yields_no_runs() {
        assert!(merge_tint_runs(&[]).is_empty());
    }

    /// A run ending on the last hex column stops at the last cell's
    /// right edge and does not bleed into the section gap before the
    /// ascii pane.
    #[test]
    fn last_column_span_stops_before_section_gap() {
        let g = geo();
        let last = 15u16;
        let hex_right = g.hex_x(last) + g.hex_cell_w();
        assert!(hex_right < g.ascii_x(0), "hex run right edge must stop before the ascii pane");
    }

    /// The header band spans the pane's top edge down to the first
    /// content row, full grid width.
    #[test]
    fn header_band_covers_top_strip() {
        let top = px(10.0);
        let grid_top = px(30.0);
        let band = header_band_bounds(top, grid_top, px(4.0), px(200.0));
        assert_eq!(band.origin, point(px(4.0), top));
        assert_eq!(band.size.width, px(200.0));
        assert_eq!(band.origin.y + band.size.height, grid_top);
    }
}
