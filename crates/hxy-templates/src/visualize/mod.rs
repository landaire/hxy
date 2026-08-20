//! Pure visualizer core shared by the frontends.
//!
//! ImHex's `[[hex::visualize("name", arg1, arg2, ...)]]` attribute
//! turns a field into a renderable artifact: an image, a waveform,
//! a disassembly. Any runtime that emits the canonical `hxy_visualize`
//! / `hxy_inline_visualize` attribute (see
//! [`hxy_plugin_host::VISUALIZE_ATTR`]) drives the same dispatch, so
//! a 010 plugin or a future WASM template gets the visualizers for
//! free.
//!
//! The attribute value is a packed string -- `name<US>arg1<US>arg2`
//! where `<US>` is ASCII 0x1F (see
//! [`hxy_plugin_host::VISUALIZE_ARG_SEP`]). [`VisualizerSpec::parse`]
//! splits it back apart for the renderer.
//!
//! This module holds the decode/transform half of every visualizer:
//! spec parsing, target collection, and the per-kind byte-to-artifact
//! functions in the submodules. Texture upload, plotting, and painting
//! stay in each frontend.

pub mod bitmap;
pub mod digram;
pub mod distribution;
pub mod image;

use std::sync::Arc;

use hxy_core::HexSource;
use hxy_plugin_host::INLINE_VISUALIZE_ATTR;
use hxy_plugin_host::VISUALIZE_ARG_SEP;
use hxy_plugin_host::VISUALIZE_ATTR;
use hxy_plugin_host::template::Node;
use hxy_plugin_host::template::ResultTree;

use crate::state::TemplateInstance;
use crate::state::TemplateInstanceId;
use crate::state::TemplateNodeIdx;

/// Decoded visualizer attribute: the named kind plus the user-
/// supplied args. `Unknown` covers unregistered names so the panel
/// can render a clear "not yet supported" placeholder rather than
/// silently dropping the field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualizerSpec {
    pub kind: VisualizerKind,
    /// Raw post-name args, in source order. Visualizer-specific
    /// parsing (number coercion, format-string lookup) happens
    /// inside each renderer; the spec just hands them through.
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisualizerKind {
    Image,
    Bitmap,
    HexViewer,
    Text,
    ChunkEntropy,
    Digram,
    LayeredDistribution,
    LinePlot,
    BarChart,
    ScatterPlot,
    Sound,
    Disassembler,
    Coordinates,
    Timestamp,
    Table,
    ThreeD,
    Unknown(String),
}

impl VisualizerKind {
    /// Short label for the sub-tab strip / row icon tooltip.
    pub fn label(&self) -> &str {
        match self {
            Self::Image => "image",
            Self::Bitmap => "bitmap",
            Self::HexViewer => "hex_viewer",
            Self::Text => "text",
            Self::ChunkEntropy => "chunk_entropy",
            Self::Digram => "digram",
            Self::LayeredDistribution => "layered_distribution",
            Self::LinePlot => "line_plot",
            Self::BarChart => "bar_chart",
            Self::ScatterPlot => "scatter_plot",
            Self::Sound => "sound",
            Self::Disassembler => "disassembler",
            Self::Coordinates => "coordinates",
            Self::Timestamp => "timestamp",
            Self::Table => "table",
            Self::ThreeD => "3d",
            Self::Unknown(name) => name.as_str(),
        }
    }
}

impl VisualizerSpec {
    /// Parse a packed `name<US>arg1<US>...` attribute value back into
    /// a kind + args list. An empty string returns `None` (caller
    /// treats the attribute as absent); a non-empty value with an
    /// empty name produces `Unknown("")` so the panel can surface
    /// the malformed attribute instead of silently ignoring it.
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.is_empty() {
            return None;
        }
        let mut parts = raw.split(VISUALIZE_ARG_SEP);
        let name = parts.next()?.to_owned();
        let args: Vec<String> = parts.map(|s| s.to_owned()).collect();
        let kind = match name.as_str() {
            "image" => VisualizerKind::Image,
            "bitmap" => VisualizerKind::Bitmap,
            "hex_viewer" => VisualizerKind::HexViewer,
            "text" => VisualizerKind::Text,
            "chunk_entropy" => VisualizerKind::ChunkEntropy,
            "digram" => VisualizerKind::Digram,
            "layered_distribution" => VisualizerKind::LayeredDistribution,
            "line_plot" => VisualizerKind::LinePlot,
            "bar_chart" => VisualizerKind::BarChart,
            "scatter_plot" => VisualizerKind::ScatterPlot,
            "sound" => VisualizerKind::Sound,
            "disassembler" => VisualizerKind::Disassembler,
            "coordinates" => VisualizerKind::Coordinates,
            "timestamp" => VisualizerKind::Timestamp,
            "table" => VisualizerKind::Table,
            "3d" => VisualizerKind::ThreeD,
            other => VisualizerKind::Unknown(other.to_owned()),
        };
        Some(Self { kind, args })
    }
}

