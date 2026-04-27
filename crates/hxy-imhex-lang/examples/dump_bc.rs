//! Print the compiled bytecode Program for a template, including
//! per-struct body op streams. Quick debug aid.

use std::env;
use std::fs;

use hxy_imhex_lang::bc;
use hxy_imhex_lang::chained_resolver;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

fn main() {
    let path = env::args().nth(1).expect("usage: dump_bc <template>");
    let src = fs::read_to_string(&path).expect("read");
    let tokens = tokenize(&src).expect("lex");
    let ast = parse(tokens).expect("parse");
    let resolver = chained_resolver([
        "/Users/lander/src/ImHex-Patterns/includes",
        "/Users/lander/src/ImHex-Patterns",
    ]);
    let prog = bc::compile_with_resolver(&ast, resolver.as_ref()).expect("bc compile");
    println!("== top-level ops ({}) ==", prog.ops.len());
    for (i, op) in prog.ops.iter().enumerate() {
        println!("  {i:4}  {op:?}");
    }
    println!();
    for (i, body) in prog.struct_bodies.iter().enumerate() {
        let name = prog.idents.get(body.display_name.0);
        println!("== struct body[{i}] = {name} ({} ops) ==", body.ops.len());
        for (j, op) in body.ops.iter().enumerate() {
            println!("  {j:4}  {op:?}");
        }
        println!();
    }
}
