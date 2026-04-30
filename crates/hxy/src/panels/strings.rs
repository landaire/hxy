//! Strings tool.
//!
//! Extract printable runs from a byte range, modeled on unix
//! `strings(1)`. Four encodings are supported (ASCII, UTF-8,
//! UTF-16 LE, UTF-16 BE); the minimum run length is configurable.
//! Results are computed once on demand on the shared background
//! pool and rendered into a per-file dock tab keyed by [`FileId`].

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
    /// Sort order the `entries` slice is currently in. Tracked here
    /// so the renderer can detect when the panel's `sort` has
    /// drifted and reshuffle in place. The extractor emits entries
    /// in offset-ascending order, which is the default.
    pub sorted_by: SortOrder,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortColumn {
    #[default]
    Offset,
    End,
    Length,
    Text,
}

/// Sort direction + the column it's applied to. Click a header to
/// either flip direction (when the column is already active) or
/// switch to that column (always asc on switch).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortOrder {
    Asc(SortColumn),
    Desc(SortColumn),
}

impl Default for SortOrder {
    fn default() -> Self {
        Self::Asc(SortColumn::Offset)
    }
}

impl SortOrder {
    pub fn column(self) -> SortColumn {
        match self {
            Self::Asc(c) | Self::Desc(c) => c,
        }
    }

    pub fn is_descending(self) -> bool {
        matches!(self, Self::Desc(_))
    }

