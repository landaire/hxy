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

use gpui::BorderStyle;
use gpui::Bounds;
use gpui::Hsla;
use gpui::Pixels;
use gpui::Point;
use gpui::Rgba;
use gpui::Size;
use gpui::Window;
use gpui::bounds;
use gpui::fill;
use gpui::outline;
use gpui::point;
use gpui::px;
use gpui::size;
use hxy_core::ByteLen;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::ColumnCount;
use hxy_core::HexSource;

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

/// Strip background tint, painted under the row colors.
const STRIP_BG_ALPHA: f32 = 0.06;

/// Fill wash over the visible slice. Kept low so the downsampled row
/// colors underneath still read through the accent tint.
const INDICATOR_FILL_ALPHA: f32 = 0.14;
/// Width of the solid accent bar on the indicator's inner edge, the
/// strongest readable cue (mirrors egui_minimap's left bracket).
const INDICATOR_BRACKET_W: f32 = 3.0;

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

/// Paints the strip's background, per-row downsampled byte colors
/// (the highlight palette's average when `colored`, grayscale
/// gradient otherwise), and the translucent viewport indicator.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_minimap(
    source: &dyn HexSource,
    source_len: ByteLen,
    columns: ColumnCount,
    colors: &PaintColors,
    colored: bool,
    palette: Option<&[Hsla; 256]>,
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
    // A persistently failing source (e.g. deleted mid-session) would
    // otherwise log once per bin -- hundreds per repaint for a tall
    // strip, versus the grid's own read path logging once per frame
    // (a single batched read). Cap logging to the first failure in
    // this pass; later bins still skip painting silently.
    let mut warned = false;

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
                if !warned {
                    tracing::warn!(?range, %err, bin, bin_count, "minimap row read failed; painting nothing for this and any further failed rows this pass");
                    warned = true;
                }
                continue;
            }
        };
        if bytes.is_empty() {
            continue;
        }
        let color = if colored {
            average_palette_color(&bytes, palette, colors)
        } else {
            average_gray_color(&bytes, colors.dark)
        };
        let y = strip.origin.y + bin_h * bin as f32;
        window.paint_quad(fill(bounds(point(strip.origin.x, y), size(strip.size.width, bin_h)), color));
    }

    paint_viewport_indicator(strip, colors, row_count, first_visible_row, rows_visible, window);
}

/// Palette average for one strip row: every byte contributes its
/// palette color (the same class / value / custom table the grid
/// paints, or the theme foreground when no highlight is installed --
/// egui's minimap fallback), and the row's fill is the mean, muted by
/// the shared minimap blend factor. egui paints per-byte cells and
/// gamma-multiplies each by the same factor; this strip paints one
/// flat color per row, so the blend rides the average instead.
fn average_palette_color(bytes: &[u8], palette: Option<&[Hsla; 256]>, colors: &PaintColors) -> Hsla {
    let Some(table) = palette else {
        return colors.foreground.opacity(hxy_core::byte_palette::MINIMAP_CELL_BLEND);
    };
    let mut counts = [0u32; 256];
    for &byte in bytes {
        counts[byte as usize] += 1;
    }
    let n = bytes.len() as f32;
    let mut sum = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
    for (value, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let c = Rgba::from(table[value]);
        let w = count as f32 / n;
        sum = Rgba { r: sum.r + c.r * w, g: sum.g + c.g * w, b: sum.b + c.b * w, a: sum.a + c.a * w };
    }
    Hsla::from(sum).opacity(hxy_core::byte_palette::MINIMAP_CELL_BLEND)
}

/// Grayscale fallback for one strip row: the row's mean byte value
/// picks a gray level on the shared `grayscale_for_byte` ramp (0x00
/// dark gray, 0xFF near-white), applied to the bin average since this
/// strip paints one flat color per row rather than per-byte cells.
/// Same blend factor as the palette average so the two modes read
/// equally muted.
fn average_gray_color(bytes: &[u8], dark: bool) -> Hsla {
    let sum: u64 = bytes.iter().map(|&b| u64::from(b)).sum();
    let mean = (sum as f32 / bytes.len() as f32).round().clamp(0.0, 255.0) as u8;
    let g = hxy_core::byte_palette::grayscale_for_byte(mean, dark);
    let l = f32::from(g.r) / 255.0;
    Hsla { h: 0.0, s: 0.0, l, a: hxy_core::byte_palette::MINIMAP_CELL_BLEND }
}

