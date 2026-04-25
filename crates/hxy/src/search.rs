//! Search bar.
//!
//! Per-file `SearchState` lives on `OpenFile`. The bar renders at the
//! bottom of a file tab when open; cross-file search runs through a
//! shared `GlobalSearchState` whose results are listed in a dedicated
//! `Tab::SearchResults`.

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;

/// What the user typed in the query box is interpreted as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchKind {
    /// UTF-8 text. Treated as bytes after encoding.
    Text,
    /// Loose hex bytes: whitespace-separated pairs, e.g. "4a 6f" or "4A6F".
    HexBytes,
    /// Integer literal parsed as decimal (or `0x...` hex), encoded
    /// at the configured width / signedness / endianness.
    Number,
}

/// Integer width for `SearchKind::Number`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberWidth {
    W8,
    W16,
    W32,
    W64,
}

impl NumberWidth {
    pub fn bytes(self) -> usize {
        match self {
            Self::W8 => 1,
            Self::W16 => 2,
            Self::W32 => 4,
            Self::W64 => 8,
        }
    }
    pub const ALL: [NumberWidth; 4] = [Self::W8, Self::W16, Self::W32, Self::W64];
    pub fn label(self) -> &'static str {
        match self {
            Self::W8 => "8",
            Self::W16 => "16",
            Self::W32 => "32",
            Self::W64 => "64",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

impl Endian {
    pub fn label(self) -> &'static str {
        match self {
            Self::Little => "LE",
            Self::Big => "BE",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("query is empty")]
    Empty,
    #[error("hex byte string is malformed: {0}")]
    BadHex(String),
    #[error("number does not parse as {radix}: {input}")]
    BadNumber { input: String, radix: u32 },
    #[error("number {value} does not fit in {bytes}-byte {sign}")]
    NumberOverflow { value: String, bytes: usize, sign: &'static str },
}

/// Per-file search state. Persists between toggles of the bar so the
/// user can hide and re-show without losing the query, but isn't
/// persisted across restarts.
#[derive(Clone, Debug)]
pub struct SearchState {
    pub open: bool,
    pub kind: SearchKind,
    pub query: String,
    pub width: NumberWidth,
    pub signed: bool,
    pub endian: Endian,
    pub all_results: bool,
    /// Cached last-encoded pattern. Re-derived whenever query/kind/width/etc.
    /// change. None means an encode error or empty.
    pub pattern: Option<Vec<u8>>,
    pub error: Option<String>,
    /// Offsets of every match in the current file, in ascending order.
    /// Populated lazily when the user runs "All results" or
    /// next/previous; Next/Previous compute `pattern` once and walk
    /// the source incrementally so we don't pay an O(n) re-scan on
    /// every keystroke.
    pub matches: Vec<u64>,
    /// Index into `matches` of the active result. None when there are
    /// no results or the user just typed.
    pub active_idx: Option<usize>,
    /// Serial bumped whenever query or settings change. Lets the file
    /// tab know to re-run pre-computed matches without storing a copy
    /// of the pattern there.
    pub serial: u64,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            open: false,
            kind: SearchKind::HexBytes,
            query: String::new(),
            width: NumberWidth::W32,
            signed: false,
            endian: Endian::Little,
            all_results: false,
            pattern: None,
            error: None,
            matches: Vec::new(),
            active_idx: None,
            serial: 0,
        }
    }
}

impl SearchState {
    /// Re-derive `pattern` and `error` from the current query / settings.
    /// Call after any field change. Returns `Ok(())` if the pattern is
    /// non-empty and parsed cleanly; clears match state on any change.
    pub fn refresh_pattern(&mut self) {
        self.error = None;
        match encode_query(self.kind, &self.query, self.width, self.signed, self.endian) {
            Ok(bytes) if bytes.is_empty() => {
                self.pattern = None;
            }
            Ok(bytes) => {
                self.pattern = Some(bytes);
            }
            Err(EncodeError::Empty) => {
                self.pattern = None;
            }
            Err(e) => {
                self.pattern = None;
                self.error = Some(e.to_string());
            }
        }
        self.matches.clear();
        self.active_idx = None;
        self.serial = self.serial.wrapping_add(1);
    }
}

/// Encode the user's query into a byte pattern. Empty input maps to
/// `Empty`, surfaced by callers as "no error, no matches".
pub fn encode_query(
    kind: SearchKind,
    query: &str,
    width: NumberWidth,
    signed: bool,
    endian: Endian,
) -> Result<Vec<u8>, EncodeError> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(EncodeError::Empty);
    }
    match kind {
        SearchKind::Text => Ok(trimmed.as_bytes().to_vec()),
        SearchKind::HexBytes => parse_hex_bytes(trimmed),
        SearchKind::Number => encode_number(trimmed, width, signed, endian),
    }
}

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, EncodeError> {
    let collapsed: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if collapsed.is_empty() {
        return Err(EncodeError::Empty);
    }
    if !collapsed.len().is_multiple_of(2) {
        return Err(EncodeError::BadHex(format!("odd nibble count ({})", collapsed.len())));
    }
    let mut out = Vec::with_capacity(collapsed.len() / 2);
    let bytes = collapsed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let pair = std::str::from_utf8(&bytes[i..i + 2]).map_err(|_| EncodeError::BadHex(s.to_owned()))?;
        let byte = u8::from_str_radix(pair, 16).map_err(|_| EncodeError::BadHex(pair.to_owned()))?;
        out.push(byte);
        i += 2;
    }
    Ok(out)
}

