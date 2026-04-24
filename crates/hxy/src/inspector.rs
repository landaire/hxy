//! Data inspector: decodes the bytes at the active tab's caret into
//! a family of datatypes (sized integers, LEB128 varints, floats,
//! common time encodings, color channels) and renders them in a
//! dock tab.
//!
//! User-registered decoders aren't wired yet -- the trait is public
//! so a future plugin-management hook can add new ones without
//! touching this module.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

/// How to read a multi-byte integer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Endian {
    #[default]
    Little,
    Big,
}

/// Radix used when rendering integer decoder output. Floats and
/// time values ignore this.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IntRadix {
    #[default]
    Decimal,
    Hex,
    Binary,
}

/// Output of a [`Decoder`]. `Text` is the default; `Color` carries
/// enough info for the renderer to paint a swatch alongside the
/// label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decoded {
    Text(String),
    Color { rgba: [u8; 4], label: String },
}

impl Decoded {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(s.into())
    }

    /// Accessor for tests / consumers that only want the label form.
    pub fn label(&self) -> &str {
        match self {
            Self::Text(s) => s.as_str(),
            Self::Color { label, .. } => label.as_str(),
        }
    }
}

/// One row in the inspector table. Implementors decode the bytes at
/// the caret into a [`Decoded`] value.
pub trait Decoder: Send + Sync {
    /// Display label (e.g. "UInt32", "FILETIME").
    fn name(&self) -> &str;

    /// Fixed byte width, or `None` for variable-length (LEB128 etc.).
    fn bytes_needed(&self) -> Option<usize>;

    /// Decode the bytes the caller has already read. Returns `None`
    /// if decoding fails (too few bytes, NaN-ish float, etc.) -- the
    /// row renders as "--".
    fn decode(&self, bytes: &[u8], endian: Endian, radix: IntRadix) -> Option<Decoded>;
}

pub struct InspectorState {
    pub endian: Endian,
    pub radix: IntRadix,
    pub show_panel: bool,
}

impl Default for InspectorState {
    fn default() -> Self {
        Self { endian: Endian::Little, radix: IntRadix::Decimal, show_panel: false }
    }
}

pub fn default_decoders() -> Vec<Arc<dyn Decoder>> {
    vec![
        Arc::new(BinaryDecoder),
        Arc::new(FixedInt { name: "Int8", width: 1, signed: true }),
        Arc::new(FixedInt { name: "UInt8", width: 1, signed: false }),
        Arc::new(FixedInt { name: "Int16", width: 2, signed: true }),
        Arc::new(FixedInt { name: "UInt16", width: 2, signed: false }),
        Arc::new(Int24 { signed: true }),
        Arc::new(Int24 { signed: false }),
        Arc::new(FixedInt { name: "Int32", width: 4, signed: true }),
        Arc::new(FixedInt { name: "UInt32", width: 4, signed: false }),
        Arc::new(FixedInt { name: "Int64", width: 8, signed: true }),
        Arc::new(FixedInt { name: "UInt64", width: 8, signed: false }),
        Arc::new(Int128 { signed: true }),
        Arc::new(Int128 { signed: false }),
        Arc::new(LebDecoder { signed: false }),
        Arc::new(LebDecoder { signed: true }),
        Arc::new(FloatDecoder { width: 4 }),
        Arc::new(FloatDecoder { width: 8 }),
        Arc::new(TimeDecoder::UnixSec32),
        Arc::new(TimeDecoder::UnixSec64),
        Arc::new(TimeDecoder::FileTime),
        Arc::new(TimeDecoder::DosDate),
        Arc::new(TimeDecoder::DosTime),
        Arc::new(TimeDecoder::OleTime),
        Arc::new(ColorDecoder::Argb),
        Arc::new(ColorDecoder::Rgba),
    ]
}

struct BinaryDecoder;

impl Decoder for BinaryDecoder {
    fn name(&self) -> &str {
        "Binary (8-bit)"
    }
    fn bytes_needed(&self) -> Option<usize> {
        Some(1)
    }
    fn decode(&self, bytes: &[u8], _endian: Endian, _radix: IntRadix) -> Option<Decoded> {
        let b = *bytes.first()?;
        Some(Decoded::text(format!("{:08b}", b)))
    }
}

struct FixedInt {
    name: &'static str,
    width: usize,
    signed: bool,
}

