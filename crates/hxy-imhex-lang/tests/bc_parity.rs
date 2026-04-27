//! Parity tests for the experimental bytecode VM. Each fixture runs
//! the same source through both the AST interpreter and the bytecode
//! VM and asserts the emitted node tree matches verbatim. New op
//! lowerings add fixtures here so regressions surface before the
//! corpus probe.

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::RunResult;
use hxy_imhex_lang::bc;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

fn run_both(src: &str, bytes: Vec<u8>) -> (RunResult, RunResult) {
    let tokens = tokenize(src).expect("lex");
    let ast = parse(tokens).expect("parse");
    let bc_program = bc::compile(&ast).expect("compile to bytecode");

    let ast_result = Interpreter::new(MemorySource::new(bytes.clone())).run(&ast);
    let bc_result = Interpreter::new(MemorySource::new(bytes)).run_bytecode_experimental(&bc_program);
    (ast_result, bc_result)
}

#[track_caller]
fn assert_node_parity(ast: &RunResult, bc_run: &RunResult) {
    assert!(ast.terminal_error.is_none(), "AST run failed: {:?}", ast.terminal_error);
    assert!(bc_run.terminal_error.is_none(), "bytecode run failed: {:?}", bc_run.terminal_error);
    assert_eq!(ast.nodes, bc_run.nodes, "node tree diverged.\n  AST: {:#?}\n  BC:  {:#?}", ast.nodes, bc_run.nodes);
}

#[test]
fn top_level_primitive_reads_match_ast() {
    let src = "u8 magic;\nu32 size;\n";
    let mut bytes = vec![0xAB];
    bytes.extend_from_slice(&0x01020304u32.to_le_bytes());
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
    assert_eq!(bc_run.nodes.len(), 2);
}

#[test]
fn top_level_signed_and_float_primitives_match_ast() {
    let src = "s16 a;\nf32 b;\nbool c;\n";
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(-1234i16).to_le_bytes());
    bytes.extend_from_slice(&3.5f32.to_le_bytes());
    bytes.push(1);
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
    assert_eq!(bc_run.nodes.len(), 3);
}

#[test]
fn top_level_generic_int_spelling_matches_ast() {
    // `u24` is one of the byte-aligned generic spellings; the
    // compile pass must accept it the same way `register_primitives`
    // does at AST runtime.
    let src = "u24 sample;\n";
    let bytes = vec![0x01, 0x02, 0x03];
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
    assert_eq!(bc_run.nodes.len(), 1);
}

#[test]
fn simple_struct_body_matches_ast() {
    let src = "\
struct Header {
    u8 magic;
    u32 size;
};
Header header;
";
    let mut bytes = vec![0xAB];
    bytes.extend_from_slice(&0x01020304u32.to_le_bytes());
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
    assert_eq!(bc_run.nodes.len(), 3);
    // Struct node carries the right length (1 + 4 = 5 bytes consumed).
    assert_eq!(bc_run.nodes[0].length, 5);
}

#[test]
fn nested_struct_body_matches_ast() {
    let src = "\
struct Inner {
    u16 a;
    u16 b;
};
struct Outer {
    u8 tag;
    Inner inner;
};
Outer outer;
";
    let mut bytes = vec![0x42];
    bytes.extend_from_slice(&0x1111u16.to_le_bytes());
    bytes.extend_from_slice(&0x2222u16.to_le_bytes());
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
    // outer, tag, inner, a, b -- 5 nodes.
    assert_eq!(bc_run.nodes.len(), 5);
    assert_eq!(bc_run.nodes[0].length, 5); // outer span
    assert_eq!(bc_run.nodes[2].length, 4); // inner span
}

#[test]
fn sequential_top_level_struct_then_primitive_matches_ast() {
    let src = "\
struct Pair {
    u8 a;
    u8 b;
};
Pair p;
u32 trailer;
";
    let bytes = vec![0x01, 0x02, 0x10, 0x20, 0x30, 0x40];
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
}

#[test]
fn fixed_char_array_at_top_level_matches_ast() {
    // `char name[5];` -> a single ScalarArray(Char, 5) node
    // carrying the bytes as a `Str` value.
    let src = "char message[5];\n";
    let bytes = b"hello".to_vec();
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
    assert_eq!(bc_run.nodes.len(), 1);
    assert_eq!(bc_run.nodes[0].length, 5);
}

#[test]
fn fixed_primitive_array_inside_struct_matches_ast() {
    let src = "\
struct Header {
    u8 magic;
    u16 samples[3];
};
Header h;
";
    let mut bytes = vec![0x42];
    bytes.extend_from_slice(&100u16.to_le_bytes());
    bytes.extend_from_slice(&200u16.to_le_bytes());
    bytes.extend_from_slice(&300u16.to_le_bytes());
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
    // Header, magic, samples. The primitive array collapses to one
    // ScalarArray span instead of a parent + N per-element nodes,
    // matching the 010-lang renderer behavior.
    assert_eq!(bc_run.nodes.len(), 3);
    // Header consumed 1 + 6 = 7 bytes.
    assert_eq!(bc_run.nodes[0].length, 7);
}

#[test]
fn fixed_char_array_inside_struct_matches_ast() {
    let src = "\
struct Greeting {
    char tag;
    char body[4];
};
Greeting g;
";
    let bytes = b"!cafe".to_vec();
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
}

