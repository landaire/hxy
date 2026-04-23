#![no_main]

//! Structured fuzzer: draws a valid-ish 010 program from the
//! `FuzzProgram` schema in `hxy_010_lang::fuzz`, emits it to source,
//! and runs the full tokenize → parse → interpret pipeline. Covers
//! interpreter paths random bytes can't reach cheaply (most random
//! inputs don't produce a keyword, let alone a typedef).

use libfuzzer_sys::fuzz_target;

fuzz_target!(|prog: hxy_010_lang::fuzz::FuzzProgram| {
    let source = prog.emit();
    let Ok(tokens) = hxy_010_lang::tokenize(&source) else { return };
    let Ok(ast) = hxy_010_lang::parse(tokens) else { return };
    let data = hxy_010_lang::MemorySource::new(prog.data);
    let _ = hxy_010_lang::Interpreter::new(data)
        .with_step_limit(50_000)
        .run(&ast);
});
