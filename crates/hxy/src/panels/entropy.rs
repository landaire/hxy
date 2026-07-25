//! Shannon-entropy panel.
//!
//! Entropy computation moved to `hxy_panels::entropy`
//! (framework-agnostic, shared with the GPUI port); re-exported here
//! under the original path. Worker dispatch and the `egui_plot`
//! rendering stay here -- they touch `FileId`, `egui_inbox`, and the
//! background pool.

use std::sync::Arc;

use egui_plot::Line;
use egui_plot::Plot;
use egui_plot::PlotBounds;
use egui_plot::PlotPoints;
use hxy_core::HexSource;
pub use hxy_panels::entropy::EntropyPoint;
pub use hxy_panels::entropy::EntropyState;
pub use hxy_panels::entropy::MAX_ENTROPY;
pub use hxy_panels::entropy::TARGET_POINTS;
pub use hxy_panels::entropy::compute_entropy;
pub use hxy_panels::entropy::pick_window_size;
pub use hxy_panels::entropy::shannon_entropy;

use crate::files::FileId;

/// In-flight entropy worker handle. Mirrors the template-run
/// pattern: a `UiInbox` lets the worker push the completed
/// result back to the UI thread without blocking.
pub struct EntropyComputation {
    pub inbox: egui_inbox::UiInbox<EntropyOutcome>,
    pub file_id: FileId,
    pub started: web_time::Instant,
}

#[derive(Clone, Debug)]
pub enum EntropyOutcome {
    Ok(EntropyState),
    Err(String),
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
    let started = web_time::Instant::now();
    crate::background::submit(move || {
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
        let label = file_label.unwrap_or("");
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
    let max_offset =
        state.points.last().map(|p| (p.offset + state.window_bytes) as f64).unwrap_or(state.source_len as f64);
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

    // Hex offset formatters: `0x...` on the x-axis ticks plus
    // a cursor-follow label that shows `offset = 0x... ; H = N
    // bits/byte`. Hex lines up with the address column in the
    // hex view so the user can mentally jump from a peak in
    // the plot to a row in the editor without converting bases.
    let format_hex_offset = |value: f64| -> String {
        if !value.is_finite() || value < 0.0 {
            return format!("{value:.0}");
        }
        let raw = value.max(0.0) as u64;
        format!("0x{raw:X}")
    };
    let x_axis_fmt =
        move |mark: egui_plot::GridMark, _: &std::ops::RangeInclusive<f64>| -> String { format_hex_offset(mark.value) };
    let label_fmt = move |pos: &egui_plot::HoverPosition<'_>| -> Option<String> {
        let point = match pos {
            egui_plot::HoverPosition::NearDataPoint { position, .. }
            | egui_plot::HoverPosition::Elsewhere { position } => position,
        };
        Some(format!("offset {}\nH = {:.2} bits/byte", format_hex_offset(point.x), point.y))
    };

    Plot::new("hxy-entropy-plot")
        .height(ui.available_height() - 4.0)
        .x_axis_label("offset")
        .y_axis_label("bits/byte")
        .y_axis_min_width(40.0)
        // egui_plot's defaults already enable wheel-zoom, drag-
        // pan, and box-zoom (Ctrl+drag) -- we just need to make
        // sure none of them are disabled here. Setting them
        // explicitly documents the user-visible behaviour.
        .allow_zoom(true)
        .allow_drag(true)
        .allow_scroll(true)
        .allow_boxed_zoom(true)
        .x_axis_formatter(x_axis_fmt)
        .label_formatter(label_fmt)
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
