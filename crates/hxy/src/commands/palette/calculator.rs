//! Calculator-side glue: a [`hxy_calculator::PathResolver`] backed
//! by an active file's [`crate::files::TemplateInstance`] list.
//!
//! Template name matching is case-insensitive on the source-file
//! stem (`PNG.bt` matches `png`). When several instances share a
//! stem (e.g. the user ran the same template against two different
//! ranges of the same file), the resolver picks the most recent
//! run unless the path spelt out an explicit `name#N` instance --
//! `1` is the oldest, the highest number is the newest.
//!
//! Field navigation walks `state.tree.nodes` by parent / child
//! links. The first segment is matched against top-level nodes;
//! when the template emitted exactly one root struct the resolver
//! falls through into that root's children automatically, so
//! `png.signature` works without forcing the user to spell out
//! `png.PNG.signature`. Subsequent segments are strict
//! parent/child name lookups -- siblings with duplicate names take
//! the first match (a common-enough escape hatch for templates
//! that loop and emit "chunk", "chunk", "chunk").

use hxy_calculator::FieldRef;
use hxy_calculator::Path;
use hxy_calculator::PathResolver;
use hxy_calculator::PathSegment;
use hxy_calculator::ResolveError;
use hxy_plugin_host::template::Node;
use hxy_plugin_host::template::Value;

use crate::files::TemplateInstance;

/// Resolver backed by an [`crate::files::OpenFile::templates`]
/// slice. Borrowing rather than cloning keeps construction cheap
/// (the palette rebuilds entries every frame); the resolver only
/// ever reads.
pub struct TemplateFieldResolver<'a> {
    instances: &'a [TemplateInstance],
}

impl<'a> TemplateFieldResolver<'a> {
    pub fn new(instances: &'a [TemplateInstance]) -> Self {
        Self { instances }
    }
}

impl PathResolver for TemplateFieldResolver<'_> {
    fn lookup(&self, path: &Path) -> Result<FieldRef, ResolveError> {
        let stem_lower = path.root.to_ascii_lowercase();
        let matches: Vec<&TemplateInstance> =
            self.instances.iter().filter(|t| display_name_stem_eq(&t.display_name, &stem_lower)).collect();
        if matches.is_empty() {
            return Err(ResolveError::UnknownTemplate { name: path.root.clone() });
        }
        let chosen = match path.instance {
            None => *matches.last().expect("non-empty"),
            Some(n) => {
                let idx = (n as usize).checked_sub(1).ok_or(ResolveError::InstanceOutOfRange {
                    name: path.root.clone(),
                    requested: n,
                    available: matches.len() as u32,
                })?;
                *matches.get(idx).ok_or(ResolveError::InstanceOutOfRange {
                    name: path.root.clone(),
                    requested: n,
                    available: matches.len() as u32,
                })?
            }
        };
        let nodes = &chosen.state.tree.nodes;
        let final_idx = walk_segments(nodes, &path.segments, path)?;
        let node = &nodes[final_idx as usize];
        Ok(FieldRef { offset: node.span.offset, length: node.span.length, value: scalar_to_i128(node) })
    }
}

fn display_name_stem_eq(display_name: &str, target_lower: &str) -> bool {
    let lower = display_name.to_ascii_lowercase();
    let stem = lower.rsplit_once('.').map(|(s, _)| s).unwrap_or(lower.as_str());
    stem == target_lower
}