fn encode_number(s: &str, width: NumberWidth, signed: bool, endian: Endian) -> Result<Vec<u8>, EncodeError> {
    let bytes = width.bytes();
    if signed {
        let v = parse_signed(s)?;
        let (min, max): (i128, i128) = match width {
            NumberWidth::W8 => (i8::MIN as i128, i8::MAX as i128),
            NumberWidth::W16 => (i16::MIN as i128, i16::MAX as i128),
            NumberWidth::W32 => (i32::MIN as i128, i32::MAX as i128),
            NumberWidth::W64 => (i64::MIN as i128, i64::MAX as i128),
        };
        if v < min || v > max {
            return Err(EncodeError::NumberOverflow { value: s.to_owned(), bytes, sign: "signed integer" });
        }
        let mask = (1u128 << (bytes * 8)) - 1;
        let unsigned = (v as i128 as u128) & mask;
        Ok(emit_with_endian(unsigned, bytes, endian))
    } else {
        let v = parse_unsigned(s)?;
        if bytes < 16 && v > ((1u128 << (bytes * 8)) - 1) {
            return Err(EncodeError::NumberOverflow { value: s.to_owned(), bytes, sign: "unsigned integer" });
        }
        Ok(emit_with_endian(v, bytes, endian))
    }
}

fn parse_signed(s: &str) -> Result<i128, EncodeError> {
    let (negative, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (radix, body) = strip_radix(rest);
    let mag = i128::from_str_radix(body, radix).map_err(|_| EncodeError::BadNumber {
        input: s.to_owned(),
        radix,
    })?;
    Ok(if negative { -mag } else { mag })
}

fn parse_unsigned(s: &str) -> Result<u128, EncodeError> {
    let (radix, body) = strip_radix(s);
    u128::from_str_radix(body, radix).map_err(|_| EncodeError::BadNumber {
        input: s.to_owned(),
        radix,
    })
}

fn strip_radix(s: &str) -> (u32, &str) {
    if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        (16, rest)
    } else if let Some(rest) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        (2, rest)
    } else if let Some(rest) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
        (8, rest)
    } else {
        (10, s)
    }
}

