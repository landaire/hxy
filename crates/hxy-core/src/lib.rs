//! Core types and file abstractions for the hxy hex editor.
//!
//! Everything here is sans-io: the types describe ranges, offsets, and
//! geometry; the [`HexSource`] trait abstracts read access over an arbitrary
//! backing store (in-memory buffer, file on disk, entry inside a zip, etc.).

#![forbid(unsafe_code)]

mod cache;
mod error;
mod geometry;
mod patched;
mod selection;
mod source;

pub use cache::Attribution;
pub use cache::AttributionBytes;
pub use cache::ByteCache;
pub use cache::CHUNK_SIZE_BYTES;
pub use cache::CacheLimit;
pub use cache::CacheStats;
pub use cache::CachedSource;
pub use cache::ChunkIndex;
pub use cache::ChunkSize;
pub use cache::HexViewKey;
pub use cache::PluginKey;
pub use cache::SourceId;
pub use cache::TemplateKey;
pub use error::Error;
pub use error::Result;
pub use geometry::ByteLen;
pub use geometry::ByteOffset;
pub use geometry::ByteRange;
pub use geometry::ColumnCount;
pub use geometry::RowIndex;
pub use patched::PatchedSource;
pub use selection::Selection;
pub use source::HexSource;
pub use source::MemorySource;
pub use source::ReadAtSource;