/// Read the visualizer / inline_visualizer attribute off `node` (if
/// present) and parse it. Returns the spec and whether it was the
/// inline variant. `None` when the node has neither attribute.
pub fn read_node_visualizer(node: &Node) -> Option<(VisualizerSpec, Inline)> {
    if let Some(spec) = lookup_visualizer(node, VISUALIZE_ATTR) {
        return Some((spec, Inline::No));
    }
    if let Some(spec) = lookup_visualizer(node, INLINE_VISUALIZE_ATTR) {
        return Some((spec, Inline::Yes));
    }
    None
}

fn lookup_visualizer(node: &Node, key: &str) -> Option<VisualizerSpec> {
    let raw = node.attributes.iter().find_map(|(k, v)| (k == key).then_some(v.as_str()))?;
    VisualizerSpec::parse(raw)
}

/// Whether a visualizer was declared as the inline variant. Inline
/// visualizers also render in the template-panel row (small thumbnail);
/// the popout tab still applies for both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inline {
    No,
    Yes,
}

/// Identity of one visualizer instance: the template instance + tree
/// node it lives on. Stable for the lifetime of the template run; a
/// re-run that drops the node also drops the cache entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VisualizerKey {
    pub instance: TemplateInstanceId,
    pub node: TemplateNodeIdx,
}

/// A visualizer-bearing field discovered in a template result tree.
/// One per node that carries a visualizer attribute; a node with both
/// `hxy_visualize` and `hxy_inline_visualize` only emits the popout
/// here (the inline marker is handled by the template panel).
pub struct VisualizerTarget {
    pub key: VisualizerKey,
    pub spec: VisualizerSpec,
    /// Display name for the sub-tab strip: the field's localized
    /// name (or `[idx]` for unnamed array elements). Built once when
    /// the target is collected so the strip render doesn't re-walk
    /// the tree.
    pub label: String,
    pub byte_offset: u64,
    pub byte_length: u64,
}

/// Walk a file's completed template instances and return every
/// visualizer target across all of them. Used by the panels to
/// populate their sub-tab strips and by the auto-open path to decide
/// whether to surface the tab at all.
pub fn collect_targets(instances: &[TemplateInstance]) -> Vec<VisualizerTarget> {
    let mut out = Vec::new();
    for instance in instances {
        collect_from_tree(instance.id, &instance.state.tree, &mut out);
    }
    out
}

fn collect_from_tree(instance: TemplateInstanceId, tree: &ResultTree, out: &mut Vec<VisualizerTarget>) {
    for (idx, node) in tree.nodes.iter().enumerate() {
        let Some((spec, _)) = read_node_visualizer(node) else { continue };
        let label = node.name.clone();
        out.push(VisualizerTarget {
            key: VisualizerKey { instance, node: TemplateNodeIdx(idx as u32) },
            spec,
            label,
            byte_offset: node.span.offset,
            byte_length: node.span.length,
        });
    }
}

/// Read `length` bytes starting at `offset` from `source`. Returns a
/// localized error string on out-of-range / read failures so the
/// panel can render it without each visualizer re-doing the boundary
/// checks.
pub fn read_field_bytes(source: &Arc<dyn HexSource>, offset: u64, length: u64) -> Result<Vec<u8>, String> {
    if length == 0 {
        return Ok(Vec::new());
    }
    let end = offset.saturating_add(length);
    let range = hxy_core::ByteRange::new(hxy_core::ByteOffset::new(offset), hxy_core::ByteOffset::new(end))
        .map_err(|e| hxy_i18n::t_args("visualizer-read-error", &[("error", &format!("{e}"))]))?;
    source.read(range).map_err(|e| hxy_i18n::t_args("visualizer-read-error", &[("error", &format!("{e}"))]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_name() {
        let s = VisualizerSpec::parse("image").expect("parses");
        assert_eq!(s.kind, VisualizerKind::Image);
        assert!(s.args.is_empty());
    }

    #[test]
    fn parse_with_args() {
        let raw = format!("bitmap{sep}RGBA8{sep}800{sep}600", sep = VISUALIZE_ARG_SEP);
        let s = VisualizerSpec::parse(&raw).expect("parses");
        assert_eq!(s.kind, VisualizerKind::Bitmap);
        assert_eq!(s.args, vec!["RGBA8", "800", "600"]);
    }

    #[test]
    fn parse_unknown() {
        let s = VisualizerSpec::parse("not_a_real_visualizer").expect("parses");
        assert!(matches!(s.kind, VisualizerKind::Unknown(ref n) if n == "not_a_real_visualizer"));
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(VisualizerSpec::parse("").is_none());
    }
}
