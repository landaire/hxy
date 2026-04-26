//! Companion to `probe_hexpat_corpus`: print the *first* failing
//! file (and the offending source snippet) for each distinct lex /
//! parse / run error message. Mirrors the 010 lang's
//! `probe_failures`.
//!
//! `cargo run --example probe_hexpat_failures --release -- <dir>`

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::ParseError;
use hxy_imhex_lang::RuntimeError;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

fn main() {
    let dir = env::args().nth(1).expect("usage: probe_hexpat_failures <dir>");
    let dir = PathBuf::from(dir);
    let mut samples: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut files = collect(&dir);
    files.sort();
    for path in &files {
        let raw = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let tokens = match tokenize(&raw) {
            Ok(t) => t,
            Err(e) => {
                let msg = normalise(&format!("LEX: {e}"));
                samples.entry(msg).or_insert_with(|| (path.display().to_string(), String::new()));
                continue;
            }
        };
        let program = match parse(tokens) {
            Ok(p) => p,
            Err(e) => {
                let msg = normalise(&format!("PARSE: {e}"));
                let snippet = parse_error_snippet(&raw, &e);
                samples.entry(msg).or_insert_with(|| (path.display().to_string(), snippet));
                continue;
            }
        };
        let result = Interpreter::new(MemorySource::new(Vec::new())).with_step_limit(200_000).run(&program);
        if let Some(err) = result.terminal_error
            && !matches!(err, RuntimeError::Source(_))
        {
            let msg = normalise(&format!("RUN: {err}"));
            samples.entry(msg).or_insert_with(|| (path.display().to_string(), String::new()));
        }
    }
    for (msg, (path, snippet)) in &samples {
        println!("== {msg}");
        println!("  file: {path}");
        if !snippet.is_empty() {
            for line in snippet.lines() {
                println!("    {line}");
            }
        }
        println!();
    }
}

fn collect(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(read) = fs::read_dir(&d) else { continue };
        for e in read.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if matches!(p.extension().and_then(|s| s.to_str()), Some("hexpat") | Some("pat")) {
                out.push(p);
            }
        }
    }
    out
}

fn normalise(msg: &str) -> String {
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

fn parse_error_snippet(src: &str, err: &ParseError) -> String {
    let msg = format!("{err}");
    let span = msg.split("at ").nth(1).and_then(|rest| {
        let dots = rest.find("..")?;
        let lo: usize = rest[..dots].parse().ok()?;
        let rest = &rest[dots + 2..];
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let hi: usize = rest[..end].parse().ok()?;
        Some((lo, hi))
    });
    let Some((lo, _)) = span else { return String::new() };
    let line_start = src[..lo].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_end = src[lo..].find('\n').map(|i| lo + i).unwrap_or(src.len());
    let snippet = &src[line_start..line_end];
    let cur = (lo - line_start).min(snippet.len());
    let mut caret = " ".repeat(cur);
    caret.push('^');
    format!("{snippet}\n{caret}")
}
