use serde::Deserialize;
use serde::Serialize;

use crate::geometry::ByteOffset;
use crate::geometry::ByteRange;

/// A contiguous byte selection with an anchor (the byte that didn't move when
/// the selection was last extended) and a cursor (the byte that did).
///
/// The range returned by [`Selection::range`] normalises these so it's always
/// `anchor..cursor` or `cursor..anchor`, whichever orders correctly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub anchor: ByteOffset,
    pub cursor: ByteOffset,
}

impl Selection {
    pub fn caret(at: ByteOffset) -> Self {
        Self { anchor: at, cursor: at }
    }

    pub fn range(self) -> ByteRange {
        let (start, end) =
            if self.anchor <= self.cursor { (self.anchor, self.cursor) } else { (self.cursor, self.anchor) };
        // SAFETY-equivalent: constructor orders start <= end.
        ByteRange::new(start, end).expect("ordered by construction")
    }

    pub fn is_caret(self) -> bool {
        self.anchor == self.cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_is_empty_range() {
        let s = Selection::caret(ByteOffset(10));
        assert!(s.is_caret());
        assert!(s.range().is_empty());
    }

    #[test]
    fn reversed_selection_normalises() {
        let s = Selection { anchor: ByteOffset(20), cursor: ByteOffset(5) };
        let r = s.range();
        assert_eq!(r.start(), ByteOffset(5));
        assert_eq!(r.end(), ByteOffset(20));
    }
}
