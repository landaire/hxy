//! Wallclock comparison: bencode.hexpat against a real torrent
//! fixture, run through the AST interpreter and the bytecode VM
//! back to back. Prints the elapsed time for each.
//!
//! Step limit is bumped (`100_000_000`) so the runs aren't capped
//! before completing -- bencode's recursive walk burns budget fast.

use std::env;
use std::fs;
use std::time::Instant;

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::bc;
use hxy_imhex_lang::chained_resolver;
use hxy_imhex_lang::extract_pragmas;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

fn main() {
    let template_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "/Users/lander/src/ImHex-Patterns/patterns/bencode.hexpat".into());
    let fixture_path = env::args().nth(2).unwrap_or_else(|| {
        "/Users/lander/src/ImHex-Patterns/tests/patterns/test_data/bencode.hexpat.torrent".into()
    });
    let src = fs::read_to_string(&template_path).expect("read template");
    let bytes = fs::read(&fixture_path).expect("read fixture");
    let tokens = tokenize(&src).expect("lex");
    let ast = parse(tokens).expect("parse");
    let resolver = chained_resolver([
        "/Users/lander/src/ImHex-Patterns/includes",
        "/Users/lander/src/ImHex-Patterns",
    ]);
    let pragmas = extract_pragmas(&src);

    let bc_program = bc::compile_with_resolver(&ast, resolver.as_ref()).expect("bc compile");

    println!(
        "template: {} ({} bytes), fixture: {} ({} bytes)",
        template_path,
        src.len(),
        fixture_path,
        bytes.len()
    );

    // AST run.
    {
        let start = Instant::now();
        let mut interp = Interpreter::new(MemorySource::new(bytes.clone()))
            .with_import_resolver(resolver.clone())
            .with_step_limit(100_000_000);
        if let Some(e) = pragmas.endian {
            interp = interp.with_default_endian(e);
        }
        let result = interp.run(&ast);
        let elapsed = start.elapsed();
        println!(
            "AST  : {} ms, {} nodes, terminal_error={:?}",
            elapsed.as_millis(),
            result.nodes.len(),
            result.terminal_error,
        );
    }

    // BC run.
    {
        let start = Instant::now();
        let mut interp = Interpreter::new(MemorySource::new(bytes))
            .with_import_resolver(resolver.clone())
            .with_step_limit(100_000_000);
        if let Some(e) = pragmas.endian {
            interp = interp.with_default_endian(e);
        }
        let result = interp.run_bytecode_experimental(&bc_program);
        let elapsed = start.elapsed();
        println!(
            "BC   : {} ms, {} nodes, terminal_error={:?}",
            elapsed.as_millis(),
            result.nodes.len(),
            result.terminal_error,
        );
    }
}
