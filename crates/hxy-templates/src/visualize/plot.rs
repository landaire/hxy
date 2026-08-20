//! Sample slicing for the numeric plot visualizers (`line_plot`,
//! `bar_chart`, `scatter_plot`).
//!
//! The `format` arg selects how to slice the field's bytes into
//! numbers (`u8`, `u16le`, `u16be`, `u32le`, `u32be`, `u64le`,
//! `u64be`, `f32le`, `f32be`, `f64le`, `f64be`). Default is `u8`.

#[derive(Clone, Copy, Debug)]
pub enum Sample {
    U8,
    U16Le,
    U16Be,
    U32Le,
    U32Be,
    U64Le,
    U64Be,
    F32Le,
    F32Be,
    F64Le,
    F64Be,
}

impl Sample {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "u8" | "byte" | "uint8" => Self::U8,
            "u16le" | "u16" | "uint16" | "uint16le" => Self::U16Le,
            "u16be" | "uint16be" => Self::U16Be,
            "u32le" | "u32" | "uint32" | "uint32le" => Self::U32Le,
            "u32be" | "uint32be" => Self::U32Be,
            "u64le" | "u64" | "uint64" | "uint64le" => Self::U64Le,
            "u64be" | "uint64be" => Self::U64Be,
            "f32le" | "f32" | "float" | "float32" => Self::F32Le,
            "f32be" | "float32be" => Self::F32Be,
            "f64le" | "f64" | "double" | "float64" => Self::F64Le,
            "f64be" | "float64be" => Self::F64Be,
            _ => return None,
        })
    }

    pub fn width(&self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16Le | Self::U16Be => 2,
            Self::U32Le | Self::U32Be | Self::F32Le | Self::F32Be => 4,
            Self::U64Le | Self::U64Be | Self::F64Le | Self::F64Be => 8,
        }
    }

    /// Read one sample off the front of `b`. Callers hand in slices
    /// of at least [`Self::width`] bytes ([`samples`] does).
    pub fn read(&self, b: &[u8]) -> f64 {
        match self {
            Self::U8 => b[0] as f64,
            Self::U16Le => u16::from_le_bytes([b[0], b[1]]) as f64,
            Self::U16Be => u16::from_be_bytes([b[0], b[1]]) as f64,
            Self::U32Le => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            Self::U32Be => u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
            Self::U64Le => u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f64,
            Self::U64Be => u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f64,
            Self::F32Le => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            Self::F32Be => f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
            Self::F64Le => f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            Self::F64Be => f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
        }
    }
}

/// First visualizer arg as a sample format; unset or unrecognized
/// falls back to `u8` (the documented default).
pub fn parse_sample(args: &[String]) -> Sample {
    args.first().and_then(|a| Sample::parse(a)).unwrap_or(Sample::U8)
}

pub fn samples(bytes: &[u8], sample: Sample) -> Vec<f64> {
    bytes.chunks_exact(sample.width()).map(|c| sample.read(c)).collect()
}
