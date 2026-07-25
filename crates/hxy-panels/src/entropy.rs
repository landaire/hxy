//! Shannon-entropy computation.
//!
//! Computes per-window entropy (Sum -p_i * log2(p_i) over the 256
//! byte values) for a byte source. Framework- and app-agnostic; the
//! `egui_plot` rendering and worker dispatch stay in `crates/hxy`.

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;

/// Theoretical maximum Shannon entropy for byte data:
/// log2(256) = 8 bits per byte. Returned by perfectly uniform
/// (random-looking) byte distributions.
pub const MAX_ENTROPY: f64 = 8.0;

/// Default upper bound on plot points. Big files use larger
/// windows so the line fits this budget; small files use a
/// minimum window so the plot has at least a few hundred
/// points where the file is large enough to provide them.
pub const TARGET_POINTS: u64 = 4096;

/// Smallest window size we'll ever use, in bytes. Below this
/// the per-window entropy estimate is too noisy to be useful
/// (256 distinct byte values can't fit into fewer than 256
/// samples without the count going to 0/1 for most slots).
pub const MIN_WINDOW_BYTES: u64 = 256;

/// Largest window size we'll use. Above this the line gets
/// too smooth to surface format boundaries; we cap there and
/// drop below TARGET_POINTS for files in the tens-of-GiB
/// range.
pub const MAX_WINDOW_BYTES: u64 = 1024 * 1024;

/// One sample on the entropy plot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EntropyPoint {
    /// Byte offset of this window's start.
    pub offset: u64,
    /// Shannon entropy in bits per byte. `0.0` for empty or
    /// constant windows; up to [`MAX_ENTROPY`] for uniformly
    /// random bytes.
    pub entropy: f64,
}

/// Result of an entropy computation. Stashed on `OpenFile`
/// after the worker finishes so re-opening the panel doesn't
/// recompute. The `source_len` field is captured at compute
/// time so a later edit / reload can be detected as drift and
/// the user prompted to recompute.
#[derive(Clone, Debug)]
pub struct EntropyState {
    pub points: Vec<EntropyPoint>,
    pub source_len: u64,
    pub window_bytes: u64,
    pub computed_at: jiff::Timestamp,
}

impl EntropyState {
    /// Mean entropy across every window. Used as a one-line
    /// summary in the panel header.
    pub fn mean(&self) -> f64 {
        if self.points.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.points.iter().map(|p| p.entropy).sum();
        sum / self.points.len() as f64
    }

    /// Highest per-window entropy in the dataset. Useful as a
    /// quick "is anything in this file actually high-entropy?"
    /// readout next to the mean.
    pub fn max(&self) -> f64 {
        self.points.iter().map(|p| p.entropy).fold(0.0_f64, f64::max)
    }
}

/// Pick a window size that fits roughly [`TARGET_POINTS`]
/// samples into `len` bytes while staying inside the
/// `[MIN_WINDOW_BYTES, MAX_WINDOW_BYTES]` envelope. Empty
/// inputs return `MIN_WINDOW_BYTES` so the caller doesn't
/// have to special-case zero.
pub fn pick_window_size(len: u64) -> u64 {
    if len == 0 {
        return MIN_WINDOW_BYTES;
    }
    let raw = len.div_ceil(TARGET_POINTS).max(1);
    raw.clamp(MIN_WINDOW_BYTES, MAX_WINDOW_BYTES)
}

/// Compute Shannon entropy for one byte slice. `0.0` when
/// `bytes` is empty.
pub fn shannon_entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for b in bytes {
        counts[*b as usize] += 1;
    }
    let total = bytes.len() as f64;
    let mut sum = 0.0_f64;
    for &c in counts.iter() {
        if c == 0 {
            continue;
        }
        let p = c as f64 / total;
        sum += p * p.log2();
    }
    -sum
}

/// Synchronous entropy compute. Called from the worker; also
/// usable from tests. Reads `source` in fixed-size windows
/// and emits one [`EntropyPoint`] per window.
pub fn compute_entropy(source: &dyn HexSource, window_bytes: u64) -> Result<Vec<EntropyPoint>, String> {
    let len = source.len().get();
    if len == 0 {
        return Ok(Vec::new());
    }
    let window = window_bytes.max(1);
    let mut out = Vec::with_capacity((len.div_ceil(window) as usize).min(TARGET_POINTS as usize + 8));
    let mut offset: u64 = 0;
    while offset < len {
        let end = (offset + window).min(len);
        let range = ByteRange::new(ByteOffset::new(offset), ByteOffset::new(end))
            .map_err(|e| format!("range {offset}..{end}: {e}"))?;
        let bytes = source.read(range).map_err(|e| format!("read {offset}..{end}: {e}"))?;
        out.push(EntropyPoint { offset, entropy: shannon_entropy(&bytes) });
        offset = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hxy_core::MemorySource;

    #[test]
    fn entropy_of_uniform_bytes_is_max() {
        let bytes: Vec<u8> = (0u16..=255).map(|v| v as u8).cycle().take(4096).collect();
        let h = shannon_entropy(&bytes);
        assert!(h > 7.99 && h <= 8.0, "expected ~8 bits/byte, got {h}");
    }

    #[test]
    fn entropy_of_constant_bytes_is_zero() {
        let bytes = vec![0xAAu8; 4096];
        assert_eq!(shannon_entropy(&bytes), 0.0);
    }

    #[test]
    fn entropy_of_empty_is_zero() {
        assert_eq!(shannon_entropy(&[]), 0.0);
    }

    #[test]
    fn pick_window_size_handles_extremes() {
        assert_eq!(pick_window_size(0), MIN_WINDOW_BYTES);
        assert_eq!(pick_window_size(100), MIN_WINDOW_BYTES);
        // Mid-sized files should land somewhere inside the
        // envelope and divide the file into roughly
        // TARGET_POINTS samples.
        let len = 64 * 1024 * 1024;
        let w = pick_window_size(len);
        assert!((MIN_WINDOW_BYTES..=MAX_WINDOW_BYTES).contains(&w));
        // Huge files cap at MAX_WINDOW_BYTES even if we'd
        // need fewer points to represent them.
        let huge = 64 * 1024 * 1024 * 1024;
        assert_eq!(pick_window_size(huge), MAX_WINDOW_BYTES);
    }

    #[test]
    fn compute_entropy_emits_one_point_per_window() {
        let source = MemorySource::new(vec![0u8; 1024]);
        let points = compute_entropy(&source, 256).unwrap();
        assert_eq!(points.len(), 4);
        for p in &points {
            assert_eq!(p.entropy, 0.0);
        }
    }
}
