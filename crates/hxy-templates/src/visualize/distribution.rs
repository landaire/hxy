//! `[[hex::visualize("layered_distribution")]]` grid builder: a
//! heatmap showing how byte values are distributed across the field.
//! The X axis walks position (chunked into ~256 columns); the Y axis
//! enumerates the 256 byte values; cell intensity is the count of
//! that byte value in that chunk. Useful for spotting "blobs of one
//! byte value" sandwiched in otherwise diverse data.

use hxy_core::color::Rgba;

pub const HEIGHT: usize = 256;
pub const TARGET_COLS: usize = 256;

/// Build the row-major `cols` x `HEIGHT` pixel grid, log-scaled
/// through the heat LUT. Returns `(cols, pixels)`; `None` when the
/// field is empty.
pub fn build_grid(bytes: &[u8]) -> Option<(usize, Vec<Rgba>)> {
    if bytes.is_empty() {
        return None;
    }
    let cols = bytes.len().clamp(1, TARGET_COLS);
    let chunk = bytes.len().div_ceil(cols);
    let actual_cols = bytes.len().div_ceil(chunk);
    let mut grid = vec![0u32; actual_cols * HEIGHT];
    let mut max_count = 1u32;
    for (col, slice) in bytes.chunks(chunk).enumerate() {
        let mut col_counts = [0u32; HEIGHT];
        for &b in slice {
            col_counts[b as usize] += 1;
        }
        for (val, &c) in col_counts.iter().enumerate() {
            // Image origin is top-left; flip Y so byte 0 sits at the
            // bottom (matches mental model of "high values up").
            let row = HEIGHT - 1 - val;
            grid[row * actual_cols + col] = c;
            if c > max_count {
                max_count = c;
            }
        }
    }
    let scale = (max_count as f32).ln().max(1.0);
    let mut pixels = Vec::with_capacity(actual_cols * HEIGHT);
    for &c in &grid {
        let v = if c == 0 { 0 } else { ((c as f32).ln() / scale * 255.0).clamp(0.0, 255.0).round() as u8 };
        pixels.push(heat(v));
    }
    Some((actual_cols, pixels))
}

pub fn heat(v: u8) -> Rgba {
    // Inferno-ish: black -> red -> yellow -> white. Three-stop lerp.
    const STOPS: [[u8; 3]; 4] = [[0, 0, 0], [0xb0, 0x10, 0x10], [0xf6, 0xb0, 0x10], [0xff, 0xff, 0xe5]];
    let segs = STOPS.len() - 1;
    let scaled = (v as f32 / 255.0) * segs as f32;
    let i = (scaled.floor() as usize).min(segs - 1);
    let t = scaled - i as f32;
    let lerp = |a: u8, b: u8| ((a as f32) * (1.0 - t) + (b as f32) * t).round() as u8;
    Rgba::rgb(
        lerp(STOPS[i][0], STOPS[i + 1][0]),
        lerp(STOPS[i][1], STOPS[i + 1][1]),
        lerp(STOPS[i][2], STOPS[i + 1][2]),
    )
}
