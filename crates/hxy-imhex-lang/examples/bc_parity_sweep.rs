//! Run every paired (template, fixture) corpus pair through both
//! the AST interpreter and the bytecode VM and compare the
//! emitted node trees. Mirrors the outcomes from
//! `probe_hexpat_paired` so the metric is directly comparable.
//!
//! Outcomes per fixture:
//! - `match`: both runs produced identical `Vec<NodeOut>` and no
//!   terminal error.
//! - `ast_only`: AST ran ok but the BC run produced a terminal
//!   error or diverged.
//! - `bc_only`: BC ran ok but the AST run produced a terminal
//!   error or diverged.
//! - `both_failed`: both runs hit a terminal error -- not a
//!   parity issue per se.
//! - `diverged`: both runs succeeded but the node trees differ.
//!
//! Usage:
//!   cargo run --release --example bc_parity_sweep -p hxy-imhex-lang \
//!       -- ~/src/ImHex-Patterns

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

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::RunResult;
use hxy_imhex_lang::SharedResolver;
use hxy_imhex_lang::bc;
use hxy_imhex_lang::chained_resolver;
use hxy_imhex_lang::extract_pragmas;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

#[derive(Default)]
struct Totals {
    templates: usize,
    no_fixture: usize,
    parse_err: usize,
    bc_compile_err: usize,
    fixtures_run: usize,
    matched: usize,
    diverged: usize,
    ast_only: usize,
    bc_only: usize,
    both_failed: usize,
    bc_timeout: usize,
    ast_timeout: usize,
    diverge_examples: Vec<(String, String)>,
    bc_only_examples: Vec<(String, String)>,
}

const TEMPLATE_DEADLINE: Duration = Duration::from_secs(5);
const MAX_FIXTURE_BYTES: usize = 16 * 1024 * 1024;

