use std::sync::Arc;

use crate::handler::VfsHandler;

/// Collection of handlers consulted on file open. Handlers are tried in
/// registration order; the first whose `matches` returns true wins.
/// Consumers register native handlers directly and (later) wasm
/// plugins loaded from the plugin directory.
#[derive(Clone, Default)]
pub struct VfsRegistry {
    handlers: Vec<Arc<dyn VfsHandler>>,
}

impl VfsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, handler: Arc<dyn VfsHandler>) {
        self.handlers.push(handler);
    }

    pub fn handlers(&self) -> &[Arc<dyn VfsHandler>] {
        &self.handlers
    }

    /// Find the first handler that claims it can mount `head`. `head`
    /// should typically be the source's first ~4 KiB.
    pub fn detect(&self, head: &[u8]) -> Option<Arc<dyn VfsHandler>> {
        self.handlers.iter().find(|h| h.matches(head)).cloned()
    }
}
