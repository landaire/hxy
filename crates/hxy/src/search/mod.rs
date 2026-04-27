//! Search bar.
//!
//! Per-file `SearchState` lives on `OpenFile`. The bar renders at the
//! bottom of a file tab when open; cross-file search runs through a
//! shared `GlobalSearchState` whose results are listed in a dedicated
//! `Tab::SearchResults`.

pub mod bar;
#[cfg(not(target_arch = "wasm32"))]
pub mod global;
#[cfg(not(target_arch = "wasm32"))]
pub mod modal;
#[cfg(not(target_arch = "wasm32"))]
pub mod replace;

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

/// Where the search may match: the whole file, or a fixed byte range
/// (set when the user opened the bar with a non-caret selection).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchScope {
    File,
    Selection { start: u64, end_exclusive: u64 },
}

impl SearchScope {
    /// Half-open `[start, end_exclusive)` window any scan should restrict
    /// to. `total` is the source length, used for the `File` arm and to
    /// clamp `Selection` so a stale selection past the current EOF can't
    /// produce an out-of-source range.
    pub fn bounds(self, total: u64) -> ByteRange {
        let (lo, hi) = match self {
            SearchScope::File => (0, total),
            SearchScope::Selection { start, end_exclusive } => (start.min(total), end_exclusive.min(total)),
        };
        let lo = lo.min(hi);
        ByteRange::new(ByteOffset::new(lo), ByteOffset::new(hi)).expect("lo <= hi by construction")
    }
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
    /// Whether the Replace input row is shown.
    pub replace_open: bool,
    /// Replacement input. Encoded with the same `kind / width / signed
    /// / endian` as the find query so a Number find round-trips into a
    /// Number replace and a Hex Bytes find into Hex Bytes.
    pub replace_query: String,
    /// Cached encoded replacement bytes. `None` on empty / error.
    pub replace_pattern: Option<Vec<u8>>,
    pub replace_error: Option<String>,
    /// Once the user has confirmed the "this will resize the file"
    /// splice prompt for the current find / replace pair, suppress
    /// it for subsequent replaces. Reset by [`Self::refresh_pattern`]
    /// and [`Self::refresh_replace_pattern`] so any change to either
    /// query reopens the prompt.
    pub splice_prompt_acked: bool,
    /// Restricts every scan -- find next, find previous, find all,
    /// replace -- to this byte window. Set to `Selection { ... }` by
    /// the host when the bar is opened with a non-caret selection;
    /// the user can also flip back to `File`.
    pub scope: SearchScope,
    /// Cross-frame side-effect queue. The bar / handler can't render
    /// toasts or modals directly (it has no `&mut HxyApp`), so it
    /// pushes intent here and the app drains it after the dock pass.
    pub pending_effects: Vec<SearchSideEffect>,
}

/// Effects raised by the search handler that the app must perform on
/// its own state (toasts, modal prompts). Drained once per frame
/// after the dock has rendered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchSideEffect {
    /// Find Next wrapped past EOF back to the start of the scope.
    WrappedForward,
    /// Find Previous wrapped past offset 0 back to the end of the scope.
    WrappedBackward,
    /// User asked to replace, find/replace differ in length, and the
    /// splice prompt has not yet been acknowledged for this pair.
    /// Carries the operation that was deferred so the modal can
    /// resume it on confirm.
    NeedsLengthMismatchAck(DeferredReplace),
    /// User asked to Replace All; show `count` confirmation modal.
    /// On confirm, the app re-issues `ReplaceAll` and the handler
    /// performs the splices.
    NeedsReplaceAllConfirm(DeferredReplaceAll),
    /// One replace was performed; payload reports it for status / toast.
    Replaced { count: usize },
}

/// In-flight Replace-Current request that's waiting on user
/// acknowledgement of the length-mismatch splice prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeferredReplace {
    pub offset: u64,
    pub find_len: u64,
    pub replace_len: u64,
}

