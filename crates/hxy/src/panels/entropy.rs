//! Shannon-entropy panel.
//!
//! Computes per-window entropy (Σ -p_i * log2(p_i) over the
//! 256 byte values) for the active file's bytes on a worker
//! thread, then renders the result as an `egui_plot` line so
//! the user can spot compressed / encrypted regions at a
//! glance. Triggered explicitly from the command palette;
//! results are cached on the file's [`OpenFile::entropy`]
//! slot so reopening the panel doesn't recompute.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use egui_plot::Line;
use egui_plot::Plot;
use egui_plot::PlotBounds;
use egui_plot::PlotPoints;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;

use crate::files::FileId;

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
pub const MAX_WINDOW_BYTES: u64 = 1 * 1024 * 1024;

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

/// In-flight entropy worker handle. Mirrors the template-run
/// pattern: a `UiInbox` lets the worker push the completed
/// result back to the UI thread without blocking.
pub struct EntropyComputation {
    pub inbox: egui_inbox::UiInbox<EntropyOutcome>,
    pub file_id: FileId,
    pub started: std::time::Instant,
}

#[derive(Clone, Debug)]
pub enum EntropyOutcome {
    Ok(EntropyState),
    Err(String),
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
/// and emits one [`EntropyPoint`] per window. Returns an error
/// when a read fails so the worker can surface it through
/// [`EntropyOutcome::Err`].
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

/// Spin up the entropy worker for `id` against the file's
/// current source. Returns the in-flight handle the host
/// stashes on `OpenFile::entropy_running`. The worker reads
/// from a clone of the source so concurrent editing doesn't
/// race with sampling -- the computed result reflects bytes
/// at compute time and gets re-fired automatically when the
/// reload path swaps the source.
pub fn spawn_compute(
    ctx: &egui::Context,
    id: FileId,
    source: Arc<dyn HexSource>,
    window_bytes: u64,
) -> EntropyComputation {
    let (sender, inbox) = egui_inbox::UiInbox::channel_with_ctx(ctx);
    let started = std::time::Instant::now();
    std::thread::spawn(move || {
        let outcome = match compute_entropy(&*source, window_bytes) {
            Ok(points) => EntropyOutcome::Ok(EntropyState {
                points,
                source_len: source.len().get(),
                window_bytes,
                computed_at: jiff::Timestamp::now(),
            }),
            Err(e) => EntropyOutcome::Err(e),
        };
        let _ = sender.send(outcome);
    });
    EntropyComputation { inbox, file_id: id, started }
}

/// Render the entropy panel. `state` is the file's most
/// recently completed compute (if any); `running` indicates
/// whether a worker is currently in flight (so the panel can
/// dim the plot and surface a "computing..." label). `file`
/// names the active file for the heading; `None` renders a
/// no-file placeholder.
pub fn show(
    ui: &mut egui::Ui,
    file_label: Option<&str>,
    state: Option<&EntropyState>,
    running: bool,
    on_compute: &mut bool,
) {
    ui.horizontal(|ui| {
        ui.heading(hxy_i18n::t("entropy-heading"));
        ui.add_space(8.0);
        let label = file_label.unwrap_or_else(|| "");
        ui.label(egui::RichText::new(label).weak());
    });
    ui.separator();

    if file_label.is_none() {
        ui.label(hxy_i18n::t("entropy-no-active-file"));
        return;
    }

    ui.horizontal(|ui| {
        let button = egui::Button::new(if running {
            hxy_i18n::t("entropy-computing")
        } else if state.is_some() {
            hxy_i18n::t("entropy-recompute")
        } else {
            hxy_i18n::t("entropy-compute")
        });
        let response = ui.add_enabled(!running, button);
        if response.clicked() {
            *on_compute = true;
        }
        if let Some(s) = state {
            ui.add_space(8.0);
            ui.label(egui::RichText::new(format_summary(s)).weak());
        }
    });
    ui.add_space(4.0);

    let Some(state) = state else {
        ui.label(hxy_i18n::t("entropy-empty"));
        return;
    };
    if state.points.is_empty() {
        ui.label(hxy_i18n::t("entropy-zero-bytes"));
        return;
    }

    let line_color = ui.visuals().widgets.active.fg_stroke.color;
    let max_offset = state
        .points
        .last()
        .map(|p| (p.offset + state.window_bytes) as f64)
        .unwrap_or(state.source_len as f64);
    let plot_points: PlotPoints = state
        .points
        .iter()
        .map(|p| {
            // Centre each window's entropy at the window
            // midpoint so zooming in lines the data up with
            // the file region rather than the window's leading
            // edge.
            let mid = p.offset as f64 + (state.window_bytes as f64) / 2.0;
            [mid, p.entropy]
        })
        .collect();
    let line = Line::new("entropy", plot_points).color(line_color);

    Plot::new("hxy-entropy-plot")
        .height(ui.available_height() - 4.0)
        .x_axis_label("offset")
        .y_axis_label("bits/byte")
        .y_axis_min_width(30.0)
        .allow_scroll(false)
        .show(ui, |plot_ui| {
            plot_ui.set_plot_bounds(PlotBounds::from_min_max([0.0, 0.0], [max_offset.max(1.0), MAX_ENTROPY]));
            plot_ui.line(line);
        });
}

fn format_summary(state: &EntropyState) -> String {
    hxy_i18n::t_args(
        "entropy-summary",
        &[
            ("mean", &format!("{:.2}", state.mean())),
            ("max", &format!("{:.2}", state.max())),
            ("window", &format_bytes(state.window_bytes)),
            ("count", &state.points.len().to_string()),
        ],
    )
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
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
        assert!(w >= MIN_WINDOW_BYTES && w <= MAX_WINDOW_BYTES);
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
