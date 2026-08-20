//! Framework-neutral color newtype shared by the frontends.
//!
//! [`Rgba`] stores the exact same four bytes as egui's
//! `Color32` (premultiplied sRGBA) and serializes to the same
//! `[r, g, b, a]` wire form, so persisted color data round-trips
//! unchanged between the egui app and egui-free crates.

use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;

/// Premultiplied sRGBA color, byte-for-byte compatible with egui's
/// `Color32` storage and serde format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);

    /// Opaque color from sRGB components.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Raw premultiplied components, stored as-is.
    // Named for symmetry with `rgb`, mirroring the Color32 API.
    #[allow(clippy::self_named_constructors)]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// From unmultiplied sRGBA components. Mirrors
    /// `Color32::from_rgba_unmultiplied`: each color channel is
    /// scaled by `a / 255` (round-to-nearest) into premultiplied
    /// storage.
    pub fn from_rgba_unmultiplied(r: u8, g: u8, b: u8, a: u8) -> Self {
        match a {
            0 => Self::TRANSPARENT,
            255 => Self::rgb(r, g, b),
            a => {
                let mul = |v: u8| fast_round(v as f32 * (a as f32 / 255.0));
                Self::rgba(mul(r), mul(g), mul(b), a)
            }
        }
    }

    /// From a packed `0xAARRGGBB` value with unmultiplied alpha (the
    /// shape template runtimes use for byte palettes).
    pub fn from_argb_u32(v: u32) -> Self {
        let a = (v >> 24) as u8;
        let r = (v >> 16) as u8;
        let g = (v >> 8) as u8;
        let b = v as u8;
        Self::from_rgba_unmultiplied(r, g, b, a)
    }

    /// Opaque color from HSV (all components in `0..=1`, `rgb` in
    /// linear space before the gamma encode). Mirrors ecolor's
    /// `Color32::from(Hsva::new(h, s, v, 1.0))` bit-for-bit so the
    /// hue-cycle fallback colors match the egui app's.
    pub fn from_hsv(h: f32, s: f32, v: f32) -> Self {
        let [r, g, b] = rgb_from_hsv(h, s, v);
        Self::rgb(gamma_u8_from_linear_f32(r), gamma_u8_from_linear_f32(g), gamma_u8_from_linear_f32(b))
    }
}

/// Serialized as the `[r, g, b, a]` byte array `Color32` uses, so
/// existing persisted JSON deserializes unchanged.
impl Serialize for Rgba {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        [self.r, self.g, self.b, self.a].serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Rgba {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let [r, g, b, a] = <[u8; 4]>::deserialize(deserializer)?;
        Ok(Self { r, g, b, a })
    }
}

/// `(x + 0.5) as u8` -- the saturating round ecolor uses.
fn fast_round(x: f32) -> u8 {
    (x + 0.5) as u8
}

/// Linear 0-1 -> sRGB gamma byte. Same piecewise curve and
/// rounding as ecolor's `gamma_u8_from_linear_f32`.
fn gamma_u8_from_linear_f32(l: f32) -> u8 {
    if l <= 0.0 {
        0
    } else if l <= 0.0031308 {
        fast_round(3294.6 * l)
    } else if l <= 1.0 {
        fast_round(269.025 * l.powf(1.0 / 2.4) - 14.025)
    } else {
        255
    }
}

/// HSV -> linear RGB, all in 0-1. Same math as ecolor's
/// `rgb_from_hsv`.
fn rgb_from_hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h = (h.fract() + 1.0).fract();
    let s = s.clamp(0.0, 1.0);

    let f = h * 6.0 - (h * 6.0).floor();
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);

    match (h * 6.0).floor() as i32 % 6 {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        5 => [v, p, q],
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_wire_format_matches_color32() {
        // Color32 serializes its internal [r, g, b, a] array; the
        // literal here is what egui 0.36 produces for
        // Color32::from_rgba_premultiplied(1, 2, 3, 4).
        let c = Rgba::rgba(1, 2, 3, 4);
        assert_eq!(serde_json::to_string(&c).unwrap(), "[1,2,3,4]");
        let back: Rgba = serde_json::from_str("[1,2,3,4]").unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn unmultiplied_alpha_edges() {
        assert_eq!(Rgba::from_rgba_unmultiplied(10, 20, 30, 0), Rgba::TRANSPARENT);
        assert_eq!(Rgba::from_rgba_unmultiplied(10, 20, 30, 255), Rgba::rgb(10, 20, 30));
        // a = 128: channels scale by 128/255 with round-to-nearest.
        assert_eq!(Rgba::from_rgba_unmultiplied(255, 0, 100, 128), Rgba::rgba(128, 0, 50, 128));
    }

    #[test]
    fn argb_u32_unpacks_aarrggbb() {
        assert_eq!(Rgba::from_argb_u32(0xFF17BECF), Rgba::rgb(0x17, 0xBE, 0xCF));
        assert_eq!(Rgba::from_argb_u32(0x00000000), Rgba::TRANSPARENT);
    }
}
