//! End-to-end check that ImHex `[[color("...")]]` attributes flow
//! all the way through the runtime adapter into the template-panel
//! state, ending up as the resolved color for the corresponding leaf.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_core::MemorySource;
use hxy_plugin_host::TemplateRuntime;
use hxy_plugin_host::template as wit;

/// Find the runtime that handles `.hexpat` templates from the
/// builtins list.
fn imhex_runtime() -> Arc<dyn TemplateRuntime> {
    hxy_lib::templates::builtin::builtins()
        .into_iter()
        .find(|r| r.extensions().iter().any(|e| e == "hexpat"))
        .expect("imhex runtime registered in builtins()")
}

#[test]
fn template_color_attribute_resolves_to_leaf_color() {
    let src = "\
#pragma endian big

struct chunk_t {
    u32 length [[color(\"17BECF\")]];
    char tag[4];
};

chunk_t c @ 0x00;
";
    let bytes: Vec<u8> = vec![0x00, 0x00, 0x01, 0x23, b'I', b'D', b'A', b'T'];
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
    let runtime = imhex_runtime();
    let parsed = runtime.parse(source, src).expect("parse");
    let tree: wit::ResultTree = parsed.execute(&[]).expect("execute");

    // 1. The wit::Node tree should carry the canonical hxy_color
    //    attribute on the length field.
    let length_node = tree
        .nodes
        .iter()
        .find(|n| n.name == "length")
        .expect("length node in result tree");
    let color_value = length_node
        .attributes
        .iter()
        .find_map(|(k, v)| (k == hxy_plugin_host::COLOR_ATTR).then_some(v.as_str()));
    assert_eq!(
        color_value,
        Some("17BECF"),
        "wit::Node 'length' missing hxy_color; full attrs = {:?}",
        length_node.attributes
    );

    // 2. The panel-side resolver should build a TemplateState whose
    //    leaf_colors contains exactly Color32::from_rgb(0x17, 0xBE, 0xCF)
    //    in the slot corresponding to the length leaf.
    use hxy_lib::panels::template::new_state_from;
    let state = new_state_from(parsed, tree.clone(), std::collections::HashMap::new());

    let length_idx = tree
        .nodes
        .iter()
        .position(|n| n.name == "length")
        .expect("length node index") as u32;
    let slot = *state
        .leaf_slot_by_node
        .get(&length_idx)
        .unwrap_or_else(|| {
            panic!(
                "length node (idx {length_idx}) is not in leaf_slot_by_node; \
                 leaves = {:?}",
                state.leaf_node_indices
            )
        });
    let resolved = state.leaf_colors[slot];
    let expected = egui::Color32::from_rgb(0x17, 0xBE, 0xCF);
    assert_eq!(
        resolved, expected,
        "leaf_colors[{slot}] resolved to {:?} instead of teal #17BECF; \
         template color attribute did not propagate end-to-end",
        resolved
    );

    // Sanity: the byte range of the length field is what the hex view
    // would tint with that resolved color.
    let (off, len) = state.leaf_boundaries[slot];
    assert_eq!(off, ByteOffset::new(0));
    assert_eq!(len.get(), 4);
    let _ = ByteRange::new(off, ByteOffset::new(off.get() + len.get())).expect("valid range");
}

#[test]
fn primitive_element_array_parent_is_a_single_color_leaf() {
    // `char keyword[]` (open-ended dynamic) emits a parent node and N
    // child Char nodes in the lang. The hex view's per-byte tinting
    // used to walk the children -- one tint per char, rainbow byte
    // soup. The fix coalesces primitive-element-array parents into a
    // single color leaf and excludes their children, so the array
    // paints as one block.
    let src = "\
struct s_t {
    char keyword[];
    u8 sentinel;
};
s_t s @ 0x00;
";
    // Eight chars then a NUL terminates the open array, then the
    // sentinel byte.
    let bytes: Vec<u8> = vec![b'I', b'D', b'A', b'T', b' ', b'i', b'n', b'g', 0x00, 0xFF];
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
    let runtime = imhex_runtime();
    let parsed = runtime.parse(source, src).expect("parse");
    let tree: wit::ResultTree = parsed.execute(&[]).expect("execute");

    let keyword_idx = tree
        .nodes
        .iter()
        .position(|n| n.name == "keyword")
        .expect("keyword node");
    // The lang emits the parent + child element nodes (their names
    // are "[0]", "[1]", ...).
    let element_count =
        tree.nodes.iter().filter(|n| n.parent == Some(keyword_idx as u32) && n.name.starts_with('[')).count();
    assert!(element_count > 0, "lang should still emit per-element nodes for browsing");

    use hxy_lib::panels::template::new_state_from;
    let state = new_state_from(parsed, tree.clone(), std::collections::HashMap::new());

    // The parent IS in leaf_slot_by_node; the children are NOT.
    assert!(
        state.leaf_slot_by_node.contains_key(&(keyword_idx as u32)),
        "primitive-element-array parent should be a color leaf"
    );
    let bad_child_count = tree
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.parent == Some(keyword_idx as u32))
        .filter(|(idx, _)| state.leaf_slot_by_node.contains_key(&(*idx as u32)))
        .count();
    assert_eq!(bad_child_count, 0, "individual char elements should not be color leaves -- the parent is");
}

#[test]
fn comment_attribute_lands_on_field_node() {
    // The Name cell in the panel attaches a hover tooltip carrying
    // hxy_comment. Verify the attr actually arrives on the right
    // wit::Node so when the user reports 'tooltip not showing' we
    // can rule out a missing-data issue.
    let src = "\
struct s_t {
    u32 width [[comment(\"Image width\")]];
    u32 height;
};
s_t s @ 0x00;
";
    let bytes: Vec<u8> = vec![0; 8];
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
    let runtime = imhex_runtime();
    let parsed = runtime.parse(source, src).expect("parse");
    let tree: wit::ResultTree = parsed.execute(&[]).expect("execute");

    let width = tree.nodes.iter().find(|n| n.name == "width").expect("width node");
    let comment = width
        .attributes
        .iter()
        .find_map(|(k, v)| (k == hxy_plugin_host::COMMENT_ATTR).then_some(v.as_str()));
    assert_eq!(comment, Some("Image width"), "wit::Node 'width' missing hxy_comment; attrs={:?}", width.attributes);

    // Sibling without [[comment]] should NOT have hxy_comment.
    let height = tree.nodes.iter().find(|n| n.name == "height").expect("height node");
    let height_comment = height.attributes.iter().find(|(k, _)| k == hxy_plugin_host::COMMENT_ATTR);
    assert!(height_comment.is_none(), "height should not carry hxy_comment, got {:?}", height_comment);
}

#[test]
fn trailing_visualizer_field_does_not_overshadow_structural_leaves() {
    // Mimic the upstream PNG hexpat's pattern of declaring a
    // visualizer at the end of the root struct that placement-reads
    // the entire span:
    //   u8 v[length] @ addressof(this) [[no_unique_address]]
    // Without first-emitted-wins on overlap, that leaf would claim
    // every byte's tint (and every breadcrumb hover), overshadowing
    // the structural fields. We don't name-match `visualizer` --
    // the rule fires on overlap with an already-accepted leaf.
    let src = "\
struct s_t {
    u8 first;
    u8 second;
    u8 v[2] @ 0x00 [[no_unique_address]];
};
s_t s @ 0x00;
";
    let bytes: Vec<u8> = vec![0xAA, 0xBB];
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
    let runtime = imhex_runtime();
    let parsed = runtime.parse(source, src).expect("parse");
    let tree: wit::ResultTree = parsed.execute(&[]).expect("execute");

    use hxy_lib::panels::template::new_state_from;
    let state = new_state_from(parsed, tree.clone(), std::collections::HashMap::new());

    // The structural fields `first` / `second` are accepted leaves.
    // The trailing `v` is dropped by the overlap rule.
    let first_idx = tree.nodes.iter().position(|n| n.name == "first").expect("first node") as u32;
    let second_idx = tree.nodes.iter().position(|n| n.name == "second").expect("second node") as u32;
    let v_idx = tree.nodes.iter().position(|n| n.name == "v").expect("v node") as u32;
    assert!(state.leaf_slot_by_node.contains_key(&first_idx), "structural `first` should be a leaf");
    assert!(state.leaf_slot_by_node.contains_key(&second_idx), "structural `second` should be a leaf");
    assert!(
        !state.leaf_slot_by_node.contains_key(&v_idx),
        "trailing overlapping `v` field should be dropped from leaves"
    );

    // Breadcrumb at byte 0 should land on `first`, not on the
    // visualizer-like `v`.
    let crumbs = hxy_lib::panels::template::breadcrumb_for_offset(&tree, &MemorySource::new(vec![0xAA, 0xBB]), 0)
        .expect("breadcrumb at byte 0");
    let leaf_label = crumbs.last().expect("at least one row");
    assert!(leaf_label.contains("first"), "breadcrumb leaf at byte 0 should be `first`, got: {leaf_label:?}");
}

#[test]
fn visible_node_indices_walks_visible_rows_in_order() {
    // Backstop for arrow-key navigation: after expanding selected
    // parents, `visible_node_indices` should return the same
    // tree-node indices the panel's row list shows, in order. Used
    // by `move_template_selection` to step up/down.
    let src = "\
struct outer_t {
    u8 first;
    u8 second;
};
outer_t o @ 0x00;
";
    let bytes: Vec<u8> = vec![1, 2];
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
    let runtime = imhex_runtime();
    let parsed = runtime.parse(source, src).expect("parse");
    let tree: wit::ResultTree = parsed.execute(&[]).expect("execute");

    use hxy_lib::files::TemplateNodeIdx;
    use hxy_lib::panels::template::{new_state_from, visible_node_indices};
    let mut state = new_state_from(parsed, tree.clone(), std::collections::HashMap::new());
    // Default: outer_t is collapsed -- only the root row is visible.
    assert_eq!(visible_node_indices(&state), vec![TemplateNodeIdx(0)]);
    // Expand the root by removing it from the collapsed set.
    state.collapsed.remove(&TemplateNodeIdx(0));
    assert_eq!(
        visible_node_indices(&state),
        vec![TemplateNodeIdx(0), TemplateNodeIdx(1), TemplateNodeIdx(2)],
        "expanded outer_t should expose its two children in tree order"
    );
}

#[test]
fn parents_start_collapsed_by_default() {
    // After new_state_from runs, every parent / array node should be
    // in `collapsed`. The user opens what they want.
    let src = "\
struct inner_t {
    u8 a;
    u8 b;
};
struct outer_t {
    inner_t one;
    inner_t two;
};
outer_t o @ 0x00;
";
    let bytes: Vec<u8> = vec![1, 2, 3, 4];
    let source: Arc<dyn HexSource> = Arc::new(MemorySource::new(bytes));
    let runtime = imhex_runtime();
    let parsed = runtime.parse(source, src).expect("parse");
    let tree: wit::ResultTree = parsed.execute(&[]).expect("execute");

    use hxy_lib::files::TemplateNodeIdx;
    use hxy_lib::panels::template::new_state_from;
    let state = new_state_from(parsed, tree.clone(), std::collections::HashMap::new());

    for (idx, _) in tree.nodes.iter().enumerate().filter(|(idx, _)| {
        tree.nodes.iter().any(|n| n.parent == Some(*idx as u32))
    }) {
        assert!(
            state.collapsed.contains(&TemplateNodeIdx(idx as u32)),
            "parent node at idx {idx} should start collapsed"
        );
    }
}