impl Decoder for FixedInt {
    fn name(&self) -> &str {
        self.name
    }
    fn bytes_needed(&self) -> Option<usize> {
        Some(self.width)
    }
    fn decode(&self, bytes: &[u8], endian: Endian, radix: IntRadix) -> Option<Decoded> {
        if bytes.len() < self.width {
            return None;
        }
        let mut buf = [0u8; 16];
        let dest_start = 16 - self.width;
        for i in 0..self.width {
            let src_byte = match endian {
                Endian::Little => bytes[self.width - 1 - i],
                Endian::Big => bytes[i],
            };
            buf[dest_start + i] = src_byte;
        }
        if self.signed {
            let unsigned = u128::from_be_bytes(buf);
            let sign_bits = 128 - self.width as u32 * 8;
            let shifted = ((unsigned as i128) << sign_bits) >> sign_bits;
            Some(Decoded::text(format_signed(shifted, self.width, radix)))
        } else {
            let unsigned = u128::from_be_bytes(buf);
            Some(Decoded::text(format_unsigned(unsigned, self.width, radix)))
        }
    }
}

/// 3-byte integer (i24 / u24). Not a standard width so it gets its
/// own impl -- handles sign extension and byte-order manually.
struct Int24 {
    signed: bool,
}

impl Decoder for Int24 {
    fn name(&self) -> &str {
        if self.signed { "Int24" } else { "UInt24" }
    }
    fn bytes_needed(&self) -> Option<usize> {
        Some(3)
    }
    fn decode(&self, bytes: &[u8], endian: Endian, radix: IntRadix) -> Option<Decoded> {
        if bytes.len() < 3 {
            return None;
        }
        let (b0, b1, b2) = (bytes[0], bytes[1], bytes[2]);
        let raw: u32 = match endian {
            Endian::Little => u32::from(b0) | (u32::from(b1) << 8) | (u32::from(b2) << 16),
            Endian::Big => u32::from(b2) | (u32::from(b1) << 8) | (u32::from(b0) << 16),
        };
        if self.signed {
            let signed = if raw & 0x80_0000 != 0 { (raw | 0xFF00_0000) as i32 } else { raw as i32 };
            Some(Decoded::text(format_signed(signed as i128, 3, radix)))
        } else {
            Some(Decoded::text(format_unsigned(raw as u128, 3, radix)))
        }
    }
}

/// i128 / u128 -- the 16-byte span the inspector already prefetches.
struct Int128 {
    signed: bool,
}

impl Decoder for Int128 {
    fn name(&self) -> &str {
        if self.signed { "Int128" } else { "UInt128" }
    }
    fn bytes_needed(&self) -> Option<usize> {
        Some(16)
    }
    fn decode(&self, bytes: &[u8], endian: Endian, radix: IntRadix) -> Option<Decoded> {
        if bytes.len() < 16 {
            return None;
        }
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&bytes[..16]);
        let raw = match endian {
            Endian::Little => u128::from_le_bytes(buf),
            Endian::Big => u128::from_be_bytes(buf),
        };
        if self.signed {
            Some(Decoded::text(format_signed(raw as i128, 16, radix)))
        } else {
            Some(Decoded::text(format_unsigned(raw, 16, radix)))
        }
    }
}

/// LEB128 / ULEB128 varint.
struct LebDecoder {
    signed: bool,
}

impl Decoder for LebDecoder {
    fn name(&self) -> &str {
        if self.signed { "LEB128" } else { "ULEB128" }
    }
    fn bytes_needed(&self) -> Option<usize> {
        None
    }
    fn decode(&self, bytes: &[u8], _endian: Endian, radix: IntRadix) -> Option<Decoded> {
        let mut result: u128 = 0;
        let mut shift = 0u32;
        let mut last: u8 = 0;
        let mut consumed = 0usize;
        for b in bytes.iter().take(19) {
            consumed += 1;
            last = *b;
            result |= u128::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 128 {
                return None;
            }
        }
        if last & 0x80 != 0 {
            return None;
        }
        let width = consumed;
        if self.signed {
            let mut signed = result as i128;
            if shift + 7 < 128 && (last & 0x40 != 0) {
                signed |= !0i128 << (shift + 7);
            }
            Some(Decoded::text(format!("{} ({} bytes)", format_signed(signed, width, radix), width)))
        } else {
            Some(Decoded::text(format!("{} ({} bytes)", format_unsigned(result, width, radix), width)))
        }
    }
}

struct FloatDecoder {
    width: usize,
}

