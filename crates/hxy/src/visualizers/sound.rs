//! `[[hex::visualize("sound", channels?, sample_rate?, format?)]]`:
//! render the field's bytes as an audio waveform plot. Playback is
//! out of scope for this milestone -- adding it would require an
//! audio backend (cpal / rodio) and per-platform device handling
//! that doesn't belong in the visualizer panel. The waveform alone
//! still surfaces structure (silence vs. noise vs. tone bursts).

use egui_plot::Line;
use egui_plot::Plot;
use egui_plot::PlotPoints;

use super::VisualizerCache;
use super::VisualizerContext;

#[derive(Default)]
pub struct SoundCache {
    pub fingerprint: Option<[u8; 32]>,
    pub samples: Vec<f64>,
    pub channels: u16,
    pub sample_rate: u32,
}

#[derive(Clone, Copy, Debug)]
enum SampleFormat {
    PcmU8,
    PcmS16Le,
    PcmS16Be,
    PcmF32Le,
}

impl SampleFormat {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "u8" | "pcm_u8" => Self::PcmU8,
            "s16" | "s16le" | "pcm_s16le" => Self::PcmS16Le,
            "s16be" | "pcm_s16be" => Self::PcmS16Be,
            "f32" | "f32le" | "pcm_f32le" => Self::PcmF32Le,
            _ => return None,
        })
    }

    fn width(&self) -> usize {
        match self {
            Self::PcmU8 => 1,
            Self::PcmS16Le | Self::PcmS16Be => 2,
            Self::PcmF32Le => 4,
        }
    }

    fn read(&self, b: &[u8]) -> f64 {
        match self {
            Self::PcmU8 => (b[0] as f64 - 128.0) / 128.0,
            Self::PcmS16Le => i16::from_le_bytes([b[0], b[1]]) as f64 / i16::MAX as f64,
            Self::PcmS16Be => i16::from_be_bytes([b[0], b[1]]) as f64 / i16::MAX as f64,
            Self::PcmF32Le => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
        }
    }
}

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext, cache: &mut VisualizerCache) {
    let cache = cache.sound.get_or_insert_with(SoundCache::default);
    let fingerprint = *blake3::hash(ctx.bytes).as_bytes();
    let stale = cache.fingerprint != Some(fingerprint);
    let channels: u16 = ctx.spec.args.first().and_then(|a| a.parse().ok()).unwrap_or(1);
    let sample_rate: u32 = ctx.spec.args.get(1).and_then(|a| a.parse().ok()).unwrap_or(44_100);
    let format = ctx.spec.args.get(2).and_then(|s| SampleFormat::parse(s)).unwrap_or(SampleFormat::PcmS16Le);

    if stale {
        cache.fingerprint = Some(fingerprint);
        cache.channels = channels;
        cache.sample_rate = sample_rate;
        cache.samples = downsample_for_plot(ctx.bytes, format, channels);
    }

    if cache.samples.is_empty() {
        ui.weak(hxy_i18n::t("visualizer-sound-empty"));
        return;
    }
    let duration_secs = cache.samples.len() as f64 / sample_rate.max(1) as f64;
    ui.label(
        egui::RichText::new(hxy_i18n::t_args(
            "visualizer-sound-info",
            &[
                ("ch", &channels.to_string()),
                ("rate", &sample_rate.to_string()),
                ("seconds", &format!("{:.2}", duration_secs)),
            ],
        ))
        .weak(),
    );
    ui.colored_label(ui.visuals().warn_fg_color, hxy_i18n::t("visualizer-sound-no-playback"));

    let points: PlotPoints = cache.samples.iter().enumerate().map(|(i, v)| [i as f64, *v]).collect();
    Plot::new(ctx.ui_id.with("sound"))
        .height(ui.available_height() - 4.0)
        .show(ui, |plot_ui| {
            plot_ui.line(Line::new("waveform", points));
        });
}

fn downsample_for_plot(bytes: &[u8], format: SampleFormat, channels: u16) -> Vec<f64> {
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