fn main() -> ExitCode {
    let Some(root) = env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: bc_parity_sweep <ImHex-Patterns root>");
        return ExitCode::from(2);
    };
    let patterns_dir = root.join("patterns");
    let test_data_dir = root.join("tests/patterns/test_data");
    if !patterns_dir.is_dir() || !test_data_dir.is_dir() {
        eprintln!("expected layout: <root>/patterns/*.hexpat and <root>/tests/patterns/test_data/...");
        return ExitCode::from(2);
    }
    let resolver = chained_resolver([root.join("includes"), root.clone()]);
    let mut templates = collect_templates(&patterns_dir);
    templates.sort();
    let mut totals = Totals::default();
    for t in &templates {
        probe(t, &test_data_dir, &resolver, &mut totals);
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

fn probe(template: &Path, test_data: &Path, resolver: &SharedResolver, t: &mut Totals) {
    t.templates += 1;
    let template_name = template.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let Ok(src) = fs::read_to_string(template) else {
        return;
    };
    let Ok(tokens) = tokenize(&src) else {
        t.parse_err += 1;
        return;
    };
    let Ok(ast) = parse(tokens) else {
        t.parse_err += 1;
        return;
    };
    let bc_program = match bc::compile_with_resolver(&ast, resolver.as_ref()) {
        Ok(p) => Arc::new(p),
        Err(_) => {
            t.bc_compile_err += 1;
            return;
        }
    };
    let fixtures = collect_fixtures(test_data, &template_name);
    if fixtures.is_empty() {
        t.no_fixture += 1;
        return;
    }
    let Some(fixture) = fixtures.first() else { return };
    let Ok(bytes) = fs::read(fixture) else { return };
    let bytes = if bytes.len() > MAX_FIXTURE_BYTES { bytes[..MAX_FIXTURE_BYTES].to_vec() } else { bytes };
    t.fixtures_run += 1;
    eprintln!(
        "  >> {template_name} ({} bytes from {})",
        bytes.len(),
        fixture.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    );
    let pragmas = extract_pragmas(&src);
    let ast_arc = Arc::new(ast);
    let ast_result = run_ast(&bytes, ast_arc.clone(), resolver.clone(), pragmas.endian);
    let bc_result = run_bc(&bytes, bc_program.clone(), ast_arc.clone(), resolver.clone(), pragmas.endian);
    let label = template_name.clone();
    match (ast_result, bc_result) {
        (Outcome::Ok(a), Outcome::Ok(b)) => {
            if nodes_equal(&a.nodes, &b.nodes) {
                t.matched += 1;
            } else {
                t.diverged += 1;
                if t.diverge_examples.len() < 25 {
                    let detail = first_divergence(&a, &b);
                    t.diverge_examples.push((label, detail));
                }
            }
        }
        (Outcome::Ok(_), Outcome::Err(msg)) => {
            t.ast_only += 1;
            if t.diverge_examples.len() < 25 {
                t.diverge_examples.push((label, format!("BC erred: {msg}")));
            }
        }
        (Outcome::Err(_), Outcome::Ok(_)) => {
            t.bc_only += 1;
            if t.bc_only_examples.len() < 25 {
                t.bc_only_examples.push((label, "BC ok but AST erred".into()));
            }
        }
        (Outcome::Err(_), Outcome::Err(_)) => t.both_failed += 1,
        (Outcome::Timeout, _) => t.ast_timeout += 1,
        (_, Outcome::Timeout) => t.bc_timeout += 1,
    }
}

enum Outcome {
    Ok(RunResult),
    Err(String),
    Timeout,
}

fn run_ast(
    bytes: &[u8],
    ast: Arc<hxy_imhex_lang::ast::Program>,
    resolver: SharedResolver,
    endian: Option<hxy_imhex_lang::Endian>,
) -> Outcome {
    let interrupt = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let bytes = bytes.to_vec();
    let interrupt_clone = interrupt.clone();
    let _ = thread::spawn(move || {
        let mut interp = Interpreter::new(MemorySource::new(bytes))
            .with_import_resolver(resolver)
            .with_step_limit(5_000_000)
            .with_interrupt(interrupt_clone);
        if let Some(e) = endian {
            interp = interp.with_default_endian(e);
        }
        let _ = tx.send(interp.run(&ast));
    });
    match rx.recv_timeout(TEMPLATE_DEADLINE) {
        Ok(r) => match r.terminal_error.clone() {
            None => Outcome::Ok(r),
            Some(e) => Outcome::Err(format!("{e}")),
        },
        Err(_) => {
            interrupt.store(true, Ordering::Relaxed);
            Outcome::Timeout
        }
    }
}

fn run_bc(
    bytes: &[u8],
    bc_program: Arc<bc::Program>,
    ast: Arc<hxy_imhex_lang::ast::Program>,
    resolver: SharedResolver,
    endian: Option<hxy_imhex_lang::Endian>,
) -> Outcome {
    let _ = ast; // future: pass to interp if VM grows AST hooks
    let interrupt = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let bytes = bytes.to_vec();
    let interrupt_clone = interrupt.clone();
    let _ = thread::spawn(move || {
        let mut interp = Interpreter::new(MemorySource::new(bytes))
            .with_import_resolver(resolver)
            .with_step_limit(5_000_000)
            .with_interrupt(interrupt_clone);
        if let Some(e) = endian {
            interp = interp.with_default_endian(e);
        }
        let _ = tx.send(interp.run_bytecode_experimental(&bc_program));
    });
    match rx.recv_timeout(TEMPLATE_DEADLINE) {
        Ok(r) => match r.terminal_error.clone() {
            None => Outcome::Ok(r),
            Some(e) => Outcome::Err(format!("{e}")),
        },
        Err(_) => {
            interrupt.store(true, Ordering::Relaxed);
            Outcome::Timeout
        }
    }
}

/// Compare node lists treating NaN floats as equal to other NaNs
/// of the same kind (otherwise IEEE PartialEq makes a NaN value
/// compare unequal to itself, causing spurious divergences).
fn nodes_equal(a: &[hxy_imhex_lang::NodeOut], b: &[hxy_imhex_lang::NodeOut]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(node_eq)
}

fn node_eq(p: (&hxy_imhex_lang::NodeOut, &hxy_imhex_lang::NodeOut)) -> bool {
    let (a, b) = p;
    a.name == b.name
        && a.ty == b.ty
        && a.offset == b.offset
        && a.length == b.length
        && a.parent == b.parent
        && a.attrs == b.attrs
        && value_eq(&a.value, &b.value)
}

fn value_eq(a: &Option<hxy_imhex_lang::Value>, b: &Option<hxy_imhex_lang::Value>) -> bool {
    use hxy_imhex_lang::Value;
    match (a, b) {
        (None, None) => true,
        (Some(av), Some(bv)) => match (av, bv) {
            (Value::Float { value: av, kind: ak }, Value::Float { value: bv, kind: bk }) => {
                ak == bk && (av.to_bits() == bv.to_bits() || (av.is_nan() && bv.is_nan()))
            }
            _ => av == bv,
        },
        _ => false,
    }
}

fn first_divergence(a: &RunResult, b: &RunResult) -> String {
    let alen = a.nodes.len();
    let blen = b.nodes.len();
    let common = alen.min(blen);
    for i in 0..common {
        if !node_eq((&a.nodes[i], &b.nodes[i])) {
            let an = &a.nodes[i];
            let bn = &b.nodes[i];
            return format!(
                "node[{i}]: AST name={} ty={:?} off={} len={} val={:?} attrs={:?} | BC name={} ty={:?} off={} len={} val={:?} attrs={:?}",
                an.name,
                an.ty,
                an.offset,
                an.length,
                an.value,
                an.attrs,
                bn.name,
                bn.ty,
                bn.offset,
                bn.length,
                bn.value,
                bn.attrs,
            );
        }
    }
    format!("AST nodes={alen} BC nodes={blen} (no element-wise diff in shared prefix)",)
}

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
    let prefix = format!("{template_name}.");
    let mut fixtures: Vec<PathBuf> = fs::read_dir(test_data)
        .map(|r| {
            r.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .filter(|p| p.file_name().and_then(|s| s.to_str()).is_some_and(|n| n.starts_with(&prefix)))
                .collect()
        })
        .unwrap_or_default();
    fixtures.sort();
    fixtures
}

