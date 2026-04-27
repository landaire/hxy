//! Offset / range parsing for the Go-To and Select palette commands.
//!
//! Input syntax:
//! - Decimal: `100`
//! - Hex: `0x100` / `0X100`
//! - Relative: `+100`, `-0x10` (to be resolved against a caller-
//!   supplied anchor, usually the current cursor)
//!
//! Range input is `<start>, <end>` or `<start>..<end>`. At most one
//! of the two endpoints may be relative; if both are, the resolved
//! range has no unambiguous anchor and parsing fails.

#![cfg(not(target_arch = "wasm32"))]

use thiserror::Error;

/// One parsed number with its absolute / relative flavour intact so
/// callers can resolve it against a cursor when needed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Number {
    Absolute(u64),
    /// Signed offset relative to some caller-supplied anchor (e.g.
    /// the current cursor). `i64` is enough for any reasonable file
    /// on modern hosts; hitting the min / max here would require a
    /// 16-EB buffer that we couldn't load anyway.
    Relative(i64),
}

impl Number {
    /// Resolve against `anchor`, clamped into `0..=max`. Returns
    /// `None` if the result would overflow the range.
    pub fn resolve(self, anchor: u64, max: u64) -> Option<u64> {
        let raw: i128 = match self {
            Number::Absolute(v) => v as i128,
            Number::Relative(delta) => anchor as i128 + delta as i128,
        };
        if !(0..=max as i128).contains(&raw) {
            return None;
        }
        Some(raw as u64)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("empty input")]
    Empty,
    #[error("not a number: {0:?}")]
    NotANumber(String),
    #[error("offset out of range for this file")]
    OutOfRange,
    #[error("a range needs a separator (`,` or `..`)")]
    MissingSeparator,
    #[error("both endpoints are relative; at least one must be absolute")]
    BothRelative,
}

pub fn parse_number(input: &str) -> Result<Number, ParseError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(ParseError::Empty);
    }
    if let Some(rest) = s.strip_prefix('+') {
        let v = parse_unsigned(rest.trim())?;
        let signed = i64::try_from(v).map_err(|_| ParseError::OutOfRange)?;
        Ok(Number::Relative(signed))
    } else if let Some(rest) = s.strip_prefix('-') {
        let v = parse_unsigned(rest.trim())?;
        let signed = i64::try_from(v).map_err(|_| ParseError::OutOfRange)?;
        Ok(Number::Relative(-signed))
    } else {
        Ok(Number::Absolute(parse_unsigned(s)?))
    }
}

fn parse_unsigned(s: &str) -> Result<u64, ParseError> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|_| ParseError::NotANumber(s.to_owned()))
    } else {
        s.parse::<u64>().map_err(|_| ParseError::NotANumber(s.to_owned()))
    }
}

/// A parsed range from the Select Range command, after resolving
/// whichever endpoint was relative against the other. `start` is
/// inclusive, `end` is exclusive -- matching [`hxy_core::ByteRange`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedRange {
    pub start: u64,
    pub end_exclusive: u64,
}

