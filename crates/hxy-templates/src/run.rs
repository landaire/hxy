//! Pure template-run helpers shared by the frontends' runners:
//! sub-range binding, offset re-anchoring, and source fingerprinting.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use hxy_core::ByteLen;
use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_plugin_host::ParsedTemplate;
use hxy_plugin_host::template::Diagnostic;
use hxy_plugin_host::template::Node;
use hxy_plugin_host::template::ResultTree;
use hxy_vfs::HandlerError;

/// Hash an expanded template source. The result is paired with each
/// run's resulting [`TemplateInstance`] and persisted; on restart we
/// re-hash the source we're about to feed the worker and only carry
/// over color overrides if the hashes match.
pub fn fingerprint_template_source(source: &str) -> Option<[u8; 32]> {
    Some(*blake3::hash(source.as_bytes()).as_bytes())
}

/// View a sub-range of an inner [`HexSource`] as if it were the whole
/// thing. Reads at offset `0` of the wrapper map to `base` of the
/// inner; `len()` is the sub-range's length. Used so a template
/// runtime sees the slice as offsets `[0, len)` and emits node spans
/// rooted at `0`. The runner re-anchors those spans to the real file
/// via [`OffsetAdjustedTemplate`].
pub struct SubrangeSource {
    inner: Arc<dyn HexSource>,
    base: ByteOffset,
    len: ByteLen,
}

impl SubrangeSource {
    pub fn new(inner: Arc<dyn HexSource>, range: ByteRange) -> Self {
        Self { inner, base: range.start(), len: range.len() }
    }
}

impl HexSource for SubrangeSource {
    fn len(&self) -> ByteLen {
        self.len
    }

    fn read(&self, range: ByteRange) -> Result<Vec<u8>, hxy_core::Error> {
        if range.end().get() > self.len.get() {
            return Err(hxy_core::Error::OutOfBounds { range, len: ByteOffset::new(self.len.get()) });
        }
        let inner_start = ByteOffset::new(self.base.get() + range.start().get());
        let inner_end = ByteOffset::new(self.base.get() + range.end().get());
        let inner_range = ByteRange::new(inner_start, inner_end)?;
        self.inner.read(inner_range)
    }
}

/// Wrap a [`ParsedTemplate`] so every emitted node's `span.offset` and
/// every diagnostic's `file_offset` is shifted by `base`. Lets the
/// rest of the app treat template node offsets as file-absolute
/// regardless of whether the template was bound to a slice.
pub struct OffsetAdjustedTemplate {
    pub inner: Arc<dyn ParsedTemplate>,
    pub base: u64,
}

impl ParsedTemplate for OffsetAdjustedTemplate {
    fn execute(&self, args: &[hxy_plugin_host::template::Arg]) -> Result<ResultTree, HandlerError> {
        let mut tree = self.inner.execute(args)?;
        adjust_tree(&mut tree, self.base);
        Ok(tree)
    }

    fn expand_array(&self, array_id: u64, start: u64, end: u64) -> Result<Vec<Node>, HandlerError> {
        let mut nodes = self.inner.expand_array(array_id, start, end)?;
        for node in &mut nodes {
            adjust_node_span(node, self.base);
        }
        Ok(nodes)
    }
}

fn adjust_tree(tree: &mut ResultTree, base: u64) {
    if base == 0 {
        return;
    }
    for node in &mut tree.nodes {
        adjust_node_span(node, base);
    }
    for diag in &mut tree.diagnostics {
        adjust_diagnostic(diag, base);
    }
}

fn adjust_node_span(node: &mut Node, base: u64) {
    node.span.offset = node.span.offset.saturating_add(base);
}

fn adjust_diagnostic(diag: &mut Diagnostic, base: u64) {
    if let Some(off) = diag.file_offset {
        diag.file_offset = Some(off.saturating_add(base));
    }
}
