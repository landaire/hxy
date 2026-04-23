//! Host-side state and `source` import implementation. One [`HostState`]
//! lives per [`Store`](wasmtime::Store); it holds the [`HexSource`] the
//! plugin is reading from and satisfies the imported `source` interface
//! the plugin calls back into.

use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;

use crate::bindings::hxy::vfs::source::Host as SourceHost;

pub struct HostState {
    pub source: Arc<dyn HexSource>,
}

impl HostState {
    pub fn new(source: Arc<dyn HexSource>) -> Self {
        Self { source }
    }
}

impl SourceHost for HostState {
    fn len(&mut self) -> u64 {
        self.source.len().get()
    }

    fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let total = self.source.len().get();
        let start = offset.min(total);
        let end = offset.saturating_add(length).min(total);
        let range = ByteRange::new(ByteOffset::new(start), ByteOffset::new(end))
            .map_err(|e| format!("invalid range {start}..{end}: {e}"))?;
        self.source.read(range).map_err(|e| e.to_string())
    }
}
