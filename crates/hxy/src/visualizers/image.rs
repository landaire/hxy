//! `[[hex::visualize("image")]]`: decode the field's bytes as an
//! image (PNG, JPEG, GIF, BMP, TIFF, WebP) and render it. The image
//! crate does the format sniff; we hand the resulting RGBA buffer to
//! egui as a [`ColorImage`] and cache the texture handle keyed by a
//! cheap blake3 of the source bytes so a re-run that produces the
//! same image keeps the same GPU texture.

use super::VisualizerCache;
use super::VisualizerContext;

#[derive(Default)]
pub struct ImageCache {
    pub fingerprint: Option<[u8; 32]>,
    pub texture: Option<egui::TextureHandle>,
    /// Decoded image dimensions in pixels (width, height). Stashed so
    /// the panel header can report the size without re-borrowing the
    /// texture (the allocator's `size()` is in points, not pixels).
    pub size: (u32, u32),
    /// `Some(message)` when the most recent decode failed -- shown
    /// in place of the image so the user sees what's wrong.
    pub error: Option<String>,
}

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext, cache: &mut VisualizerCache) {
    let cache = cache.image.get_or_insert_with(ImageCache::default);
    let fingerprint = *blake3::hash(ctx.bytes).as_bytes();
    let stale = cache.fingerprint != Some(fingerprint);
    if stale {
        cache.fingerprint = Some(fingerprint);
        cache.texture = None;
        cache.error = None;
        cache.size = (0, 0);
        match ::image::load_from_memory(ctx.bytes) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                let pixels = rgba.into_raw();
                let color_image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &pixels);
                let texture = ui.ctx().load_texture(
                    format!("hxy-visualizer-image-{:?}", ctx.ui_id),
                    color_image,
                    egui::TextureOptions::LINEAR,
                );
                cache.texture = Some(texture);
                cache.size = (w, h);
            }
            Err(e) => {
                cache.error = Some(format!("{e}"));
            }
        }
    }

    if let Some(err) = &cache.error {
        ui.colored_label(
            ui.visuals().error_fg_color,
            hxy_i18n::t_args("visualizer-image-decode-failed", &[("error", err)]),
        );
        return;
    }
    let Some(texture) = &cache.texture else {
        ui.weak(hxy_i18n::t("visualizer-image-empty"));
        return;
    };

    let (w, h) = cache.size;
    ui.label(
        egui::RichText::new(hxy_i18n::t_args(
            "visualizer-image-info",
            &[("w", &w.to_string()), ("h", &h.to_string()), ("bytes", &ctx.bytes.len().to_string())],
        ))
        .weak(),
    );

    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        let avail = ui.available_size();
        let img_size = egui::vec2(w as f32, h as f32);
        // Default fit-to-width but let the user scroll for full
        // detail. Aspect-preserving downscale; never upscale beyond
        // the source pixel count (pixel-art friendly).
        let scale = if avail.x > 0.0 && img_size.x > avail.x { avail.x / img_size.x } else { 1.0 };
        let display = img_size * scale;
        ui.add(egui::Image::new(texture).fit_to_exact_size(display));
    });
}
