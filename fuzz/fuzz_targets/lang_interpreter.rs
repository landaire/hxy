#![no_main]

//! Interpreter fuzzer: split input into (template, data), parse,
//! execute. Bounds execution to keep libFuzzer from stalling on
//! pathological templates — interpreter loops can legitimately run
//! forever on valid programs (e.g. `while (1) {}`).

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

/// One fuzz run. The template source is `template`; the byte source
/// the template walks is `data`. `arbitrary`'s derive lets us split
/// an input byte stream into these two fields cleanly.
#[derive(Debug, Arbitrary)]
struct Run<'a> {
    template: &'a str,
    data: &'a [u8],
}

fuzz_target!(|run: Run<'_>| {
    let Ok(tokens) = hxy_010_lang::tokenize(run.template) else { return };
    let Ok(program) = hxy_010_lang::parse(tokens) else { return };
    let source = hxy_010_lang::MemorySource::new(run.data.to_vec());
    // Tight step budget so libFuzzer isn't blocked by a valid
    // template that happens to loop a lot.
    let _ = hxy_010_lang::Interpreter::new(source)
        .with_step_limit(50_000)
        .run(&program);
});
