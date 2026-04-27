//! Phase 4 acceptance tests. Cover template instantiation
//! (`struct Foo<auto N>`), pointer reads (`Type *p : u32`), and
//! struct inheritance composition (`struct B : A`). Each template
//! is hand-rolled here; no template content is copied from the
//! GPL-licensed corpus.

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::RunResult;
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
fn struct_template_binds_value_param() {
    // `struct FixedBuf<auto N> { u8 data[N]; }` reads N bytes when
    // instantiated with FixedBuf<3>.
    let src = "\
struct FixedBuf<auto N> {
    u8 data[N];
};
FixedBuf<3> buf;
";
    let result = run(src, vec![0x01, 0x02, 0x03, 0xFF]);
    assert_no_terminal_error(&result);
    let buf = result.nodes.first().unwrap();
    assert_eq!(buf.length, 3);
    let data = result.nodes.iter().find(|n| n.name == "data").unwrap();
    assert_eq!(data.length, 3);
}

#[test]
fn template_param_is_visible_in_assertion() {
    // The corpus's `type::Magic<auto ExpectedValue>` shape: bind a
    // string-typed template arg, compare against a read.
    let src = "\
struct Magic<auto Expected> {
    char value[std::string::length(Expected)];
    std::assert(value == Expected, \"magic mismatch\");
};
Magic<\"FOO\"> m;
";
    let result = run(src, b"FOO".to_vec());
    assert_no_terminal_error(&result);
    let value_node = result.nodes.iter().find(|n| n.name == "value").unwrap();
    assert!(matches!(value_node.value, Some(Value::Str(ref s)) if s == "FOO"));
}

#[test]
fn template_assert_failure_is_surfaced() {
    let src = "\
struct Magic<auto Expected> {
    char value[std::string::length(Expected)];
    std::assert(value == Expected, \"magic mismatch\");
};
Magic<\"BAR\"> m;
";
    let result = run(src, b"FOO".to_vec());
    assert!(result.terminal_error.is_some(), "assert should halt");
    assert!(result.diagnostics.iter().any(|d| d.message.contains("magic mismatch")));
}

#[test]
fn template_via_template_keyword_prefix_form() {
    // `template<T> struct Foo<T>` -- the leading `template<...>`
    // and the type's own `<T>` should produce the same AST.
    let src = "\
template<auto N>
struct Sized<auto N> {
    u8 bytes[N];
};
Sized<2> s;
";
    let result = run(src, vec![0xAA, 0xBB, 0xCC]);
    assert_no_terminal_error(&result);
    let bytes = result.nodes.iter().find(|n| n.name == "bytes").unwrap();
    assert_eq!(bytes.length, 2);
}

#[test]
fn enum_template_binds_param() {
    // `enum E<auto Backing> : u32 { ... }` -- the param doesn't
    // change the read width here, but it should parse + run.
    let src = "\
enum Tagged<auto Tag> : u8 {
    A = 1,
    B = 2
};
Tagged<\"label\"> t;
";
    let result = run(src, vec![1]);
    assert_no_terminal_error(&result);
    let node = result.nodes.first().unwrap();
    assert!(matches!(node.value, Some(Value::UInt { value: 1, .. })));
    assert!(node.attrs.iter().any(|(k, v)| k == "hxy_enum_variant" && v == "A"));
}

#[test]
fn pointer_field_reads_address_and_dereferences() {
    // Layout:
    //   [0..4] = u32 pointer = 0x08
    //   [8..12] = u32 target = 0xCAFEBABE
    let src = "\
struct File {
    u32 *target : u32;
};
File f;
";
    let mut bytes = 0x08u32.to_le_bytes().to_vec();
    bytes.extend_from_slice(&[0u8; 4]); // padding 4..8
    bytes.extend_from_slice(&0xCAFEBABEu32.to_le_bytes()); // target @ 8..12
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    // The renderer sees both a `target__ptr` leaf at offset 0
    // (the address slot) and a `target` leaf at offset 8 (the
    // dereferenced read).
    let ptr_node = result.nodes.iter().find(|n| n.name == "target__ptr").unwrap();
    assert_eq!(ptr_node.offset, 0);
    let target_node = result.nodes.iter().find(|n| n.name == "target").unwrap();
    assert_eq!(target_node.offset, 8);
    assert!(matches!(target_node.value, Some(Value::UInt { value: 0xCAFE_BABE, .. })));
}

#[test]
fn pointer_advances_cursor_past_address_only() {
    // After a pointer field `Type *p : u32;`, the surrounding
    // sequential cursor should land at offset 4 (after the u32
    // address read), not at the dereferenced offset.
    let src = "\
struct File {
    u32 *target : u32;
    u8 next;
};
File f;
";
    let mut bytes = 0x10u32.to_le_bytes().to_vec();
    bytes.push(0xAA); // next byte at offset 4
    bytes.extend_from_slice(&[0u8; 11]); // padding to offset 16
    bytes.extend_from_slice(&0xDEADu32.to_le_bytes());
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    let next = result.nodes.iter().find(|n| n.name == "next").unwrap();
    assert_eq!(next.offset, 4);
    assert!(matches!(next.value, Some(Value::UInt { value: 0xAA, .. })));
}

#[test]
fn inheritance_composes_parent_body_before_child() {
    // `struct B : A { u32 b; }` reads A's fields first, then B's.
    let src = "\
struct A {
    u32 a;
};
struct B : A {
    u32 b;
};
B inst;
";
    let mut bytes = 0x11111111u32.to_le_bytes().to_vec();
    bytes.extend_from_slice(&0x22222222u32.to_le_bytes());
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    let a = result.nodes.iter().find(|n| n.name == "a").expect("parent field read");
    let b = result.nodes.iter().find(|n| n.name == "b").expect("child field read");
    assert_eq!(a.offset, 0);
    assert!(matches!(a.value, Some(Value::UInt { value: 0x1111_1111, .. })));
    assert_eq!(b.offset, 4);
    assert!(matches!(b.value, Some(Value::UInt { value: 0x2222_2222, .. })));
}

#[test]
fn inheritance_lengths_account_for_parent_body() {
    let src = "\
struct A { u8 a; u8 b; };
struct C : A { u32 c; };
C inst;
";
    let mut bytes = vec![0x01, 0x02];
    bytes.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    let inst = result.nodes.first().unwrap();
    // parent contributes 2 bytes, child u32 contributes 4 -- total 6.
    assert_eq!(inst.length, 6);
}