/// Walk `segments` starting from the top level of `nodes` and
/// return the final node index. Auto-descends through a single
/// top-level root when the first segment doesn't match any
/// top-level name -- mirrors how the user mentally treats the
/// root struct as transparent.
fn walk_segments(nodes: &[Node], segments: &[PathSegment], path: &Path) -> Result<u32, ResolveError> {
    let mut cursor: Option<u32> = None;
    let mut at_first_segment = true;
    for seg in segments {
        let parent_label = match cursor {
            Some(idx) => nodes[idx as usize].name.clone(),
            None => path.root.clone(),
        };
        match seg {
            PathSegment::Name(name) => {
                let direct = find_child_by_name(nodes, cursor, name);
                let resolved = match direct {
                    Some(i) => i,
                    None if at_first_segment && cursor.is_none() => {
                        let top = top_level(nodes);
                        if top.len() == 1 {
                            find_child_by_name(nodes, Some(top[0]), name).ok_or(ResolveError::FieldNotFound {
                                parent: parent_label,
                                component: name.clone(),
                            })?
                        } else {
                            return Err(ResolveError::FieldNotFound {
                                parent: parent_label,
                                component: name.clone(),
                            });
                        }
                    }
                    None => {
                        return Err(ResolveError::FieldNotFound { parent: parent_label, component: name.clone() });
                    }
                };
                cursor = Some(resolved);
            }
            PathSegment::Index(n) => {
                let parent_idx = cursor.ok_or(ResolveError::IndexOutOfBounds {
                    parent: parent_label.clone(),
                    index: *n,
                    len: 0,
                })?;
                let kids = children_of(nodes, parent_idx);
                let target = (*n as usize).checked_sub(0).filter(|&i| i < kids.len()).ok_or(
                    ResolveError::IndexOutOfBounds {
                        parent: parent_label,
                        index: *n,
                        len: kids.len() as u64,
                    },
                )?;
                cursor = Some(kids[target]);
            }
        }
        at_first_segment = false;
    }
    cursor.ok_or(ResolveError::NotAScalar { path: path.display() })
}

fn top_level(nodes: &[Node]) -> Vec<u32> {
    nodes.iter().enumerate().filter(|(_, n)| n.parent.is_none()).map(|(i, _)| i as u32).collect()
}

fn children_of(nodes: &[Node], parent: u32) -> Vec<u32> {
    nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.parent == Some(parent))
        .map(|(i, _)| i as u32)
        .collect()
}

/// Find the first child of `parent` (or top-level when `parent`
/// is `None`) whose `name` matches exactly. Case-sensitive --
/// template field names like `IDAT` shouldn't accidentally match
/// `idat`.
fn find_child_by_name(nodes: &[Node], parent: Option<u32>, name: &str) -> Option<u32> {
    nodes
        .iter()
        .enumerate()
        .find(|(_, n)| n.parent == parent && n.name == name)
        .map(|(i, _)| i as u32)
}

