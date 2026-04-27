//! End-to-end probe: pair every `.hexpat` template with the upstream
//! repo's `tests/patterns/test_data/<name>.hexpat.<ext>` (or
//! directory of fixtures) and run the interpreter against the real
//! bytes. Mirrors the upstream `tests/patterns/CMakeLists.txt`
//! pairing rules so we measure against the same fixtures the
//! upstream CI runs.
//!
//! `cargo run --example probe_hexpat_paired --release -- <repo>`
//!
//! Outcomes per template:
//! - `parse_err`: the template itself wouldn't parse.
//! - `no_fixture`: no paired binary exists -- skipped.
//! - `run_ok`: interpreter ran the template against the fixture
//!   without a terminal error.
//! - `run_err`: interpreter halted; the bucketed message goes into
//!   the top-N table.
//!
//! `run_ok` is the metric we actually care about; the empty-buffer
//! probe (`probe_hexpat_corpus`) reports "parse_ok" for templates
//! that wouldn't ever read real data.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::chained_resolver;
use hxy_imhex_lang::extract_pragmas;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

#[derive(Default)]
struct Totals {
    templates: usize,
    parse_err: usize,
    no_fixture: usize,
    fixtures_run: usize,
    run_ok: usize,
    run_err: usize,
    run_messages: BTreeMap<String, usize>,
    /// Per-template result for the report tail.
    failures: Vec<(String, String)>,
}

fn main() -> ExitCode {
    let Some(root) = env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: probe_hexpat_paired <ImHex-Patterns root>");
        return ExitCode::from(2);
    };
    let patterns_dir = root.join("patterns");
    let test_data_dir = root.join("tests/patterns/test_data");
    if !patterns_dir.is_dir() || !test_data_dir.is_dir() {
        eprintln!(
            "expected layout: <root>/patterns/*.hexpat and <root>/tests/patterns/test_data/...\n\
             got root={}",
            root.display()
        );
        return ExitCode::from(2);
    }
    let resolver = chained_resolver([root.join("includes"), root.clone()]);
    let mut templates = collect_templates(&patterns_dir);
    templates.sort();
    let mut totals = Totals::default();
    for tmpl in &templates {
        probe_template(tmpl, &test_data_dir, &resolver, &mut totals);
    }
    print_summary(&totals);
    ExitCode::SUCCESS
}

fn collect_templates(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|read| {
            read.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("hexpat"))
                .collect()
        })
        .unwrap_or_default()
}

