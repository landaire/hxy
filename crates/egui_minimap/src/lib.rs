//! A scrollable minimap widget for egui.
//!
//! See the crate-level README for a quick example. The headline types
//! are [`Minimap`] (the widget builder), [`MinimapSource`] (what the
//! caller implements to provide row paint), and [`Viewport`] (the
//! caller's current scroll state, used to position the indicator).

#![warn(missing_docs)]

use std::ops::Range;

use egui::Color32;
use egui::Id;
use egui::Pos2;
use egui::Rect;
use egui::Response;
use egui::Sense;
use egui::Stroke;
use egui::StrokeKind;
use egui::Ui;
use egui::Vec2;

/// Caller-implemented data source. Tells the widget how many rows the
/// source has, and how to paint any one of them into a horizontal
/// strip rect.
///
/// Implementations may paint cells, gradients, single-color stripes,
/// glyph silhouettes, or anything else that fits inside a thin row.
/// The widget calls [`paint_rows`](Self::paint_rows) once per frame
/// with the contiguous range of source rows that fit in the minimap
/// window; the default implementation loops over [`paint_row`]
/// (Self::paint_row), but sources that benefit from a single batch
/// read (e.g. a hex view streaming from disk) can override it.
pub trait MinimapSource {
    /// Total number of rows in the source. May change between frames.
    fn row_count(&self) -> usize;

    /// Paint one row into `row_rect`. Required.
    fn paint_row(&self, painter: &egui::Painter, row_rect: Rect, row: usize);

    /// Paint a contiguous range of rows. Defaults to a loop over
    /// [`paint_row`](Self::paint_row); override when batching pays for
    /// itself (e.g. one I/O read per frame instead of one per row).
    fn paint_rows(&self, painter: &egui::Painter, column_rect: Rect, rows: Range<usize>, cell_height: f32) {
        for (i, row) in rows.enumerate() {
            let y = column_rect.top() + i as f32 * cell_height;
            let row_rect =
                Rect::from_min_size(Pos2::new(column_rect.left(), y), Vec2::new(column_rect.width(), cell_height));
            self.paint_row(painter, row_rect, row);
        }
    }
}

/// Caller's current scroll state, used to draw the viewport indicator
/// and translate the indicator's position back into a target offset.
///
/// All measurements are in pixels in the source's content space (i.e.
/// the same units the host's `ScrollArea` works in). `total_rows` is
/// usually `source.row_count()`; specifying it separately lets hosts
/// account for inserted blank rows (e.g. side-by-side comparison
/// views).
///
/// Hosts whose rows have **variable** heights (wrapped chat, mixed-
/// font code) should pass [`ViewportRows::Variable`] with cumulative
/// per-row offsets; the indicator + window math then keys off pixel
/// positions instead of `total_rows * row_height`.
#[derive(Clone, Copy, Debug)]
pub struct Viewport<'a> {
    /// Total rows the host considers part of the content. Often equal
    /// to [`MinimapSource::row_count`]; can differ when the host
    /// inserts blank gap rows.
    pub total_rows: usize,
    /// Current vertical scroll offset, in source-content pixels.
    pub scroll_offset: f32,
    /// Height of the host's viewport, in pixels.
    pub viewport_height: f32,
    /// How rows are sized in source-content pixels.
    pub rows: ViewportRows<'a>,
}

/// Per-row sizing info for the host's content.
#[derive(Clone, Copy, Debug)]
pub enum ViewportRows<'a> {
    /// All rows share the same pixel height. Cheap path, fine for
    /// hex bytes / fixed-line-height code.
    Uniform {
        /// Source-content pixel height of every row.
        row_height: f32,
    },
    /// Pre-computed cumulative content y-offsets. `offsets[i]` is
    /// the top of row `i`; `offsets[total_rows]` is the total
    /// content height. Length must be `total_rows + 1`. Used for
    /// content where wrapping or font mixes produce per-row
    /// variation (chat, syntax-highlighted code with multi-line
    /// blocks).
    Variable {
        /// Cumulative content y-offsets, length `total_rows + 1`.
        offsets: &'a [f32],
    },
}