/// Project a node's value into an `i128` when it's an integer
/// scalar; returns `None` for structs / arrays (no value at all),
/// floats, strings, and byte buffers. The caller decides whether
/// `None` is an error -- bare-path arithmetic surfaces it as
/// `NotAScalar`, but `offset()` / `len()` don't care: they only
/// read the span.
fn scalar_to_i128(node: &Node) -> Option<i128> {
    match &node.value {
        Some(Value::U8Val(v)) => Some(*v as i128),
        Some(Value::U16Val(v)) => Some(*v as i128),
        Some(Value::U32Val(v)) => Some(*v as i128),
        Some(Value::U64Val(v)) => Some(*v as i128),
        Some(Value::S8Val(v)) => Some(*v as i128),
        Some(Value::S16Val(v)) => Some(*v as i128),
        Some(Value::S32Val(v)) => Some(*v as i128),
        Some(Value::S64Val(v)) => Some(*v as i128),
        Some(Value::BoolVal(v)) => Some(if *v { 1 } else { 0 }),
        Some(Value::EnumVal((_, v))) => Some(*v as i128),
        Some(Value::F32Val(_)) | Some(Value::F64Val(_)) => None,
        Some(Value::StringVal(_)) | Some(Value::BytesVal(_)) => None,
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::TemplateInstance;
    use crate::files::TemplateInstanceId;
    use hxy_core::ByteOffset;
    use hxy_core::ByteRange;
    use hxy_plugin_host::template as wit;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn scalar_node(name: &str, parent: Option<u32>, value: wit::Value) -> wit::Node {
        scalar_node_at(name, parent, value, 0, 0)
    }

    fn scalar_node_at(name: &str, parent: Option<u32>, value: wit::Value, offset: u64, length: u64) -> wit::Node {
        wit::Node {
            name: name.to_owned(),
            type_name: wit::NodeType::Scalar(wit::ScalarKind::U64K),
            span: wit::Span { offset, length },
            value: Some(value),
            parent,
            array: None,
            display: None,
            attributes: vec![],
        }
    }

    fn struct_node(name: &str, parent: Option<u32>) -> wit::Node {
        struct_node_at(name, parent, 0, 0)
    }

    fn struct_node_at(name: &str, parent: Option<u32>, offset: u64, length: u64) -> wit::Node {
        wit::Node {
            name: name.to_owned(),
            type_name: wit::NodeType::StructType(name.to_owned()),
            span: wit::Span { offset, length },
            value: None,
            parent,
            array: None,
            display: None,
            attributes: vec![],
        }
    }

    fn instance(display_name: &str, nodes: Vec<wit::Node>, run_id: u64) -> TemplateInstance {
        TemplateInstance {
            id: TemplateInstanceId::new(run_id),
            source_path: PathBuf::from(display_name),
            display_name: display_name.to_owned(),
            range: ByteRange::new(ByteOffset(0), ByteOffset(0)).unwrap(),
            source_fingerprint: None,
            state: crate::files::TemplateState {
                parsed: None,
                tree: wit::ResultTree { nodes, diagnostics: vec![], byte_palette: None },
                expanded_arrays: HashMap::new(),
                collapsed: HashSet::new(),
                hovered_node: None,
                selected_node: None,
                leaf_boundaries: vec![],
                leaf_colors: vec![],
                leaf_node_indices: vec![],
                leaf_slot_by_node: HashMap::new(),
                node_color_overrides: HashMap::new(),
                show_colors: true,
                byte_palette_override: None,
            },
        }
    }

    fn parse_path(s: &str) -> Path {
        match hxy_calculator::parse(s).expect("parse") {
            hxy_calculator::Expr::Path(p) => p,
            other => panic!("expected path, got {other:?}"),
        }
    }

    fn lookup_value(resolver: &TemplateFieldResolver, expr: &str) -> i128 {
        let path = parse_path(expr);
        resolver.lookup(&path).expect("lookup").value.expect("scalar value")
    }

    /// Two-level tree:  PNG (root struct) -> length (u64=8192).
    /// Verifies the auto-descend through a single root.
    #[test]
    fn auto_descends_single_root() {
        let nodes = vec![struct_node("PNG", None), scalar_node("length", Some(0), wit::Value::U64Val(8192))];
        let inst = instance("png.bt", nodes, 1);
        let resolver = TemplateFieldResolver::new(std::slice::from_ref(&inst));
        assert_eq!(lookup_value(&resolver, "png.length"), 8192);
    }

    #[test]
    fn most_recent_run_wins_when_unsuffixed() {
        let nodes_old = vec![scalar_node("length", None, wit::Value::U64Val(100))];
        let nodes_new = vec![scalar_node("length", None, wit::Value::U64Val(200))];
        let instances = vec![instance("png.bt", nodes_old, 1), instance("png.bt", nodes_new, 2)];
        let resolver = TemplateFieldResolver::new(&instances);
        assert_eq!(lookup_value(&resolver, "png.length"), 200);
    }

    #[test]
    fn instance_suffix_picks_oldest() {
        let nodes_old = vec![scalar_node("length", None, wit::Value::U64Val(100))];
        let nodes_new = vec![scalar_node("length", None, wit::Value::U64Val(200))];
        let instances = vec![instance("png.bt", nodes_old, 1), instance("png.bt", nodes_new, 2)];
        let resolver = TemplateFieldResolver::new(&instances);
        assert_eq!(lookup_value(&resolver, "png#1.length"), 100);
        assert_eq!(lookup_value(&resolver, "png#2.length"), 200);
    }

    #[test]
    fn instance_out_of_range_errors() {
        let nodes = vec![scalar_node("length", None, wit::Value::U64Val(1))];
        let instances = vec![instance("png.bt", nodes, 1)];
        let resolver = TemplateFieldResolver::new(&instances);
        let err = resolver.lookup(&parse_path("png#5.length")).unwrap_err();
        assert!(matches!(err, ResolveError::InstanceOutOfRange { requested: 5, available: 1, .. }));
    }

    #[test]
    fn unknown_template_errors() {
        let resolver = TemplateFieldResolver::new(&[]);
        let err = resolver.lookup(&parse_path("missing.field")).unwrap_err();
        assert!(matches!(err, ResolveError::UnknownTemplate { .. }));
    }

    #[test]
    fn unknown_field_errors() {
        let nodes = vec![scalar_node("length", None, wit::Value::U64Val(1))];
        let inst = instance("png.bt", nodes, 1);
        let resolver = TemplateFieldResolver::new(std::slice::from_ref(&inst));
        let err = resolver.lookup(&parse_path("png.missing")).unwrap_err();
        assert!(matches!(err, ResolveError::FieldNotFound { component, .. } if component == "missing"));
    }

    #[test]
    fn struct_field_lookup_succeeds_with_no_value() {
        // png -> chunks (struct, no value). lookup() succeeds and
        // returns FieldRef with value: None; the eval layer is what
        // surfaces NotAScalar for bare-path arithmetic.
        let nodes = vec![struct_node("PNG", None), struct_node_at("chunks", Some(0), 8, 32)];
        let inst = instance("png.bt", nodes, 1);
        let resolver = TemplateFieldResolver::new(std::slice::from_ref(&inst));
        let f = resolver.lookup(&parse_path("png.chunks")).unwrap();
        assert_eq!(f.value, None);
        assert_eq!(f.offset, 8);
        assert_eq!(f.length, 32);
    }

    #[test]
    fn nested_indexed_field() {
        // PNG -> chunks (struct) -> chunk0 (struct) -> length (u64=42)
        //                        -> chunk1 (struct) -> length (u64=99)
        let nodes = vec![
            struct_node("PNG", None),                               // 0
            struct_node("chunks", Some(0)),                         // 1
            struct_node("chunk0", Some(1)),                         // 2
            scalar_node("length", Some(2), wit::Value::U64Val(42)), // 3
            struct_node("chunk1", Some(1)),                         // 4
            scalar_node("length", Some(4), wit::Value::U64Val(99)), // 5
        ];
        let inst = instance("png.bt", nodes, 1);
        let resolver = TemplateFieldResolver::new(std::slice::from_ref(&inst));
        assert_eq!(lookup_value(&resolver, "png.chunks[0].length"), 42);
        assert_eq!(lookup_value(&resolver, "png.chunks[1].length"), 99);
    }

    #[test]
    fn case_insensitive_template_name_match() {
        let nodes = vec![scalar_node("length", None, wit::Value::U64Val(7))];
        let inst = instance("PNG.BT", nodes, 1);
        let resolver = TemplateFieldResolver::new(std::slice::from_ref(&inst));
        assert_eq!(lookup_value(&resolver, "png.length"), 7);
    }

    #[test]
    fn span_offset_and_length_match_node() {
        // Field at offset 0x100, length 8.
        let nodes = vec![scalar_node_at("magic", None, wit::Value::U32Val(0xDEAD), 0x100, 8)];
        let inst = instance("png.bt", nodes, 1);
        let resolver = TemplateFieldResolver::new(std::slice::from_ref(&inst));
        let field = resolver.lookup(&parse_path("png.magic")).unwrap();
        assert_eq!(field.offset, 0x100);
        assert_eq!(field.length, 8);
        assert_eq!(field.value, Some(0xDEAD));
    }
}
