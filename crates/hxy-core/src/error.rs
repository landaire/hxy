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

    /// Source needs the chunk(s) covering the requested range to be
    /// async-primed before this sync read can succeed. Currently only
    /// raised by the wasm `Blob`-backed source, where reads must hop
    /// through `FileReader.readAsArrayBuffer` (async) before the bytes
    /// are visible to the sync `HexSource::read` API.
    #[error("source not primed for {range} (chunk {missing_chunk} missing)")]
    NotPrimed { range: ByteRange, missing_chunk: u64 },
}
