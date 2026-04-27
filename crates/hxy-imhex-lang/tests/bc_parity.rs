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
    let bc_result =
        Interpreter::new(MemorySource::new(bytes)).run_bytecode_experimental(&bc_program);
    (ast_result, bc_result)
}

#[track_caller]
fn assert_node_parity(ast: &RunResult, bc_run: &RunResult) {
    assert!(
        ast.terminal_error.is_none(),
        "AST run failed: {:?}",
        ast.terminal_error
    );
    assert!(
        bc_run.terminal_error.is_none(),
        "bytecode run failed: {:?}",
        bc_run.terminal_error
    );
    assert_eq!(
        ast.nodes, bc_run.nodes,
        "node tree diverged.\n  AST: {:#?}\n  BC:  {:#?}",
        ast.nodes, bc_run.nodes
    );
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
    // Header, magic, samples (parent), three u16 children.
    assert_eq!(bc_run.nodes.len(), 6);
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
