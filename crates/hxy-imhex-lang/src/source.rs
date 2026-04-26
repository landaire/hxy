//! Byte-source abstraction the interpreter reads from. Mirrors the
//! [`hxy_010_lang::HexSource`] trait so the same host adapter shape
//! (`MemorySource` + a shim from `hxy_core::HexSource`) carries over.
//! We don't depend on `hxy-core` directly to keep this crate usable
//! standalone (fuzzing, future tooling); the host adapter does the
//! tiny conversion.

use thiserror::Error;

#[derive(Clone, Debug, Error, PartialEq)]
pub enum SourceError {
    #[error("read past end of source: offset={offset} end={end} len={len}")]
    OutOfBounds { offset: u64, end: u64, len: u64 },

    /// Wrapper for whatever the host returns. Lets the interpreter
    /// surface read errors without coupling to the host's error
    /// taxonomy.
    #[error("host read failed: {0}")]
    Host(String),
}

pub trait HexSource: Send + Sync {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, SourceError>;
}

/// In-memory byte source. The first cut tests use this directly; the
/// host wraps it through a shim trait.
pub struct MemorySource {
    bytes: Vec<u8>,
}

impl MemorySource {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

impl HexSource for MemorySource {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, SourceError> {
        let end = offset.saturating_add(length);
        let total = self.len();
        if end > total {
            return Err(SourceError::OutOfBounds { offset, end, len: total });
        }
        Ok(self.bytes[offset as usize..end as usize].to_vec())
    }
}

// The interpreter manages its own cursor offset on `self.pos` --
// having a borrowed [`Cursor`] type over the source conflicted with
// `&mut self` execution. The earlier draft of this file carried
// such a type; the refactor that landed it on the interpreter let
// us delete the trait-internal cursor wrapper. If a future phase
// needs a borrowed cursor (e.g. for a streaming variant), pull it
// back here and call through `HexSource::read` for the actual I/O.