fn emit_with_endian(v: u128, bytes: usize, endian: Endian) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes);
    let full = v.to_le_bytes();
    out.extend_from_slice(&full[..bytes]);
    if matches!(endian, Endian::Big) {
        out.reverse();
    }
    out
}

/// Sliding-window byte search. Returns the lowest match offset >= `from`
/// in `[from, source.len())`, or wraps to `[0, from)` if `wrap` is true.
/// `source` is read in 64 KiB chunks plus `pattern.len() - 1` bytes of
/// overlap so a match straddling two chunks isn't missed.
pub fn find_next(source: &dyn HexSource, pattern: &[u8], from: u64, wrap: bool) -> Option<u64> {
    if pattern.is_empty() {
        return None;
    }
    let total = source.len().get();
    if total < pattern.len() as u64 {
        return None;
    }
    if let Some(off) = scan_forward(source, pattern, from, total) {
        return Some(off);
    }
    if wrap && from > 0 {
        let wrap_end = (from + pattern.len() as u64 - 1).min(total);
        return scan_forward(source, pattern, 0, wrap_end);
    }
    None
}

/// Reverse counterpart of [`find_next`]. Returns the largest match
/// offset < `from`, or wraps if requested.
pub fn find_prev(source: &dyn HexSource, pattern: &[u8], from: u64, wrap: bool) -> Option<u64> {
    if pattern.is_empty() {
        return None;
    }
    let total = source.len().get();
    if total < pattern.len() as u64 {
        return None;
    }
    if let Some(off) = scan_backward(source, pattern, 0, from) {
        return Some(off);
    }
    if wrap {
        let wrap_start = from.saturating_sub(pattern.len() as u64 - 1);
        return scan_backward(source, pattern, wrap_start, total);
    }
    None
}

/// Find every match in the source, in ascending order.
pub fn find_all(source: &dyn HexSource, pattern: &[u8]) -> Vec<u64> {
    if pattern.is_empty() {
        return Vec::new();
    }
    let total = source.len().get();
    if total < pattern.len() as u64 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut from = 0u64;
    while let Some(off) = scan_forward(source, pattern, from, total) {
        out.push(off);
        from = off + 1;
    }
    out
}

const CHUNK: u64 = 64 * 1024;

fn scan_forward(source: &dyn HexSource, pattern: &[u8], start: u64, end: u64) -> Option<u64> {
    if pattern.is_empty() {
        return None;
    }
    let plen = pattern.len() as u64;
    if end < plen || start + plen > end {
        return None;
    }
    let mut pos = start;
    while pos < end {
        let chunk_end = (pos + CHUNK).min(end);
        let read_end = (chunk_end + plen - 1).min(end).min(source.len().get());
        if read_end <= pos {
            return None;
        }
        let range = ByteRange::new(ByteOffset::new(pos), ByteOffset::new(read_end)).ok()?;
        let buf = source.read(range).ok()?;
        if let Some(idx) = find_in_slice(&buf, pattern) {
            return Some(pos + idx as u64);
        }
        if chunk_end >= end {
            break;
        }
        pos = chunk_end;
    }
    None
}

fn scan_backward(source: &dyn HexSource, pattern: &[u8], start: u64, end: u64) -> Option<u64> {
    if pattern.is_empty() {
        return None;
    }
    let plen = pattern.len() as u64;
    if end < plen || start + plen > end {
        return None;
    }
    let mut pos = end;
    while pos > start {
        let chunk_start = pos.saturating_sub(CHUNK).max(start);
        let read_start = chunk_start;
        let read_end = (pos + plen - 1).min(end).min(source.len().get());
        if read_end <= read_start {
            break;
        }
        let range = ByteRange::new(ByteOffset::new(read_start), ByteOffset::new(read_end)).ok()?;
        let buf = source.read(range).ok()?;
        if let Some(idx) = find_last_in_slice(&buf, pattern) {
            return Some(read_start + idx as u64);
        }
        if chunk_start == start {
            break;
        }
        pos = chunk_start;
    }
    None
}

