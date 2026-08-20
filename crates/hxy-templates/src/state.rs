//! Framework-agnostic template panel core: the per-instance state
//! model ([`TemplateState`]), tree flattening into visible rows,
//! the leaf/color model, and the event vocabulary both frontends'
//! template panels emit.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use hxy_core::ByteRange;
use hxy_core::color::Rgba;
pub use hxy_core::copy::CopyKind;
use hxy_plugin_host::ParsedTemplate;
use hxy_plugin_host::template::Node;

/// Identifier for one template applied to a file. Allocated by the
/// owning file so lookups inside its template list don't need to
/// compare paths or ranges. Two instances of the same template run
/// against different ranges get distinct ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TemplateInstanceId(u64);

impl TemplateInstanceId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

/// One completed template applied to a slice of the file. The owned
/// [`TemplateState`] reports node offsets in **file-absolute** coordinates
/// (the runner adjusts them by `range.start()` on the way in), so
/// downstream code -- hex view tinting, breadcrumb tooltips, copy
/// formatting -- doesn't need to know whether the template was run
/// against the whole file or a sub-range.
pub struct TemplateInstance {
    pub id: TemplateInstanceId,
    /// Path of the template source file. Carried so reload can re-fire
    /// the same template, and so the panel header can show the source.
    pub source_path: PathBuf,
    /// Short name for the tab strip (template's filename or library
    /// display name).
    pub display_name: String,
    /// Byte range of the file the template was bound to. The whole file
    /// for the default "Run template..." flow; a user-picked range for
    /// "Run template at selection..." or for nested templates over
    /// embedded streams (e.g. zlib-decompressed PNG IDAT).
    pub range: ByteRange,
    /// BLAKE3 of the *expanded* template source (post `#include`) at
    /// the moment the run kicked off. Persisted so a restart-time
    /// auto-rerun can detect "the template author edited the file
    /// since last session" -- in which case the node-id-keyed color
    /// overrides are dropped because the indices may no longer align.
    /// `None` for error-only instances where no run happened.
    pub source_fingerprint: Option<[u8; 32]>,
    pub state: TemplateState,
}

/// Result of applying a template-language runtime to the tab's byte
/// source. Holds the parsed template (so deferred arrays can be
/// expanded lazily) and the current tree view state.
/// Index into a [`TemplateState::tree`]'s flat node list. Newtype so
/// we don't confuse it with the `u64` array ids the runtime hands out
/// for deferred arrays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TemplateNodeIdx(pub u32);

/// Opaque identifier for a deferred array, handed back to the plugin
/// when the UI wants to materialise more elements. Distinct from
/// [`TemplateNodeIdx`] -- same `u64` width as the WIT record but
/// typed so we can't pass a node index where an array id is wanted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TemplateArrayId(pub u64);

