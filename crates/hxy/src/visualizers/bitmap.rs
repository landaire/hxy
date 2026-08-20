//! `[[hex::visualize("bitmap", format, width, height)]]`: render
//! the field's bytes as a raw pixel buffer at the declared
//! dimensions. The format string picks how 1..4 bytes per pixel
//! map onto RGBA. Mismatched byte counts surface an error rather
//! than silently truncating -- a runtime that did the math wrong
//! shouldn't get a garbled image.

use hxy_templates::visualize::bitmap::blake3_short_with_args;
use hxy_templates::visualize::bitmap::decode;

use super::VisualizerCache;
use super::VisualizerContext;

#[derive(Default)]
pub struct BitmapCache {
    pub fingerprint: Option<[u8; 32]>,
    pub texture: Option<egui::TextureHandle>,
    pub size: (u32, u32),
    pub error: Option<String>,
}

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext, cache: &mut VisualizerCache) {
    let cache = cache.bitmap.get_or_insert_with(BitmapCache::default);
    let fingerprint = blake3_short_with_args(ctx.bytes, &ctx.spec.args);
    let stale = cache.fingerprint != Some(fingerprint);

    if stale {
        cache.fingerprint = Some(fingerprint);
        cache.texture = None;
        cache.error = None;
        cache.size = (0, 0);
        match decode(ctx.bytes, &ctx.spec.args) {
            Ok((w, h, rgba)) => {
                let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                let texture = ui.ctx().load_texture(
                    format!("hxy-visualizer-bitmap-{:?}", ctx.ui_id),
                    color,
                    egui::TextureOptions::NEAREST,
                );
                cache.texture = Some(texture);
                cache.size = (w, h);
            }
            Err(e) => cache.error = Some(e),
        }
    }

    if let Some(err) = &cache.error {
        ui.colored_label(ui.visuals().error_fg_color, err);
        return;
    }
    let Some(texture) = &cache.texture else {
        ui.weak(hxy_i18n::t("visualizer-bitmap-empty"));
        return;
    };
    let (w, h) = cache.size;
    ui.label(
        egui::RichText::new(hxy_i18n::t_args(
            "visualizer-bitmap-info",
            &[("w", &w.to_string()), ("h", &h.to_string())],
        ))
        .weak(),
    );

    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        let avail = ui.available_size();
        let img_size = egui::vec2(w as f32, h as f32);
        let scale = if avail.x > 0.0 && img_size.x > avail.x { avail.x / img_size.x } else { 1.0 };
        let display = img_size * scale;
        ui.add(egui::Image::new(texture).fit_to_exact_size(display));
    });
}