fn probe_template(
    template: &Path,
    test_data_dir: &Path,
    resolver: &hxy_imhex_lang::SharedResolver,
    totals: &mut Totals,
) {
    totals.templates += 1;
    let template_name = template.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let src = match fs::read_to_string(template) {
        Ok(s) => s,
        Err(_) => return,
    };
    let tokens = match tokenize(&src) {
        Ok(t) => t,
        Err(_) => {
            totals.parse_err += 1;
            return;
        }
    };
    let program = match parse(tokens) {
        Ok(p) => p,
        Err(_) => {
            totals.parse_err += 1;
            return;
        }
    };
    let fixtures = collect_fixtures(test_data_dir, &template_name);
    if fixtures.is_empty() {
        totals.no_fixture += 1;
        return;
    }
    // One fixture per template -- the survey wants coverage breadth,
    // not depth. Directory-style fixtures pick the alphabetically-
    // first entry deterministically.
    let Some(fixture) = fixtures.first() else { return };
    totals.fixtures_run += 1;
    let bytes = match fs::read(fixture) {
        Ok(b) => b,
        Err(_) => return,
    };
    // Cap input size as a guardrail. The upstream test_data
    // directory tops out around a few MB; 16 MiB covers every
    // current fixture (lcesave's largest is ~4.2 MB) while still
    // preventing a future giant blob from exploding memory.
    const MAX_FIXTURE_BYTES: usize = 16 * 1024 * 1024;
    let bytes = if bytes.len() > MAX_FIXTURE_BYTES {
        bytes[..MAX_FIXTURE_BYTES].to_vec()
    } else {
        bytes
    };
    eprintln!("  >> {template_name}");

    // Run each template on its own thread with a wall-clock deadline.
    // The interpreter polls `interrupt` and bails out with `TimedOut`
    // when the flag flips, so the worker thread eventually exits even
    // if we've already abandoned the channel.
    const TEMPLATE_DEADLINE: Duration = Duration::from_secs(5);
    let interrupt = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let resolver_for_thread = resolver.clone();
    let program = Arc::new(program);
    let prog_for_thread = program.clone();
    let interrupt_for_thread = interrupt.clone();
    let pragmas = extract_pragmas(&src);
    // Detach the worker rather than joining: in pathological cases
    // a single statement can dominate runtime (deep recursion in a
    // big node tree), and joining would block the probe waiting
    // for an interpreter that's already past the deadline. The
    // interrupt flag still trips the worker on its next exec_stmt,
    // so it self-terminates -- we just don't wait for it.
    let _ = thread::spawn(move || {
        let mut interp = Interpreter::new(MemorySource::new(bytes))
            .with_import_resolver(resolver_for_thread)
            .with_step_limit(2_000_000)
            .with_interrupt(interrupt_for_thread);
        if let Some(e) = pragmas.endian {
            interp = interp.with_default_endian(e);
        }
        let _ = tx.send(interp.run(&prog_for_thread));
    });
    let start = Instant::now();
    let recv = rx.recv_timeout(TEMPLATE_DEADLINE);
    if recv.is_err() {
        interrupt.store(true, Ordering::Relaxed);
        eprintln!("     (timeout after {} ms; interrupt sent)", start.elapsed().as_millis());
    }
    let result = recv.ok();
    match result {
        Some(r) => match r.terminal_error {
            None => totals.run_ok += 1,
            Some(err) => {
                totals.run_err += 1;
                let msg = normalise(&format!("{err}"));
                *totals.run_messages.entry(msg.clone()).or_insert(0) += 1;
                if totals.failures.len() < 100 {
                    let label = format!(
                        "{} -> {}",
                        template_name,
                        fixture.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
                    );
                    totals.failures.push((label, msg));
                }
            }
        },
        None => {
            totals.run_err += 1;
            *totals.run_messages.entry("timeout (5s wall-clock)".into()).or_insert(0) += 1;
            if totals.failures.len() < 25 {
                let label = format!(
                    "{} -> {}",
                    template_name,
                    fixture.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
                );
                totals.failures.push((label, "timeout".into()));
            }
        }
    }
}

/// Match the upstream CMakeLists.txt rules:
///   - first try `<test_data>/<template>/*` (directory of fixtures)
///   - else try `<test_data>/<template>.*` (single file with any
///     trailing extension)
fn collect_fixtures(test_data: &Path, template_name: &str) -> Vec<PathBuf> {
    let dir_form = test_data.join(template_name);
    if dir_form.is_dir() {
        let mut fixtures: Vec<PathBuf> = fs::read_dir(&dir_form)
            .map(|r| r.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect())
            .unwrap_or_default();
        fixtures.sort();
        if !fixtures.is_empty() {
            return fixtures;
        }
    }
    // `bmp.hexpat.bmp`, `7z.hexpat.7z`, ...
    let prefix = format!("{template_name}.");
    let mut fixtures: Vec<PathBuf> = fs::read_dir(test_data)
        .map(|r| {
            r.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .filter(|p| {
                    p.file_name()
                        .and_then(|s| s.to_str())
                        .is_some_and(|n| n.starts_with(&prefix))
                })
                .collect()
        })
        .unwrap_or_default();
    fixtures.sort();
    fixtures
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

fn print_summary(t: &Totals) {
    println!("== probe_hexpat_paired summary ==");
    println!("templates:      {}", t.templates);
    println!("  parse_err:    {}", t.parse_err);
    println!("  no_fixture:   {}", t.no_fixture);
    println!("fixtures_run:   {}", t.fixtures_run);
    println!("  run_ok:       {}", t.run_ok);
    println!("  run_err:      {}", t.run_err);
    let pct = if t.fixtures_run > 0 {
        (t.run_ok as f64) * 100.0 / (t.fixtures_run as f64)
    } else {
        0.0
    };
    println!("  pass rate:    {pct:.1}%");
    println!();
    println!("-- top run errors --");
    let mut v: Vec<_> = t.run_messages.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (msg, count) in v.into_iter().take(15) {
        println!("  {count:>4}  {msg}");
    }
    println!();
    println!("-- first failing fixtures --");
    for (label, msg) in t.failures.iter().take(100) {
        println!("  {label}");
        println!("      {msg}");
    }
}