pub struct TemplateState {
    /// `None` when the state was built as a diagnostics-only surface
    /// (e.g. missing runtime, parse failure) -- in that case
    /// `expand_array` can't be called and the panel renders only the
    /// diagnostics header.
    pub parsed: Option<Arc<dyn ParsedTemplate>>,
    pub tree: hxy_plugin_host::template::ResultTree,
    /// Array id -> materialised children, by order of expansion.
    pub expanded_arrays: HashMap<TemplateArrayId, Vec<Node>>,
    /// Indexes of nodes whose subtrees the user has collapsed. Default
    /// is expanded; we store the negation so freshly-run templates
    /// reveal everything without per-node defaults.
    pub collapsed: HashSet<TemplateNodeIdx>,
    /// Last-frame's hover target in the panel table: the node index
    /// whose row the pointer is over, if any. Consumed by the hex
    /// view to paint a highlight over that node's byte span.
    pub hovered_node: Option<TemplateNodeIdx>,
    /// Currently keyboard-selected row in the panel. Persists across
    /// frames so up/down/left/right have somewhere to operate from
    /// after the user clicked an initial row. Distinct from
    /// `hovered_node` (which follows the pointer) and from the
    /// editor's byte selection (which is what the hex view paints);
    /// click and arrow-key moves both update this AND re-fire the
    /// `Select` side effects so the hex view follows along.
    pub selected_node: Option<TemplateNodeIdx>,
    /// Precomputed (offset, length) spans for every leaf node in
    /// the tree, sorted by offset. Passed to `HexView` so it can
    /// draw field-boundary outlines without walking the tree each
    /// frame.
    pub leaf_boundaries: Vec<(hxy_core::ByteOffset, hxy_core::ByteLen)>,
    /// One tint per entry in `leaf_boundaries`. The hex view uses
    /// these to paint each field's bytes a distinct color when
    /// [`Self::show_colors`] is on. Resolved at construction (and on
    /// every override change) as `node_color_overrides` >
    /// `hxy_color`/`hxy_bg_color` attribute > hue-cycle fallback.
    pub leaf_colors: Vec<Rgba>,
    /// Node index for each entry in `leaf_boundaries` / `leaf_colors`.
    /// Lets the panel and override pipeline map "this row" -> "this
    /// field's byte coloring slot" in O(1) via [`Self::leaf_slot_by_node`].
    pub leaf_node_indices: Vec<u32>,
    /// Reverse index: node-tree index -> position in
    /// `leaf_boundaries`. Built once at construction; consulted on
    /// every panel row to decide whether the Color column gets a
    /// swatch (only nodes that actually paint bytes do).
    pub leaf_slot_by_node: HashMap<u32, usize>,
    /// User-dialed color overrides, keyed by node-tree index. Take
    /// precedence over template-supplied `hxy_color` and over the
    /// hue-cycle fallback. Persisted across restarts via the app's
    /// persisted template instance state
    /// (`PersistedTemplateInstance::node_color_overrides`).
    pub node_color_overrides: HashMap<u32, Rgba>,
    /// When true, the hex view recolors bytes by their containing
    /// template field. Toggled from the template panel header.
    pub show_colors: bool,
    /// Plugin-supplied per-byte palette (one color per value 0..=255),
    /// extracted once from the runtime's `ResultTree::byte_palette`.
    /// When `Some`, overrides the user's byte-value highlight for
    /// this tab.
    pub byte_palette_override: Option<Arc<[Rgba; 256]>>,
}

/// Events the app needs to handle after the panel renders.
pub enum TemplateEvent {
    /// User clicked the panel's close button. Hides the panel for
    /// every template on this file; doesn't drop any instances.
    HidePanel,
    /// User clicked a tab strip entry; switch to that template.
    SetActive(TemplateInstanceId),
    /// User clicked the close button on a tab; remove that instance
    /// (whether running or completed). The panel itself stays open if
    /// other instances remain.
    RemoveInstance(TemplateInstanceId),
    ExpandArray {
        array_id: TemplateArrayId,
        count: u64,
    },
    ToggleCollapse(TemplateNodeIdx),
    /// The pointer is currently over a row. `None` fires on the first
    /// frame the pointer leaves the table.
    Hover(Option<TemplateNodeIdx>),
    /// User clicked the row -- jump the hex view to this node's span
    /// and select it.
    Select(TemplateNodeIdx),
    /// User picked a copy option from the row's context menu. `kind`
    /// names what to format and how.
    Copy {
        idx: TemplateNodeIdx,
        kind: CopyKind,
    },
    /// User picked "Save bytes to file...". App should pop up a save
    /// dialog and write this node's byte span.
    SaveBytes(TemplateNodeIdx),
    /// User toggled per-field byte tinting in the hex view.
    ToggleColors(bool),
    /// User picked a new tint for `idx`'s field via the Color column
    /// swatch. The override survives across re-runs and (per the
    /// app's `PersistedTemplateInstance`) across restarts as
    /// long as the template source's BLAKE3 fingerprint matches.
    SetColor {
        idx: TemplateNodeIdx,
        color: Rgba,
    },
    /// User reset `idx`'s field tint back to the auto color
    /// (template-supplied attribute or hue-cycle fallback). Right-click
    /// on the swatch.
    ResetColor(TemplateNodeIdx),
    /// Keyboard arrow-key navigation: move the selected row by `delta`
    /// positions in the visible row list, skipping non-Node rows
    /// (synthesized array elements have no tree-node identity). The
    /// app handler clamps and re-fires the `Select` side effects so
    /// the hex view jumps to the new field.
    MoveSelection(i32),
    /// Left-arrow: collapse the currently selected node if expanded.
    CollapseSelected,
    /// Right-arrow: expand the currently selected node if collapsed.
    ExpandSelected,
    /// User clicked the visualizer icon on a row whose field carries
    /// a `[[hex::visualize(...)]]` attribute. Host opens / focuses
    /// the file's visualizer dock tab and selects this node as the
    /// active sub-tab.
    OpenVisualizer(TemplateNodeIdx),
}

