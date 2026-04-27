//! Benchmark hxy-010-lang vs hxy-imhex-lang on paired templates.
//!
//! For each fixture in `~/src/ImHex-Patterns/tests/patterns/test_data/`
//! (whose name embeds the format, e.g. `wav.hexpat.wav`), find the
//! matching `.hexpat` template and the matching `.bt` template (in
//! `~/Downloads/`, case-insensitive). Time each interpreter on the
//! same fixture and report the side-by-side numbers.
//!
//! Usage:
//!   cargo run -p hxy-010-lang --release --example bench_pair --
//!     <imhex-patterns-root> <bt-templates-dir>
//!
//! Each template is run REPS times; we report the best wall-clock to
//! avoid measurement noise from the OS.

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use std::time::Instant;

const REPS: u32 = 3;
const STEP_LIMIT_010: u64 = 50_000_000;
const STEP_LIMIT_IMHEX: u64 = 50_000_000;
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
struct Row {
    name: String,
    fixture_size: u64,
    bt_time: Option<Duration>,
    bt_status: String,
    hexpat_time: Option<Duration>,
    hexpat_status: String,
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let imhex_root = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var("HOME").unwrap()).join("src/ImHex-Patterns"));
    let bt_dir =
        args.next().map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env::var("HOME").unwrap()).join("Downloads"));
    let test_data_dir = imhex_root.join("tests/patterns/test_data");
    let patterns_dir = imhex_root.join("patterns");
    if !test_data_dir.is_dir() || !patterns_dir.is_dir() || !bt_dir.is_dir() {
        eprintln!(
            "expected layout: <imhex-root>/{{patterns,tests/patterns/test_data}} and <bt-dir>/*.bt\n\
             got imhex_root={} bt_dir={}",
            imhex_root.display(),
            bt_dir.display()
        );
        return ExitCode::from(2);
    }

    // Map lowercase basename (no .bt) -> path so we can look up the
    // matching .bt template by the fixture's format prefix.
    let bt_index: BTreeMap<String, PathBuf> = fs::read_dir(&bt_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(OsStr::to_str) == Some("bt"))
        .map(|p| (p.file_stem().unwrap().to_string_lossy().to_lowercase(), p))
        .collect();

    let mut rows = Vec::new();
    for entry in fs::read_dir(&test_data_dir).unwrap().flatten() {
        let fixture = entry.path();
        if !fixture.is_file() {
            continue;
        }
        let fname = fixture.file_name().unwrap().to_string_lossy().into_owned();
        // Strip `.hexpat.<ext>` to recover the format name. Skip
        // entries that don't have `.hexpat.` in the middle.
        let Some(idx) = fname.find(".hexpat.") else { continue };
        let name = &fname[..idx];
        let hexpat_path = patterns_dir.join(format!("{name}.hexpat"));
        let bt_path = bt_index.get(&name.to_lowercase()).cloned();
        if !hexpat_path.is_file() || bt_path.is_none() {
            continue;
        }
        let bt_path = bt_path.unwrap();
        let fixture_size = fs::metadata(&fixture).map(|m| m.len()).unwrap_or(0);
        eprintln!("benching {name}");
        let bytes = fs::read(&fixture).unwrap_or_default();
        let bt_src = fs::read_to_string(&bt_path).unwrap_or_default();
        let hexpat_src = fs::read_to_string(&hexpat_path).unwrap_or_default();

        let (bt_time, bt_status) = bench_bt(&bt_src, &bytes);
        let (hexpat_time, hexpat_status) = bench_imhex(&hexpat_src, &bytes, &imhex_root);

        rows.push(Row { name: name.to_owned(), fixture_size, bt_time, bt_status, hexpat_time, hexpat_status });
    }

    print_report(&rows);
    ExitCode::SUCCESS
}

fn bench_bt(src: &str, bytes: &[u8]) -> (Option<Duration>, String) {
    use hxy_010_lang::Interpreter;
    use hxy_010_lang::MemorySource;
    use hxy_010_lang::parse;
    use hxy_010_lang::tokenize;

    let tokens = match tokenize(src) {
        Ok(t) => t,
        Err(e) => return (None, format!("tokenize: {e}")),
    };
    let program = match parse(tokens) {
        Ok(p) => p,
        Err(e) => return (None, format!("parse: {e:?}")),
    };
    let mut best: Option<Duration> = None;
    let mut last_status = String::from("ok");
    for _ in 0..REPS {
        let source = MemorySource::new(bytes.to_vec());
        let interp = Interpreter::new(source).with_timeout(TIMEOUT);
        let start = Instant::now();
        let result = interp.run(&program);
        let elapsed = start.elapsed();
        last_status = match &result.terminal_error {
            None => {
                let warns =
                    result.diagnostics.iter().filter(|d| matches!(d.severity, hxy_010_lang::Severity::Error)).count();
                if warns > 0 { format!("err({warns})") } else { "ok".into() }
            }
            Some(e) => normalize(&format!("{e}")),
        };
        best = Some(best.map_or(elapsed, |b| b.min(elapsed)));
    }
    (best, last_status)
}

