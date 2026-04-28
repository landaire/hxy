//! Byte-cache occupancy debug panel.
//!
//! Renders one row per (attribution, source) tuple the cache is
//! holding chunks for, sorted by tracked-cache bytes descending.
//! The numbers come from [`hxy_core::ByteCache::stats`] and are
//! "tracked-cache bytes" -- the sum of chunk lengths attributed to
//! each consumer, not actual heap allocator pages. The disclaimer
//! line on the panel header makes that explicit.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;
use std::sync::Arc;

use hxy_core::Attribution;
use hxy_core::ByteCache;
use hxy_core::CacheStats;

use crate::files::FileId;
use crate::files::OpenFile;

/// Lookup table mapping each [`hxy_core::HexViewKey`] back to a
/// display name. Built fresh each frame from `app.files`; rebuilds
/// are cheap because the file count is bounded by tabs.
pub struct ViewLabels<'a> {
    by_file_id: HashMap<u64, &'a str>,
}

impl<'a> ViewLabels<'a> {
    pub fn from_files(files: &'a HashMap<FileId, OpenFile>) -> Self {
        let mut by_file_id = HashMap::with_capacity(files.len());
        for (id, file) in files {
            by_file_id.insert(id.get(), file.display_name.as_str());
        }
        Self { by_file_id }
    }

    fn label_for(&self, attribution: Attribution) -> String {
        match attribution {
            Attribution::HexView(key) => match self.by_file_id.get(&key.0) {
                Some(name) => hxy_i18n::t_args("memory-panel-row-hex-view", &[("name", name)]),
                None => hxy_i18n::t_args("memory-panel-row-unknown", &[("id", &format!("{}", key.0))]),
            },
            Attribution::Template(key) => match self.by_file_id.get(&key.0) {
                Some(name) => hxy_i18n::t_args("memory-panel-row-template", &[("name", name)]),
                None => hxy_i18n::t_args("memory-panel-row-unknown", &[("id", &format!("{}", key.0))]),
            },
            Attribution::Plugin(key) => {
                hxy_i18n::t_args("memory-panel-row-plugin", &[("name", &format!("{}", key.0))])
            }
        }
    }
}

/// Format a byte count as MiB, rounded to one decimal place.
fn mib_of(bytes: u64) -> f32 {
    (bytes as f64 / (1024.0 * 1024.0)) as f32
}

pub fn memory_ui(ui: &mut egui::Ui, byte_cache: &Arc<ByteCache>, labels: &ViewLabels<'_>) {
    let stats: CacheStats = byte_cache.stats();
    if stats.chunk_count == 0 {
        ui.label(hxy_i18n::t("memory-panel-empty"));
        return;
    }
    let summary = hxy_i18n::t_args(
        "memory-panel-summary",
        &[
            ("used_mib", &format!("{:.1}", mib_of(stats.used_bytes))),
            ("limit_mib", &format!("{:.0}", mib_of(stats.limit_bytes))),
            ("chunks", &format!("{}", stats.chunk_count)),
            ("hits", &format!("{}", stats.hits)),
            ("misses", &format!("{}", stats.misses)),
        ],
    );
    ui.label(summary);
    ui.add_space(2.0);
    ui.colored_label(ui.visuals().weak_text_color(), hxy_i18n::t("memory-panel-disclaimer"));
    ui.separator();
    egui::Grid::new("hxy-memory-panel-rows").num_columns(2).striped(true).show(ui, |ui| {
        for entry in &stats.by_attribution {
            ui.label(labels.label_for(entry.attribution));
            ui.label(hxy_i18n::t_args(
                "memory-panel-bytes-mib",
                &[("mib", &format!("{:.1}", mib_of(entry.bytes)))],
            ));
            ui.end_row();
        }
    });
}