/// One visible row in the flattened table -- either a real node or a
/// placeholder row inside an expanded deferred array. Array elements
/// don't live in `tree.nodes`, so they get a distinct row kind.
#[derive(Clone)]
pub enum RowKind {
    Node {
        idx: TemplateNodeIdx,
        depth: usize,
        is_parent: bool,
        collapsed: bool,
    },
    /// "[N x type, stride bytes each]" placeholder with an Expand button.
    DeferredArray {
        array_id: TemplateArrayId,
        count: u64,
        stride: u64,
        first_offset: u64,
        element_type: String,
        depth: usize,
    },
    /// Materialised element of an expanded deferred array.
    ArrayElement {
        array_id: TemplateArrayId,
        index: usize,
        depth: usize,
    },
    /// Synthetic element of an expanded primitive `ScalarArray` node.
    /// The lang emits these arrays as a single contiguous node with no
    /// children -- expanding the row into per-element rows happens
    /// here in the panel by decoding bytes from the source on demand,
    /// so we don't pay tree-size cost on collapsed arrays.
    ScalarArrayElement {
        parent_idx: TemplateNodeIdx,
        index: u64,
        depth: usize,
    },
}

pub fn children_by_parent(nodes: &[Node]) -> HashMap<Option<TemplateNodeIdx>, Vec<TemplateNodeIdx>> {
    let mut map: HashMap<Option<TemplateNodeIdx>, Vec<TemplateNodeIdx>> = HashMap::new();
    for (idx, node) in nodes.iter().enumerate() {
        let parent = node.parent.map(TemplateNodeIdx);
        map.entry(parent).or_default().push(TemplateNodeIdx(idx as u32));
    }
    map
}

/// Flatten the tree into the exact list of rows we want the table
/// to render, respecting collapsed subtrees and expanded deferred
/// arrays. Done up-front so the table widget can virtualize with
/// accurate row counts.
pub fn build_visible(
    state: &TemplateState,
    children: &HashMap<Option<TemplateNodeIdx>, Vec<TemplateNodeIdx>>,
) -> Vec<RowKind> {
    let mut out = Vec::new();
    let roots = children.get(&None).cloned().unwrap_or_default();
    for root in roots {
        emit_node(state, children, root, 0, &mut out);
    }
    out
}

fn emit_node(
    state: &TemplateState,
    children: &HashMap<Option<TemplateNodeIdx>, Vec<TemplateNodeIdx>>,
    idx: TemplateNodeIdx,
    depth: usize,
    out: &mut Vec<RowKind>,
) {
    let node = &state.tree.nodes[idx.0 as usize];
    let kids = children.get(&Some(idx)).cloned().unwrap_or_default();
    let has_array = node.array.is_some();
    // Fixed-size primitive arrays (`u32 length[4]`, `char name[N]`)
    // come back as a single ScalarArray node with no children. Treat
    // them as parents anyway so the user can drill into individual
    // elements; the rows themselves get synthesized lazily when the
    // user expands.
    let scalar_array_count = match node.type_name {
        hxy_plugin_host::template::NodeType::ScalarArray((_, n)) if n > 0 => Some(n),
        _ => None,
    };
    let is_parent = !kids.is_empty() || has_array || scalar_array_count.is_some();
    let collapsed = state.collapsed.contains(&idx);

    out.push(RowKind::Node { idx, depth, is_parent, collapsed });

    if collapsed {
        return;
    }
    for cid in kids {
        emit_node(state, children, cid, depth + 1, out);
    }
    if let Some(arr) = node.array.as_ref() {
        let array_id = TemplateArrayId(arr.id);
        if let Some(elements) = state.expanded_arrays.get(&array_id) {
            for i in 0..elements.len() {
                out.push(RowKind::ArrayElement { array_id, index: i, depth: depth + 1 });
            }
        } else {
            out.push(RowKind::DeferredArray {
                array_id,
                count: arr.count,
                stride: arr.stride,
                first_offset: arr.first_offset,
                element_type: arr.element_type.clone(),
                depth: depth + 1,
            });
        }
    }
    if let Some(count) = scalar_array_count {
        for i in 0..count {
            out.push(RowKind::ScalarArrayElement { parent_idx: idx, index: i, depth: depth + 1 });
        }
    }
}

