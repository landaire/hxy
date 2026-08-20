//! `[[hex::visualize("digram")]]` grid builder: a 256x256 heatmap
//! of consecutive byte pairs. Each (b[i], b[i+1]) increments cell
//! `(b[i], b[i+1])`; the resulting grid surfaces structure that a
//! 1D byte distribution misses (e.g. UTF-8 bytes pile along
//! diagonals; uniform random fills the whole square evenly).

use hxy_core::color::Rgba;

pub const SIDE: usize = 256;

/// Count byte pairs into a row-major `SIDE` x `SIDE` pixel grid,
/// log-scaled through the viridis LUT. `None` when there are fewer
/// than two bytes (no pairs to count).
pub fn build_grid(bytes: &[u8]) -> Option<Vec<Rgba>> {
    if bytes.len() < 2 {
        return None;
    }
    let mut counts = vec![0u32; SIDE * SIDE];
    for window in bytes.windows(2) {
        let idx = (window[0] as usize) * SIDE + window[1] as usize;
        counts[idx] = counts[idx].saturating_add(1);
    }
    let max = *counts.iter().max().unwrap_or(&0).max(&1);
    // Log scaling -- raw counts produce a near-black texture for
    // any non-pathological input because a few cells dominate.
    let scale = (max as f64).ln().max(1.0);
    let mut pixels = Vec::with_capacity(SIDE * SIDE);
    for &c in &counts {
        let v = if c == 0 {
            0
        } else {
            let normalized = ((c as f64).ln() / scale).clamp(0.0, 1.0);
            (normalized * 255.0).round() as u8
        };
        pixels.push(viridis(v));
    }
    Some(pixels)
}

/// Cheap viridis approximation: 5 anchor stops linearly interpolated.
/// Doesn't ship a full LUT but reads close enough that low-count
/// cells are dim and saturated cells are bright yellow.
pub fn viridis(v: u8) -> Rgba {
    const STOPS: [[u8; 3]; 5] =
        [[0x44, 0x01, 0x54], [0x3b, 0x52, 0x8b], [0x21, 0x90, 0x8c], [0x5e, 0xc9, 0x62], [0xfd, 0xe7, 0x25]];
    let segs = STOPS.len() - 1;
    let scaled = (v as f32 / 255.0) * segs as f32;
    let i = scaled.floor() as usize;
    let i = i.min(segs - 1);
    let t = scaled - i as f32;
    let lerp = |a: u8, b: u8| ((a as f32) * (1.0 - t) + (b as f32) * t).round() as u8;
    Rgba::rgb(
        lerp(STOPS[i][0], STOPS[i + 1][0]),
        lerp(STOPS[i][1], STOPS[i + 1][1]),
        lerp(STOPS[i][2], STOPS[i + 1][2]),
    )
}
