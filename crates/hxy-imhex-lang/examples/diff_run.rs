//! Run a template + fixture through both AST and BC, dump every
//! diverging node side-by-side. Quick debug aid for tracking down
//! per-template parity bugs.

use std::env;
use std::fs;

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::bc;
use hxy_imhex_lang::chained_resolver;
use hxy_imhex_lang::extract_pragmas;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

fn main() {
    let template = env::args().nth(1).expect("usage: diff_run <template> <fixture>");
    let fixture = env::args().nth(2).expect("usage: diff_run <template> <fixture>");
    let src = fs::read_to_string(&template).expect("read template");
    let bytes = fs::read(&fixture).expect("read fixture");
    let tokens = tokenize(&src).expect("lex");
    let ast = parse(tokens).expect("parse");
    let resolver = chained_resolver([
        "/Users/lander/src/ImHex-Patterns/includes",
        "/Users/lander/src/ImHex-Patterns",
    ]);
    let pragmas = extract_pragmas(&src);
    let bc_program = bc::compile_with_resolver(&ast, resolver.as_ref()).expect("bc compile");

    let mut a = Interpreter::new(MemorySource::new(bytes.clone()))
        .with_import_resolver(resolver.clone())
        .with_step_limit(100_000_000);
    if let Some(e) = pragmas.endian {
        a = a.with_default_endian(e);
    }
    let ar = a.run(&ast);

    let mut b = Interpreter::new(MemorySource::new(bytes))
        .with_import_resolver(resolver.clone())
        .with_step_limit(100_000_000);
    if let Some(e) = pragmas.endian {
        b = b.with_default_endian(e);
    }
    let br = b.run_bytecode_experimental(&bc_program);

    println!("AST nodes: {}, terminal: {:?}", ar.nodes.len(), ar.terminal_error);
    println!("BC  nodes: {}, terminal: {:?}", br.nodes.len(), br.terminal_error);
    let common = ar.nodes.len().min(br.nodes.len());
    let mut diffs = 0;
    for i in 0..common {
        if ar.nodes[i] != br.nodes[i] {
            diffs += 1;
            if diffs <= 30 {
                let a = &ar.nodes[i];
                let b = &br.nodes[i];
                println!(
                    "node[{i}] DIFF:\n  AST: name={} ty={:?} off={} len={} val={:?} parent={:?}\n  BC:  name={} ty={:?} off={} len={} val={:?} parent={:?}",
                    a.name, a.ty, a.offset, a.length, a.value, a.parent,
                    b.name, b.ty, b.offset, b.length, b.value, b.parent,
                );
            }
        }
    }
    println!("total node diffs in shared prefix: {diffs}");
}
