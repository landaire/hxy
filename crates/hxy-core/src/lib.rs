//! Core types and file abstractions for the hxy hex editor.
//!
//! Everything here is sans-io: the types describe ranges, offsets, and
//! geometry; the [`HexSource`] trait abstracts read access over an arbitrary
//! backing store (in-memory buffer, file on disk, entry inside a zip, etc.).

#![forbid(unsafe_code)]

mod error;
mod geometry;
mod selection;
mod source;

pub use error::Error;
pub use error::Result;
pub use geometry::ByteLen;
pub use geometry::ByteOffset;
pub use geometry::ByteRange;
pub use geometry::ColumnCount;
pub use geometry::RowIndex;
pub use selection::Selection;
pub use source::HexSource;
pub use source::MemorySource;
pub use source::ReadAtSource;
