#![no_main]

//! Lexer fuzzer: feed arbitrary UTF-8 to `tokenize` and assert it
//! never panics. Invalid bytes are dropped before calling in so
//! libFuzzer spends its budget on interesting UTF-8 inputs rather
//! than failing `str::from_utf8` early.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let _ = hxy_010_lang::tokenize(s);
});
