//! One-shot debugging probe: lex + parse a single file and print
//! the precise error if any. Used while investigating individual
//! corpus failures.

use std::env;
use std::fs;

use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

fn main() {
    let path = env::args().nth(1).expect("usage: probe_one <file>");
    let src = fs::read_to_string(&path).expect("read file");
    match tokenize(&src) {
        Ok(tokens) => match parse(tokens) {
            Ok(_) => println!("ok"),
            Err(e) => println!("parse: {e}"),
        },
        Err(e) => {
            println!("lex: {e}");
            if let hxy_imhex_lang::LexError::UnexpectedChar { offset, .. } = e {
                let lo = offset.saturating_sub(40);
                let hi = (offset + 40).min(src.len());
                println!("context: {}", &src[lo..hi].replace('\n', " | "));
            }
        }
    }
}
