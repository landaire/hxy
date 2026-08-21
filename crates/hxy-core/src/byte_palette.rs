//! Byte-class and byte-value palettes shared by the frontends.
//!
//! The color tables, gradient parameters, and helper functions here
//! are the single source of truth for hex-view byte tinting; the egui
//! and gpui views convert [`Rgba`] to their native color types at the
//! paint boundary.

use crate::color::Rgba;

/// Where the byte-value palette should be applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueHighlight {
    /// Paint the palette as a background fill per byte. Text gets a
    /// contrast-adjusted color so it stays readable over the tint.
    Background,
    /// Tint the hex/ascii glyphs themselves; leave the background alone.
    Text,
}

/// Coarse categorization of a byte value for palette lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteClass {
    Null,
    AllBits,
    Whitespace,
    Printable,
    Control,
    Extended,
}

impl ByteClass {
    pub fn of(byte: u8) -> Self {
        match byte {
            0x00 => Self::Null,
            0xFF => Self::AllBits,
            b'\t' | b'\n' | b'\r' => Self::Whitespace,
            0x01..=0x1F | 0x7F => Self::Control,
            0x20..=0x7E => Self::Printable,
            0x80..=0xFE => Self::Extended,
        }
    }
}

/// Palette for byte-class tinting. Each variant of [`ByteClass`] maps
/// to one color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BytePalette {
    pub null: Rgba,
    pub all_bits: Rgba,
    pub whitespace: Rgba,
    pub printable: Rgba,
    pub control: Rgba,
    pub extended: Rgba,
}

impl BytePalette {
    /// Pick a palette variant appropriate for the theme and highlight
    /// mode. Background mode uses muted semi-transparent tints;
    /// text mode uses saturated opaque colors readable against the theme
    /// background.
    pub fn for_theme_and_mode(dark: bool, mode: ValueHighlight) -> Self {
        match (dark, mode) {
            (true, ValueHighlight::Background) => Self::BG_DARK,
            (false, ValueHighlight::Background) => Self::BG_LIGHT,
            (true, ValueHighlight::Text) => Self::TEXT_DARK,
            (false, ValueHighlight::Text) => Self::TEXT_LIGHT,
        }
    }

    // Background tints are *meant* to be muted -- they lay underneath
    // the byte glyphs and want to suggest the byte class without
    // grabbing the eye on its own. Picked so a wall of 0xFF settles
    // instead of glares on a dark theme, and so saturated reds and
    // purples don't vibrate against the surface.
    pub const BG_DARK: Self = Self {
        null: Rgba::rgb(60, 60, 64),
        all_bits: Rgba::rgb(140, 110, 42),
        whitespace: Rgba::rgb(46, 78, 120),
        printable: Rgba::rgb(46, 104, 64),
        control: Rgba::rgb(122, 62, 62),
        extended: Rgba::rgb(102, 60, 122),
    };

    pub const BG_LIGHT: Self = Self {
        null: Rgba::rgb(220, 220, 220),
        all_bits: Rgba::rgb(245, 215, 110),
        whitespace: Rgba::rgb(180, 210, 240),
        printable: Rgba::rgb(190, 235, 200),
        control: Rgba::rgb(240, 190, 190),
        extended: Rgba::rgb(225, 195, 240),
    };

    // Text-mode colors have to read against the dark surface but
    // shouldn't burn the eye in long bytes blobs at night. Each of
    // these caps perceived luminance around 145-160, keeps the hue
    // separation between classes, and stays visibly de-saturated.
    pub const TEXT_DARK: Self = Self {
        null: Rgba::rgb(144, 144, 148),
        all_bits: Rgba::rgb(214, 174, 96),
        whitespace: Rgba::rgb(128, 176, 218),
        printable: Rgba::rgb(136, 200, 152),
        control: Rgba::rgb(218, 138, 138),
        extended: Rgba::rgb(188, 149, 210),
    };

    pub const TEXT_LIGHT: Self = Self {
        null: Rgba::rgb(120, 120, 120),
        all_bits: Rgba::rgb(180, 120, 20),
        whitespace: Rgba::rgb(30, 90, 180),
        printable: Rgba::rgb(30, 130, 60),
        control: Rgba::rgb(180, 50, 50),
        extended: Rgba::rgb(130, 40, 170),
    };