impl Decoder for FloatDecoder {
    fn name(&self) -> &str {
        if self.width == 4 { "Float32" } else { "Float64" }
    }
    fn bytes_needed(&self) -> Option<usize> {
        Some(self.width)
    }
    fn decode(&self, bytes: &[u8], endian: Endian, _radix: IntRadix) -> Option<Decoded> {
        if bytes.len() < self.width {
            return None;
        }
        let text = match self.width {
            4 => {
                let mut a = [0u8; 4];
                a.copy_from_slice(&bytes[..4]);
                let v = match endian {
                    Endian::Little => f32::from_le_bytes(a),
                    Endian::Big => f32::from_be_bytes(a),
                };
                format!("{}", v)
            }
            _ => {
                let mut a = [0u8; 8];
                a.copy_from_slice(&bytes[..8]);
                let v = match endian {
                    Endian::Little => f64::from_le_bytes(a),
                    Endian::Big => f64::from_be_bytes(a),
                };
                format!("{}", v)
            }
        };
        Some(Decoded::text(text))
    }
}

enum TimeDecoder {
    /// 32-bit Unix epoch seconds, signed.
    UnixSec32,
    /// 64-bit Unix epoch seconds, signed.
    UnixSec64,
    /// Windows FILETIME -- 100-ns intervals since 1601-01-01 UTC.
    FileTime,
    /// DOS date -- 16-bit packed year/month/day.
    DosDate,
    /// DOS time -- 16-bit packed hour/min/sec×2.
    DosTime,
    /// OLE automation date -- f64 days since 1899-12-30.
    OleTime,
}

impl Decoder for TimeDecoder {
    fn name(&self) -> &str {
        match self {
            Self::UnixSec32 => "time_t (32)",
            Self::UnixSec64 => "time64_t",
            Self::FileTime => "FILETIME",
            Self::DosDate => "DOSDATE",
            Self::DosTime => "DOSTIME",
            Self::OleTime => "OLETIME",
        }
    }
    fn bytes_needed(&self) -> Option<usize> {
        Some(match self {
            Self::UnixSec32 => 4,
            Self::UnixSec64 | Self::FileTime | Self::OleTime => 8,
            Self::DosDate | Self::DosTime => 2,
        })
    }
    fn decode(&self, bytes: &[u8], endian: Endian, _radix: IntRadix) -> Option<Decoded> {
        match self {
            Self::UnixSec32 => {
                if bytes.len() < 4 {
                    return None;
                }
                let mut a = [0u8; 4];
                a.copy_from_slice(&bytes[..4]);
                let secs = match endian {
                    Endian::Little => i32::from_le_bytes(a),
                    Endian::Big => i32::from_be_bytes(a),
                };
                format_unix_seconds(secs as i64)
            }
            Self::UnixSec64 => {
                if bytes.len() < 8 {
                    return None;
                }
                let mut a = [0u8; 8];
                a.copy_from_slice(&bytes[..8]);
                let secs = match endian {
                    Endian::Little => i64::from_le_bytes(a),
                    Endian::Big => i64::from_be_bytes(a),
                };
                format_unix_seconds(secs)
            }
            Self::FileTime => {
                if bytes.len() < 8 {
                    return None;
                }
                let mut a = [0u8; 8];
                a.copy_from_slice(&bytes[..8]);
                let ticks = match endian {
                    Endian::Little => u64::from_le_bytes(a),
                    Endian::Big => u64::from_be_bytes(a),
                };
                // FILETIME epoch is 1601-01-01 UTC. Unix is 1970-01-01.
                // Delta: 11644473600 seconds.
                let secs = (ticks / 10_000_000) as i128 - 11_644_473_600;
                format_unix_seconds(secs.try_into().ok()?)
            }
            Self::DosDate => {
                if bytes.len() < 2 {
                    return None;
                }
                let raw = match endian {
                    Endian::Little => u16::from_le_bytes([bytes[0], bytes[1]]),
                    Endian::Big => u16::from_be_bytes([bytes[0], bytes[1]]),
                };
                let day = raw & 0x1F;
                let month = (raw >> 5) & 0x0F;
                let year = 1980 + ((raw >> 9) & 0x7F) as i32;
                Some(Decoded::text(format!("{year:04}-{month:02}-{day:02}")))
            }
            Self::DosTime => {
                if bytes.len() < 2 {
                    return None;
                }
                let raw = match endian {
                    Endian::Little => u16::from_le_bytes([bytes[0], bytes[1]]),
                    Endian::Big => u16::from_be_bytes([bytes[0], bytes[1]]),
                };
                let secs = (raw & 0x1F) * 2;
                let mins = (raw >> 5) & 0x3F;
                let hours = (raw >> 11) & 0x1F;
                Some(Decoded::text(format!("{hours:02}:{mins:02}:{secs:02}")))
            }
            Self::OleTime => {
                if bytes.len() < 8 {
                    return None;
                }
                let mut a = [0u8; 8];
                a.copy_from_slice(&bytes[..8]);
                let days = match endian {
                    Endian::Little => f64::from_le_bytes(a),
                    Endian::Big => f64::from_be_bytes(a),
                };
                if !days.is_finite() {
                    return None;
                }
                // OLE epoch: 1899-12-30. Unix epoch: 1970-01-01.
                // Delta: 25569 days.
                let unix_days = days - 25_569.0;
                let unix_secs = (unix_days * 86_400.0) as i64;
                format_unix_seconds(unix_secs)
            }
        }
    }
}

