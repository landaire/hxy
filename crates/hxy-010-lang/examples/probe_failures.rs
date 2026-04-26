//! Companion to `probe_corpus`: print the *first* failing file (and
//! the offending source snippet around the failure span) for each
//! distinct lex / parse / run error message. Useful for narrowing
//! parser fixes without dumping the full corpus.
//!
//! `cargo run --example probe_failures --release -- <dir>`

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use hxy_010_lang::Interpreter;
use hxy_010_lang::MemorySource;
use hxy_010_lang::RuntimeError;
use hxy_010_lang::parse;
use hxy_010_lang::tokenize;

fn main() {
    let dir = env::args().nth(1).expect("usage: probe_failures <dir>");
    let dir = PathBuf::from(dir);
    let mut samples: BTreeMap<String, (String, String)> = BTreeMap::new(); // msg -> (path, snippet)
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("bt"))
        .collect();
    files.sort();
    for path in &files {
        let raw = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let tokens = match tokenize(&raw) {
            Ok(t) => t,
            Err(e) => {
                let msg = normalize(&format!("LEX: {e}"));
                samples.entry(msg).or_insert_with(|| (path.display().to_string(), String::new()));
                continue;
            }
        };
        let program = match parse(tokens) {
            Ok(p) => p,
            Err(e) => {
                let msg = normalize(&format!("PARSE: {e}"));
                let snippet = parse_error_snippet(&raw, &e);
                samples.entry(msg).or_insert_with(|| (path.display().to_string(), snippet));
                continue;
            }
        };
        let result = Interpreter::new(MemorySource::new(Vec::new())).run(&program);
        if let Some(err) = result.terminal_error
            && !matches!(err, RuntimeError::Source(_))
        {
            let msg = normalize(&format!("RUN: {err}"));
            samples.entry(msg).or_insert_with(|| (path.display().to_string(), String::new()));
        }
    }
    for (msg, (path, snippet)) in &samples {
        println!("== {msg}");
        println!("  file: {path}");
        if !snippet.is_empty() {
            println!("  near:");
            for line in snippet.lines() {
                println!("    {line}");
            }
        }
        println!();
    }
}

fn normalize(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut chars = msg.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                chars.next();
            }
            out.push('N');
        } else {
            out.push(c);
        }
    }
    out
}

/// Extract the source slice around a `ParseError`'s span. Best
/// effort: `Display` for `ParseError` includes the span; we re-parse
/// the message rather than reach into the type, so changes to the
/// error format won't drag this along.
fn parse_error_snippet(src: &str, err: &hxy_010_lang::ParseError) -> String {
    let msg = format!("{err}");
    let span = msg.split("at ").nth(1).and_then(|rest| {
        let dots = rest.find("..")?;
        let lo: usize = rest[..dots].parse().ok()?;
        let rest = &rest[dots + 2..];
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let hi: usize = rest[..end].parse().ok()?;
        Some((lo, hi))
    });
    let Some((lo, _hi)) = span else { return String::new() };
    // 80 chars before, 80 after, on the same line where possible.
    let line_start = src[..lo].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = src[lo..].find('\n').map(|i| lo + i).unwrap_or(src.len());
    let snippet = &src[line_start..line_end];
    let cur = (lo - line_start).min(snippet.len());
    let mut caret = " ".repeat(cur);
    caret.push('^');
    format!("{snippet}\n{caret}")
}
