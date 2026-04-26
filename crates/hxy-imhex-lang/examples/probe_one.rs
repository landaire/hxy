//! One-shot debugging probe: lex + parse a single file and print
//! the precise error if any. Used while investigating individual
//! corpus failures.

use std::env;
use std::fs;

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::chained_resolver;
use hxy_imhex_lang::extract_pragmas;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

fn main() {
    let path = env::args().nth(1).expect("usage: probe_one <template> [fixture]");
    let fixture = env::args().nth(2);
    let src = fs::read_to_string(&path).expect("read file");
    let tokens = match tokenize(&src) {
        Ok(t) => t,
        Err(e) => {
            println!("lex: {e}");
            if let hxy_imhex_lang::LexError::UnexpectedChar { offset, .. } = e {
                let lo = offset.saturating_sub(40);
                let hi = (offset + 40).min(src.len());
                println!("context: {}", &src[lo..hi].replace('\n', " | "));
            }
            return;
        }
    };
    let prog = match parse(tokens) {
        Ok(p) => p,
        Err(e) => {
            println!("parse: {e}");
            return;
        }
    };
    let bytes = fixture.map(|p| fs::read(p).unwrap_or_default()).unwrap_or_default();
    let resolver =
        chained_resolver(["/Users/lander/src/ImHex-Patterns/includes", "/Users/lander/src/ImHex-Patterns"]);
    let pragmas = extract_pragmas(&src);
    let mut interp = Interpreter::new(MemorySource::new(bytes))
        .with_import_resolver(resolver)
        .with_step_limit(500_000);
    if let Some(e) = pragmas.endian {
        interp = interp.with_default_endian(e);
    }
    let result = interp.run(&prog);
    match result.terminal_error {
        None => println!("ok ({} nodes)", result.nodes.len()),
        Some(e) => println!("run: {e}"),
    }
}
