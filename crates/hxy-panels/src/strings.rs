//! Strings-tool extraction: printable-run scanning, modeled on unix
//! `strings(1)`. Framework- and app-agnostic; the egui panel that
//! renders results and dispatches worker threads stays in
//! `crates/hxy`.

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use serde::Deserialize;
use serde::Serialize;

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

/// Sort `entries` in place by the given order. Pulled out so the
/// renderer can re-sort lazily when the panel's sort changes.
pub fn sort_entries(entries: &mut [StringEntry], order: SortOrder) {
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
