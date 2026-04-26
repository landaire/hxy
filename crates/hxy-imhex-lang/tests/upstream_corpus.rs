//! End-to-end run against the upstream `~/src/ImHex-Patterns` (or
//! `.imhex-patterns/`) corpus. Pairs every `<name>.hexpat` template
//! with the matching `<name>.hexpat.<ext>` fixture under
//! `tests/patterns/test_data/` and runs the interpreter against the
//! real bytes.
//!
//! `#[ignore]`d so it doesn't block default `cargo test`. The
//! upstream repo is GPL'd and not vendored; opt in explicitly with
//!
//!     cargo test -p hxy-imhex-lang --test upstream_corpus -- --ignored --nocapture
//!
//! Set `HXY_IMHEX_CORPUS=<path>` to override the search path.
//! When the path doesn't exist, the test logs a message and passes
//! so it stays inert in CI environments without the corpus.

use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::chained_resolver;
use hxy_imhex_lang::extract_pragmas;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

const TEMPLATE_DEADLINE: Duration = Duration::from_secs(5);
const MAX_FIXTURE_BYTES: usize = 4 * 1024 * 1024;
const STEP_LIMIT: u64 = 500_000;

/// Minimum number of fixtures we expect to pass end-to-end. Bump
/// this as bugs get fixed -- it acts as a regression backstop. The
/// raw count matters more than the percentage since the upstream
/// fixture set grows over time.
const MIN_RUN_OK: usize = 60;

#[test]
#[ignore = "needs the upstream ~/src/ImHex-Patterns repo; opt in with --ignored"]
fn paired_fixture_run_passes_baseline() {
    let Some(root) = locate_corpus() else {
        println!("skipping: no upstream corpus found (set HXY_IMHEX_CORPUS or clone to ~/src/ImHex-Patterns)");
        return;
    };
    println!("using corpus at: {}", root.display());

    let patterns_dir = root.join("patterns");
    let test_data_dir = root.join("tests/patterns/test_data");
    assert!(patterns_dir.is_dir(), "patterns/ missing under {}", root.display());
    assert!(test_data_dir.is_dir(), "tests/patterns/test_data/ missing under {}", root.display());

    let resolver = chained_resolver([root.join("includes"), root.clone()]);
    let mut templates: Vec<PathBuf> = fs::read_dir(&patterns_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("hexpat"))
        .collect();
    templates.sort();

    let mut run_ok = 0usize;
    let mut run_err = 0usize;
    let mut no_fixture = 0usize;
    let mut parse_err = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for template in &templates {
        let name = template.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let Ok(src) = fs::read_to_string(template) else { continue };
        let Ok(tokens) = tokenize(&src) else {
            parse_err += 1;
            failures.push(format!("LEX  {name}"));
            continue;
        };
        let Ok(program) = parse(tokens) else {
            parse_err += 1;
            failures.push(format!("PARSE  {name}"));
            continue;
        };
        let Some(fixture) = pick_fixture(&test_data_dir, &name) else {
            no_fixture += 1;
            continue;
        };
        let Ok(bytes) = fs::read(&fixture) else { continue };
        let bytes = if bytes.len() > MAX_FIXTURE_BYTES { bytes[..MAX_FIXTURE_BYTES].to_vec() } else { bytes };
        let pragmas = extract_pragmas(&src);
        let outcome = run_with_deadline(program, bytes, resolver.clone(), pragmas.endian);
        match outcome {
            RunOutcome::Ok => run_ok += 1,
            RunOutcome::Err(msg) => {
                run_err += 1;
                if failures.len() < 30 {
                    failures.push(format!("RUN  {name}: {msg}"));
                }
            }
            RunOutcome::Timeout => {
                run_err += 1;
                if failures.len() < 30 {
                    failures.push(format!("TIMEOUT  {name}"));
                }
            }
        }
    }

    println!("\nupstream corpus end-to-end:");
    println!("  templates:    {}", templates.len());
    println!("  parse_err:    {parse_err}");
    println!("  no_fixture:   {no_fixture}");
    println!("  run_ok:       {run_ok}");
    println!("  run_err:      {run_err}");
    if !failures.is_empty() {
        println!("\nfirst failures:");
        for f in failures.iter().take(20) {
            println!("  {f}");
        }
    }
    assert!(
        run_ok >= MIN_RUN_OK,
        "regression: only {run_ok} fixtures passed end-to-end; baseline is {MIN_RUN_OK}",
    );
}

fn locate_corpus() -> Option<PathBuf> {
    if let Ok(env_path) = env::var("HXY_IMHEX_CORPUS") {
        let p = PathBuf::from(env_path);
        if p.is_dir() {
            return Some(p);
        }
    }
    let home = env::var("HOME").ok().map(PathBuf::from)?;
    let candidates = [home.join("src/ImHex-Patterns"), PathBuf::from(".imhex-patterns")];
    candidates.into_iter().find(|p| p.is_dir())
}

fn pick_fixture(test_data: &Path, template_name: &str) -> Option<PathBuf> {
    // First try `<test_data>/<template>/*` (directory of variants).
    let dir_form = test_data.join(template_name);
    if dir_form.is_dir()
        && let Ok(read) = fs::read_dir(&dir_form)
    {
        let mut entries: Vec<PathBuf> = read.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
        entries.sort();
        if let Some(first) = entries.first() {
            return Some(first.clone());
        }
    }
    // Fall back to `<test_data>/<template>.<ext>`.
    let prefix = format!("{template_name}.");
    let mut entries: Vec<PathBuf> = fs::read_dir(test_data)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.file_name().and_then(|s| s.to_str()).is_some_and(|n| n.starts_with(&prefix)))
        .collect();
    entries.sort();
    entries.into_iter().next()
}

enum RunOutcome {
    Ok,
    Err(String),
    Timeout,
}

fn run_with_deadline(
    program: hxy_imhex_lang::ast::Program,
    bytes: Vec<u8>,
    resolver: hxy_imhex_lang::SharedResolver,
    endian_pragma: Option<&'static str>,
) -> RunOutcome {
    let interrupt = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let interrupt_for_thread = interrupt.clone();
    // Detach the worker. Joining could block past the deadline if a
    // single statement (e.g. an O(N) member lookup in a deep tree)
    // hasn't reached the next interrupt poll yet. The interrupt
    // still trips the thread on its next step boundary, so it
    // self-terminates without blocking the test.
    let _ = thread::spawn(move || {
        let result = Interpreter::new(MemorySource::new(bytes))
            .with_import_resolver(resolver)
            .with_default_endian(endian_pragma)
            .with_step_limit(STEP_LIMIT)
            .with_interrupt(interrupt_for_thread)
            .run(&program);
        let _ = tx.send(result);
    });
    match rx.recv_timeout(TEMPLATE_DEADLINE) {
        Ok(r) => match r.terminal_error {
            None => RunOutcome::Ok,
            Some(e) => RunOutcome::Err(format!("{e}")),
        },
        Err(_) => {
            interrupt.store(true, Ordering::Relaxed);
            RunOutcome::Timeout
        }
    }
}