fn bench_imhex(src: &str, bytes: &[u8], imhex_root: &Path) -> (Option<Duration>, String) {
    use hxy_imhex_lang::Interpreter;
    use hxy_imhex_lang::MemorySource;
    use hxy_imhex_lang::chained_resolver;
    use hxy_imhex_lang::extract_pragmas;
    use hxy_imhex_lang::parse;
    use hxy_imhex_lang::tokenize;

    let tokens = match tokenize(src) {
        Ok(t) => t,
        Err(e) => return (None, format!("tokenize: {e:?}")),
    };
    let program = match parse(tokens) {
        Ok(p) => p,
        Err(e) => return (None, format!("parse: {e:?}")),
    };
    let resolver = chained_resolver([imhex_root.join("includes"), imhex_root.to_path_buf()]);
    let pragmas = extract_pragmas(src);
    let mut best: Option<Duration> = None;
    let mut last_status = String::from("ok");
    for _ in 0..REPS {
        let source = MemorySource::new(bytes.to_vec());
        let mut interp =
            Interpreter::new(source).with_import_resolver(resolver.clone()).with_step_limit(STEP_LIMIT_IMHEX);
        if let Some(e) = pragmas.endian {
            interp = interp.with_default_endian(e);
        }
        let start = Instant::now();
        let result = interp.run(&program);
        let elapsed = start.elapsed();
        last_status = match &result.terminal_error {
            None => "ok".into(),
            Some(e) => normalize(&format!("{e}")),
        };
        best = Some(best.map_or(elapsed, |b| b.min(elapsed)));
    }
    let _ = STEP_LIMIT_010;
    (best, last_status)
}

fn normalize(msg: &str) -> String {
    let trimmed: String = msg.chars().take(60).collect();
    trimmed.replace('\n', " ")
}

fn print_report(rows: &[Row]) {
    // Header
    println!();
    println!(
        "{:<14} {:>10} {:>14} {:>14} {:>10}  {:<28} {:<28}",
        "format", "size", "010-lang", "imhex-lang", "ratio", "010 status", "imhex status",
    );
    println!("{:-<132}", "");

    let mut bt_total = Duration::ZERO;
    let mut hexpat_total = Duration::ZERO;
    let mut both_ok = 0u32;
    for r in rows {
        let bt_ms = r.bt_time.map(|d| d.as_secs_f64() * 1000.0);
        let hp_ms = r.hexpat_time.map(|d| d.as_secs_f64() * 1000.0);
        let ratio = match (bt_ms, hp_ms) {
            (Some(b), Some(h)) if h > 0.0 => format!("{:.2}x", b / h),
            _ => "-".into(),
        };
        let bt_str = bt_ms.map(|m| format!("{m:.3} ms")).unwrap_or_else(|| "-".into());
        let hp_str = hp_ms.map(|m| format!("{m:.3} ms")).unwrap_or_else(|| "-".into());
        println!(
            "{:<14} {:>10} {:>14} {:>14} {:>10}  {:<28} {:<28}",
            r.name,
            human_bytes(r.fixture_size),
            bt_str,
            hp_str,
            ratio,
            truncate(&r.bt_status, 28),
            truncate(&r.hexpat_status, 28),
        );
        if r.bt_status == "ok" && r.hexpat_status == "ok" {
            if let (Some(b), Some(h)) = (r.bt_time, r.hexpat_time) {
                bt_total += b;
                hexpat_total += h;
                both_ok += 1;
            }
        }
    }
    println!("{:-<132}", "");
    println!(
        "rows: {} (both-ok: {})\n\
         010-lang   total (both-ok rows): {:.3} ms\n\
         imhex-lang total (both-ok rows): {:.3} ms\n\
         010/imhex ratio:                 {:.2}x",
        rows.len(),
        both_ok,
        bt_total.as_secs_f64() * 1000.0,
        hexpat_total.as_secs_f64() * 1000.0,
        if hexpat_total.as_nanos() > 0 { bt_total.as_secs_f64() / hexpat_total.as_secs_f64() } else { f64::NAN }
    );
    let _ = STEP_LIMIT_010;
}

fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        let cut: String = s.chars().take(n.saturating_sub(3)).collect();
        format!("{cut}...")
    }
}