/// In-flight Replace-All request that's waiting on the count-confirm
/// modal (and possibly the length-mismatch prompt after).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeferredReplaceAll {
    pub matches: Vec<u64>,
    pub find_len: u64,
    pub replace_len: u64,
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
            replace_open: false,
            replace_query: String::new(),
            replace_pattern: None,
            replace_error: None,
            splice_prompt_acked: false,
            scope: SearchScope::File,
            pending_effects: Vec::new(),
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
        self.splice_prompt_acked = false;
    }

    /// Re-derive `replace_pattern` and `replace_error` from the current
    /// `replace_query` and shared `kind / width / signed / endian`.
    /// Mirrors [`Self::refresh_pattern`] for the replace input. Resets
    /// the splice-prompt acknowledgement so any change to the replace
    /// query reopens the resize prompt.
    pub fn refresh_replace_pattern(&mut self) {
        self.replace_error = None;
        match encode_query(self.kind, &self.replace_query, self.width, self.signed, self.endian) {
            Ok(bytes) if bytes.is_empty() => self.replace_pattern = None,
            Ok(bytes) => self.replace_pattern = Some(bytes),
            Err(EncodeError::Empty) => self.replace_pattern = None,
            Err(e) => {
                self.replace_pattern = None;
                self.replace_error = Some(e.to_string());
            }
        }
        self.splice_prompt_acked = false;
    }

    /// Whether the current find/replace pair would change the file
    /// size. Returns `None` when either side is unset (so the caller
    /// can skip the prompt).
    pub fn replace_changes_length(&self) -> Option<bool> {
        let find = self.pattern.as_ref()?;
        let repl = self.replace_pattern.as_ref()?;
        Some(find.len() != repl.len())
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
    let mag = i128::from_str_radix(body, radix).map_err(|_| EncodeError::BadNumber { input: s.to_owned(), radix })?;
    Ok(if negative { -mag } else { mag })
}

fn parse_unsigned(s: &str) -> Result<u128, EncodeError> {
    let (radix, body) = strip_radix(s);
    u128::from_str_radix(body, radix).map_err(|_| EncodeError::BadNumber { input: s.to_owned(), radix })
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

/// One result from a directional search, plus whether the caller's
/// `wrap` request had to fire to find it. The host uses `wrapped` to
/// decide when to surface a "search wrapped" toast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchHit {
    pub offset: u64,
    pub wrapped: bool,
}

/// Sliding-window byte search restricted to `bounds`.
/// Returns the lowest match offset >= `from` inside the bounds, or
/// wraps back to `bounds.start()` if `wrap` is true. `source` is read
/// in 64 KiB chunks plus `pattern.len() - 1` bytes of overlap so a
/// match straddling two chunks isn't missed.
pub fn find_next(source: &dyn HexSource, pattern: &[u8], from: u64, wrap: bool, bounds: ByteRange) -> Option<MatchHit> {
    if pattern.is_empty() {
        return None;
    }
    let bounds = clip_bounds(bounds, source.len().get());
    let lo = bounds.start().get();
    let hi = bounds.end().get();
    if hi.saturating_sub(lo) < pattern.len() as u64 {
        return None;
    }
    let from = from.clamp(lo, hi);
    if let Some(off) = scan_forward(source, pattern, from, hi) {
        return Some(MatchHit { offset: off, wrapped: false });
    }
    if wrap && from > lo {
        let wrap_end = (from + pattern.len() as u64 - 1).min(hi);
        if let Some(off) = scan_forward(source, pattern, lo, wrap_end) {
            return Some(MatchHit { offset: off, wrapped: true });
        }
    }
    None
}

/// Reverse counterpart of [`find_next`]. Returns the largest match
/// offset < `from`, or wraps to the end of `bounds` if requested.
pub fn find_prev(source: &dyn HexSource, pattern: &[u8], from: u64, wrap: bool, bounds: ByteRange) -> Option<MatchHit> {
    if pattern.is_empty() {
        return None;
    }
    let bounds = clip_bounds(bounds, source.len().get());
    let lo = bounds.start().get();
    let hi = bounds.end().get();
    if hi.saturating_sub(lo) < pattern.len() as u64 {
        return None;
    }
    let from = from.clamp(lo, hi);
    if let Some(off) = scan_backward(source, pattern, lo, from) {
        return Some(MatchHit { offset: off, wrapped: false });
    }
    if wrap {
        let wrap_start = from.saturating_sub(pattern.len() as u64 - 1).max(lo);
        if let Some(off) = scan_backward(source, pattern, wrap_start, hi) {
            return Some(MatchHit { offset: off, wrapped: true });
        }
    }
    None
}

