//! `[[hex::visualize("layered_distribution")]]`: render a heatmap
//! showing how byte values are distributed across the field. The X
//! axis walks position (chunked into ~256 columns); the Y axis
//! enumerates the 256 byte values; cell intensity is the count of
//! that byte value in that chunk. Useful for spotting "blobs of one
//! byte value" sandwiched in otherwise diverse data.

use hxy_templates::visualize::distribution::HEIGHT;
use hxy_templates::visualize::distribution::build_grid;

use super::VisualizerCache;
use super::VisualizerContext;

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
    let Some((actual_cols, grid)) = build_grid(ctx.bytes) else {
        return (None, 0);
    };
    let mut pixels = Vec::with_capacity(grid.len() * 4);
    for p in &grid {
        pixels.extend_from_slice(&[p.r, p.g, p.b, p.a]);
    }
    let img = egui::ColorImage::from_rgba_unmultiplied([actual_cols, HEIGHT], &pixels);
    let tex = ui.ctx().load_texture(
        format!("hxy-visualizer-distribution-{:?}", ctx.ui_id),
        img,
        egui::TextureOptions::NEAREST,
    );
    (Some(tex), actual_cols)
}
