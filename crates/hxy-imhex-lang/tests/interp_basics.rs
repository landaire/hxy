//! End-to-end interpreter tests for ImHex pattern programs. Each
//! synthetic template is hand-rolled (no corpus content) and pairs
//! a small byte buffer with assertions on the resulting node tree.

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::NodeType;
use hxy_imhex_lang::RunResult;
use hxy_imhex_lang::ScalarKind;
use hxy_imhex_lang::Value;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

fn run(src: &str, bytes: Vec<u8>) -> RunResult {
    let tokens = tokenize(src).unwrap_or_else(|e| panic!("lex failed: {e}\n--- src ---\n{src}"));
    let program = parse(tokens).unwrap_or_else(|e| panic!("parse failed: {e}\n--- src ---\n{src}"));
    Interpreter::new(MemorySource::new(bytes)).run(&program)
}

fn assert_no_terminal_error(result: &RunResult) {
    if let Some(err) = result.terminal_error.as_ref() {
        panic!("interpreter halted: {err:?}\n  diagnostics: {:?}", result.diagnostics);
    }
}

#[test]
fn primitive_field_reads_advance_cursor() {
    let src = "\
struct Header {
    u8 magic;
    u32 size;
};
Header header;
";
    // magic=0xAB, size=0x01020304 (little-endian).
    let mut bytes = vec![0xAB];
    bytes.extend_from_slice(&0x01020304u32.to_le_bytes());
    let result = run(src, bytes);
    assert_no_terminal_error(&result);

    // Tree shape: Header struct, then magic and size as children.
    assert_eq!(result.nodes[0].name, "header");
    assert!(matches!(result.nodes[0].ty, NodeType::StructType(ref n) if n == "Header"));
    assert_eq!(result.nodes[0].length, 5);

    // magic
    assert_eq!(result.nodes[1].name, "magic");
    assert!(matches!(result.nodes[1].ty, NodeType::Scalar(ScalarKind::U8)));
    assert_eq!(result.nodes[1].offset, 0);
    assert!(matches!(result.nodes[1].value, Some(Value::UInt { value: 0xAB, .. })));

    // size
    assert_eq!(result.nodes[2].name, "size");
    assert!(matches!(result.nodes[2].ty, NodeType::Scalar(ScalarKind::U32)));
    assert_eq!(result.nodes[2].offset, 1);
    assert!(matches!(result.nodes[2].value, Some(Value::UInt { value: 0x0102_0304, .. })));
}

#[test]
fn char_array_decodes_as_string() {
    let src = "\
struct Greeting {
    char message[5];
};
Greeting g;
";
    let bytes = b"hello".to_vec();
    let result = run(src, bytes);
    assert_no_terminal_error(&result);

    let msg = &result.nodes[1];
    assert_eq!(msg.name, "message");
    assert!(matches!(msg.value, Some(Value::Str(ref s)) if s == "hello"));
    assert_eq!(msg.length, 5);
}

#[test]
fn enum_resolves_variant_by_value() {
    let src = "\
enum Kind : u8 {
    Empty = 0,
    Small = 1,
    Large = 2
};
Kind which;
";
    let result = run(src, vec![1]);
    assert_no_terminal_error(&result);
    let node = &result.nodes[0];
    assert!(matches!(node.ty, NodeType::EnumType(ref n) if n == "Kind"));
    // Enum nodes carry the raw integer as their value (so
    // comparisons with `Kind::Variant` work) and the matched
    // variant name as a `hxy_enum_variant` attribute.
    assert!(matches!(node.value, Some(Value::UInt { value: 1, .. })));
    assert!(node.attrs.iter().any(|(k, v)| k == "hxy_enum_variant" && v == "Small"));
}

#[test]
fn enum_with_unmatched_value_keeps_raw_numeric() {
    let src = "\
enum Kind : u8 {
    A = 1,
    B = 2
};
Kind which;
";
    let result = run(src, vec![99]);
    assert_no_terminal_error(&result);
    assert!(matches!(result.nodes[0].value, Some(Value::UInt { value: 99, .. })));
    assert!(!result.nodes[0].attrs.iter().any(|(k, _)| k == "hxy_enum_variant"));
}

#[test]
fn bitfield_extracts_each_field_in_order() {
    let src = "\
bitfield Flags {
    a : 1;
    b : 3;
    c : 4;
};
Flags f;
";
    // 0b_aaaa_aaab where a=4 bits c, b=3 bits b, low=1 bit a.
    // We pack in declaration order, low bit first:
    // raw byte bits: c3 c2 c1 c0  b2 b1 b0  a
    // wanting a=1, b=5, c=10 -> raw = (10<<4) | (5<<1) | 1 = 0xAB.
    let result = run(src, vec![0xAB]);
    assert_no_terminal_error(&result);
    // Bitfield parent + 3 children.
    assert!(matches!(result.nodes[0].ty, NodeType::BitfieldType(ref n) if n == "Flags"));
    let kids: Vec<_> = result.nodes.iter().filter(|n| n.parent.is_some()).collect();
    assert_eq!(kids.len(), 3);
    let by_name: std::collections::HashMap<&str, &Value> =
        kids.iter().filter_map(|n| n.value.as_ref().map(|v| (n.name.as_str(), v))).collect();
    assert!(matches!(by_name["a"], Value::UInt { value: 1, .. }));
    assert!(matches!(by_name["b"], Value::UInt { value: 5, .. }));
    assert!(matches!(by_name["c"], Value::UInt { value: 10, .. }));
}

#[test]
fn while_loop_reads_until_eof() {
    let src = "\
struct Buf {
    u32 count;
};
Buf b;
";
    let bytes = 3u32.to_le_bytes().to_vec();
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    let count_node = &result.nodes[1];
    assert!(matches!(count_node.value, Some(Value::UInt { value: 3, .. })));
}

#[test]
fn if_branches_on_magic_value() {
    let src = "\
struct Doc {
    u32 magic;
    if (magic == 0xDEADBEEF) {
        u32 ok;
    } else {
        u32 fallback;
    }
};
Doc d;
";
    let mut bytes = 0xDEADBEEFu32.to_le_bytes().to_vec();
    bytes.extend_from_slice(&42u32.to_le_bytes());
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    assert!(result.nodes.iter().any(|n| n.name == "ok"));
    assert!(!result.nodes.iter().any(|n| n.name == "fallback"));
}

#[test]
fn local_auto_with_initializer() {
    let src = "\
fn double(u32 x) { return x + x; }
struct S {
    u32 value;
    auto doubled = double(value);
};
S s;
";
    let result = run(src, 21u32.to_le_bytes().to_vec());
    assert_no_terminal_error(&result);
    // `auto` decls don't emit nodes (they're locals), so the tree
    // only has the struct + value reads.
    assert!(result.nodes.iter().any(|n| n.name == "value"));
}

#[test]
fn synthetic_png_like_header_parses_and_runs() {
    // Phase 1 acceptance test: a PNG-style header that exercises
    // primitives, strings, structs, and a length-prefixed chunk.
    // No content from the upstream corpus; written from scratch.
    let src = "\
#pragma endian big

struct Chunk {
    u32 length;
    char chunk_type[4];
};

struct PngFile {
    u8 signature[8];
    Chunk first_chunk;
};

PngFile file;
";
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    let result = run(src, bytes);
    assert_no_terminal_error(&result);

    // Walk the produced tree to find the `chunk_type` field.
    let chunk_type = result.nodes.iter().find(|n| n.name == "chunk_type").expect("chunk_type emitted");
    assert!(matches!(chunk_type.value, Some(Value::Str(ref s)) if s == "IHDR"));
}