fn find_in_slice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn find_last_in_slice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hxy_core::MemorySource;

    fn src(bytes: &[u8]) -> MemorySource {
        MemorySource::new(bytes.to_vec())
    }

    #[test]
    fn parses_spaced_hex() {
        let p = encode_query(SearchKind::HexBytes, "4a 6f", NumberWidth::W8, false, Endian::Little).unwrap();
        assert_eq!(p, vec![0x4a, 0x6f]);
    }

    #[test]
    fn parses_tight_hex_uppercase() {
        let p = encode_query(SearchKind::HexBytes, "DEADBEEF", NumberWidth::W8, false, Endian::Little).unwrap();
        assert_eq!(p, vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn rejects_odd_hex() {
        let r = encode_query(SearchKind::HexBytes, "abc", NumberWidth::W8, false, Endian::Little);
        assert!(matches!(r, Err(EncodeError::BadHex(_))));
    }

    #[test]
    fn encodes_unsigned_le_u16() {
        let p = encode_query(SearchKind::Number, "256", NumberWidth::W16, false, Endian::Little).unwrap();
        assert_eq!(p, vec![0x00, 0x01]);
    }

    #[test]
    fn encodes_unsigned_be_u16() {
        let p = encode_query(SearchKind::Number, "256", NumberWidth::W16, false, Endian::Big).unwrap();
        assert_eq!(p, vec![0x01, 0x00]);
    }

    #[test]
    fn encodes_signed_negative_le_i16() {
        let p = encode_query(SearchKind::Number, "-1", NumberWidth::W16, true, Endian::Little).unwrap();
        assert_eq!(p, vec![0xff, 0xff]);
    }

    #[test]
    fn encodes_hex_literal() {
        let p = encode_query(SearchKind::Number, "0x1234", NumberWidth::W16, false, Endian::Little).unwrap();
        assert_eq!(p, vec![0x34, 0x12]);
    }

    #[test]
    fn rejects_overflow() {
        let r = encode_query(SearchKind::Number, "256", NumberWidth::W8, false, Endian::Little);
        assert!(matches!(r, Err(EncodeError::NumberOverflow { .. })));
    }

    #[test]
    fn forward_finds_first_match() {
        let s = src(b"abcXYabcXY");
        let p = b"XY";
        assert_eq!(find_next(&s, p, 0, false), Some(3));
    }

    #[test]
    fn forward_skips_past_cursor() {
        let s = src(b"abcXYabcXY");
        let p = b"XY";
        assert_eq!(find_next(&s, p, 4, false), Some(8));
    }

    #[test]
    fn forward_wraps_when_requested() {
        let s = src(b"abcXYabc");
        let p = b"XY";
        assert_eq!(find_next(&s, p, 6, true), Some(3));
        assert_eq!(find_next(&s, p, 6, false), None);
    }

    #[test]
    fn backward_finds_previous() {
        let s = src(b"abcXYabcXY");
        let p = b"XY";
        assert_eq!(find_prev(&s, p, 9, false), Some(3));
    }

    #[test]
    fn find_all_collects() {
        let s = src(b"abcXYabcXY");
        let p = b"XY";
        assert_eq!(find_all(&s, p), vec![3, 8]);
    }

    #[test]
    fn match_straddles_chunk_boundary() {
        // Construct a source larger than CHUNK so the pattern straddles
        // a chunk boundary; the scanner's overlap window must still
        // catch it.
        let mut bytes = vec![b'.'; (CHUNK as usize) - 1];
        bytes.extend_from_slice(b"NEEDLE");
        bytes.extend_from_slice(&vec![b'.'; 1024]);
        let s = src(&bytes);
        let p = b"NEEDLE";
        assert_eq!(find_next(&s, p, 0, false), Some((CHUNK as u64) - 1));
    }
}
