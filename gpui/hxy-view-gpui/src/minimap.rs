//! Right-edge minimap strip: a whole-file downsampled overview painted
//! next to the hex grid, with a translucent viewport indicator and
//! click/drag-to-center scrolling.
//!
//! Unlike `egui_minimap` (crates/egui_minimap), which windows a huge
//! file into whatever number of pixel-rows fit the strip and scrolls
//! that window, this strip always compresses the *entire* file into
//! the available height: each strip row averages
//! `row_count.div_ceil(capacity)` source rows, matching the task
//! brief's simpler "whole file always visible" downsampling contract.

use gpui::bounds;
use gpui::fill;
use gpui::outline;
use gpui::point;
use gpui::px;
use gpui::size;
use gpui::BorderStyle;
use gpui::Bounds;
use gpui::Hsla;
use gpui::Pixels;
use gpui::Point;
use gpui::Rgba;
use gpui::Size;
use gpui::Window;
use hxy_core::ByteLen;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::ColumnCount;
use hxy_core::HexSource;

use crate::paint::is_printable;
use crate::paint::PaintColors;

/// Gap between the grid's content and the strip.
pub(crate) const STRIP_GAP: f32 = 8.0;

/// Cap on bytes read per strip row. One `read` per row; wide bins
/// (many source rows compressed into one strip pixel-row) sample only
/// their first `MAX_BYTES_PER_ROW` bytes rather than reading the
/// whole span, bounding the read cost independent of file size.
const MAX_BYTES_PER_ROW: u64 = 4096;

/// Pixel height of one strip row before source rows start merging
/// into a shared bin. Matches `egui_minimap`'s default
/// `cell_height_devices` (2 device px), tuned for flat per-row color
/// stripes (see `Minimap::new`'s doc in crates/egui_minimap/src/lib.rs).
const ROW_H: f32 = 2.0;

/// Alpha applied to each byte-class base color when painting a strip
/// row. Low enough that a run of one class reads as a soft tint
/// rather than a solid block, mirroring `byte_color`'s muted/
/// foreground/accent palette at reduced strength.
const CLASS_ALPHA: f32 = 0.55;

/// Strip background tint, painted under the row colors.
const STRIP_BG_ALPHA: f32 = 0.06;

const INDICATOR_FILL_ALPHA: f32 = 0.18;
const INDICATOR_OUTLINE_ALPHA: f32 = 0.55;

/// Bounds of the minimap strip, latched once per frame alongside the
/// rest of [`crate::FrameInfo`]. A plain origin/size pair rather than
/// `gpui::Bounds` (which isn't `Copy`) so it can live inside the
/// `Copy` `FrameInfo`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MinimapBounds {
    pub origin: Point<Pixels>,
    pub size: Size<Pixels>,
}

impl MinimapBounds {
    fn to_bounds(self) -> Bounds<Pixels> {
        bounds(self.origin, self.size)
    }
}

/// Strip width: 8 monospace characters, floored at 48px. Matches
/// hxy-view's `minimap_width` (hxy-view/src/lib.rs) -- `egui_minimap`
/// itself has no fixed default; the width is entirely host-chosen
/// there too, so this reuses the egui host's own formula rather than
/// a value that doesn't exist in the shared crate.
pub(crate) fn strip_width(char_w: Pixels) -> Pixels {
    (char_w * 8.0).max(px(48.0))
}

/// Places the strip flush with the right edge of `content_bounds`,
/// spanning `content_bounds`'s full height.
pub(crate) fn strip_bounds(content_bounds: Bounds<Pixels>, char_w: Pixels) -> MinimapBounds {
    let w = strip_width(char_w);
    MinimapBounds {
        origin: point(content_bounds.origin.x + content_bounds.size.width - w, content_bounds.origin.y),
        size: size(w, content_bounds.size.height),
    }
}

