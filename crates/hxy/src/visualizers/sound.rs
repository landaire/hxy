//! `[[hex::visualize("sound", channels?, sample_rate?, format?)]]`:
//! render the field's bytes as an audio waveform plot. Playback is
//! out of scope for this milestone -- adding it would require an
//! audio backend (cpal / rodio) and per-platform device handling
//! that doesn't belong in the visualizer panel. The waveform alone
//! still surfaces structure (silence vs. noise vs. tone bursts).

use egui_plot::Line;
use egui_plot::Plot;
use egui_plot::PlotPoints;
use hxy_templates::visualize::sound::SampleFormat;
pub use hxy_templates::visualize::sound::SoundCache;
use hxy_templates::visualize::sound::downsample_for_plot;

use super::VisualizerCache;
use super::VisualizerContext;

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
    Plot::new(ctx.ui_id.with("sound")).height(ui.available_height() - 4.0).show(ui, |plot_ui| {
        plot_ui.line(Line::new("waveform", points));
    });
}
