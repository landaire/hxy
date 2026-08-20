//! Breadcrumb derivation for the hex view's hover tooltip: walk the
//! template tree to the leaf field covering a byte offset and render
//! the compact or full struct path for it.

use hxy_core::HexSource;
use hxy_core::format::TemplateValueFormats;

use crate::format::decode_scalar_bytes;
use crate::format::format_value;
use crate::format::scalar_kind_name;
use crate::format::scalar_kind_width;

/// Walk `tree` to find the first leaf node whose span contains `byte`
/// and return a top-down path of "{type} {name}[ = {value}]" strings.
/// `None` when no template field covers the offset.
///
/// "First" matters because some templates declare a trailing
/// visualizer / peek field that overlaps the whole struct
/// (`u8 v[length] @ addressof(this) [[no_unique_address]]`); a
/// last-emitted-wins walk would always end on it instead of the
/// structural field the user is hovering. "Leaf" matters because
/// otherwise the root struct (which contains every byte) would win
/// before we reach anything specific.
///
/// Verbosity for [`breadcrumb_for_offset`]. The hex view picks
/// between them based on whether the user is holding the Alt /
/// Option modifier: by default the tooltip stays compact (just
/// the leaf field, or the per-element row for scalar arrays);
/// holding the modifier reveals the full struct path from the
/// root down to the leaf so the user can see exactly which
/// nested field they're hovering over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreadcrumbDetail {
    /// Single line. Non-array leaves render as `type name = value`;
    /// scalar arrays render as `name[index] = value` (the array's
    /// field name plus the cursor's element index).
    Leaf,
    /// Full struct chain root-to-leaf, plus the per-element row
    /// for scalar arrays. Triggered while a modifier key is held.
    Full,
}

/// When the chosen leaf is a primitive `ScalarArray`, the breadcrumb
/// gets an extra leaf row showing the specific element under the
/// cursor -- e.g. `compressed_profile[77] = 120` -- decoded on the
/// fly from `source`. That's the reason the source is taken as an
/// argument: primitive arrays are emitted as a single contiguous
/// node, so individual element values aren't in the tree.
pub fn breadcrumb_for_offset(
    tree: &hxy_plugin_host::template::ResultTree,
    source: &dyn HexSource,
    byte: u64,
    detail: BreadcrumbDetail,
    fmts: &TemplateValueFormats,
    inverse: bool,
) -> Option<Vec<String>> {
    let mut has_children = vec![false; tree.nodes.len()];
    for node in &tree.nodes {
        if let Some(parent) = node.parent
            && (parent as usize) < has_children.len()
        {
            has_children[parent as usize] = true;
        }
    }
    let leaf = tree.nodes.iter().enumerate().find_map(|(idx, node)| {
        if has_children[idx] {
            return None;
        }
        let start = node.span.offset;
        let end = start.saturating_add(node.span.length);
        (byte >= start && byte < end).then_some(idx as u32)
    })?;
    let leaf_node = &tree.nodes[leaf as usize];

    if matches!(detail, BreadcrumbDetail::Leaf) {
        // Compact form: prefer the per-element row for scalar
        // arrays, otherwise emit the leaf's own type/name/value
        // line. Single-row output -- the tooltip won't stretch
        // across the hex view.
        if let Some(row) = array_element_row(leaf_node, source, byte, fmts, inverse) {
            return Some(vec![row]);
        }
        return Some(vec![format_leaf_line(leaf_node, fmts, inverse)]);
    }

    // Walk parent chain leaf -> root.
    let mut chain: Vec<u32> = Vec::new();
    let mut cursor = Some(leaf);
    while let Some(idx) = cursor {
        chain.push(idx);
        cursor = tree.nodes.get(idx as usize).and_then(|n| n.parent);
    }
    chain.reverse();

    let mut raw: Vec<String> = chain
        .iter()
        .map(|idx| {
            let node = &tree.nodes[*idx as usize];
            let is_leaf = *idx == leaf;
            let ty = hxy_plugin_host::node_display_type(node);
            let value_str = if is_leaf { format_node_value(node, fmts, inverse) } else { None };
            match value_str {
                Some(v) => format!("{} {} = {}", ty, node.name, v),
                None => format!("{} {}", ty, node.name),
            }
        })
        .collect();

    if let Some(row) = array_element_row(leaf_node, source, byte, fmts, inverse) {
        raw.push(row);
    }

    // Decorate as a degenerate (linear) tree. Root has no connector;
    // every deeper row gets `└─ ` prefixed by 3 spaces per ancestor
    // depth so the indents line up with `tree` / `exa -T` output.
    let lines: Vec<String> = raw
        .into_iter()
        .enumerate()
        .map(|(depth, label)| {
            if depth == 0 {
                label
            } else {
                let indent = "   ".repeat(depth - 1);
                format!("{indent}\u{2514}\u{2500} {label}")
            }
        })
        .collect();
    Some(lines)
}

/// Format a single leaf node as `<type> <name> = <value>` (or
/// `<type> <name>` when the node has no scalar value). Used by
/// both the compact and full breadcrumbs to render the leaf row.
fn format_leaf_line(node: &hxy_plugin_host::template::Node, fmts: &TemplateValueFormats, inverse: bool) -> String {
    let ty = hxy_plugin_host::node_display_type(node);
    match format_node_value(node, fmts, inverse) {
        Some(v) => format!("{} {} = {}", ty, node.name, v),
        None => format!("{} {}", ty, node.name),
    }
}

/// Produce the per-element breadcrumb row for a primitive scalar
/// array. Returns `None` when the leaf isn't a scalar array, the
/// byte lands in the array's padding, or the source read fails.
fn array_element_row(
    leaf: &hxy_plugin_host::template::Node,
    source: &dyn HexSource,
    byte: u64,
    fmts: &TemplateValueFormats,
    inverse: bool,
) -> Option<String> {
    use hxy_plugin_host::template::NodeType;

    let (kind, count) = match &leaf.type_name {
        NodeType::ScalarArray((k, n)) => (*k, *n),
        _ => return None,
    };
    let elem_width = scalar_kind_width(kind)?;
    if elem_width == 0 || count == 0 {
        return None;
    }
    let array_start = leaf.span.offset;
    let relative = byte.checked_sub(array_start)?;
    let index = relative / elem_width;
    if index >= count {
        return None;
    }
    let elem_offset = array_start + index * elem_width;
    let range = hxy_core::ByteRange::new(
        hxy_core::ByteOffset::new(elem_offset),
        hxy_core::ByteOffset::new(elem_offset + elem_width),
    )
    .ok()?;
    let bytes = source.read(range).ok()?;
    let endian = leaf
        .attributes
        .iter()
        .find_map(|(k, v)| (k == hxy_plugin_host::ENDIAN_ATTR).then_some(v.as_str()))
        .unwrap_or("little");
    let value = decode_scalar_bytes(kind, &bytes, endian, leaf.display, fmts, inverse)?;
    let type_label = scalar_kind_name(kind);
    Some(format!("{type_label} {}[{index}] = {value}", leaf.name))
}

/// Tooltip-flavour wrapper around [`format_value`]. Kept as its
/// own name (instead of directly inlining `format_value`) so the
/// breadcrumb call site in [`breadcrumb_for_offset`] still reads
/// as "format this node's value for the tooltip" rather than
/// leaking the user-format detail into the call.
fn format_node_value(
    node: &hxy_plugin_host::template::Node,
    fmts: &TemplateValueFormats,
    inverse: bool,
) -> Option<String> {
    format_value(node, fmts, inverse)
}
