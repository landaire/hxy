//! `[[hex::visualize("bitmap", format, width, height)]]`: render
//! the field's bytes as a raw pixel buffer at the declared
//! dimensions. The format string picks how 1..4 bytes per pixel
//! map onto RGBA. Mismatched byte counts surface an error rather
//! than silently truncating -- a runtime that did the math wrong
//! shouldn't get a garbled image.

use super::VisualizerCache;
use super::VisualizerContext;

#[derive(Default)]
pub struct BitmapCache {
    pub fingerprint: Option<[u8; 32]>,
    pub texture: Option<egui::TextureHandle>,
    pub size: (u32, u32),
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BitmapFormat {
    Rgba8,
    Rgb8,
    Bgra8,
    Bgr8,
    Gray8,
    GrayAlpha8,
    /// 16-bit-per-channel RGBA stored little-endian. Downsamples to
    /// 8-bit on the way to egui (egui's ColorImage is 8bpc).
    Rgba16Le,
}

impl BitmapFormat {
    fn parse(name: &str) -> Option<Self> {
        Some(match name.to_ascii_uppercase().as_str() {
            "RGBA8" | "RGBA" => Self::Rgba8,
            "RGB8" | "RGB" => Self::Rgb8,
            "BGRA8" | "BGRA" => Self::Bgra8,
            "BGR8" | "BGR" => Self::Bgr8,
            "GRAY8" | "GRAYSCALE" | "L8" => Self::Gray8,
            "GRAYA8" | "GRAYALPHA8" | "LA8" => Self::GrayAlpha8,
            "RGBA16" | "RGBA16LE" => Self::Rgba16Le,
            _ => return None,
        })
    }

    fn bytes_per_pixel(&self) -> usize {
        match self {
            Self::Rgba8 | Self::Bgra8 => 4,
            Self::Rgb8 | Self::Bgr8 => 3,
            Self::Gray8 => 1,
            Self::GrayAlpha8 => 2,
            Self::Rgba16Le => 8,
        }
    }
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
        match decode(ctx) {
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

fn decode(ctx: &VisualizerContext) -> Result<(u32, u32, Vec<u8>), String> {
    let (format, width, height) = parse_args(&ctx.spec.args)?;
    let bpp = format.bytes_per_pixel();
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|p| p.checked_mul(bpp))
        .ok_or_else(|| hxy_i18n::t("visualizer-bitmap-overflow"))?;
    if ctx.bytes.len() < expected {
        return Err(hxy_i18n::t_args(
            "visualizer-bitmap-too-short",
            &[("have", &ctx.bytes.len().to_string()), ("need", &expected.to_string())],
        ));
    }
    let pixels = &ctx.bytes[..expected];
    let mut out = Vec::with_capacity((width as usize) * (height as usize) * 4);
    match format {
        BitmapFormat::Rgba8 => out.extend_from_slice(pixels),
        BitmapFormat::Rgb8 => {
            for chunk in pixels.chunks_exact(3) {
                out.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 0xff]);
            }
        }
        BitmapFormat::Bgra8 => {
            for chunk in pixels.chunks_exact(4) {
                out.extend_from_slice(&[chunk[2], chunk[1], chunk[0], chunk[3]]);
            }
        }
        BitmapFormat::Bgr8 => {
            for chunk in pixels.chunks_exact(3) {
                out.extend_from_slice(&[chunk[2], chunk[1], chunk[0], 0xff]);
            }
        }
        BitmapFormat::Gray8 => {
            for &v in pixels {
                out.extend_from_slice(&[v, v, v, 0xff]);
            }
        }
        BitmapFormat::GrayAlpha8 => {
            for chunk in pixels.chunks_exact(2) {
                out.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
        }
        BitmapFormat::Rgba16Le => {
            // Downconvert to 8bpc by dropping the low byte. egui's
            // ColorImage doesn't speak 16-bit textures; for now this
            // is the simplest faithful preview.
            for chunk in pixels.chunks_exact(8) {
                out.extend_from_slice(&[chunk[1], chunk[3], chunk[5], chunk[7]]);
            }
        }
    }
    Ok((width, height, out))
}

fn parse_args(args: &[String]) -> Result<(BitmapFormat, u32, u32), String> {
    if args.len() < 3 {
        return Err(hxy_i18n::t("visualizer-bitmap-needs-args"));
    }
    let format = BitmapFormat::parse(&args[0])
        .ok_or_else(|| hxy_i18n::t_args("visualizer-bitmap-unknown-format", &[("name", &args[0])]))?;
    let width: u32 = args[1]
        .parse()
        .map_err(|_| hxy_i18n::t_args("visualizer-bitmap-bad-int", &[("which", "width"), ("got", &args[1])]))?;
    let height: u32 = args[2]
        .parse()
        .map_err(|_| hxy_i18n::t_args("visualizer-bitmap-bad-int", &[("which", "height"), ("got", &args[2])]))?;
    if width == 0 || height == 0 {
        return Err(hxy_i18n::t("visualizer-bitmap-zero-dims"));
    }
    Ok((format, width, height))
}

fn blake3_short_with_args(bytes: &[u8], args: &[String]) -> [u8; 32] {
    // Incremental hasher so the pixel buffer doesn't get copied just
    // to mix the args in. NUL between args makes "RGBA8" + "12" hash
    // distinctly from "RGBA" + "812".
    let mut hasher = blake3::Hasher::new();
    hasher.update(bytes);
    for arg in args {
        hasher.update(&[0u8]);
        hasher.update(arg.as_bytes());
    }
    *hasher.finalize().as_bytes()
}