fn print_summary(t: &Totals) {
    println!("== bc_parity_sweep summary ==");
    println!("templates:      {}", t.templates);
    println!("  parse_err:    {}", t.parse_err);
    println!("  bc_compile_err: {}", t.bc_compile_err);
    println!("  no_fixture:   {}", t.no_fixture);
    println!("fixtures_run:   {}", t.fixtures_run);
    println!("  matched:      {}", t.matched);
    println!("  diverged:     {}", t.diverged);
    println!("  ast_only:     {}  (AST passed, BC failed)", t.ast_only);
    println!("  bc_only:      {}  (BC passed, AST failed)", t.bc_only);
    println!("  both_failed:  {}", t.both_failed);
    println!("  ast_timeout:  {}", t.ast_timeout);
    println!("  bc_timeout:   {}", t.bc_timeout);
    let pct = if t.fixtures_run > 0 { (t.matched as f64) * 100.0 / (t.fixtures_run as f64) } else { 0.0 };
    println!("  parity rate:  {pct:.1}%");
    println!();
    println!("-- first divergences (BC erred or differed) --");
    let _ = BTreeMap::<String, ()>::new();
    for (label, msg) in t.diverge_examples.iter().take(25) {
        println!("  {label}");
        println!("      {msg}");
    }
    if !t.bc_only_examples.is_empty() {
        println!();
        println!("-- bc_only (BC found something AST didn't) --");
        for (label, msg) in t.bc_only_examples.iter().take(10) {
            println!("  {label}: {msg}");
        }
    }
}
