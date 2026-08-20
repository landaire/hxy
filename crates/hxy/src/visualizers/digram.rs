//! `[[hex::visualize("digram")]]`: render a 256x256 heatmap of
//! consecutive byte pairs. Each (b[i], b[i+1]) increments cell
//! `(b[i], b[i+1])`; the resulting texture surfaces structure that
//! a 1D byte distribution misses (e.g. UTF-8 bytes pile along
//! diagonals; uniform random fills the whole square evenly).

use hxy_templates::visualize::digram::SIDE;
use hxy_templates::visualize::digram::build_grid;

use super::VisualizerCache;
use super::VisualizerContext;

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
    let grid = build_grid(ctx.bytes)?;
    let mut pixels = Vec::with_capacity(grid.len() * 4);
    for p in &grid {
        pixels.extend_from_slice(&[p.r, p.g, p.b, p.a]);
    }
    let img = egui::ColorImage::from_rgba_unmultiplied([SIDE, SIDE], &pixels);
    Some(ui.ctx().load_texture(format!("hxy-visualizer-digram-{:?}", ctx.ui_id), img, egui::TextureOptions::NEAREST))
}
