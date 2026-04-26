//! Bulk diagnostic: walk a directory of `.hexpat` / `.pat` ImHex
//! pattern files and report lex / parse outcomes per file. Mirrors
//! `hxy-010-lang`'s `probe_corpus` so the two crates' coverage can
//! be tracked side-by-side as they land.
//!
//! `cargo run --example probe_hexpat_corpus --release -- <dir>`
//!
//! The directory is *not* tracked by this repo. Point the probe at
//! `.imhex-patterns/` after running `scripts/fetch_imhex_patterns.sh`,
//! or at your own local checkout of the GPL-licensed corpus.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::RuntimeError;
use hxy_imhex_lang::SourceError;
use hxy_imhex_lang::chained_resolver;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

#[derive(Default)]
struct Totals {
    files: usize,
    lex_err: usize,
    parse_err: usize,
    parse_ok: usize,
    /// Subset of parse_ok: dispatched to the interpreter cleanly.
    /// `run_source` separates "parse + dispatch worked, only stopped
    /// because the empty input couldn't satisfy a read" from real
    /// runtime failures.
    run_ok: usize,
    run_source: usize,
    run_err: usize,
    /// Normalised lexer error messages, counted across the corpus.
    lex_messages: BTreeMap<String, usize>,
    /// Normalised parser error messages.
    parse_messages: BTreeMap<String, usize>,
    /// Normalised runtime error messages.
    run_messages: BTreeMap<String, usize>,
}

fn main() {
    let dir = env::args().nth(1).expect("usage: probe_hexpat_corpus <dir>");
    let dir = PathBuf::from(dir);
    let mut files: Vec<PathBuf> = match collect_pattern_files(&dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("walk({}): {}", dir.display(), e);
            std::process::exit(2);
        }
    };
    files.sort();
    // Wire an import resolver against the same corpus root so
    // `import std.io;` references inside the templates resolve.
    // Walks `<root>/includes/` first (where the upstream std/
    // library lives) then the corpus root for templates that
    // import siblings.
    let resolver = chained_resolver([dir.join("includes"), dir.clone()]);
    let mut totals = Totals::default();
    for path in &files {
        probe_one(path, &resolver, &mut totals);
    }
    print_summary(&totals);
}

/// Recursively collect `.hexpat` and `.pat` files under `dir`. Both
/// extensions are pattern-language source -- corpus templates use
/// `.hexpat`, the `std/` library and tests use `.pat`.
fn collect_pattern_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let read = match fs::read_dir(&d) {
            Ok(r) => r,
            Err(_) => continue, // unreadable subdir -- skip
        };
        for entry in read.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if matches!(p.extension().and_then(|s| s.to_str()), Some("hexpat") | Some("pat")) {
                out.push(p);
            }
        }
    }
    Ok(out)
}

fn probe_one(path: &Path, resolver: &hxy_imhex_lang::SharedResolver, totals: &mut Totals) {
    totals.files += 1;
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => {
            totals.lex_err += 1;
            *totals.lex_messages.entry("io error".into()).or_insert(0) += 1;
            return;
        }
    };
    let tokens = match tokenize(&raw) {
        Ok(t) => t,
        Err(e) => {
            totals.lex_err += 1;
            *totals.lex_messages.entry(normalise(&format!("{e}"))).or_insert(0) += 1;
            return;
        }
    };
    let program = match parse(tokens) {
        Ok(p) => {
            totals.parse_ok += 1;
            p
        }
        Err(e) => {
            totals.parse_err += 1;
            *totals.parse_messages.entry(normalise(&format!("{e}"))).or_insert(0) += 1;
            return;
        }
    };
    // Run against an empty buffer with a tight step limit. Most
    // corpus templates can't actually finish on no input -- the
    // first read trips `SourceError`, which we bucket separately so
    // it doesn't drown out real runtime failures (undefined names,
    // unknown types).
    let result = Interpreter::new(MemorySource::new(Vec::new()))
        .with_import_resolver(resolver.clone())
        .with_step_limit(200_000)
        .run(&program);
    match result.terminal_error {
        None => totals.run_ok += 1,
        Some(RuntimeError::Source(_)) => totals.run_source += 1,
        Some(other) => {
            totals.run_err += 1;
            *totals.run_messages.entry(normalise(&format!("{other}"))).or_insert(0) += 1;
        }
    }
    let _ = SourceError::Host(String::new()); // silence unused-import paranoia
}

/// Strip variable noise (offsets, ident contents) so structurally
/// identical errors bucket together.
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

fn print_summary(t: &Totals) {
    println!("== probe_hexpat_corpus summary ==");
    println!("files:      {}", t.files);
    println!("lex_err:    {}", t.lex_err);
    println!("parse_err:  {}", t.parse_err);
    println!("parse_ok:   {}", t.parse_ok);
    println!("  run_ok:     {}", t.run_ok);
    println!("  run_source: {} (read past EOF on empty buffer; benign)", t.run_source);
    println!("  run_err:    {}", t.run_err);
    println!();
    println!("-- top lex errors --");
    print_topk(&t.lex_messages, 15);
    println!("-- top parse errors --");
    print_topk(&t.parse_messages, 20);
    println!("-- top run errors --");
    print_topk(&t.run_messages, 15);
}

fn print_topk(map: &BTreeMap<String, usize>, k: usize) {
    let mut v: Vec<_> = map.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (msg, count) in v.into_iter().take(k) {
        println!("  {:>4}  {}", count, msg);
    }
    println!();
}
