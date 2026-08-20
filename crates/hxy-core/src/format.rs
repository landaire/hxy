//! Numeric display formats shared by every frontend: which base an
//! offset / length / value renders in, the per-integer-type format
//! bundle for template field values, and the offset formatter
//! itself.

use serde::Deserialize;
use serde::Serialize;

/// Base used to render a single numeric value (offset, length,
/// end position). The wider [`NumericFormat`] picks one of these
/// per call; this enum is just the leaf format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NumericBase {
    #[default]
    Hex,
    Decimal,
}

impl NumericBase {
    pub fn toggle(self) -> Self {
        match self {
            Self::Hex => Self::Decimal,
            Self::Decimal => Self::Hex,
        }
    }
}

/// How to format byte offsets / lengths / end positions across
/// the UI. `Always(b)` always uses `b`; `Threshold { ... }`
/// switches between `small` (when value < threshold) and `large`
/// (when value >= threshold), so the user can keep small numbers
/// readable in decimal while big addresses stay compact in hex.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NumericFormat {
    Always(NumericBase),
    Threshold { small: NumericBase, large: NumericBase, threshold: u64 },
}

impl Default for NumericFormat {
    fn default() -> Self {
        Self::Always(NumericBase::Hex)
    }
}

impl NumericFormat {
    /// Pick the base that applies to `value`. For `Threshold`,
    /// `large` kicks in at-or-above the threshold so a setting of
    /// `threshold = 256` reads as "show hex once we're past a byte's
    /// worth".
    pub fn pick(self, value: u64) -> NumericBase {
        match self {
            Self::Always(b) => b,
            Self::Threshold { small, large, threshold } => {
                if value >= threshold {
                    large
                } else {
                    small
                }
            }
        }
    }

    /// Quick toggle for the click-to-flip status-bar widget. For
    /// `Always(b)`, swap to the other base; for `Threshold`, swap
    /// the two bases (keeping the threshold intact) so the user
    /// gets the inverted view without losing their threshold pick.
    pub fn toggle(self) -> Self {
        match self {
            Self::Always(b) => Self::Always(b.toggle()),
            Self::Threshold { small, large, threshold } => Self::Threshold { small: large, large: small, threshold },
        }
    }
}

/// Type alias kept for the existing call sites that only care
/// about a single base (the status bar's hover tooltip, the
/// click-to-toggle helper). Lets us drop in [`NumericFormat`]
/// without churning every call to `OffsetBase::toggle()`.
pub type OffsetBase = NumericBase;

/// Per-integer-type formats for template scalar field values.
/// The shape is one [`NumericFormat`] per signed / unsigned
/// width so the user can keep, say, `u8` in hex (single bytes
/// often read as flags / magic) while having `u32` in decimal
/// (counters, lengths). Each slot still defers to a template's
/// explicit `[[hex]]` / `[[decimal]]` hint when set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateValueFormats {
    #[serde(default = "default_unsigned_format")]
    pub u8: NumericFormat,
    #[serde(default = "default_unsigned_format")]
    pub u16: NumericFormat,
    #[serde(default = "default_unsigned_format")]
    pub u32: NumericFormat,
    #[serde(default = "default_unsigned_format")]
    pub u64: NumericFormat,
    #[serde(default = "default_signed_format")]
    pub s8: NumericFormat,
    #[serde(default = "default_signed_format")]
    pub s16: NumericFormat,
    #[serde(default = "default_signed_format")]
    pub s32: NumericFormat,
    #[serde(default = "default_signed_format")]
    pub s64: NumericFormat,
}

impl Default for TemplateValueFormats {
    fn default() -> Self {
        Self {
            u8: default_unsigned_format(),
            u16: default_unsigned_format(),
            u32: default_unsigned_format(),
            u64: default_unsigned_format(),
            s8: default_signed_format(),
            s16: default_signed_format(),
            s32: default_signed_format(),
            s64: default_signed_format(),
        }
    }
}

/// Identity for the eight integer slots in
/// [`TemplateValueFormats`]. Used by the settings UI to walk the
/// slots in a single loop, and by the panel formatters to look up
/// the right slot per `Value::*Val` arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntValueType {
    U8,
    U16,
    U32,
    U64,
    S8,
    S16,
    S32,
    S64,
}

impl IntValueType {
    /// All eight variants in display order (unsigned widths
    /// first, then signed widths). The settings panel iterates
    /// this; the panel formatters use [`Self::format_for`].
    pub fn all() -> &'static [Self] {
        &[Self::U8, Self::U16, Self::U32, Self::U64, Self::S8, Self::S16, Self::S32, Self::S64]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::S8 => "s8",
            Self::S16 => "s16",
            Self::S32 => "s32",
            Self::S64 => "s64",
        }
    }
}

impl TemplateValueFormats {
    /// Borrow the [`NumericFormat`] for one int slot.
    pub fn slot(&self, ty: IntValueType) -> NumericFormat {
        match ty {
            IntValueType::U8 => self.u8,
            IntValueType::U16 => self.u16,
            IntValueType::U32 => self.u32,
            IntValueType::U64 => self.u64,
            IntValueType::S8 => self.s8,
            IntValueType::S16 => self.s16,
            IntValueType::S32 => self.s32,
            IntValueType::S64 => self.s64,
        }
    }

    /// Mutable handle to one slot, for the settings UI to bind
    /// directly into.
    pub fn slot_mut(&mut self, ty: IntValueType) -> &mut NumericFormat {
        match ty {
            IntValueType::U8 => &mut self.u8,
            IntValueType::U16 => &mut self.u16,
            IntValueType::U32 => &mut self.u32,
            IntValueType::U64 => &mut self.u64,
            IntValueType::S8 => &mut self.s8,
            IntValueType::S16 => &mut self.s16,
            IntValueType::S32 => &mut self.s32,
            IntValueType::S64 => &mut self.s64,
        }
    }
}

/// Unsigned widths typically read as hex by default -- they're
/// the natural form for byte values, magic numbers, flags, and
/// raw addresses. Users who want decimal can flip individual
/// widths in the per-type settings.
fn default_unsigned_format() -> NumericFormat {
    NumericFormat::Always(NumericBase::Hex)
}

/// Signed widths read as decimal -- hex of a negative bit
/// pattern (`-1_i32` rendering as `0xFFFFFFFF`) is a
/// developer-debug view, not a default.
fn default_signed_format() -> NumericFormat {
    NumericFormat::Always(NumericBase::Decimal)
}

pub fn format_offset(value: u64, base: OffsetBase) -> String {
    match base {
        NumericBase::Hex => format!("0x{value:X}"),
        NumericBase::Decimal => format!("{value}"),
    }
}
