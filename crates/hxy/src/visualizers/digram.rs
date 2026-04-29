//! `[[hex::visualize("digram")]]`: render a 256x256 heatmap of
//! consecutive byte pairs. Each (b[i], b[i+1]) increments cell
//! `(b[i], b[i+1])`; the resulting texture surfaces structure that
//! a 1D byte distribution misses (e.g. UTF-8 bytes pile along
//! diagonals; uniform random fills the whole square evenly).

use super::VisualizerCache;
use super::VisualizerContext;

const SIDE: usize = 256;

#[derive(Default)]
pub struct DigramCache {
    pub fingerprint: Option<[u8; 32]>,
    pub texture: Option<egui::TextureHandle>,
}

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext, cache: &mut VisualizerCache) {
    let cache = cache.digram.get_or_insert_with(DigramCache::default);
    let fingerprint = *blake3::hash(ctx.bytes).as_bytes();
    if cache.fingerprint != Some(fingerprint) {
        cache.fingerprint = Some(fingerprint);
        cache.texture = build_texture(ui, ctx);
    }
    let Some(tex) = &cache.texture else {
        ui.weak(hxy_i18n::t("visualizer-digram-empty"));
        return;
    };
    ui.label(
        egui::RichText::new(hxy_i18n::t_args(
            "visualizer-digram-info",
            &[("pairs", &ctx.bytes.len().saturating_sub(1).to_string())],
        ))
        .weak(),
    );
    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        let avail = ui.available_size();
        let side = avail.x.min(avail.y).max(64.0);
        ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(side, side)));
    });
}

fn build_texture(ui: &egui::Ui, ctx: &VisualizerContext) -> Option<egui::TextureHandle> {
    if ctx.bytes.len() < 2 {
        return None;
    }
    let mut counts = vec![0u32; SIDE * SIDE];
    for window in ctx.bytes.windows(2) {
        let idx = (window[0] as usize) * SIDE + window[1] as usize;
        counts[idx] = counts[idx].saturating_add(1);
    }
    let max = *counts.iter().max().unwrap_or(&0).max(&1);
    // Log scaling -- raw counts produce a near-black texture for
    // any non-pathological input because a few cells dominate.
    let scale = (max as f64).ln().max(1.0);
    let mut pixels = Vec::with_capacity(SIDE * SIDE * 4);
    for &c in &counts {
        let v = if c == 0 {
            0
        } else {
            let normalized = ((c as f64).ln() / scale).clamp(0.0, 1.0);
            (normalized * 255.0).round() as u8
        };
        let color = viridis(v);
        pixels.extend_from_slice(&color);
    }
    let img = egui::ColorImage::from_rgba_unmultiplied([SIDE, SIDE], &pixels);
    Some(ui.ctx().load_texture(format!("hxy-visualizer-digram-{:?}", ctx.ui_id), img, egui::TextureOptions::NEAREST))
}

/// Cheap viridis approximation: 5 anchor stops linearly interpolated.
/// Doesn't ship a full LUT but reads close enough that low-count
/// cells are dim and saturated cells are bright yellow.
fn viridis(v: u8) -> [u8; 4] {
    const STOPS: [[u8; 3]; 5] = [
        [0x44, 0x01, 0x54],
        [0x3b, 0x52, 0x8b],
        [0x21, 0x90, 0x8c],
        [0x5e, 0xc9, 0x62],
        [0xfd, 0xe7, 0x25],
    ];
    let segs = STOPS.len() - 1;
    let scaled = (v as f32 / 255.0) * segs as f32;
    let i = scaled.floor() as usize;
    let i = i.min(segs - 1);
    let t = scaled - i as f32;
    let lerp = |a: u8, b: u8| ((a as f32) * (1.0 - t) + (b as f32) * t).round() as u8;
    [lerp(STOPS[i][0], STOPS[i + 1][0]), lerp(STOPS[i][1], STOPS[i + 1][1]), lerp(STOPS[i][2], STOPS[i + 1][2]), 0xff]
}

