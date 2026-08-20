//! `[[hex::visualize("timestamp", format?)]]`: decode the field's
//! bytes as a numeric timestamp and render it in human-readable
//! form. Supported `format`s:
//!
//! - `unix` (default for 4-byte fields): seconds since 1970-01-01 UTC
//! - `unix_ms`: milliseconds since 1970-01-01 UTC
//! - `unix_us`: microseconds since 1970-01-01 UTC
//! - `unix64` (default for 8-byte fields): same as `unix` but reads
//!   8 bytes and supports a wider range
//! - `windows`: 100ns ticks since 1601-01-01 UTC (Windows FILETIME)
//! - `mac`: seconds since 1904-01-01 UTC (HFS+ / classic Mac)
//!
//! Bytes are read little-endian; flip via the runtime-side endian
//! attribute if needed.

use hxy_templates::visualize::timestamp::decode;
use jiff::Timestamp;

use super::VisualizerContext;

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext) {
    let format = ctx
        .spec
        .args
        .first()
        .map(|s| s.as_str())
        .unwrap_or_else(|| if ctx.bytes.len() >= 8 { "unix64" } else { "unix" });

    match decode(ctx.bytes, format) {
        Ok(ts) => {
            ui.label(egui::RichText::new(hxy_i18n::t_args("visualizer-timestamp-info", &[("format", format)])).weak());
            ui.add_space(4.0);
            ui.label(egui::RichText::new(format!("{ts}")).strong().monospace());
            ui.add_space(2.0);
            ui.label(egui::RichText::new(format_iso(ts)).monospace());
        }
        Err(e) => {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
    }
}

fn format_iso(ts: Timestamp) -> String {
    // jiff's Display impl is already RFC 3339; explicit second copy
    // for clarity in the panel since the first label uses `Display`
    // too. Kept separate so future tweaks (locale-aware date) don't
    // touch the always-machine-readable line.
    format!("{ts}")
}