impl ViewportRows<'_> {
    fn content_height(&self, total_rows: usize) -> f32 {
        match *self {
            ViewportRows::Uniform { row_height } => total_rows as f32 * row_height.max(1.0),
            ViewportRows::Variable { offsets } => offsets.last().copied().unwrap_or(0.0).max(0.0),
        }
    }

    /// Convert a pixel offset to a fractional row index. Linear under
    /// `Uniform`; uses the cumulative-offset table under `Variable`,
    /// interpolating inside whichever row the offset lands in so
    /// the indicator slides smoothly across row boundaries.
    fn fractional_row_at(&self, total_rows: usize, pixel_offset: f32) -> f32 {
        if total_rows == 0 || pixel_offset <= 0.0 {
            return 0.0;
        }
        match *self {
            ViewportRows::Uniform { row_height } => (pixel_offset / row_height.max(1.0)).max(0.0),
            ViewportRows::Variable { offsets } => {
                let total_h = offsets.last().copied().unwrap_or(0.0);
                if pixel_offset >= total_h || total_h <= 0.0 {
                    return total_rows as f32;
                }
                // Binary search for the row containing pixel_offset.
                // `partition_point` returns the first index where the
                // predicate is false, so we get the row whose top is
                // strictly greater than pixel_offset; subtract 1 to
                // land on the row that contains it.
                let next = offsets.partition_point(|&y| y <= pixel_offset);
                let row = next.saturating_sub(1);
                let row_top = offsets[row];
                let row_bot = offsets[row + 1];
                let row_h = (row_bot - row_top).max(1.0);
                let frac_within = ((pixel_offset - row_top) / row_h).clamp(0.0, 1.0);
                row as f32 + frac_within
            }
        }
    }
}

/// Result of one [`Minimap::show`] call.
#[derive(Debug, Clone)]
pub struct MinimapResponse {
    /// New scroll target requested by user interaction, in
    /// source-content pixels. `None` if the user didn't interact this
    /// frame. The widget also stashes this value under `scroll_id` in
    /// the egui ctx so [`ScrollArea`](egui::ScrollArea) can read it
    /// back via the shared id pattern.
    pub scroll_target: Option<f32>,

    /// The egui response for the minimap rect. Useful for hover state,
    /// custom tooltips, etc.
    pub response: Response,

    /// Where the rendered window starts, in source rows. Useful for
    /// painting overlays on top of the minimap.
    pub window_top_row: usize,

    /// Number of source rows visible in the minimap window this frame.
    pub shown_rows: usize,

    /// Pixel height of one minimap row. Snapped to a whole device
    /// pixel so successive frames don't flicker the row edges.
    pub cell_height: f32,

    /// The rect the minimap painted into.
    pub minimap_rect: Rect,
}

/// Drag origin for a minimap interaction. Stashed on `drag_started`
/// so subsequent `dragged` events know which scroll model to use.
#[derive(Clone, Copy)]
struct MinimapDragStart {
    pointer_y: f32,
    scroll_offset: f32,
    started_in_grab: bool,
}

/// The minimap widget. Construct with [`Minimap::new`], chain the
/// builder methods, then call [`Minimap::show`].
pub struct Minimap<'s> {
    source: &'s dyn MinimapSource,
    scroll_id: Option<Id>,
    viewport: Option<Viewport<'s>>,
    background: Option<Color32>,
    cell_height_devices: u32,
}

impl<'s> Minimap<'s> {
    /// Create a new minimap bound to `source`.
    pub fn new(source: &'s dyn MinimapSource) -> Self {
        Self {
            source,
            scroll_id: None,
            viewport: None,
            background: None,
            // Default tuned for the original hex-bytes use case
            // where each row is a flat color stripe; sources that
            // paint tiny text (chat messages, source code, ...)
            // should bump this to 4-8 device pixels via
            // `cell_height_devices`.
            cell_height_devices: 2,
        }
    }

    /// Egui id used to stash the requested scroll target. Pair this
    /// with [`ScrollArea::id_salt`](egui::ScrollArea::id_salt) on the
    /// host's scroll area so the host can read the stashed value next
    /// frame.
    pub fn scroll_id(mut self, id: Id) -> Self {
        self.scroll_id = Some(id);
        self
    }

    /// Caller's scroll state. Required.
    pub fn viewport(mut self, viewport: Viewport<'s>) -> Self {
        self.viewport = Some(viewport);
        self
    }

    /// Override the minimap background fill. Defaults to the theme's
    /// `extreme_bg_color`.
    pub fn background(mut self, color: Color32) -> Self {
        self.background = Some(color);
        self
    }

