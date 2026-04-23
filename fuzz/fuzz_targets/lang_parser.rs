#![no_main]

//! Parser fuzzer: arbitrary UTF-8 → tokenize → parse. Either result
//! is acceptable; we're hunting for panics (unwrap on bad tokens,
//! arithmetic overflow on span math, etc.).

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let Ok(tokens) = hxy_010_lang::tokenize(s) else { return };
    let _ = hxy_010_lang::parse(tokens);
});
