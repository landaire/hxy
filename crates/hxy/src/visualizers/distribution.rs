//! `[[hex::visualize("layered_distribution")]]`: render a heatmap
//! showing how byte values are distributed across the field. The X
//! axis walks position (chunked into ~256 columns); the Y axis
//! enumerates the 256 byte values; cell intensity is the count of
//! that byte value in that chunk. Useful for spotting "blobs of one
//! byte value" sandwiched in otherwise diverse data.

use super::VisualizerCache;
use super::VisualizerContext;

const HEIGHT: usize = 256;
const TARGET_COLS: usize = 256;

#[derive(Default)]
pub struct DistributionCache {
    pub fingerprint: Option<[u8; 32]>,
    pub texture: Option<egui::TextureHandle>,
    pub width: usize,
}

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext, cache: &mut VisualizerCache) {
    let cache = cache.distribution.get_or_insert_with(DistributionCache::default);
    let fingerprint = *blake3::hash(ctx.bytes).as_bytes();
    if cache.fingerprint != Some(fingerprint) {
        cache.fingerprint = Some(fingerprint);
        let (texture, width) = build_texture(ui, ctx);
        cache.texture = texture;
        cache.width = width;
    }
    let Some(tex) = &cache.texture else {
        ui.weak(hxy_i18n::t("visualizer-distribution-empty"));
        return;
    };
    ui.label(
        egui::RichText::new(hxy_i18n::t_args(
            "visualizer-distribution-info",
            &[("bytes", &ctx.bytes.len().to_string()), ("cols", &cache.width.to_string())],
        ))
        .weak(),
    );
    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        let avail = ui.available_size();
        let aspect = cache.width as f32 / HEIGHT as f32;
        let width = avail.x.min(avail.y * aspect).max(64.0);
        let height = width / aspect;
        ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(width, height)));
    });
}

fn build_texture(ui: &egui::Ui, ctx: &VisualizerContext) -> (Option<egui::TextureHandle>, usize) {
    if ctx.bytes.is_empty() {
        return (None, 0);
    }
    let cols = ctx.bytes.len().min(TARGET_COLS).max(1);
    let chunk = ctx.bytes.len().div_ceil(cols);
    let actual_cols = ctx.bytes.len().div_ceil(chunk);
    let mut grid = vec![0u32; actual_cols * HEIGHT];
    let mut max_count = 1u32;
    for (col, slice) in ctx.bytes.chunks(chunk).enumerate() {
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
    let mut pixels = Vec::with_capacity(actual_cols * HEIGHT * 4);
    for &c in &grid {
        let v = if c == 0 {
            0
        } else {
            ((c as f32).ln() / scale * 255.0).clamp(0.0, 255.0).round() as u8
        };
        pixels.extend_from_slice(&heat(v));
    }
    let img = egui::ColorImage::from_rgba_unmultiplied([actual_cols, HEIGHT], &pixels);
    let tex = ui.ctx().load_texture(
        format!("hxy-visualizer-distribution-{:?}", ctx.ui_id),
        img,
        egui::TextureOptions::NEAREST,
    );
    (Some(tex), actual_cols)
}

fn heat(v: u8) -> [u8; 4] {
    // Inferno-ish: black -> red -> yellow -> white. Three-stop lerp.
    const STOPS: [[u8; 3]; 4] = [[0, 0, 0], [0xb0, 0x10, 0x10], [0xf6, 0xb0, 0x10], [0xff, 0xff, 0xe5]];
    let segs = STOPS.len() - 1;
    let scaled = (v as f32 / 255.0) * segs as f32;
    let i = (scaled.floor() as usize).min(segs - 1);
    let t = scaled - i as f32;
    let lerp = |a: u8, b: u8| ((a as f32) * (1.0 - t) + (b as f32) * t).round() as u8;
    [lerp(STOPS[i][0], STOPS[i + 1][0]), lerp(STOPS[i][1], STOPS[i + 1][1]), lerp(STOPS[i][2], STOPS[i + 1][2]), 0xff]
}