pub fn expand_array(state: &mut TemplateState, array_id: TemplateArrayId, count: u64) {
    const MAX_INITIAL: u64 = 512;
    let Some(parsed) = state.parsed.as_ref() else { return };
    let end = count.min(MAX_INITIAL);
    match parsed.expand_array(array_id.0, 0, end) {
        Ok(elements) => {
            state.expanded_arrays.insert(array_id, elements);
        }
        Err(e) => tracing::warn!(error = %e, "expand array"),
    }
}

/// Tree-node indices for every visible Node row, in panel display
/// order. Non-Node rows (deferred-array placeholders, expanded array
/// elements, synthetic primitive-array elements) are filtered out
/// because they don't have a stable `TemplateNodeIdx`. Used by the
/// arrow-key navigation handler to step from one selectable row to
/// the next.
pub fn visible_node_indices(state: &TemplateState) -> Vec<TemplateNodeIdx> {
    let children = children_by_parent(&state.tree.nodes);
    let visible = build_visible(state, &children);
    visible
        .into_iter()
        .filter_map(|r| match r {
            RowKind::Node { idx, .. } => Some(idx),
            _ => None,
        })
        .collect()
}

pub fn toggle_collapse(state: &mut TemplateState, idx: TemplateNodeIdx) {
    if !state.collapsed.remove(&idx) {
        state.collapsed.insert(idx);
    }
}

pub fn new_state(parsed: Arc<dyn ParsedTemplate>) -> Result<TemplateState, hxy_vfs::HandlerError> {
    let tree = parsed.execute(&[])?;
    Ok(new_state_from(parsed, tree, HashMap::new()))
}

/// Build a [`TemplateState`] from an already-computed tree. Used by
/// the background-run path where the worker thread executes the
/// template and sends the result back to the UI. `node_color_overrides`
/// is non-empty when the run is a restart-time auto-rerun replaying
/// the user's previously persisted picks.
pub fn new_state_from(
    parsed: Arc<dyn ParsedTemplate>,
    tree: hxy_plugin_host::template::ResultTree,
    node_color_overrides: HashMap<u32, Rgba>,
) -> TemplateState {
    let children_of = build_children_index(&tree);
    let (leaf_boundaries, leaf_node_indices) = collect_leaves(&tree, &children_of);
    let leaf_slot_by_node: HashMap<u32, usize> = leaf_node_indices.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    let leaf_colors = resolve_leaf_colors(&tree, &leaf_node_indices, &node_color_overrides);
    let collapsed = initial_collapsed(&tree, &children_of);
    let byte_palette_override = build_byte_palette_override(tree.byte_palette.as_deref());
    TemplateState {
        parsed: Some(parsed),
        tree,
        expanded_arrays: HashMap::new(),
        collapsed,
        hovered_node: None,
        selected_node: None,
        leaf_boundaries,
        leaf_colors,
        leaf_node_indices,
        leaf_slot_by_node,
        node_color_overrides,
        show_colors: true,
        byte_palette_override,
    }
}

/// Recompute `leaf_colors` after a change to `node_color_overrides`.
/// Cheap (O(leaves)) and called from the SetColor / ResetColor event
/// handlers so the hex view picks up the new tint on the next frame
/// without a full template re-run.
pub fn recompute_leaf_colors(state: &mut TemplateState) {
    state.leaf_colors = resolve_leaf_colors(&state.tree, &state.leaf_node_indices, &state.node_color_overrides);
}

/// Unpack the runtime's optional 256-entry `0xAARRGGBB` table into an
/// `Arc<[Rgba; 256]>`. Any length other than 256 is rejected -- we
/// keep the contract tight so the hex view can index without bounds
/// checks. Returns `None` when the runtime didn't supply a palette.
fn build_byte_palette_override(palette: Option<&[u32]>) -> Option<Arc<[Rgba; 256]>> {
    let raw = palette?;
    if raw.len() != 256 {
        return None;
    }
    let mut out = [Rgba::TRANSPARENT; 256];
    for (i, packed) in raw.iter().enumerate() {
        out[i] = Rgba::from_argb_u32(*packed);
    }
    Some(Arc::new(out))
}

