//! AST counterpart to `probe_bc`. Step limit pumped to 100M so
//! the full bencode walk completes for profiling comparisons.

use std::env;
use std::fs;
use std::time::Instant;

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::chained_resolver;
use hxy_imhex_lang::extract_pragmas;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

fn main() {
    let path = env::args().nth(1).expect("usage: probe_ast <template> [fixture]");
    let fixture = env::args().nth(2);
    let src = fs::read_to_string(&path).expect("read template");
    let tokens = tokenize(&src).expect("lex");
    let prog = parse(tokens).expect("parse");
    let bytes = fixture.map(|p| fs::read(p).unwrap_or_default()).unwrap_or_default();
    let resolver = chained_resolver(["/Users/lander/src/ImHex-Patterns/includes", "/Users/lander/src/ImHex-Patterns"]);
    let pragmas = extract_pragmas(&src);
    let mut interp =
        Interpreter::new(MemorySource::new(bytes)).with_import_resolver(resolver).with_step_limit(100_000_000);
    if let Some(e) = pragmas.endian {
        interp = interp.with_default_endian(e);
    }
    let start = Instant::now();
    let result = interp.run(&prog);
    let elapsed = start.elapsed();
    match result.terminal_error {
        None => println!("ok ({} nodes) ast={} ms", result.nodes.len(), elapsed.as_millis()),
        Some(e) => println!("run: {e} (after {} ms)", elapsed.as_millis()),
    }
}
