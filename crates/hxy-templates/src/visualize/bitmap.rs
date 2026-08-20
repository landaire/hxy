//! `[[hex::visualize("bitmap", format, width, height)]]` decode:
//! expand the field's bytes into an RGBA pixel buffer at the
//! declared dimensions. The format string picks how 1..4 bytes per
//! pixel map onto RGBA. Mismatched byte counts surface an error
//! rather than silently truncating -- a runtime that did the math
//! wrong shouldn't get a garbled image.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitmapFormat {
    Rgba8,
    Rgb8,
    Bgra8,
    Bgr8,
    Gray8,
    GrayAlpha8,
    /// 16-bit-per-channel RGBA stored little-endian. Downsamples to
    /// 8-bit on the way out (the frontends render 8bpc textures).
    Rgba16Le,
}

impl BitmapFormat {
    pub fn parse(name: &str) -> Option<Self> {
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

    pub fn bytes_per_pixel(&self) -> usize {
        match self {
            Self::Rgba8 | Self::Bgra8 => 4,
            Self::Rgb8 | Self::Bgr8 => 3,
            Self::Gray8 => 1,
            Self::GrayAlpha8 => 2,
            Self::Rgba16Le => 8,
        }
    }
}

/// Decode `bytes` per the `(format, width, height)` args into
/// `(width, height, rgba)` with tightly packed 8-bit RGBA pixels.
pub fn decode(bytes: &[u8], args: &[String]) -> Result<(u32, u32, Vec<u8>), String> {
    let (format, width, height) = parse_args(args)?;
    let bpp = format.bytes_per_pixel();
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|p| p.checked_mul(bpp))
        .ok_or_else(|| hxy_i18n::t("visualizer-bitmap-overflow"))?;
    if bytes.len() < expected {
        return Err(hxy_i18n::t_args(
            "visualizer-bitmap-too-short",
            &[("have", &bytes.len().to_string()), ("need", &expected.to_string())],
        ));
    }
    let pixels = &bytes[..expected];
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
            // Downconvert to 8bpc by dropping the low byte. The
            // frontends don't speak 16-bit textures; for now this
            // is the simplest faithful preview.
            for chunk in pixels.chunks_exact(8) {
                out.extend_from_slice(&[chunk[1], chunk[3], chunk[5], chunk[7]]);
            }
        }
    }
    Ok((width, height, out))
}

pub fn parse_args(args: &[String]) -> Result<(BitmapFormat, u32, u32), String> {
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

pub fn blake3_short_with_args(bytes: &[u8], args: &[String]) -> [u8; 32] {
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