/// Marks the strip rows spanned by the grid's current viewport, in the
/// theme accent: an accent wash, a crisp accent outline, and a solid
/// accent bar on the inner edge. The accent (vs the old low-alpha
/// foreground wash) and the bar make the visible slice legible over the
/// downsampled row colors. Mirrors `egui_minimap`'s indicator plus its
/// left bracket (`Minimap::show`, crates/egui_minimap/src/lib.rs).
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
    window.paint_quad(fill(indicator, colors.accent.opacity(INDICATOR_FILL_ALPHA)));
    window.paint_quad(outline(indicator, colors.accent, BorderStyle::Solid));
    let bracket = bounds(indicator.origin, size(px(INDICATOR_BRACKET_W).min(indicator.size.width), indicator.size.height));
    window.paint_quad(fill(bracket, colors.accent));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> PaintColors {
        PaintColors {
            foreground: gpui::hsla(0.2, 0.5, 0.5, 1.0),
            muted: gpui::hsla(0.3, 0.5, 0.5, 1.0),
            accent: gpui::hsla(0.4, 0.5, 0.5, 1.0),
            selection: gpui::hsla(0.5, 0.5, 0.5, 1.0),
            hover: gpui::hsla(0.6, 0.5, 0.5, 1.0),
            background: gpui::hsla(0.7, 0.5, 0.5, 1.0),
            dark: true,
        }
    }

    const BLEND: f32 = hxy_core::byte_palette::MINIMAP_CELL_BLEND;

    /// Colored mode without an installed palette falls back to the
    /// theme foreground (egui's minimap fallback), muted by the shared
    /// blend; a uniform palette row is that entry, equally muted.
    #[test]
    fn palette_average_uses_table_or_foreground_fallback() {
        let c = colors();
        assert_eq!(average_palette_color(&[0x00, 0x41, 0xFF], None, &c), c.foreground.opacity(BLEND));

        let entry = gpui::hsla(0.25, 0.8, 0.4, 1.0);
        let table = [entry; 256];
        let averaged = average_palette_color(&[0x10, 0x20, 0x30], Some(&table), &c);
        let expected = Hsla::from(Rgba::from(entry)).opacity(BLEND);
        assert!((averaged.h - expected.h).abs() < 1e-4, "{averaged:?} vs {expected:?}");
        assert!((averaged.l - expected.l).abs() < 1e-4);
        assert!((averaged.a - expected.a).abs() < 1e-4);
    }

    /// A mixed row averages the per-byte palette colors by count.
    #[test]
    fn palette_average_weights_by_byte_count() {
        let c = colors();
        let mut table = [gpui::hsla(0.0, 0.0, 0.0, 1.0); 256];
        table[0x00] = Hsla::from(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 });
        table[0x01] = Hsla::from(Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 });
        let averaged = average_palette_color(&[0x00, 0x01], Some(&table), &c);
        let rgba = Rgba::from(Hsla { a: 1.0, ..averaged });
        assert!((rgba.r - 0.5).abs() < 1e-3, "half red: {rgba:?}");
        assert!((rgba.b - 0.5).abs() < 1e-3, "half blue: {rgba:?}");
        assert!((averaged.a - BLEND).abs() < 1e-4, "blend factor applied");
    }

    /// Grayscale rows sit on the shared ramp endpoints (dark: 40..230)
    /// at the shared blend alpha.
    #[test]
    fn gray_average_rides_the_shared_ramp() {
        let low = average_gray_color(&[0x00, 0x00], true);
        assert!((low.l - 40.0 / 255.0).abs() < 1e-4);
        assert!((low.a - BLEND).abs() < 1e-4);
        let high = average_gray_color(&[0xFF], true);
        assert!((high.l - 230.0 / 255.0).abs() < 1e-4);
        let light = average_gray_color(&[0xFF], false);
        assert!((light.l - 220.0 / 255.0).abs() < 1e-4);
    }
}