pub fn error_state(message: String) -> TemplateState {
    TemplateState {
        parsed: None,
        tree: hxy_plugin_host::template::ResultTree {
            nodes: Vec::new(),
            diagnostics: vec![hxy_plugin_host::template::Diagnostic {
                message,
                severity: hxy_plugin_host::template::Severity::Error,
                file_offset: None,
                template_line: None,
            }],
            byte_palette: None,
        },
        expanded_arrays: HashMap::new(),
        collapsed: HashSet::new(),
        hovered_node: None,
        selected_node: None,
        leaf_boundaries: Vec::new(),
        leaf_colors: Vec::new(),
        leaf_node_indices: Vec::new(),
        leaf_slot_by_node: HashMap::new(),
        node_color_overrides: HashMap::new(),
        show_colors: true,
        byte_palette_override: None,
    }
}

/// Pick `n` distinct hues using the golden angle so neighbouring
/// leaves don't land on similar colors. The base colors are vivid
/// enough to read as glyphs in `ValueHighlight::Text` mode; callers
/// that paint them as backgrounds apply `gamma_multiply` to mute
/// them on the fly. Used as the per-leaf fallback when neither a
/// user override nor a template-supplied `hxy_color` attribute
/// applies.
fn fallback_leaf_color(slot: usize) -> Rgba {
    let hue = (slot as f32 * 0.381966) % 1.0;
    Rgba::from_hsv(hue, 0.6, 0.9)
}

/// Per-leaf color resolution: user override > template
/// `hxy_color` attribute > hue-cycle fallback. The fallback's slot
/// index is just the leaf's position in `leaf_node_indices`, which
/// keeps the auto colors stable across runs of the same template
/// (so a field that previously sat at slot 7 still gets slot 7's
/// hue if no override is set).
fn resolve_leaf_colors(
    tree: &hxy_plugin_host::template::ResultTree,
    leaf_node_indices: &[u32],
    overrides: &HashMap<u32, Rgba>,
) -> Vec<Rgba> {
    leaf_node_indices
        .iter()
        .enumerate()
        .map(|(slot, &node_idx)| {
            if let Some(c) = overrides.get(&node_idx) {
                return *c;
            }
            if let Some(node) = tree.nodes.get(node_idx as usize)
                && let Some(c) = parse_color_attr(node)
            {
                return c;
            }
            fallback_leaf_color(slot)
        })
        .collect()
}

/// Pull a `hxy_color` attribute off `node` and parse it as an sRGB(A)
/// hex string. Accepted shapes (case-insensitive, optional `#` /
/// `0x` prefix): `RRGGBB` and `AARRGGBB`. `None` when the attribute
/// is missing or doesn't parse.
fn parse_color_attr(node: &Node) -> Option<Rgba> {
    let raw = node.attributes.iter().find_map(|(k, v)| (k == hxy_plugin_host::COLOR_ATTR).then_some(v.as_str()))?;
    parse_hex_color(raw)
}

fn parse_hex_color(s: &str) -> Option<Rgba> {
    let s = s.trim();
    let s = s.strip_prefix('#').unwrap_or(s);
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some(Rgba::rgb(r, g, b))
        }
        8 => {
            let a = u8::from_str_radix(&s[0..2], 16).ok()?;
            let r = u8::from_str_radix(&s[2..4], 16).ok()?;
            let g = u8::from_str_radix(&s[4..6], 16).ok()?;
            let b = u8::from_str_radix(&s[6..8], 16).ok()?;
            Some(Rgba::from_rgba_unmultiplied(r, g, b, a))
        }
        _ => None,
    }
}

/// Per-parent child index. Built once at TemplateState construction
/// and reused for both leaf detection and the initial collapse set.
fn build_children_index(tree: &hxy_plugin_host::template::ResultTree) -> Vec<Vec<u32>> {
    let mut out: Vec<Vec<u32>> = vec![Vec::new(); tree.nodes.len()];
    for (idx, node) in tree.nodes.iter().enumerate() {
        if let Some(parent) = node.parent
            && (parent as usize) < out.len()
        {
            out[parent as usize].push(idx as u32);
        }
    }
    out
}