fn format_unix_seconds(secs: i64) -> Option<Decoded> {
    let ts = jiff::Timestamp::from_second(secs).ok()?;
    Some(Decoded::text(ts.to_string()))
}

enum ColorDecoder {
    /// u32 with alpha-first byte order (0xAARRGGBB as read LE/BE).
    Argb,
    /// u32 with alpha-last byte order (0xRRGGBBAA as read LE/BE).
    Rgba,
}

impl Decoder for ColorDecoder {
    fn name(&self) -> &str {
        match self {
            Self::Argb => "ARGB (u32)",
            Self::Rgba => "RGBA (u32)",
        }
    }
    fn bytes_needed(&self) -> Option<usize> {
        Some(4)
    }
    fn decode(&self, bytes: &[u8], endian: Endian, _radix: IntRadix) -> Option<Decoded> {
        if bytes.len() < 4 {
            return None;
        }
        let mut a = [0u8; 4];
        a.copy_from_slice(&bytes[..4]);
        let raw = match endian {
            Endian::Little => u32::from_le_bytes(a),
            Endian::Big => u32::from_be_bytes(a),
        };
        let (ar, rr, gg, bb) = match self {
            Self::Argb => {
                (((raw >> 24) & 0xFF) as u8, ((raw >> 16) & 0xFF) as u8, ((raw >> 8) & 0xFF) as u8, (raw & 0xFF) as u8)
            }
            Self::Rgba => {
                ((raw & 0xFF) as u8, ((raw >> 24) & 0xFF) as u8, ((raw >> 16) & 0xFF) as u8, ((raw >> 8) & 0xFF) as u8)
            }
        };
        let tuple = match self {
            Self::Argb => format!("argb=({ar},{rr},{gg},{bb})"),
            Self::Rgba => format!("rgba=({rr},{gg},{bb},{ar})"),
        };
        let label = format!("#{rr:02X}{gg:02X}{bb:02X}  α={ar} {tuple}");
        Some(Decoded::Color { rgba: [rr, gg, bb, ar], label })
    }
}

fn format_unsigned(value: u128, width: usize, radix: IntRadix) -> String {
    match radix {
        IntRadix::Decimal => format!("{value}"),
        IntRadix::Hex => format!("0x{:0>1$X}", value, width * 2),
        IntRadix::Binary => format!("0b{:0>1$b}", value, width * 8),
    }
}

fn format_signed(value: i128, width: usize, radix: IntRadix) -> String {
    match radix {
        IntRadix::Decimal => format!("{value}"),
        IntRadix::Hex => {
            // Show hex representation of the underlying bit pattern
            // (two's-complement in `width` bytes), not of the abstract
            // integer value -- matches what a reader would see in the
            // raw file.
            let mask = if width >= 16 { u128::MAX } else { (1u128 << (width * 8)) - 1 };
            let raw = (value as u128) & mask;
            format!("0x{:0>1$X}", raw, width * 2)
        }
        IntRadix::Binary => {
            let mask = if width >= 16 { u128::MAX } else { (1u128 << (width * 8)) - 1 };
            let raw = (value as u128) & mask;
            format!("0b{:0>1$b}", raw, width * 8)
        }
    }
}