    /// Click feedback: clicking the active column flips direction;
    /// clicking a different column switches to it asc.
    pub fn cycle(self, target: SortColumn) -> Self {
        if self.column() == target {
            if self.is_descending() { Self::Asc(target) } else { Self::Desc(target) }
        } else {
            Self::Asc(target)
        }
    }
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
    /// Active sort applied to `last_result.entries`. Defaults to
    /// ascending offset, matching the order the extractor produces.
    pub sort: SortOrder,
    /// Byte range the pointer is currently over in the result table,
    /// used by the hex view to paint a hover highlight on the
    /// matched bytes. Mirrors `TemplateState::hovered_node` -- the
    /// hex view reads from here whenever the pointer rests over a
    /// row in this panel. Reset to `None` each frame when no cell
    /// sees the pointer; also cleared on tab close so a stale value
    /// doesn't keep the highlight stuck after the panel goes away.
    pub hovered_entry: Option<ByteRange>,
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
    Ok(StringsResult {
        entries,
        truncated,
        computed_at: jiff::Timestamp::now(),
        source_len,
        config: config.clone(),
        sorted_by: SortOrder::default(),
    })
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
                        let s = std::str::from_utf8(&buf[i..i + valid]).expect("valid_up_to delineates utf-8 prefix");
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

/// Render the per-file Strings panel without virtual addressing
/// (offset / end columns show raw file offsets). Returns user-
/// emitted events so the host can dispatch them (run / jump)
/// without taking a `&mut HxyApp` borrow during rendering.
pub fn show(ui: &mut egui::Ui, file_label: Option<&str>, panel: &mut StringsPanel) -> Vec<StringsEvent> {
    show_inner(ui, file_label, panel, None)
}

/// Render the per-file Strings panel with virtual addressing
/// applied: offset / end values are rendered as `entry + base`
/// and the column headers switch to "Address" / "End address".
/// Use this variant only when the file has an accepted virtual
/// base; otherwise call [`show`].
pub fn show_with_vaddr(
    ui: &mut egui::Ui,
    file_label: Option<&str>,
    panel: &mut StringsPanel,
    virtual_base: u64,
) -> Vec<StringsEvent> {
    show_inner(ui, file_label, panel, Some(virtual_base))
}

fn show_inner(
    ui: &mut egui::Ui,
    file_label: Option<&str>,
    panel: &mut StringsPanel,
    virtual_base: Option<u64>,
) -> Vec<StringsEvent> {
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
        egui::ComboBox::from_id_salt("strings-encoding").selected_text(panel.config.encoding.label()).show_ui(
            ui,
            |ui| {
                for enc in Encoding::ALL {
                    ui.selectable_value(&mut panel.config.encoding, enc, enc.label());
                }
            },
        );
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
        let base = virtual_base.unwrap_or(0);
        ui.label(hxy_i18n::t_args(
            "strings-range",
            &[
                ("start", &format!("0x{:X}", range.start().get().saturating_add(base))),
                ("end", &format!("0x{:X}", range.end().get().saturating_add(base))),
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
    let total = result.entries.len();

    let summary = if filter.is_empty() {
        hxy_i18n::t_args("strings-summary", &[("count", &total.to_string())])
    } else {
        hxy_i18n::t_args("strings-summary-filtered", &[("count", &total.to_string()), ("filter", &panel.filter)])
    };
    ui.label(summary);
    if result.truncated {
        ui.colored_label(
            egui::Color32::from_rgb(245, 204, 78),
            hxy_i18n::t_args("strings-truncated", &[("max", &MAX_RESULTS.to_string())]),
        );
    }

    // Re-sort the entries vector in place when the panel's sort
    // order has drifted from what the result was last sorted by.
    // Stored on the result rather than recomputed each frame so a
    // re-render with the same sort doesn't pay the sort cost.
    if result.sorted_by != panel.sort {
        let order = panel.sort;
        let result_mut = panel.last_result.as_mut().expect("matched as Some above");
        sort_entries(&mut result_mut.entries, order);
        result_mut.sorted_by = order;
    }
    let result = panel.last_result.as_ref().expect("matched as Some above");

    // Filter is applied as a Vec<usize> of indices into the
    // (possibly re-sorted) entries vector so the egui_table delegate
    // can map row_nr -> entry without rescanning each frame.
    let visible: Vec<usize> = if filter.is_empty() {
        (0..result.entries.len()).collect()
    } else {
        result
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.text.to_lowercase().contains(&filter).then_some(i))
            .collect()
    };

    if visible.is_empty() {
        if !filter.is_empty() {
            ui.label(hxy_i18n::t("strings-no-matches"));
        }
        return events;
    }

    let mut delegate = StringsTableDelegate {
        entries: &result.entries,
        visible: &visible,
        sort: panel.sort,
        pending_sort: None,
        pending_hover: None,
        virtual_base,
        events: &mut events,
    };

    let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
    let table = egui_table::Table::new()
        .id_salt("hxy-strings-table")
        .num_rows(visible.len() as u64)
        .columns(vec![
            egui_table::Column::new(110.0).range(70.0..=200.0).resizable(true).id(egui::Id::new("strings-col-offset")),
            egui_table::Column::new(110.0).range(70.0..=200.0).resizable(true).id(egui::Id::new("strings-col-end")),
            egui_table::Column::new(80.0).range(50.0..=160.0).resizable(true).id(egui::Id::new("strings-col-length")),
            egui_table::Column::new(360.0).range(80.0..=2000.0).resizable(true).id(egui::Id::new("strings-col-text")),
        ])
        .headers(vec![egui_table::HeaderRow::new(row_height + 2.0)])
        .auto_size_mode(egui_table::AutoSizeMode::Always);
    table.show(ui, &mut delegate);

    // Copy values out of the delegate before any further mutable
    // borrow of `panel`, since the delegate still holds a shared
    // borrow on `panel.last_result.entries` until it goes out of
    // scope. ByteRange and SortOrder are Copy.
    let new_sort = delegate.pending_sort;
    let new_hover = delegate.pending_hover;
    if let Some(s) = new_sort {
        panel.sort = s;
    }
    panel.hovered_entry = new_hover;

    events
}

/// Sort `entries` in place by the given order. Pulled out so the
/// renderer can re-sort lazily when the panel's sort changes.
fn sort_entries(entries: &mut [StringEntry], order: SortOrder) {
    use std::cmp::Ordering;
    let cmp = |a: &StringEntry, b: &StringEntry| -> Ordering {
        match order.column() {
            SortColumn::Offset => a.offset.cmp(&b.offset),
            SortColumn::End => a.end.cmp(&b.end),
            SortColumn::Length => a.length().cmp(&b.length()),
            SortColumn::Text => a.text.cmp(&b.text),
        }
    };
    if order.is_descending() {
        entries.sort_by(|a, b| cmp(a, b).reverse());
    } else {
        entries.sort_by(cmp);
    }
}

struct StringsTableDelegate<'a> {
    entries: &'a [StringEntry],
    visible: &'a [usize],
    sort: SortOrder,
    /// Set by `header_cell_ui` when the user clicked a column
    /// header. The caller writes it back onto the panel after
    /// `Table::show` returns.
    pending_sort: Option<SortOrder>,
    /// Byte range of the row whose cell currently contains the
    /// pointer, or `None` when no cell sees the pointer this frame.
    /// Mirrored back onto `panel.hovered_entry` post-render so the
    /// hex view picks it up.
    pending_hover: Option<ByteRange>,
    /// Active virtual base. When `Some`, offset / end columns
    /// render as virtual addresses and headers swap to "Address" /
    /// "End address" labels.
    virtual_base: Option<u64>,
    events: &'a mut Vec<StringsEvent>,
}

impl egui_table::TableDelegate for StringsTableDelegate<'_> {
    fn header_cell_ui(&mut self, ui: &mut egui::Ui, cell: &egui_table::HeaderCellInfo) {
        let (label_key, sort_col) = match cell.col_range.start {
            0 => {
                let key = if self.virtual_base.is_some() { "strings-col-address" } else { "strings-col-offset" };
                (key, SortColumn::Offset)
            }
            1 => {
                let key = if self.virtual_base.is_some() { "strings-col-end-address" } else { "strings-col-end" };
                (key, SortColumn::End)
            }
            2 => ("strings-col-length", SortColumn::Length),
            3 => ("strings-col-text", SortColumn::Text),
            _ => return,
        };
        let mut text = hxy_i18n::t(label_key);
        if self.sort.column() == sort_col {
            let glyph = if self.sort.is_descending() {
                egui_phosphor::regular::CARET_DOWN
            } else {
                egui_phosphor::regular::CARET_UP
            };
            text.push(' ');
            text.push_str(glyph);
        }
        ui.add_space(6.0);
        let resp = ui.add(egui::Label::new(egui::RichText::new(text).strong()).sense(egui::Sense::click()));
        if resp.clicked() {
            self.pending_sort = Some(self.sort.cycle(sort_col));
        }
    }

    fn cell_ui(&mut self, ui: &mut egui::Ui, cell: &egui_table::CellInfo) {
        let row = cell.row_nr as usize;
        let Some(entry_idx) = self.visible.get(row).copied() else { return };
        let Some(entry) = self.entries.get(entry_idx) else { return };
        // Pointer-over-cell counts as pointer-over-row for the hex
        // view hover highlight: every cell in a row resolves to the
        // same byte range, so the last cell that sees the pointer
        // wins each frame and produces a stable hover signal.
        if ui.rect_contains_pointer(ui.max_rect())
            && let Ok(range) = ByteRange::new(ByteOffset::new(entry.offset), ByteOffset::new(entry.end))
        {
            self.pending_hover = Some(range);
        }
        ui.add_space(4.0);
        let base = self.virtual_base.unwrap_or(0);
        match cell.col_nr {
            0 => {
                let display = entry.offset.saturating_add(base);
                if ui.link(egui::RichText::new(format!("0x{display:X}")).monospace()).clicked() {
                    self.events.push(StringsEvent::Jump { offset: entry.offset, end: entry.end });
                }
            }
            1 => {
                let display = entry.end.saturating_add(base);
                ui.monospace(format!("0x{display:X}"));
            }
            2 => {
                ui.monospace(format!("{}", entry.length()));
            }
            3 => {
                ui.add(
                    egui::Label::new(egui::RichText::new(&entry.text).monospace())
                        .wrap_mode(egui::TextWrapMode::Extend)
                        .selectable(true),
                );
            }
            _ => {}
        }
    }
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
        StringsConfig { encoding, min_length, range: ByteRange::new(ByteOffset::new(0), ByteOffset::new(len)).unwrap() }
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
