use std::io;

use thiserror::Error;

use crate::geometry::ByteOffset;
use crate::geometry::ByteRange;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("range {range} exceeds source length {len}")]
    OutOfBounds { range: ByteRange, len: ByteOffset },

    #[error("invalid range: start {start} > end {end}")]
    InvalidRange { start: ByteOffset, end: ByteOffset },

    #[error("column count must be non-zero")]
    ZeroColumns,

    #[error("I/O error while reading {range}")]
    Io {
        range: ByteRange,
        #[source]
        source: io::Error,
    },
}
