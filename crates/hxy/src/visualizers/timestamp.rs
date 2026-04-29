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

use jiff::Timestamp;

use super::VisualizerContext;

pub fn show(ui: &mut egui::Ui, ctx: &VisualizerContext) {
    let format = ctx.spec.args.first().map(|s| s.as_str()).unwrap_or_else(|| {
        if ctx.bytes.len() >= 8 { "unix64" } else { "unix" }
    });

    match decode(ctx.bytes, format) {
        Ok(ts) => {
            ui.label(
                egui::RichText::new(hxy_i18n::t_args(
                    "visualizer-timestamp-info",
                    &[("format", format)],
                ))
                .weak(),
            );
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

fn decode(bytes: &[u8], format: &str) -> Result<Timestamp, String> {
    match format {
        "unix" => {
            if bytes.len() < 4 {
                return Err(hxy_i18n::t("visualizer-timestamp-need-4"));
            }
            let secs = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as i64;
            Timestamp::from_second(secs)
                .map_err(|e| hxy_i18n::t_args("visualizer-timestamp-bad", &[("err", &format!("{e}"))]))
        }
        "unix64" => {
            if bytes.len() < 8 {
                return Err(hxy_i18n::t("visualizer-timestamp-need-8"));
            }
            let secs = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
            Timestamp::from_second(secs)
                .map_err(|e| hxy_i18n::t_args("visualizer-timestamp-bad", &[("err", &format!("{e}"))]))
        }
        "unix_ms" => {
            if bytes.len() < 8 {
                return Err(hxy_i18n::t("visualizer-timestamp-need-8"));
            }
            let ms = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
            Timestamp::from_millisecond(ms)
                .map_err(|e| hxy_i18n::t_args("visualizer-timestamp-bad", &[("err", &format!("{e}"))]))
        }
        "unix_us" => {
            if bytes.len() < 8 {
                return Err(hxy_i18n::t("visualizer-timestamp-need-8"));
            }
            let us = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
            Timestamp::from_microsecond(us)
                .map_err(|e| hxy_i18n::t_args("visualizer-timestamp-bad", &[("err", &format!("{e}"))]))
        }
        "windows" | "filetime" => {
            if bytes.len() < 8 {
                return Err(hxy_i18n::t("visualizer-timestamp-need-8"));
            }
            // Windows FILETIME: 100-nanosecond ticks since 1601-01-01 UTC.
            // Convert to unix seconds (delta = 11644473600s) then to a
            // Timestamp.
            let ticks = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
            let secs = (ticks / 10_000_000) as i64 - 11_644_473_600i64;
            let nanos = ((ticks % 10_000_000) * 100) as i32;
            Timestamp::new(secs, nanos)
                .map_err(|e| hxy_i18n::t_args("visualizer-timestamp-bad", &[("err", &format!("{e}"))]))
        }
        "mac" | "hfs" => {
            if bytes.len() < 4 {
                return Err(hxy_i18n::t("visualizer-timestamp-need-4"));
            }
            // HFS+ epoch: seconds since 1904-01-01 UTC; delta = 2082844800s.
            let secs = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as i64;
            Timestamp::from_second(secs - 2_082_844_800)
                .map_err(|e| hxy_i18n::t_args("visualizer-timestamp-bad", &[("err", &format!("{e}"))]))
        }
        other => Err(hxy_i18n::t_args("visualizer-timestamp-unknown", &[("name", other)])),
    }
}

fn format_iso(ts: Timestamp) -> String {
    // jiff's Display impl is already RFC 3339; explicit second copy
    // for clarity in the panel since the first label uses `Display`
    // too. Kept separate so future tweaks (locale-aware date) don't
    // touch the always-machine-readable line.
    format!("{ts}")
}