impl ResolvedRange {
    pub fn len(self) -> u64 {
        self.end_exclusive.saturating_sub(self.start)
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
}

/// Parse `"<start><sep><end>"` with Rust-style range separators:
///
/// | Separator | End semantics       |
/// |-----------|---------------------|
/// | `..`      | exclusive (default) |
/// | `..=`     | inclusive           |
/// | `,`       | exclusive (alias)   |
///
/// One endpoint may be relative; both being relative is rejected
/// because there is no anchor to measure them against. The absolute
/// endpoint anchors the relative one -- `"100, +50"` spans
/// `100..150`, `"+100, 200"` spans `(cursor+100)..200`, etc.
/// `source_len` bounds the final resolved offsets.
pub fn parse_range(input: &str, source_len: u64) -> Result<ResolvedRange, ParseError> {
    let s = input.trim();
    // Order matters: `..=` must be tried before `..` or the longer
    // form gets consumed by the shorter one.
    let (raw_start, raw_end, inclusive) = if let Some((a, b)) = s.split_once("..=") {
        (a.trim(), b.trim(), true)
    } else if let Some((a, b)) = s.split_once("..") {
        (a.trim(), b.trim(), false)
    } else if let Some((a, b)) = s.split_once(',') {
        (a.trim(), b.trim(), false)
    } else {
        return Err(ParseError::MissingSeparator);
    };
    let start = parse_number(raw_start)?;
    let end = parse_number(raw_end)?;
    let (start_abs, end_abs) = match (start, end) {
        (Number::Absolute(s), Number::Absolute(e)) => (s, e),
        (Number::Absolute(s), Number::Relative(d)) => {
            let end = i128::from(s) + i128::from(d);
            if !(0..=i128::from(source_len)).contains(&end) {
                return Err(ParseError::OutOfRange);
            }
            (s, end as u64)
        }
        (Number::Relative(d), Number::Absolute(e)) => {
            let start = i128::from(e) + i128::from(d);
            if !(0..=i128::from(source_len)).contains(&start) {
                return Err(ParseError::OutOfRange);
            }
            (start as u64, e)
        }
        (Number::Relative(_), Number::Relative(_)) => return Err(ParseError::BothRelative),
    };
    let max = source_len;
    if start_abs > max || end_abs > max {
        return Err(ParseError::OutOfRange);
    }
    // Accept either ordering and normalise so lo <= hi, then turn
    // an inclusive end into the equivalent exclusive index by
    // adding one byte. `..=source_len` would overflow the buffer so
    // reject it explicitly.
    let (lo, hi) = if start_abs <= end_abs { (start_abs, end_abs) } else { (end_abs, start_abs) };
    let end_exclusive = if inclusive {
        if hi >= source_len {
            return Err(ParseError::OutOfRange);
        }
        hi + 1
    } else {
        hi
    };
    Ok(ResolvedRange { start: lo, end_exclusive })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_decimal_and_hex() {
        assert_eq!(parse_number("0"), Ok(Number::Absolute(0)));
        assert_eq!(parse_number(" 100 "), Ok(Number::Absolute(100)));
        assert_eq!(parse_number("0x10"), Ok(Number::Absolute(16)));
        assert_eq!(parse_number("0XFF"), Ok(Number::Absolute(255)));
    }

    #[test]
    fn plus_minus_prefix() {
        assert_eq!(parse_number("+10"), Ok(Number::Relative(10)));
        assert_eq!(parse_number("-0x10"), Ok(Number::Relative(-16)));
        assert_eq!(parse_number("+ 5"), Ok(Number::Relative(5)));
    }

    #[test]
    fn resolve_against_cursor() {
        assert_eq!(Number::Relative(10).resolve(100, 4096), Some(110));
        assert_eq!(Number::Relative(-50).resolve(100, 4096), Some(50));
        assert_eq!(Number::Absolute(200).resolve(100, 4096), Some(200));
        assert_eq!(Number::Relative(-200).resolve(100, 4096), None);
        assert_eq!(Number::Absolute(5000).resolve(100, 4096), None);
    }

    #[test]
    fn parse_range_accepts_comma_and_exclusive_range() {
        let r = parse_range("100, 200", 4096).unwrap();
        assert_eq!(r.start, 100);
        assert_eq!(r.end_exclusive, 200);
        let r = parse_range("100..200", 4096).unwrap();
        assert_eq!(r.start, 100);
        assert_eq!(r.end_exclusive, 200);
    }

    #[test]
    fn parse_range_inclusive_form_bumps_end_by_one() {
        let r = parse_range("100..=200", 4096).unwrap();
        assert_eq!(r.start, 100);
        assert_eq!(r.end_exclusive, 201);
        assert_eq!(r.len(), 101);
    }

    #[test]
    fn parse_range_inclusive_with_relative_endpoint() {
        let r = parse_range("100..=+4", 4096).unwrap();
        assert_eq!(r.start, 100);
        assert_eq!(r.end_exclusive, 105);
        assert_eq!(r.len(), 5);
    }

    #[test]
    fn parse_range_inclusive_past_source_len_rejected() {
        // `0..=source_len-1` is fine (covers the whole file); the
        // `..=source_len` form would try to include one byte past
        // the end, which isn't addressable.
        assert!(parse_range("0..=99", 100).is_ok());
        assert_eq!(parse_range("0..=100", 100), Err(ParseError::OutOfRange));
    }

    #[test]
    fn parse_range_resolves_single_relative_endpoint() {
        // end relative to start.
        let r = parse_range("100, +50", 4096).unwrap();
        assert_eq!(r.start, 100);
        assert_eq!(r.end_exclusive, 150);
        // start relative to end.
        let r = parse_range("-10, 100", 4096).unwrap();
        assert_eq!(r.start, 90);
        assert_eq!(r.end_exclusive, 100);
    }

    #[test]
    fn parse_range_normalises_swapped_endpoints() {
        let r = parse_range("200, 100", 4096).unwrap();
        assert_eq!(r.start, 100);
        assert_eq!(r.end_exclusive, 200);
    }

    #[test]
    fn parse_range_rejects_both_relative() {
        assert_eq!(parse_range("+10, +20", 4096), Err(ParseError::BothRelative));
    }

    #[test]
    fn parse_range_requires_separator() {
        assert_eq!(parse_range("100", 4096), Err(ParseError::MissingSeparator));
    }
}
