use serde::Deserialize;
use serde::Serialize;

use crate::geometry::ByteOffset;
use crate::geometry::ByteRange;

/// A byte selection with an anchor (the byte that didn't move when the
/// selection was last extended) and a cursor (the byte that did). The
/// range returned by [`Selection::range`] covers every byte from the
/// lower endpoint up to and including the upper one -- a caret (anchor
/// == cursor) is therefore a one-byte range, not an empty one.
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
        let (start, end_inclusive) =
            if self.anchor <= self.cursor { (self.anchor, self.cursor) } else { (self.cursor, self.anchor) };
        let end_exclusive = ByteOffset::new(end_inclusive.get().saturating_add(1));
        ByteRange::new(start, end_exclusive).expect("ordered by construction")
    }

    pub fn is_caret(self) -> bool {
        self.anchor == self.cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_covers_one_byte() {
        let s = Selection::caret(ByteOffset(10));
        assert!(s.is_caret());
        assert_eq!(s.range().len().get(), 1);
        assert_eq!(s.range().start(), ByteOffset(10));
        assert_eq!(s.range().end(), ByteOffset(11));
    }

    #[test]
    fn reversed_selection_includes_both_endpoints() {
        let s = Selection { anchor: ByteOffset(20), cursor: ByteOffset(5) };
        let r = s.range();
        assert_eq!(r.start(), ByteOffset(5));
        assert_eq!(r.end(), ByteOffset(21));
        assert_eq!(r.len().get(), 16);
    }
}