/// Draw the inspector into `ui`. `caret_offset` is the cursor byte
/// position; `bytes` is a prefetched window of data (typically 16
/// bytes) starting at that offset.
pub fn show(
    ui: &mut egui::Ui,
    state: &mut InspectorState,
    decoders: &[Arc<dyn Decoder>],
    caret_offset: Option<u64>,
    bytes: &[u8],
) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{} Inspector", egui_phosphor::regular::EYE)).strong());
        ui.separator();
        ui.label("Endianness:");
        ui.selectable_value(&mut state.endian, Endian::Little, "Little");
        ui.selectable_value(&mut state.endian, Endian::Big, "Big");
        ui.separator();
        ui.label("Int radix:");
        ui.selectable_value(&mut state.radix, IntRadix::Decimal, "Dec");
        ui.selectable_value(&mut state.radix, IntRadix::Hex, "Hex");
        ui.selectable_value(&mut state.radix, IntRadix::Binary, "Bin");
    });
    ui.separator();

    if caret_offset.is_none() {
        ui.weak("No caret -- click a byte in the hex view.");
        return;
    }
    ui.add_space(4.0);

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("hxy_inspector_grid").num_columns(2).striped(true).min_col_width(100.0).show(ui, |ui| {
            for dec in decoders {
                ui.label(dec.name());
                let decoded = dec.decode(bytes, state.endian, state.radix);
                match decoded {
                    Some(Decoded::Text(s)) => {
                        ui.add(egui::Label::new(egui::RichText::new(&s).monospace()).truncate());
                    }
                    Some(Decoded::Color { rgba, label }) => {
                        ui.horizontal(|ui| {
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                            let fill = egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
                            ui.painter().rect_filled(rect, 3.0, fill);
                            // Thin outline so light colors on a
                            // light background still read as a
                            // distinct swatch.
                            ui.painter().rect_stroke(
                                rect,
                                3.0,
                                egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.fg_stroke.color),
                                egui::StrokeKind::Inside,
                            );
                            ui.add(egui::Label::new(egui::RichText::new(&label).monospace()).truncate());
                        });
                    }
                    None => {
                        ui.weak("--");
                    }
                }
                ui.end_row();
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label(d: &dyn Decoder, bytes: &[u8], endian: Endian) -> Option<String> {
        d.decode(bytes, endian, IntRadix::Decimal).map(|d| d.label().to_owned())
    }

    #[test]
    fn int16_little_endian() {
        let d = FixedInt { name: "Int16", width: 2, signed: true };
        assert_eq!(label(&d, &[0x01, 0xFF], Endian::Little).as_deref(), Some("-255"));
        assert_eq!(label(&d, &[0x01, 0xFF], Endian::Big).as_deref(), Some("511"));
    }

    #[test]
    fn uint16_little_endian() {
        let d = FixedInt { name: "UInt16", width: 2, signed: false };
        // Regression: an earlier version dest-filled the low bytes of
        // a 16-byte big-endian buffer by starting at buf[0] instead of
        // buf[14], so the rendered hex had 32 digits for a 2-byte
        // value.
        assert_eq!(label(&d, &[0x01, 0x80], Endian::Little).as_deref(), Some("32769"));
    }

    #[test]
    fn uint24_le() {
        let d = Int24 { signed: false };
        assert_eq!(label(&d, &[0x01, 0x02, 0x03], Endian::Little).as_deref(), Some("197121"));
    }

    #[test]
    fn int24_sign_extends() {
        let d = Int24 { signed: true };
        assert_eq!(label(&d, &[0xFF, 0xFF, 0xFF], Endian::Little).as_deref(), Some("-1"));
    }

    #[test]
    fn uleb128_basic() {
        let d = LebDecoder { signed: false };
        assert_eq!(label(&d, &[0x7F], Endian::Little).as_deref(), Some("127 (1 bytes)"));
        assert_eq!(label(&d, &[0x80, 0x01], Endian::Little).as_deref(), Some("128 (2 bytes)"));
    }

    #[test]
    fn leb128_negative() {
        let d = LebDecoder { signed: true };
        assert_eq!(label(&d, &[0x7E], Endian::Little).as_deref(), Some("-2 (1 bytes)"));
    }

    #[test]
    fn dos_date_unpacks_year_month_day() {
        let d = TimeDecoder::DosDate;
        let raw: u16 = (46 << 9) | (4 << 5) | 23;
        let bytes = raw.to_le_bytes();
        assert_eq!(label(&d, &bytes, Endian::Little).as_deref(), Some("2026-04-23"));
    }

    #[test]
    fn argb_color_emits_rgba_payload() {
        let d = ColorDecoder::Argb;
        let bytes = 0x80_FF_00_00u32.to_be_bytes();
        let decoded = d.decode(&bytes, Endian::Big, IntRadix::Decimal).unwrap();
        match decoded {
            Decoded::Color { rgba, label } => {
                assert_eq!(rgba, [0xFF, 0x00, 0x00, 0x80]);
                assert!(label.starts_with("#FF0000"), "got {label:?}");
            }
            other => panic!("expected Color, got {other:?}"),
        }
    }
}
