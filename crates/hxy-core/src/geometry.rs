use std::fmt;
use std::num::NonZeroU16;
use std::ops::Range;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;

/// Absolute byte offset within a [`HexSource`](crate::HexSource).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(transparent)]
pub struct ByteOffset(pub u64);

impl ByteOffset {
    pub const ZERO: Self = Self(0);

    pub fn new(offset: u64) -> Self {
        Self(offset)
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn checked_add_len(self, len: ByteLen) -> Option<Self> {
        self.0.checked_add(len.0).map(Self)
    }

    pub fn saturating_add_len(self, len: ByteLen) -> Self {
        Self(self.0.saturating_add(len.0))
    }
}

impl fmt::Display for ByteOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:X}", self.0)
    }
}

impl From<u64> for ByteOffset {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// Length of a byte range (distinct from [`ByteOffset`] so they can't be
/// confused at call sites).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(transparent)]
pub struct ByteLen(pub u64);

impl ByteLen {
    pub const ZERO: Self = Self(0);

    pub fn new(len: u64) -> Self {
        Self(len)
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for ByteLen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

impl From<u64> for ByteLen {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// Half-open byte range `[start, end)`.
///
/// Validated on construction: `start <= end` is an invariant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ByteRange {
    start: ByteOffset,
    end: ByteOffset,
}

impl ByteRange {
    pub fn new(start: ByteOffset, end: ByteOffset) -> Result<Self, Error> {
        if start > end {
            return Err(Error::InvalidRange { start, end });
        }
        Ok(Self { start, end })
    }

    pub fn from_offset_and_len(start: ByteOffset, len: ByteLen) -> Result<Self, Error> {
        let end = start.checked_add_len(len).ok_or(Error::InvalidRange { start, end: ByteOffset(u64::MAX) })?;
        Ok(Self { start, end })
    }

    pub fn start(self) -> ByteOffset {
        self.start
    }

    pub fn end(self) -> ByteOffset {
        self.end
    }

    pub fn len(self) -> ByteLen {
        ByteLen(self.end.0 - self.start.0)
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub fn contains(self, offset: ByteOffset) -> bool {
        self.start <= offset && offset < self.end
    }

    pub fn as_u64_range(self) -> Range<u64> {
        self.start.0..self.end.0
    }
}

impl fmt::Display for ByteRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

/// Number of hex columns rendered per row. Non-zero.
///
/// hxy defaults to `16` columns. Exposed as a newtype so callers can't
/// accidentally pass a row-index where a column-count is expected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct ColumnCount(NonZeroU16);

impl ColumnCount {
    pub const DEFAULT: Self = match NonZeroU16::new(16) {
        Some(n) => Self(n),
        None => unreachable!(),
    };

    pub fn new(cols: u16) -> Result<Self, Error> {
        NonZeroU16::new(cols).map(Self).ok_or(Error::ZeroColumns)
    }

    pub fn get(self) -> u16 {
        self.0.get()
    }

    pub fn as_u64(self) -> u64 {
        u64::from(self.0.get())
    }
}

impl Default for ColumnCount {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Row index in a hex view. Row `n` starts at byte offset `n * columns`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(transparent)]
pub struct RowIndex(pub u64);

impl RowIndex {
    pub fn new(row: u64) -> Self {
        Self(row)
    }

    pub fn get(self) -> u64 {
        self.0
    }

    /// Byte offset of the first cell in this row, given the column count.
    pub fn start_offset(self, columns: ColumnCount) -> ByteOffset {
        ByteOffset(self.0.saturating_mul(columns.as_u64()))
    }
}

impl fmt::Display for RowIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_invariant() {
        assert!(ByteRange::new(ByteOffset(5), ByteOffset(3)).is_err());
        let r = ByteRange::new(ByteOffset(0), ByteOffset(10)).unwrap();
        assert_eq!(r.len().get(), 10);
        assert!(!r.is_empty());
        assert!(r.contains(ByteOffset(0)));
        assert!(r.contains(ByteOffset(9)));
        assert!(!r.contains(ByteOffset(10)));
    }

    #[test]
    fn row_start_offset_with_default_columns() {
        let row = RowIndex::new(3);
        assert_eq!(row.start_offset(ColumnCount::DEFAULT), ByteOffset(48));
    }

    #[test]
    fn column_count_rejects_zero() {
        assert!(ColumnCount::new(0).is_err());
        assert_eq!(ColumnCount::new(32).unwrap().get(), 32);
    }
}