    pub fn color_for(&self, byte: u8) -> Rgba {
        match ByteClass::of(byte) {
            ByteClass::Null => self.null,
            ByteClass::AllBits => self.all_bits,
            ByteClass::Whitespace => self.whitespace,
            ByteClass::Printable => self.printable,
            ByteClass::Control => self.control,
            ByteClass::Extended => self.extended,
        }
    }
}

/// Every byte value gets a unique color from a fixed HSL hue wheel.
/// Saturation and lightness are tuned per theme/mode so the resulting
/// colors stay readable under the view's fixed text contrast rules.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ValueGradient {
    pub saturation: f32,
    pub lightness: f32,
}

impl ValueGradient {
    pub const BG_DARK: Self = Self { saturation: 0.55, lightness: 0.32 };
    pub const BG_LIGHT: Self = Self { saturation: 0.5, lightness: 0.78 };
    pub const TEXT_DARK: Self = Self { saturation: 0.75, lightness: 0.68 };
    pub const TEXT_LIGHT: Self = Self { saturation: 0.7, lightness: 0.4 };

    pub fn for_theme_and_mode(dark: bool, mode: ValueHighlight) -> Self {
        match (dark, mode) {
            (true, ValueHighlight::Background) => Self::BG_DARK,
            (false, ValueHighlight::Background) => Self::BG_LIGHT,
            (true, ValueHighlight::Text) => Self::TEXT_DARK,
            (false, ValueHighlight::Text) => Self::TEXT_LIGHT,
        }
    }

