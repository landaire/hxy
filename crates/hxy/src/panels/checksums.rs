//! Checksum tool.
//!
//! Streaming hash computation moved to `hxy_panels::checksums`
//! (framework-agnostic, shared with the GPUI port); re-exported here
//! under the original path. Worker dispatch and rendering stay here
//! -- they touch `FileId`, `egui_inbox`, and the background pool.

use std::sync::Arc;

use hxy_core::HexSource;
pub use hxy_panels::checksums::Algorithm;
pub use hxy_panels::checksums::ChecksumConfig;
pub use hxy_panels::checksums::ChecksumResult;
pub use hxy_panels::checksums::compute;

use crate::files::FileId;

#[derive(Clone, Debug)]
pub enum ChecksumOutcome {
    Ok(ChecksumResult),
    Err(String),
}

pub struct ChecksumComputation {
    pub inbox: egui_inbox::UiInbox<ChecksumOutcome>,
    pub file_id: FileId,
    pub started: web_time::Instant,
}

#[derive(Default)]
pub struct ChecksumsPanel {
    pub config: ChecksumConfig,
    pub last_result: Option<ChecksumResult>,
    pub running: Option<ChecksumComputation>,
}

/// Spin up a checksum worker on the shared background pool.
pub fn spawn_compute(
    ctx: &egui::Context,
    id: FileId,
    source: Arc<dyn HexSource>,
    config: ChecksumConfig,
) -> ChecksumComputation {
    let (sender, inbox) = egui_inbox::UiInbox::channel_with_ctx(ctx);
    let started = web_time::Instant::now();
    crate::background::submit(move || {
        let outcome = match compute(&*source, &config) {
            Ok(result) => ChecksumOutcome::Ok(result),
            Err(e) => ChecksumOutcome::Err(e),
        };
        let _ = sender.send(outcome);
    });
    ChecksumComputation { inbox, file_id: id, started }
}

#[derive(Clone, Debug)]
pub enum ChecksumsEvent {
    /// User pressed the "Run" button. Caller re-runs against the
    /// current panel config.
    Run,
    /// Copy `text` to the clipboard. The host owns clipboard
    /// access; the panel just emits the request.
    Copy(String),
}

/// Render the per-file Checksums panel without virtual addressing
/// (range labels show raw file offsets).
pub fn show(ui: &mut egui::Ui, file_label: Option<&str>, panel: &mut ChecksumsPanel) -> Vec<ChecksumsEvent> {
    show_inner(ui, file_label, panel, None)
}

/// Render the per-file Checksums panel with virtual addressing
/// applied to the range label.
pub fn show_with_vaddr(
    ui: &mut egui::Ui,
    file_label: Option<&str>,
    panel: &mut ChecksumsPanel,
    virtual_base: u64,
) -> Vec<ChecksumsEvent> {
    show_inner(ui, file_label, panel, Some(virtual_base))
}

fn show_inner(
    ui: &mut egui::Ui,
    file_label: Option<&str>,
    panel: &mut ChecksumsPanel,
    virtual_base: Option<u64>,
) -> Vec<ChecksumsEvent> {
    let mut events: Vec<ChecksumsEvent> = Vec::new();
    ui.horizontal(|ui| {
        ui.heading(hxy_i18n::t("checksums-heading"));
        ui.add_space(8.0);
        let label = file_label.unwrap_or("");
        ui.label(egui::RichText::new(label).weak());
    });
    ui.separator();

    if file_label.is_none() {
        ui.label(hxy_i18n::t("checksums-no-active-file"));
        return events;
    }

    let running = panel.running.is_some();

    ui.horizontal_wrapped(|ui| {
        for alg in Algorithm::ALL {
            let mut enabled = panel.config.algorithms.contains(&alg);
            if ui.checkbox(&mut enabled, alg.label()).changed() {
                if enabled {
                    panel.config.algorithms.insert(alg);
                } else {
                    panel.config.algorithms.remove(&alg);
                }
            }
        }
    });

    ui.horizontal(|ui| {
        let run_label = if running { hxy_i18n::t("checksums-running") } else { hxy_i18n::t("checksums-run") };
        let run_button = egui::Button::new(run_label);
        let can_run = !running && !panel.config.algorithms.is_empty();
        if ui.add_enabled(can_run, run_button).clicked() {
            events.push(ChecksumsEvent::Run);
        }
        if !panel.config.algorithms.is_empty()
            && let Some(result) = panel.last_result.as_ref()
        {
            let copy_button = egui::Button::new(hxy_i18n::t("checksums-copy-all"));
            if ui.add(copy_button).clicked() {
                events.push(ChecksumsEvent::Copy(format_all_for_copy(result)));
            }
        }
    });

    let range = panel.config.range;
    if !range.is_empty() {
        let base = virtual_base.unwrap_or(0);
        ui.label(hxy_i18n::t_args(
            "checksums-range",
            &[
                ("start", &format!("0x{:X}", range.start().get().saturating_add(base))),
                ("end", &format!("0x{:X}", range.end().get().saturating_add(base))),
                ("length", &format_bytes(range.len().get())),
            ],
        ));
    }

    ui.separator();

    let Some(result) = panel.last_result.as_ref() else {
        if running {
            ui.label(hxy_i18n::t("checksums-running"));
        } else {
            ui.label(hxy_i18n::t("checksums-no-results-yet"));
        }
        return events;
    };

    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("checksums-results-grid").num_columns(3).striped(true).show(ui, |ui| {
            ui.label(egui::RichText::new(hxy_i18n::t("checksums-col-algorithm")).strong());
            ui.label(egui::RichText::new(hxy_i18n::t("checksums-col-value")).strong());
            ui.label("");
            ui.end_row();
            for (alg, value) in &result.values {
                ui.label(alg.label());
                // Extend wrap mode keeps the hex digest on one
                // line; the outer ScrollArea handles overflow
                // when the panel is narrow. `selectable(true)`
                // lets the user double-click to grab the value
                // without taking the dedicated Copy button path.
                ui.add(
                    egui::Label::new(egui::RichText::new(value).monospace())
                        .wrap_mode(egui::TextWrapMode::Extend)
                        .selectable(true),
                );
                if ui.small_button(hxy_i18n::t("checksums-copy")).clicked() {
                    events.push(ChecksumsEvent::Copy(value.clone()));
                }
                ui.end_row();
            }
        });
    });

    events
}

fn format_all_for_copy(result: &ChecksumResult) -> String {
    let mut out = String::new();
    for (alg, value) in &result.values {
        use std::fmt::Write;
        let _ = writeln!(&mut out, "{}: {}", alg.label(), value);
    }
    out
}

fn format_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GiB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}
