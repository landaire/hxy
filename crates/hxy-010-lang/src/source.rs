//! Abstraction over the byte source a template runs against.
//!
//! The real integration talks to WIT's `source` interface via
//! wasmtime, but the interpreter itself is language-agnostic and
//! doesn't know about that. Any implementation of [`HexSource`] will
//! do — including an in-memory `Vec<u8>` for tests.

use std::cell::RefCell;

use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum SourceError {
    #[error("read past end of source: requested [{offset}..{end}), source is {len} bytes")]
    OutOfBounds { offset: u64, end: u64, len: u64 },

    #[error("underlying host error: {0}")]
    Host(String),
}

/// Something the interpreter can read bytes from.
pub trait HexSource {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read `length` bytes at `offset`. Out-of-range reads return
    /// [`SourceError::OutOfBounds`] — the interpreter turns this into
    /// a diagnostic rather than a trap.
    fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, SourceError>;
}

/// In-memory [`HexSource`] backed by a `Vec<u8>`. Used by tests.
#[derive(Debug)]
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
        if end > self.len() {
            return Err(SourceError::OutOfBounds { offset, end, len: self.len() });
        }
        let start = offset as usize;
        let end_usize = end as usize;
        Ok(self.bytes[start..end_usize].to_vec())
    }
}

/// Convenience wrapper used inside the interpreter to track the
/// current `FTell()` position. Held behind a `RefCell` so evaluation
/// functions can stay `&self`.
#[derive(Debug)]
pub struct Cursor<S: HexSource> {
    pub(crate) source: S,
    pub(crate) pos: RefCell<u64>,
}

impl<S: HexSource> Cursor<S> {
    pub fn new(source: S) -> Self {
        Self { source, pos: RefCell::new(0) }
    }

    pub fn len(&self) -> u64 {
        self.source.len()
    }

    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    pub fn tell(&self) -> u64 {
        *self.pos.borrow()
    }

    pub fn seek(&self, offset: u64) {
        *self.pos.borrow_mut() = offset;
    }

    pub fn at_eof(&self) -> bool {
        self.tell() >= self.len()
    }

    /// Read `length` bytes starting at the current position and
    /// advance the cursor.
    pub fn read_advance(&self, length: u64) -> Result<Vec<u8>, SourceError> {
        let offset = self.tell();
        let bytes = self.source.read(offset, length)?;
        self.seek(offset + length);
        Ok(bytes)
    }

    /// Read `length` bytes at `offset` without moving the cursor.
    pub fn read_at(&self, offset: u64, length: u64) -> Result<Vec<u8>, SourceError> {
        self.source.read(offset, length)
    }
}