/// Clamp `bounds` to the live source length so a scope captured before
/// a truncating edit can't reference offsets past the current EOF.
fn clip_bounds(bounds: ByteRange, total: u64) -> ByteRange {
    let lo = bounds.start().get().min(total);
    let hi = bounds.end().get().min(total);
    let lo = lo.min(hi);
    ByteRange::new(ByteOffset::new(lo), ByteOffset::new(hi)).expect("lo <= hi by construction")
}

/// Find every match inside `bounds`, in ascending order.
pub fn find_all(source: &dyn HexSource, pattern: &[u8], bounds: ByteRange) -> Vec<u64> {
    if pattern.is_empty() {
        return Vec::new();
    }
    let bounds = clip_bounds(bounds, source.len().get());
    let lo = bounds.start().get();
    let hi = bounds.end().get();
    if hi.saturating_sub(lo) < pattern.len() as u64 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut from = lo;
    while let Some(off) = scan_forward(source, pattern, from, hi) {
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

    fn whole(s: &MemorySource) -> ByteRange {
        ByteRange::new(ByteOffset::new(0), ByteOffset::new(s.len().get())).unwrap()
    }

    fn br(start: u64, end_exclusive: u64) -> ByteRange {
        ByteRange::new(ByteOffset::new(start), ByteOffset::new(end_exclusive)).unwrap()
    }

    #[test]
    fn forward_finds_first_match() {
        let s = src(b"abcXYabcXY");
        let hit = find_next(&s, b"XY", 0, false, whole(&s)).expect("match");
        assert_eq!(hit, MatchHit { offset: 3, wrapped: false });
    }

    #[test]
    fn forward_skips_past_cursor() {
        let s = src(b"abcXYabcXY");
        let hit = find_next(&s, b"XY", 4, false, whole(&s)).expect("match");
        assert_eq!(hit, MatchHit { offset: 8, wrapped: false });
    }

    #[test]
    fn forward_wraps_when_requested() {
        let s = src(b"abcXYabc");
        let hit = find_next(&s, b"XY", 6, true, whole(&s)).expect("match");
        assert_eq!(hit, MatchHit { offset: 3, wrapped: true });
        assert_eq!(find_next(&s, b"XY", 6, false, whole(&s)), None);
    }

    #[test]
    fn backward_finds_previous() {
        let s = src(b"abcXYabcXY");
        let hit = find_prev(&s, b"XY", 9, false, whole(&s)).expect("match");
        assert_eq!(hit, MatchHit { offset: 3, wrapped: false });
    }

    #[test]
    fn backward_wraps_when_requested() {
        let s = src(b"XYabc");
        let hit = find_prev(&s, b"XY", 0, true, whole(&s)).expect("match");
        assert_eq!(hit, MatchHit { offset: 0, wrapped: true });
    }

    #[test]
    fn find_all_collects() {
        let s = src(b"abcXYabcXY");
        assert_eq!(find_all(&s, b"XY", whole(&s)), vec![3, 8]);
    }

    #[test]
    fn find_all_respects_bounds() {
        let s = src(b"XYabcXYdefXY");
        assert_eq!(find_all(&s, b"XY", br(3, 10)), vec![5]);
    }

    #[test]
    fn next_inside_selection_scope() {
        let s = src(b"XYabcXYdefXY");
        let hit = find_next(&s, b"XY", 0, false, br(3, 10)).expect("scoped match");
        assert_eq!(hit.offset, 5);
        assert_eq!(find_next(&s, b"XY", 7, false, br(3, 10)), None);
    }

    #[test]
    fn match_straddles_chunk_boundary() {
        let mut bytes = vec![b'.'; (CHUNK as usize) - 1];
        bytes.extend_from_slice(b"NEEDLE");
        bytes.extend_from_slice(&vec![b'.'; 1024]);
        let s = src(&bytes);
        let hit = find_next(&s, b"NEEDLE", 0, false, whole(&s)).expect("match");
        assert_eq!(hit.offset, CHUNK - 1);
    }
}