/// Paints the strip's background, per-row downsampled byte-class
/// colors, and the translucent viewport indicator.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_minimap(
    source: &dyn HexSource,
    source_len: ByteLen,
    columns: ColumnCount,
    colors: &PaintColors,
    strip: MinimapBounds,
    row_count: u64,
    first_visible_row: u64,
    rows_visible: f32,
    window: &mut Window,
) {
    let strip_rect = strip.to_bounds();
    window.paint_quad(fill(strip_rect, colors.muted.opacity(STRIP_BG_ALPHA)));

    if strip.size.width <= px(0.0) || strip.size.height <= px(0.0) || row_count == 0 {
        return;
    }

    let capacity = (f32::from(strip.size.height) / ROW_H).floor().max(1.0) as u64;
    let rows_per_bin = row_count.div_ceil(capacity).max(1);
    let bin_count = row_count.div_ceil(rows_per_bin).max(1);
    let bin_h = px(f32::from(strip.size.height) / bin_count as f32);
    let cols = columns.as_u64();
    let len = source_len.get();

    for bin in 0..bin_count {
        let row_start = bin.saturating_mul(rows_per_bin);
        if row_start >= row_count {
            break;
        }
        let row_end = row_start.saturating_add(rows_per_bin).min(row_count);
        let byte_start = row_start.saturating_mul(cols).min(len);
        let byte_end = row_end.saturating_mul(cols).min(len).min(byte_start.saturating_add(MAX_BYTES_PER_ROW));
        if byte_start >= byte_end {
            continue;
        }
        let Ok(range) = ByteRange::new(ByteOffset::new(byte_start), ByteOffset::new(byte_end)) else { continue };
        let bytes = match source.read(range) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(?range, %err, "minimap row read failed; painting nothing for this row");
                continue;
            }
        };
        if bytes.is_empty() {
            continue;
        }
        let color = average_class_color(&bytes, colors);
        let y = strip.origin.y + bin_h * bin as f32;
        window.paint_quad(fill(bounds(point(strip.origin.x, y), size(strip.size.width, bin_h)), color));
    }

    paint_viewport_indicator(strip, colors, row_count, first_visible_row, rows_visible, window);
}

/// Byte-class average for one strip row: each byte votes for the zero
/// / printable-ascii / other class (same three classes and boundary
/// as `paint::byte_color`), and the row's fill is the vote-weighted
/// blend of each class's low-alpha base color.
fn average_class_color(bytes: &[u8], colors: &PaintColors) -> Hsla {
    let mut zero = 0u32;
    let mut printable = 0u32;
    let mut other = 0u32;
    for &byte in bytes {
        if byte == 0 {
            zero += 1;
        } else if is_printable(byte) {
            printable += 1;
        } else {
            other += 1;
        }
    }
    let n = bytes.len() as f32;
    let weighted = |base: Hsla, count: u32| -> Rgba {
        let c = Rgba::from(base.opacity(CLASS_ALPHA));
        let w = count as f32 / n;
        Rgba { r: c.r * w, g: c.g * w, b: c.b * w, a: c.a * w }
    };
    let z = weighted(colors.muted, zero);
    let p = weighted(colors.foreground, printable);
    let o = weighted(colors.accent, other);
    Hsla::from(Rgba { r: z.r + p.r + o.r, g: z.g + p.g + o.g, b: z.b + p.b + o.b, a: z.a + p.a + o.a })
}

/// Translucent quad over the strip rows spanned by the grid's current
/// viewport, mirroring `egui_minimap`'s indicator (`Minimap::show`,
/// crates/egui_minimap/src/lib.rs): filled band plus a full outline.
fn paint_viewport_indicator(
    strip: MinimapBounds,
    colors: &PaintColors,
    row_count: u64,
    first_visible_row: u64,
    rows_visible: f32,
    window: &mut Window,
) {
    let total = row_count as f32;
    let strip_h = f32::from(strip.size.height);
    let top_frac = (first_visible_row as f32 / total).clamp(0.0, 1.0);
    let bot_frac = ((first_visible_row as f32 + rows_visible) / total).clamp(0.0, 1.0);
    let top = strip.origin.y + px(strip_h * top_frac);
    let bot = strip.origin.y + px(strip_h * bot_frac);
    let indicator = bounds(point(strip.origin.x, top), size(strip.size.width, (bot - top).max(px(1.0))));
    window.paint_quad(fill(indicator, colors.foreground.opacity(INDICATOR_FILL_ALPHA)));
    window.paint_quad(outline(indicator, colors.foreground.opacity(INDICATOR_OUTLINE_ALPHA), BorderStyle::Solid));
}
