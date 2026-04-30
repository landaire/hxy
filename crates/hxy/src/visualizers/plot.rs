//! Numeric-data visualizers backed by `egui_plot`.
//!
//! - `line_plot(format)`  -> `egui_plot::Line`
//! - `bar_chart(format)`  -> `egui_plot::BarChart`
//! - `scatter_plot(...)`  -> `egui_plot::Points` (paired x/y)
//! - `chunk_entropy(window?)` -> per-chunk Shannon entropy line
//!
//! `format` selects how to slice the field's bytes into numbers
//! (`u8`, `u16le`, `u16be`, `u32le`, `u32be`, `u64le`, `u64be`,
//! `f32le`, `f32be`, `f64le`, `f64be`). Default is `u8`. The plots
//! are read-only -- editing comes back through the main hex view.

use egui_plot::Bar;
use egui_plot::BarChart;
use egui_plot::Line;
use egui_plot::Plot;
use egui_plot::PlotPoints;
use egui_plot::Points;

use super::VisualizerContext;
use crate::panels::entropy;

#[derive(Clone, Copy, Debug)]
enum Sample {
    U8,
    U16Le,
    U16Be,
    U32Le,
    U32Be,
    U64Le,
    U64Be,
    F32Le,
    F32Be,
    F64Le,
    F64Be,
}

impl Sample {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "u8" | "byte" | "uint8" => Self::U8,
            "u16le" | "u16" | "uint16" | "uint16le" => Self::U16Le,
            "u16be" | "uint16be" => Self::U16Be,
            "u32le" | "u32" | "uint32" | "uint32le" => Self::U32Le,
            "u32be" | "uint32be" => Self::U32Be,
            "u64le" | "u64" | "uint64" | "uint64le" => Self::U64Le,
            "u64be" | "uint64be" => Self::U64Be,
            "f32le" | "f32" | "float" | "float32" => Self::F32Le,
            "f32be" | "float32be" => Self::F32Be,
            "f64le" | "f64" | "double" | "float64" => Self::F64Le,
            "f64be" | "float64be" => Self::F64Be,
            _ => return None,
        })
    }

    fn width(&self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16Le | Self::U16Be => 2,
            Self::U32Le | Self::U32Be | Self::F32Le | Self::F32Be => 4,
            Self::U64Le | Self::U64Be | Self::F64Le | Self::F64Be => 8,
        }
    }

    fn read(&self, b: &[u8]) -> f64 {
        match self {
            Self::U8 => b[0] as f64,
            Self::U16Le => u16::from_le_bytes([b[0], b[1]]) as f64,
            Self::U16Be => u16::from_be_bytes([b[0], b[1]]) as f64,
            Self::U32Le => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            Self::U32Be => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
            Self::U64Le => u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f64,
            Self::U64Be => u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f64,
            Self::F32Le => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            Self::F32Be => f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
            Self::F64Le => f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            Self::F64Be => f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
        }
    }
}

fn parse_sample(args: &[String]) -> Sample {
    args.first().and_then(|a| Sample::parse(a)).unwrap_or(Sample::U8)
}

fn samples(bytes: &[u8], sample: Sample) -> Vec<f64> {
    bytes.chunks_exact(sample.width()).map(|c| sample.read(c)).collect()
}

pub fn show_line(ui: &mut egui::Ui, ctx: &VisualizerContext) {
    let sample = parse_sample(&ctx.spec.args);
    let values = samples(ctx.bytes, sample);
    if values.is_empty() {
        ui.weak(hxy_i18n::t("visualizer-plot-no-samples"));
        return;
    }
    let points: PlotPoints = values.iter().enumerate().map(|(i, v)| [i as f64, *v]).collect();
    Plot::new(ctx.ui_id.with("line")).height(ui.available_height() - 4.0).show(ui, |plot_ui| {
        plot_ui.line(Line::new("samples", points));
    });
}

pub fn show_bar(ui: &mut egui::Ui, ctx: &VisualizerContext) {
    let sample = parse_sample(&ctx.spec.args);
    let values = samples(ctx.bytes, sample);
    if values.is_empty() {
        ui.weak(hxy_i18n::t("visualizer-plot-no-samples"));
        return;
    }
    let bars: Vec<Bar> = values.iter().enumerate().map(|(i, v)| Bar::new(i as f64, *v)).collect();
    Plot::new(ctx.ui_id.with("bar")).height(ui.available_height() - 4.0).show(ui, |plot_ui| {
        plot_ui.bar_chart(BarChart::new("samples", bars));
    });
}

pub fn show_scatter(ui: &mut egui::Ui, ctx: &VisualizerContext) {
    // Scatter pairs consecutive samples as (x, y). Useful for
    // looking at correlated quantities packed back-to-back. Two
    // separate format args (`scatter_plot(u16le, u16le)`) would
    // let the user mix widths; today both axes share one format
    // for simplicity.
    let sample = parse_sample(&ctx.spec.args);
    let values = samples(ctx.bytes, sample);
    if values.len() < 2 {
        ui.weak(hxy_i18n::t("visualizer-plot-no-samples"));
        return;
    }
    let points: PlotPoints = values.chunks_exact(2).map(|c| [c[0], c[1]]).collect();
    Plot::new(ctx.ui_id.with("scatter")).height(ui.available_height() - 4.0).data_aspect(1.0).show(ui, |plot_ui| {
        plot_ui.points(Points::new("samples", points).radius(2.0));
    });
}

pub fn show_chunk_entropy(ui: &mut egui::Ui, ctx: &VisualizerContext) {
    // Reuse the entropy panel's compute. The window arg is optional;
    // unset picks a sensible default for the field's length.
    let window: u64 = ctx
        .spec
        .args
        .first()
        .and_then(|a| a.parse().ok())
        .unwrap_or_else(|| entropy::pick_window_size(ctx.bytes.len() as u64));
    let window = window.max(1);

    let mut points: Vec<(f64, f64)> = Vec::new();
    let mut offset: usize = 0;
    while offset < ctx.bytes.len() {
        let end = (offset + window as usize).min(ctx.bytes.len());
        let h = entropy::shannon_entropy(&ctx.bytes[offset..end]);
        let mid = offset as f64 + (window as f64) / 2.0;
        points.push((mid, h));
        offset = end;
    }
    if points.is_empty() {
        ui.weak(hxy_i18n::t("visualizer-plot-no-samples"));
        return;
    }
    let line_points: PlotPoints = points.iter().map(|(x, y)| [*x, *y]).collect();
    let color = ui.visuals().widgets.active.fg_stroke.color;
    let max_x = ctx.bytes.len() as f64;
    Plot::new(ctx.ui_id.with("chunk_entropy"))
        .height(ui.available_height() - 4.0)
        .y_axis_label(hxy_i18n::t("visualizer-entropy-y"))
        .x_axis_label(hxy_i18n::t("visualizer-entropy-x"))
        .show(ui, |plot_ui| {
            plot_ui.set_plot_bounds(egui_plot::PlotBounds::from_min_max(
                [0.0, 0.0],
                [max_x.max(1.0), entropy::MAX_ENTROPY],
            ));
            plot_ui.line(Line::new("entropy", line_points).color(color));
        });
}