    pub fn color_for(&self, byte: u8) -> Rgba {
        let hue = (f32::from(byte) / 256.0) * 360.0;
        hsl_to_rgb(hue, self.saturation, self.lightness)
    }
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Rgba {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_norm = h / 60.0;
    let x = c * (1.0 - (h_norm.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match h_norm as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let cv = |v: f32| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgba::rgb(cv(r1), cv(g1), cv(b1))
}

/// Pick a glyph color for text painted on top of `bg`. Brighter
/// backgrounds get a darker grey; darker backgrounds get near-white. The
/// `default_fg` is returned unchanged when `bg` is transparent. `bg` is
/// premultiplied (Color32-style), so the channel luminance already
/// accounts for translucent tints over a dark surface.
pub fn contrast_text_color(bg: Rgba, default_fg: Rgba) -> Rgba {
    if bg.a == 0 {
        return default_fg;
    }
    let luminance = 0.299 * f32::from(bg.r) + 0.587 * f32::from(bg.g) + 0.114 * f32::from(bg.b);
    let t = (luminance / 255.0).clamp(0.0, 1.0);
    let white = 240.0_f32;
    let gray = 30.0_f32;
    let v = (white * (1.0 - t) + gray * t).round() as u8;
    Rgba::rgb(v, v, v)
}

/// Background tint for patched bytes when the user's highlight mode
/// paints glyphs. Saturated red stands out against the default cell
/// fill on both light and dark themes. Premultiplied, like
/// `Color32::from_rgba_premultiplied(0x80, 0x10, 0x10, 0xB0)`.
pub const MODIFIED_BYTE_BG: Rgba = Rgba::rgba(0x80, 0x10, 0x10, 0xB0);
/// Foreground tint for patched bytes when the base highlight already
/// owns the cell fill (background mode or highlighting disabled).
pub const MODIFIED_BYTE_FG: Rgba = Rgba::rgb(0xFF, 0x5A, 0x4A);

/// Uncolored minimap gradient. Byte value 0x00 maps to the theme's
/// darkest content shade and 0xFF to near-white (or the opposite on
/// light mode), giving a faint brightness ramp that still reveals
/// structure without dragging in the palette.
pub fn grayscale_for_byte(byte: u8, dark: bool) -> Rgba {
    let t = f32::from(byte) / 255.0;
    let (lo, hi) = if dark { (40.0, 230.0) } else { (40.0, 220.0) };
    let v = (lo * (1.0 - t) + hi * t).round() as u8;
    Rgba::rgb(v, v, v)
}

/// Gamma multiplier the minimap applies to its cell colors. The hex
/// view paints the same palette color underneath glyphs that occupy a
/// fair chunk of each cell, so the perceived intensity drops; the
/// minimap fills its cells edge-to-edge, so the same RGB would read
/// brighter. Multiplying by this factor makes the two surfaces match
/// perceptually.
pub const MINIMAP_CELL_BLEND: f32 = 0.65;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_class_boundaries() {
        assert_eq!(ByteClass::of(0x00), ByteClass::Null);
        assert_eq!(ByteClass::of(0xFF), ByteClass::AllBits);
        for b in [b'\t', b'\n', b'\r'] {
            assert_eq!(ByteClass::of(b), ByteClass::Whitespace);
        }
        assert_eq!(ByteClass::of(0x01), ByteClass::Control);
        assert_eq!(ByteClass::of(0x1F), ByteClass::Control);
        assert_eq!(ByteClass::of(0x7F), ByteClass::Control);
        assert_eq!(ByteClass::of(0x20), ByteClass::Printable);
        assert_eq!(ByteClass::of(0x7E), ByteClass::Printable);
        assert_eq!(ByteClass::of(0x80), ByteClass::Extended);
        assert_eq!(ByteClass::of(0xFE), ByteClass::Extended);
    }

    #[test]
    fn class_tables_keep_the_egui_values() {
        assert_eq!(BytePalette::BG_DARK.null, Rgba::rgb(60, 60, 64));
        assert_eq!(BytePalette::BG_LIGHT.all_bits, Rgba::rgb(245, 215, 110));
        assert_eq!(BytePalette::TEXT_DARK.whitespace, Rgba::rgb(128, 176, 218));
        assert_eq!(BytePalette::TEXT_LIGHT.extended, Rgba::rgb(130, 40, 170));
        assert_eq!(BytePalette::for_theme_and_mode(true, ValueHighlight::Background), BytePalette::BG_DARK);
        assert_eq!(BytePalette::for_theme_and_mode(false, ValueHighlight::Text), BytePalette::TEXT_LIGHT);
        assert_eq!(BytePalette::BG_DARK.color_for(b'A'), BytePalette::BG_DARK.printable);
    }

    #[test]
    fn value_gradient_walks_the_hue_wheel() {
        assert_eq!(ValueGradient::for_theme_and_mode(true, ValueHighlight::Text), ValueGradient::TEXT_DARK);
        // Byte 0 sits at hue 0 (pure red family for these S/L params).
        let g = ValueGradient::TEXT_DARK;
        let red = g.color_for(0);
        assert!(red.r > red.g && red.r > red.b, "hue 0 leans red: {red:?}");
        // Half way around the wheel lands on the cyan side.
        let cyan = g.color_for(128);
        assert!(cyan.b >= cyan.r && cyan.g > cyan.r, "hue 180 leans cyan: {cyan:?}");
        assert_ne!(g.color_for(10), g.color_for(11), "every byte value gets its own color");
    }

    #[test]
    fn contrast_text_color_flips_on_luminance() {
        let default_fg = Rgba::rgb(1, 2, 3);
        assert_eq!(contrast_text_color(Rgba::TRANSPARENT, default_fg), default_fg);
        // Dark background -> near-white glyph.
        assert_eq!(contrast_text_color(Rgba::rgb(0, 0, 0), default_fg), Rgba::rgb(240, 240, 240));
        // Bright background -> dark grey glyph.
        assert_eq!(contrast_text_color(Rgba::rgb(255, 255, 255), default_fg), Rgba::rgb(30, 30, 30));
    }

    #[test]
    fn grayscale_ramp_endpoints() {
        assert_eq!(grayscale_for_byte(0x00, true), Rgba::rgb(40, 40, 40));
        assert_eq!(grayscale_for_byte(0xFF, true), Rgba::rgb(230, 230, 230));
        assert_eq!(grayscale_for_byte(0xFF, false), Rgba::rgb(220, 220, 220));
    }
}