#[test]
fn ident_sized_char_array_matches_ast() {
    // The classic dyn-width string read: a length byte followed by
    // that many bytes. Bencode's ASCII-decimal-prefixed strings are
    // the canonical case (`String { ASCIIDecimal length; char
    // value[length]; }`); this synthetic version uses a u8 length
    // for simplicity.
    let src = "\
struct PString {
    u8 length;
    char value[length];
};
PString s;
";
    let mut bytes = vec![5];
    bytes.extend_from_slice(b"hello");
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
    // s, length, value -- 3 nodes total.
    assert_eq!(bc_run.nodes.len(), 3);
    assert_eq!(bc_run.nodes[0].length, 6);
    assert_eq!(bc_run.nodes[2].length, 5);
}

#[test]
fn ident_sized_primitive_array_matches_ast() {
    // Element-by-element variant: a u8 count followed by `count`
    // u16 little-endian samples. Exercises the parent + N-children
    // emission path on the dyn-size code path.
    let src = "\
struct SampleSet {
    u8 count;
    u16 samples[count];
};
SampleSet set;
";
    let mut bytes = vec![3];
    bytes.extend_from_slice(&0x1111u16.to_le_bytes());
    bytes.extend_from_slice(&0x2222u16.to_le_bytes());
    bytes.extend_from_slice(&0x3333u16.to_le_bytes());
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
}

#[test]
fn binop_sized_char_array_matches_ast() {
    // `count - 1` exercises BinOp lowering through the dyn array.
    // Common in length-prefixed strings that count the trailing
    // separator (`u8 length; char value[length - 1];`).
    let src = "\
struct PStringMinus1 {
    u8 length;
    char value[length - 1];
};
PStringMinus1 s;
";
    let mut bytes = vec![5];
    bytes.extend_from_slice(b"hello"); // we only consume 4
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
    assert_eq!(bc_run.nodes[2].length, 4);
}

#[test]
fn binop_mul_sized_primitive_array_matches_ast() {
    // `count * 2` so the same input feeds both runs.
    let src = "\
struct DoubleSet {
    u8 count;
    u8 data[count * 2];
};
DoubleSet d;
";
    let bytes = vec![2, 0xAA, 0xBB, 0xCC, 0xDD];
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
}

#[test]
fn top_level_placement_primitive_matches_ast() {
    // `Type x @ N` at top level seeks the cursor and reads from
    // there; the cursor stays at the read end. The emitted node
    // gets an `hxy_placement` attribute carrying the offset.
    let src = "u32 magic @ 0x04;\n";
    let mut bytes = vec![0xFF; 4];
    bytes.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
    assert_eq!(bc_run.nodes.len(), 1);
    assert_eq!(bc_run.nodes[0].offset, 4);
    assert!(bc_run.nodes[0].attrs.iter().any(|(k, v)| k == "hxy_placement" && v == "4"));
}

#[test]
fn top_level_placement_char_array_matches_ast() {
    // Real-world shape from n64.hexpat: `char name[20] @ 0x20;`.
    let src = "char name[20] @ 0x20;\n";
    let mut bytes = vec![0xAA; 0x20];
    bytes.extend_from_slice(b"NINTENDO 64 ROM TXTX");
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
    assert_eq!(bc_run.nodes[0].offset, 0x20);
    assert_eq!(bc_run.nodes[0].length, 20);
}

#[test]
fn simple_enum_inside_struct_matches_ast() {
    let src = "\
enum Tag : u8 {
    A,
    B,
    C
};
struct S {
    Tag tag;
    u16 value;
};
S s;
";
    let bytes = vec![1, 0xCD, 0xAB];
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
}

#[test]
fn fixed_struct_array_matches_ast() {
    let src = "\
struct Color {
    u8 r;
    u8 g;
    u8 b;
};
struct Palette {
    Color colors[4];
};
Palette pal;
";
    let mut bytes = Vec::new();
    for i in 0..4u8 {
        bytes.extend_from_slice(&[i, i + 1, i + 2]);
    }
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
    // Palette + colors-array + 4 Color structs + 12 primitive children = 18.
    assert_eq!(bc_run.nodes.len(), 18);
}

#[test]
fn decorative_attrs_pass_through_matches_ast() {
    // `[[name(...), comment(...)]]` are pure pass-through to the
    // emitted node's attr list -- the renderer reads them but the
    // VM doesn't need to do anything special.
    let src = "u32 magic [[name(\"Magic\"), comment(\"file magic\")]];\n";
    let bytes = 0xDEADBEEFu32.to_le_bytes().to_vec();
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
    assert!(bc_run.nodes[0].attrs.iter().any(|(k, v)| k == "name" && v == "Magic"));
    assert!(bc_run.nodes[0].attrs.iter().any(|(k, v)| k == "comment" && v == "file magic"));
}

#[test]
fn top_level_placement_struct_matches_ast() {
    // Real-world shape from cda.hexpat: two top-level placed structs.
    let src = "\
struct Header {
    u32 RIFF;
    s32 size;
};
struct DataInfo {
    u32 range;
    u32 duration;
};
Header header @ 0;
DataInfo data @ 0x10;
";
    let mut bytes = vec![0u8; 0x20];
    bytes[0..4].copy_from_slice(&0x46464952u32.to_le_bytes()); // "RIFF"
    bytes[4..8].copy_from_slice(&42i32.to_le_bytes());
    bytes[0x10..0x14].copy_from_slice(&7u32.to_le_bytes());
    bytes[0x14..0x18].copy_from_slice(&100u32.to_le_bytes());
    let (ast, bc_run) = run_both(src, bytes);
    assert_node_parity(&ast, &bc_run);
}