/// Collect "color leaves" -- the nodes whose byte spans should receive
/// distinct tints in the hex view (and whose rows show a swatch in the
/// panel's Color column). Returns parallel vectors of spans and tree
/// node indices, sorted by offset.
///
/// A node is a color leaf when:
/// - it has no children (the typical scalar field), or
/// - it's the parent of a primitive-element array (every child has
///   `Scalar(_)` type). In that case the children are *excluded*: a
///   `char keyword[]` should paint as one continuous teal block, not
///   eighteen rainbow bytes, even though the lang did emit eighteen
///   per-element nodes for browsing.
///
/// Deferred arrays and zero-length nodes are always excluded.
fn collect_leaves(
    tree: &hxy_plugin_host::template::ResultTree,
    children_of: &[Vec<u32>],
) -> (Vec<(hxy_core::ByteOffset, hxy_core::ByteLen)>, Vec<u32>) {
    // A node is "absorbed by a primitive-array parent" when its
    // immediate parent has all-scalar children. The parent owns the
    // tint for the whole span; the absorbed child contributes
    // nothing of its own to leaf coloring even though it would
    // otherwise pass the no-children filter below.
    let absorbed: Vec<bool> = (0..tree.nodes.len())
        .map(|idx| {
            let Some(parent) = tree.nodes[idx].parent else {
                return false;
            };
            let parent = parent as usize;
            if parent >= children_of.len() {
                return false;
            }
            all_children_scalar(tree, &children_of[parent])
        })
        .collect();
    // Walk in declaration / tree order and accept a leaf only when
    // its span doesn't overlap any leaf we've already accepted.
    // First-emitted wins on overlap. Drops trailing "visualizer"
    // fields some templates declare as
    // `u8 v[length] @ addressof(this) [[no_unique_address]]` --
    // those would otherwise claim every byte's tint and overshadow
    // the structural fields. Same shape works for any other late
    // peek field declared with the same overlap pattern, no
    // hardcoding to a name.
    let mut accepted: Vec<(hxy_core::ByteOffset, hxy_core::ByteLen, u32)> = Vec::new();
    for (idx, node) in tree.nodes.iter().enumerate() {
        if node.array.is_some() || node.span.length == 0 || absorbed[idx] {
            continue;
        }
        let kids = &children_of[idx];
        if !(kids.is_empty() || all_children_scalar(tree, kids)) {
            continue;
        }
        let new_start = node.span.offset;
        let new_end = new_start.saturating_add(node.span.length);
        let overlaps = accepted.iter().any(|(s, l, _)| {
            let s_start = s.get();
            let s_end = s_start.saturating_add(l.get());
            new_start < s_end && s_start < new_end
        });
        if overlaps {
            continue;
        }
        accepted.push((hxy_core::ByteOffset::new(new_start), hxy_core::ByteLen::new(node.span.length), idx as u32));
    }
    accepted.sort_by_key(|(start, _, _)| start.get());
    let boundaries = accepted.iter().map(|(s, l, _)| (*s, *l)).collect();
    let node_indices = accepted.into_iter().map(|(_, _, n)| n).collect();
    (boundaries, node_indices)
}

fn all_children_scalar(tree: &hxy_plugin_host::template::ResultTree, kids: &[u32]) -> bool {
    kids.iter().all(|&c| {
        tree.nodes
            .get(c as usize)
            .is_some_and(|n| matches!(n.type_name, hxy_plugin_host::template::NodeType::Scalar(_)))
    })
}

/// Initial set of collapsed nodes: every node that *can* be expanded
/// (parent of children, deferred array, or fixed-size primitive
/// scalar array). Templates can be deep enough that landing on the
/// fully-expanded tree is overwhelming; the user opens what they
/// need.
fn initial_collapsed(
    tree: &hxy_plugin_host::template::ResultTree,
    children_of: &[Vec<u32>],
) -> HashSet<TemplateNodeIdx> {
    (0..tree.nodes.len() as u32)
        .filter(|&idx| {
            let node = &tree.nodes[idx as usize];
            !children_of[idx as usize].is_empty()
                || node.array.is_some()
                || matches!(node.type_name, hxy_plugin_host::template::NodeType::ScalarArray((_, n)) if n > 0)
        })
        .map(TemplateNodeIdx)
        .collect()
}
