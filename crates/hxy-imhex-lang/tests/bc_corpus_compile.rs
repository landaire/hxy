//! Asserts that specific real corpus templates compile through the
//! bytecode VM. These are the smoke test for "can we actually run
//! anything from the wild?" -- each new feature should add the
//! template(s) it unlocks here, plus a small synthetic byte input
//! so we know the VM doesn't just compile the program but also
//! produces the same node tree the AST walker does.
//!
//! The corpus path is hard-coded to the user's checkout because
//! the upstream repo is GPL-licensed and we deliberately don't
//! bundle it. Tests are gated with `#[ignore]` only when the file
//! isn't present.

use std::path::Path;

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::RunResult;
use hxy_imhex_lang::bc;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

const CORPUS_ROOT: &str = "/Users/lander/src/ImHex-Patterns/patterns";

fn read_template(name: &str) -> Option<String> {
    let p = Path::new(CORPUS_ROOT).join(name);
    std::fs::read_to_string(p).ok()
}

fn run_both(src: &str, bytes: Vec<u8>) -> (RunResult, RunResult) {
    let tokens = tokenize(src).expect("lex");
    let ast = parse(tokens).expect("parse");
    let bc_program = bc::compile(&ast).expect("compile to bytecode");

    let ast_result = Interpreter::new(MemorySource::new(bytes.clone())).run(&ast);
    let bc_result =
        Interpreter::new(MemorySource::new(bytes)).run_bytecode_experimental(&bc_program);
    (ast_result, bc_result)
}

#[test]
fn corpus_n64_compiles_and_matches_ast() {
    let Some(src) = read_template("n64.hexpat") else {
        eprintln!("skip: n64.hexpat not present in {CORPUS_ROOT}");
        return;
    };
    // Just enough bytes to satisfy `char name[20] @ 0x20;`.
    let mut bytes = vec![0xAAu8; 0x20];
    bytes.extend_from_slice(b"NINTENDO 64 ROM TXTX");
    let (ast, bc_run) = run_both(&src, bytes);
    assert!(ast.terminal_error.is_none(), "AST: {:?}", ast.terminal_error);
    assert!(bc_run.terminal_error.is_none(), "BC: {:?}", bc_run.terminal_error);
    assert_eq!(ast.nodes, bc_run.nodes);
}

#[test]
fn corpus_cda_compiles_and_matches_ast() {
    let Some(src) = read_template("cda.hexpat") else {
        eprintln!("skip: cda.hexpat not present in {CORPUS_ROOT}");
        return;
    };
    // Header is 28 bytes (32-bit RIFF/size/CDDA/fmt/lengthofchunk +
    // 16-bit version/range + 32-bit identifier = 4*5 + 2*2 + 4 = 28).
    // DataInfo is 16 bytes (4*2 + 8*1 = 16).
    // Layout: Header @ 0, DataInfo @ 0x1C.
    let mut bytes = vec![0u8; 0x1C + 16];
    bytes[0..4].copy_from_slice(&0x46464952u32.to_le_bytes()); // "RIFF"
    bytes[4..8].copy_from_slice(&42i32.to_le_bytes());
    let (ast, bc_run) = run_both(&src, bytes);
    assert!(ast.terminal_error.is_none(), "AST: {:?}", ast.terminal_error);
    assert!(bc_run.terminal_error.is_none(), "BC: {:?}", bc_run.terminal_error);
    assert_eq!(ast.nodes, bc_run.nodes);
}
