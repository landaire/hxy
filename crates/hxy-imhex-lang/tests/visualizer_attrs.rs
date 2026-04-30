//! End-to-end check that `[[hex::visualize(...)]]` and
//! `[[hex::inline_visualize(...)]]` round-trip from template source
//! through the interpreter into the canonical attribute form host
//! consumers (the visualizer panel) read.

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

const ARG_SEP: &str = "\u{1f}";

fn run(template: &str, bytes: Vec<u8>) -> hxy_imhex_lang::RunResult {
    let tokens = tokenize(template).expect("tokenize");
    let program = parse(tokens).expect("parse");
    let interp = Interpreter::new(MemorySource::new(bytes));
    interp.run(&program)
}

fn find_attr<'a>(nodes: &'a [hxy_imhex_lang::NodeOut], field: &str, key: &str) -> Option<&'a str> {
    nodes.iter().find(|n| n.name == field)?.attrs.iter().find_map(|(k, v)| (k == key).then_some(v.as_str()))
}

#[test]
fn visualize_attribute_canonicalised_and_packed() {
    // Single-arg "image" visualizer: just the name, no extra args.
    // The canonicalizer rewrites `hex::visualize` -> `hxy_visualize`
    // and the multi-arg encoder packs every arg with the unit
    // separator. With one arg the output is the bare name.
    let template = r#"
        u32 data [[hex::visualize("image")]];
    "#;
    let result = run(template, vec![0u8; 4]);
    assert!(result.terminal_error.is_none(), "interp failed: {:?}", result.terminal_error);
    let value = find_attr(&result.nodes, "data", "hxy_visualize").expect("data field carries hxy_visualize attribute");
    assert_eq!(value, "image");
}

#[test]
fn visualize_attribute_packs_multi_arg_format() {
    // bitmap takes (format, width, height). All three should land in
    // the value, US-separated, in source order.
    let template = r#"
        u8 data[12] [[hex::visualize("bitmap", "RGBA8", 1, 3)]];
    "#;
    let result = run(template, vec![0u8; 12]);
    assert!(result.terminal_error.is_none(), "interp failed: {:?}", result.terminal_error);
    let value = find_attr(&result.nodes, "data", "hxy_visualize").expect("data field carries hxy_visualize attribute");
    let expected = format!("bitmap{ARG_SEP}RGBA8{ARG_SEP}1{ARG_SEP}3");
    assert_eq!(value, expected);
}

#[test]
fn inline_visualize_attribute_uses_distinct_key() {
    // Inline variant maps to a separate canonical key so the panel
    // can render both inline thumbnails (in the row) and popout
    // tabs from the same node.
    let template = r#"
        u8 data[4] [[hex::inline_visualize("hex_viewer")]];
    "#;
    let result = run(template, vec![1, 2, 3, 4]);
    assert!(result.terminal_error.is_none(), "interp failed: {:?}", result.terminal_error);
    let value = find_attr(&result.nodes, "data", "hxy_inline_visualize")
        .expect("data field carries hxy_inline_visualize attribute");
    assert_eq!(value, "hex_viewer");
    // And not the popout key, since these are distinct attributes.
    assert!(find_attr(&result.nodes, "data", "hxy_visualize").is_none());
}
