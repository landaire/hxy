//! Strings tool.
//!
//! Extract printable runs from a byte range, modeled on unix
//! `strings(1)`. Four encodings are supported (ASCII, UTF-8,
//! UTF-16 LE, UTF-16 BE); the minimum run length is configurable.
//! Results are computed once on demand on the shared background
//! pool and rendered into a per-file dock tab keyed by [`FileId`].

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use serde::Deserialize;
use serde::Serialize;

use crate::files::FileId;

/// Default minimum run length, matching unix `strings(1)`.
pub const DEFAULT_MIN_LENGTH: usize = 4;

/// Hard cap on results held in memory. Hits past this point are
/// dropped and the result is flagged `truncated` so the UI can tell
/// the user to narrow the range.
pub const MAX_RESULTS: usize = 100_000;

/// Read window. Big enough to amortize per-call overhead, small
/// enough to keep memory bounded for huge files.
const CHUNK_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Encoding {
    #[default]
    Ascii,
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ascii => "ASCII",
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
        }
    }

    pub const ALL: [Encoding; 4] = [Self::Ascii, Self::Utf8, Self::Utf16Le, Self::Utf16Be];
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StringsConfig {
    pub encoding: Encoding,
    pub min_length: usize,
    pub range: ByteRange,
}

impl Default for StringsConfig {
    fn default() -> Self {
        Self {
            encoding: Encoding::default(),
            min_length: DEFAULT_MIN_LENGTH,
            // Empty placeholder; callers replace with the actual scope
            // before submitting work.
            range: ByteRange::new(ByteOffset::new(0), ByteOffset::new(0)).expect("empty range valid"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct StringEntry {
    pub offset: u64,
    /// One past the last byte of the run -- always > `offset` because
    /// the run extractor only emits runs of at least one codepoint.
    pub end: u64,
    pub text: String,
}

impl StringEntry {
    pub fn length(&self) -> u64 {
        self.end - self.offset
    }
}

#[derive(Clone, Debug)]
pub struct StringsResult {
    pub entries: Vec<StringEntry>,
    /// True when `MAX_RESULTS` was hit and later runs were dropped.
    pub truncated: bool,
    pub computed_at: jiff::Timestamp,
    pub source_len: u64,
    pub config: StringsConfig,
}

#[derive(Clone, Debug)]
pub enum StringsOutcome {
    Ok(StringsResult),
    Err(String),
}

pub struct StringsComputation {
    pub inbox: egui_inbox::UiInbox<StringsOutcome>,
    pub file_id: FileId,
    pub started: std::time::Instant,
}

#[derive(Default)]
pub struct StringsPanel {
    pub config: StringsConfig,
    pub last_result: Option<StringsResult>,
    pub running: Option<StringsComputation>,
    /// Substring filter applied client-side over the result list.
    /// Held on the panel rather than recomputed each frame so it
    /// survives view scroll / repaint.
    pub filter: String,
}

/// Synchronous strings extractor. Reads the configured range from
/// `source` in fixed-size chunks and emits one [`StringEntry`] per
/// printable run that meets the minimum length. Runs that span chunk
/// boundaries are stitched via per-encoding carry state.
pub fn extract(source: &dyn HexSource, config: &StringsConfig) -> Result<StringsResult, String> {
    let source_len = source.len().get();
    let range_start = config.range.start().get();
    let range_end = config.range.end().get();
    if range_end > source_len {
        return Err(format!("range {range_start}..{range_end} exceeds source length {source_len}"));
    }
    let mut scanner = Scanner::new(config.encoding, config.min_length);
    let mut entries: Vec<StringEntry> = Vec::new();
    let mut truncated = false;
    let mut offset = range_start;
    while offset < range_end {
        let stop = (offset + CHUNK_BYTES).min(range_end);
        let chunk_range = ByteRange::new(ByteOffset::new(offset), ByteOffset::new(stop))
            .map_err(|e| format!("range {offset}..{stop}: {e}"))?;
        let bytes = source.read(chunk_range).map_err(|e| format!("read {offset}..{stop}: {e}"))?;
        scanner.feed(&bytes, offset, &mut entries);
        offset = stop;
        if entries.len() >= MAX_RESULTS {
            truncated = true;
            entries.truncate(MAX_RESULTS);
            break;
        }
    }
    if !truncated {
        scanner.flush(range_end, &mut entries);
        if entries.len() > MAX_RESULTS {
            entries.truncate(MAX_RESULTS);
            truncated = true;
        }
    }
    Ok(StringsResult { entries, truncated, computed_at: jiff::Timestamp::now(), source_len, config: config.clone() })
}

/// Spin up a strings worker. Returns the in-flight handle the host
/// stashes on `OpenFile::strings_panel.running`.
pub fn spawn_compute(
    ctx: &egui::Context,
    id: FileId,
    source: Arc<dyn HexSource>,
    config: StringsConfig,
) -> StringsComputation {
    let (sender, inbox) = egui_inbox::UiInbox::channel_with_ctx(ctx);
    let started = std::time::Instant::now();
    crate::background::submit(move || {
        let outcome = match extract(&*source, &config) {
            Ok(result) => StringsOutcome::Ok(result),
            Err(e) => StringsOutcome::Err(e),
        };
        let _ = sender.send(outcome);
    });
    StringsComputation { inbox, file_id: id, started }
}

struct Scanner {
    encoding: Encoding,
    min_length: usize,
    /// Codepoint count of the currently-accumulating run (not byte
    /// count) so the min-length test matches user intent regardless
    /// of multi-byte encodings.
    run_chars: usize,
    /// Byte offset where the current run started.
    run_start: u64,
    /// Accumulated text for the current run.
    run_text: String,
    /// Pending raw bytes carried into the next chunk: a partial UTF-8
    /// codepoint or the trailing odd byte of a UTF-16 chunk. Up to 3
    /// bytes for UTF-8 and 1 byte for UTF-16.
    pending: Vec<u8>,
    /// File offset corresponding to `pending[0]`.
    pending_offset: u64,
}

impl Scanner {
    fn new(encoding: Encoding, min_length: usize) -> Self {
        Self {
            encoding,
            min_length: min_length.max(1),
            run_chars: 0,
            run_start: 0,
            run_text: String::new(),
            pending: Vec::new(),
            pending_offset: 0,
        }
    }

    fn feed(&mut self, chunk: &[u8], chunk_offset: u64, out: &mut Vec<StringEntry>) {
        match self.encoding {
            Encoding::Ascii => self.feed_ascii(chunk, chunk_offset, out),
            Encoding::Utf8 => self.feed_utf8(chunk, chunk_offset, out),
            Encoding::Utf16Le => self.feed_utf16(chunk, chunk_offset, out, true),
            Encoding::Utf16Be => self.feed_utf16(chunk, chunk_offset, out, false),
        }
    }

    /// Push one accepted codepoint, recording the run start when this
    /// is the first codepoint.
    fn push_char(&mut self, c: char, off: u64) {
        if self.run_chars == 0 {
            self.run_start = off;
            self.run_text.clear();
        }
        self.run_text.push(c);
        self.run_chars += 1;
    }

    /// Commit the current run to `out` if it meets the length
    /// threshold; either way reset run state.
    fn flush(&mut self, end_off: u64, out: &mut Vec<StringEntry>) {
        if self.run_chars >= self.min_length {
            out.push(StringEntry { offset: self.run_start, end: end_off, text: std::mem::take(&mut self.run_text) });
        } else {
            self.run_text.clear();
        }
        self.run_chars = 0;
    }

    fn feed_ascii(&mut self, chunk: &[u8], chunk_offset: u64, out: &mut Vec<StringEntry>) {
        for (i, &b) in chunk.iter().enumerate() {
            let off = chunk_offset + i as u64;
            if (0x20..=0x7E).contains(&b) {
                self.push_char(b as char, off);
            } else {
                self.flush(off, out);
            }
        }
    }

    fn feed_utf8(&mut self, chunk: &[u8], chunk_offset: u64, out: &mut Vec<StringEntry>) {
        let (buf, base) = if self.pending.is_empty() {
            (std::borrow::Cow::Borrowed(chunk), chunk_offset)
        } else {
            let mut combined = std::mem::take(&mut self.pending);
            let base = self.pending_offset;
            combined.extend_from_slice(chunk);
            (std::borrow::Cow::Owned(combined), base)
        };
        let mut i: usize = 0;
        while i < buf.len() {
            match std::str::from_utf8(&buf[i..]) {
                Ok(s) => {
                    self.consume_utf8_str(s, base + i as u64, out);
                    i = buf.len();
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        let s =
                            std::str::from_utf8(&buf[i..i + valid]).expect("valid_up_to delineates utf-8 prefix");
                        self.consume_utf8_str(s, base + i as u64, out);
                    }
                    let after_valid = i + valid;
                    match e.error_len() {
                        Some(skip) => {
                            // Genuine invalid sequence -- break the
                            // current run and step past the offending
                            // bytes.
                            self.flush(base + after_valid as u64, out);
                            i = after_valid + skip;
                        }
                        None => {
                            // Trailing partial codepoint; carry into
                            // the next chunk.
                            self.pending = buf[after_valid..].to_vec();
                            self.pending_offset = base + after_valid as u64;
                            return;
                        }
                    }
                }
            }
        }
    }

    fn consume_utf8_str(&mut self, s: &str, base: u64, out: &mut Vec<StringEntry>) {
        for (off_in_s, c) in s.char_indices() {
            let off = base + off_in_s as u64;
            if printable_codepoint(c) {
                self.push_char(c, off);
            } else {
                self.flush(off, out);
            }
        }
    }

    fn feed_utf16(&mut self, chunk: &[u8], chunk_offset: u64, out: &mut Vec<StringEntry>, little_endian: bool) {
        let (buf, base) = if self.pending.is_empty() {
            (std::borrow::Cow::Borrowed(chunk), chunk_offset)
        } else {
            let mut combined = std::mem::take(&mut self.pending);
            let base = self.pending_offset;
            combined.extend_from_slice(chunk);
            (std::borrow::Cow::Owned(combined), base)
        };
        let mut i: usize = 0;
        while i + 2 <= buf.len() {
            let pair = [buf[i], buf[i + 1]];
            let unit = if little_endian { u16::from_le_bytes(pair) } else { u16::from_be_bytes(pair) };
            let off = base + i as u64;
            // BMP-only: surrogate pairs are treated as a run-breaker.
            // The vast majority of UTF-16 strings in binaries (Windows
            // resources, PE imports) sit in the BMP, so this is good
            // enough for v1.
            match char::from_u32(unit as u32) {
                Some(c) if printable_codepoint(c) => self.push_char(c, off),
                _ => self.flush(off, out),
            }
            i += 2;
        }
        // Save trailing odd byte (or none) for the next chunk.
        if i < buf.len() {
            self.pending = vec![buf[i]];
            self.pending_offset = base + i as u64;
        }
    }
}

/// Treat any non-control codepoint as printable. `is_control` returns
/// true for ASCII C0/C1, U+007F, and Unicode category Cc, which lines
/// up with what `strings(1)` rejects in practice. Whitespace inside
/// runs is preserved -- a sentence with spaces should be one run, not
/// many.
fn printable_codepoint(c: char) -> bool {
    !c.is_control()
}

/// User actions emitted by the panel during render. Drained by the
/// host so panel rendering doesn't have to take `&mut HxyApp`.
#[derive(Clone, Copy, Debug)]
pub enum StringsEvent {
    /// User pressed the "Run" button. The host re-runs against the
    /// current panel config.
    Run,
    /// User clicked a result row -- the host should jump the active
    /// hex view to this byte range and select it.
    Jump { offset: u64, end: u64 },
}

/// Render the per-file Strings panel. Returns user-emitted events
/// so the host can dispatch them (run / jump) without taking a
/// `&mut HxyApp` borrow during rendering.
pub fn show(ui: &mut egui::Ui, file_label: Option<&str>, panel: &mut StringsPanel) -> Vec<StringsEvent> {
    let mut events: Vec<StringsEvent> = Vec::new();
    ui.horizontal(|ui| {
        ui.heading(hxy_i18n::t("strings-heading"));
        ui.add_space(8.0);
        let label = file_label.unwrap_or("");
        ui.label(egui::RichText::new(label).weak());
    });
    ui.separator();

    if file_label.is_none() {
        ui.label(hxy_i18n::t("strings-no-active-file"));
        return events;
    }

    let running = panel.running.is_some();

    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("strings-encoding")
            .selected_text(panel.config.encoding.label())
            .show_ui(ui, |ui| {
                for enc in Encoding::ALL {
                    ui.selectable_value(&mut panel.config.encoding, enc, enc.label());
                }
            });
        ui.label(hxy_i18n::t("strings-min-length"));
        let mut min: u64 = panel.config.min_length as u64;
        ui.add(egui::DragValue::new(&mut min).range(1..=4096));
        panel.config.min_length = min.max(1) as usize;

        let run_label = if running { hxy_i18n::t("strings-running") } else { hxy_i18n::t("strings-run") };
        let run_button = egui::Button::new(run_label);
        if ui.add_enabled(!running, run_button).clicked() {
            events.push(StringsEvent::Run);
        }
    });

    let range = panel.config.range;
    if !range.is_empty() {
        ui.label(hxy_i18n::t_args(
            "strings-range",
            &[
                ("start", &format!("0x{:X}", range.start().get())),
                ("end", &format!("0x{:X}", range.end().get())),
                ("length", &format_bytes(range.len().get())),
            ],
        ));
    }

    ui.horizontal(|ui| {
        ui.label(hxy_i18n::t("strings-filter"));
        ui.add(egui::TextEdit::singleline(&mut panel.filter).desired_width(180.0));
    });

    ui.separator();

    let Some(result) = panel.last_result.as_ref() else {
        if running {
            ui.label(hxy_i18n::t("strings-running"));
        } else {
            ui.label(hxy_i18n::t("strings-no-results-yet"));
        }
        return events;
    };

    let filter = panel.filter.trim().to_lowercase();
    let mut shown: usize = 0;
    let total = result.entries.len();

    let summary = if filter.is_empty() {
        hxy_i18n::t_args("strings-summary", &[("count", &total.to_string())])
    } else {
        hxy_i18n::t_args(
            "strings-summary-filtered",
            &[("count", &total.to_string()), ("filter", &panel.filter)],
        )
    };
    ui.label(summary);
    if result.truncated {
        ui.colored_label(
            egui::Color32::from_rgb(245, 204, 78),
            hxy_i18n::t_args("strings-truncated", &[("max", &MAX_RESULTS.to_string())]),
        );
    }

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("strings-results-grid")
            .num_columns(4)
            .striped(true)
            .show(ui, |ui| {
                ui.label(egui::RichText::new(hxy_i18n::t("strings-col-offset")).strong());
                ui.label(egui::RichText::new(hxy_i18n::t("strings-col-end")).strong());
                ui.label(egui::RichText::new(hxy_i18n::t("strings-col-length")).strong());
                ui.label(egui::RichText::new(hxy_i18n::t("strings-col-text")).strong());
                ui.end_row();

                for entry in &result.entries {
                    if !filter.is_empty() && !entry.text.to_lowercase().contains(&filter) {
                        continue;
                    }
                    if ui.link(format!("0x{:X}", entry.offset)).clicked() {
                        events.push(StringsEvent::Jump { offset: entry.offset, end: entry.end });
                    }
                    ui.monospace(format!("0x{:X}", entry.end));
                    ui.monospace(format!("{}", entry.length()));
                    ui.label(egui::RichText::new(&entry.text).monospace());
                    ui.end_row();
                    shown += 1;
                }
            });
        if shown == 0 && !filter.is_empty() {
            ui.label(hxy_i18n::t("strings-no-matches"));
        }
    });

    events
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(encoding: Encoding, min_length: usize, len: u64) -> StringsConfig {
        StringsConfig {
            encoding,
            min_length,
            range: ByteRange::new(ByteOffset::new(0), ByteOffset::new(len)).unwrap(),
        }
    }

    fn run(encoding: Encoding, min_length: usize, bytes: &[u8]) -> Vec<StringEntry> {
        let source = hxy_core::MemorySource::new(bytes.to_vec());
        let result = extract(&source, &cfg(encoding, min_length, bytes.len() as u64)).unwrap();
        result.entries
    }

    #[test]
    fn ascii_extracts_printable_runs() {
        let bytes = b"\x00hello\x00world\x00";
        let entries = run(Encoding::Ascii, 4, bytes);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "hello");
        assert_eq!(entries[0].offset, 1);
        assert_eq!(entries[0].end, 6);
        assert_eq!(entries[1].text, "world");
    }

    #[test]
    fn ascii_respects_min_length() {
        let bytes = b"abc\x00abcd\x00";
        let entries = run(Encoding::Ascii, 4, bytes);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "abcd");
    }

    #[test]
    fn ascii_run_is_terminated_by_high_byte() {
        let bytes = b"hello\xff\xff\xffworld";
        let entries = run(Encoding::Ascii, 4, bytes);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "hello");
        assert_eq!(entries[1].text, "world");
    }

    #[test]
    fn utf16_le_extracts_bmp_run() {
        // "hi" in UTF-16LE plus a null pair, then "world".
        let mut bytes: Vec<u8> = Vec::new();
        for c in "hi".chars() {
            bytes.extend_from_slice(&(c as u16).to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]);
        for c in "world".chars() {
            bytes.extend_from_slice(&(c as u16).to_le_bytes());
        }
        let entries = run(Encoding::Utf16Le, 2, &bytes);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "hi");
        assert_eq!(entries[1].text, "world");
    }

    #[test]
    fn utf16_be_extracts_bmp_run() {
        let mut bytes: Vec<u8> = Vec::new();
        for c in "hi".chars() {
            bytes.extend_from_slice(&(c as u16).to_be_bytes());
        }
        let entries = run(Encoding::Utf16Be, 2, &bytes);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "hi");
    }

    #[test]
    fn utf8_handles_multibyte_codepoints() {
        let bytes = "café\x00world".as_bytes();
        let entries = run(Encoding::Utf8, 3, bytes);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "café");
        assert_eq!(entries[1].text, "world");
    }

    #[test]
    fn empty_range_produces_no_results() {
        let bytes: &[u8] = &[];
        let entries = run(Encoding::Ascii, 4, bytes);
        assert!(entries.is_empty());
    }
}
