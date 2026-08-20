//! `[[hex::visualize("sound", channels?, sample_rate?, format?)]]`
//! decode: slice the field's bytes as PCM samples and downsample
//! them into a plottable waveform. Playback is out of scope -- the
//! waveform alone still surfaces structure (silence vs. noise vs.
//! tone bursts).

/// Audio waveform downsample, computed once per byte fingerprint.
#[derive(Default)]
pub struct SoundCache {
    pub fingerprint: Option<[u8; 32]>,
    pub samples: Vec<f64>,
    pub channels: u16,
    pub sample_rate: u32,
}

#[derive(Clone, Copy, Debug)]
pub enum SampleFormat {
    PcmU8,
    PcmS16Le,
    PcmS16Be,
    PcmF32Le,
}

impl SampleFormat {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "u8" | "pcm_u8" => Self::PcmU8,
            "s16" | "s16le" | "pcm_s16le" => Self::PcmS16Le,
            "s16be" | "pcm_s16be" => Self::PcmS16Be,
            "f32" | "f32le" | "pcm_f32le" => Self::PcmF32Le,
            _ => return None,
        })
    }

    pub fn width(&self) -> usize {
        match self {
            Self::PcmU8 => 1,
            Self::PcmS16Le | Self::PcmS16Be => 2,
            Self::PcmF32Le => 4,
        }
    }

    /// Read one normalized sample off the front of `b`. Callers hand
    /// in slices of at least [`Self::width`] bytes.
    pub fn read(&self, b: &[u8]) -> f64 {
        match self {
            Self::PcmU8 => (b[0] as f64 - 128.0) / 128.0,
            Self::PcmS16Le => i16::from_le_bytes([b[0], b[1]]) as f64 / i16::MAX as f64,
            Self::PcmS16Be => i16::from_be_bytes([b[0], b[1]]) as f64 / i16::MAX as f64,
            Self::PcmF32Le => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
        }
    }
}

/// Bucketed averaging down to ~4096 points so the plot stays light
/// regardless of field size. Multi-channel input plots channel 0
/// (the stride skips the others).
pub fn downsample_for_plot(bytes: &[u8], format: SampleFormat, channels: u16) -> Vec<f64> {
    const TARGET: usize = 4096;
    let stride = format.width() * channels.max(1) as usize;
    let total = bytes.len() / stride;
    if total == 0 {
        return Vec::new();
    }
    let bucket = total.div_ceil(TARGET).max(1);
    let mut out = Vec::with_capacity(total.div_ceil(bucket));
    let mut i = 0;
    while i < total {
        let mut sum = 0.0f64;
        let mut count = 0;
        for j in 0..bucket {
            let idx = i + j;
            if idx >= total {
                break;
            }
            let off = idx * stride;
            sum += format.read(&bytes[off..off + format.width()]);
            count += 1;
        }
        out.push(sum / count.max(1) as f64);
        i += bucket;
    }
    out
}