    /// Set the height of one minimap row in *device* pixels. The
    /// widget snaps to a whole device pixel so successive frames
    /// render the same edges; the exact logical-pixel height comes
    /// from `device_pixels / pixels_per_point`.
    ///
    /// Pick a value that fits your source's glyphs:
    /// - **2** (default): flat colored cells, suits hex bytes /
    ///   single-color stripes per row.
    /// - **4-8**: tiny glyphs you can squint at — Zed-style code
    ///   minimaps, text/chat overviews.
    pub fn cell_height_devices(mut self, devices: u32) -> Self {
        self.cell_height_devices = devices.max(1);
        self
    }

    /// Paint the minimap into `rect` and process input. Returns
    /// positioning info for caller-controlled overlay painting.
    pub fn show(self, ui: &mut Ui, rect: Rect) -> MinimapResponse {
        let viewport = self.viewport.expect("Minimap::viewport(...) is required before show(...)");
        let scroll_id = self.scroll_id.expect("Minimap::scroll_id(...) is required before show(...)");
        let source = self.source;

        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let bg = self.background.unwrap_or(ui.visuals().extreme_bg_color);
        painter.rect_filled(rect, 0.0, bg);

        let total_rows = viewport.total_rows;
        if rect.width() < 1.0 || rect.height() < 1.0 || total_rows == 0 {
            return MinimapResponse {
                scroll_target: None,
                response,
                window_top_row: 0,
                shown_rows: 0,
                cell_height: 0.0,
                minimap_rect: rect,
            };
        }

        // Snap cell height to a whole number of physical pixels.
        // egui's tessellator feathers rect edges over ~1 device pixel
        // for AA; a fractional `cell_h` in device space lands each
        // row's edges in slightly different spots from frame to frame
        // as scroll changes, which the eye reads as edge flicker.
        let ppp = ui.ctx().pixels_per_point();
        let target_devices = self.cell_height_devices.max(1) as f32;
        let cell_h = (target_devices * ppp).round().max(1.0) / ppp;

        let minimap_capacity_rows = (rect.height() / cell_h).floor() as usize;
        if minimap_capacity_rows == 0 {
            return MinimapResponse {
                scroll_target: None,
                response,
                window_top_row: 0,
                shown_rows: 0,
                cell_height: cell_h,
                minimap_rect: rect,
            };
        }

        // Linear map from the host's scroll fraction to the minimap's
        // top row. Keeps the indicator travelling the full minimap
        // height as the user scrolls, instead of pinning to the
        // middle.
        //
        // For uniform-height rows, fractional row indices land
        // exactly on row boundaries and the math is linear. For
        // variable-height rows, the cumulative-offset table is
        // queried per pixel offset so the indicator + window line
        // up with what's actually under the host's viewport.
        let viewport_top_row_f = viewport.rows.fractional_row_at(total_rows, viewport.scroll_offset);
        let viewport_bot_row_f =
            viewport.rows.fractional_row_at(total_rows, viewport.scroll_offset + viewport.viewport_height);
        let viewport_rows_f = (viewport_bot_row_f - viewport_top_row_f).max(1.0);
        let capacity_f = minimap_capacity_rows as f32;
        let content_height = viewport.rows.content_height(total_rows);
        let max_scroll = (content_height - viewport.viewport_height).max(0.0);
        let scroll_frac = if max_scroll > 0.0 { (viewport.scroll_offset / max_scroll).clamp(0.0, 1.0) } else { 0.0 };
        let max_top = (total_rows as f32 - capacity_f).max(0.0);
        let window_top_f = scroll_frac * max_top;
        let window_top_row = window_top_f.floor() as usize;
        let row_subpixel_shift = (window_top_f - window_top_row as f32) * cell_h;
        // Read one extra row so the row peeking in from the bottom
        // (after applying the negative y-shift) still has bytes to
        // draw.
        let shown_rows = (minimap_capacity_rows + 1).min(total_rows.saturating_sub(window_top_row));

        // Hand over to the source.
        let source_rows = window_top_row..(window_top_row + shown_rows);
        let column_rect = Rect::from_min_size(
            Pos2::new(rect.left(), rect.top() - row_subpixel_shift),
            Vec2::new(rect.width(), shown_rows as f32 * cell_h),
        );
        source.paint_rows(&painter, column_rect, source_rows, cell_h);

        // Viewport indicator. High-contrast outline plus an accent
        // bracket on the left edge so single-row spans still register.
        let indicator_top_y = rect.top() + (viewport_top_row_f - window_top_f) * cell_h;
        let indicator_height = viewport_rows_f * cell_h;
        let indicator = Rect::from_min_max(
            Pos2::new(rect.left(), indicator_top_y.max(rect.top())),
            Pos2::new(rect.right(), (indicator_top_y + indicator_height).min(rect.bottom())),
        );
        let active = response.hovered() || response.dragged();
        let dark = ui.visuals().dark_mode;
        let (fill, outline) = if dark {
            let fill_alpha = if active { 70 } else { 28 };
            let outline_alpha = if active { 255 } else { 110 };
            (
                Color32::from_rgba_unmultiplied(255, 255, 255, fill_alpha),
                Color32::from_rgba_unmultiplied(255, 255, 255, outline_alpha),
            )
        } else {
            let fill_alpha = if active { 70 } else { 28 };
            let outline_alpha = if active { 255 } else { 110 };
            (
                Color32::from_rgba_unmultiplied(0, 0, 0, fill_alpha),
                Color32::from_rgba_unmultiplied(20, 20, 20, outline_alpha),
            )
        };
        painter.rect_filled(indicator, 0.0, fill);
        painter.rect_stroke(indicator, 0.0, Stroke::new(2.0, outline), StrokeKind::Inside);
        let accent = ui.visuals().selection.bg_fill;
        let bracket_color = if active { accent } else { accent.gamma_multiply(0.4) };
        let bracket = Rect::from_min_max(indicator.left_top(), Pos2::new(indicator.left() + 4.0, indicator.bottom()));
        painter.rect_filled(bracket, 0.0, bracket_color);

        // Click vs. drag dispatch.
        //
        // - Click outside the indicator: jump (cursor.y -> file
        //   position fraction).
        // - Click inside the indicator: no-op. The user grabbed the
        //   handle but didn't slide; teleporting on press would yank
        //   the indicator out from under the cursor.
        // - Drag started inside the indicator: relative scroll
        //   (1px cursor motion = max_scroll / (rect.h - indicator.h)).
        // - Drag started outside: jump on press, then absolute
        //   mapping for the rest of the drag.
        let pointer = response
            .interact_pointer_pos()
            .or_else(|| response.hover_pos().filter(|_| response.is_pointer_button_down_on()));
        let drag_state_id = scroll_id.with("egui_minimap_drag_start");
        let absolute_target = |pos: Pos2| -> f32 {
            let y = (pos.y - rect.top()).clamp(0.0, rect.height());
            let frac = y / rect.height();
            (frac * max_scroll).clamp(0.0, max_scroll)
        };

        let mut scroll_target = None;

        if response.drag_started()
            && let Some(pos) = pointer
        {
            let started_in_grab = indicator.contains(pos);
            ui.ctx().data_mut(|d| {
                d.insert_temp(
                    drag_state_id,
                    MinimapDragStart { pointer_y: pos.y, scroll_offset: viewport.scroll_offset, started_in_grab },
                )
            });
            if !started_in_grab {
                let target = absolute_target(pos);
                ui.ctx().data_mut(|d| d.insert_temp(scroll_id, target));
                ui.ctx().request_repaint();
                scroll_target = Some(target);
            }
        } else if response.dragged()
            && let Some(pos) = pointer
        {
            let start = ui.ctx().data(|d| d.get_temp::<MinimapDragStart>(drag_state_id));
            let target = match start {
                Some(start) if start.started_in_grab => {
                    let max_travel = (rect.height() - indicator.height()).max(1.0);
                    let scroll_per_pixel = max_scroll / max_travel;
                    let delta_scroll = (pos.y - start.pointer_y) * scroll_per_pixel;
                    (start.scroll_offset + delta_scroll).clamp(0.0, max_scroll)
                }
                _ => absolute_target(pos),
            };
            ui.ctx().data_mut(|d| d.insert_temp(scroll_id, target));
            ui.ctx().request_repaint();
            scroll_target = Some(target);
        } else if response.clicked()
            && let Some(pos) = pointer
            && !indicator.contains(pos)
        {
            let target = absolute_target(pos);
            ui.ctx().data_mut(|d| d.insert_temp(scroll_id, target));
            ui.ctx().request_repaint();
            scroll_target = Some(target);
        }

        MinimapResponse { scroll_target, response, window_top_row, shown_rows, cell_height: cell_h, minimap_rect: rect }
    }
}
