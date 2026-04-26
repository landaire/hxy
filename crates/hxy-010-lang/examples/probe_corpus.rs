//! Bulk diagnostic: walk a directory of `.bt` templates and report
//! lex / parse / run outcomes per file. Used to assess coverage of
//! the 010 dialect against a real-world corpus.
//!
//! `cargo run --example probe_corpus --release -- <dir>`
//!
//! Per file we report:
//! - `lex_err`: lexer rejected it
//! - `parse_err`: parser rejected it
//! - `run_err`: interpreter aborted with a `RuntimeError`
//! - `ok`: ran to completion (terminal_error == None) against an
//!   empty buffer. Most templates can't actually finish on an empty
//!   buffer (they'll early-out via `Exit` / `Warning`), but if the
//!   first `read` triggers an out-of-bounds we still treat that as
//!   a correctness signal: the template parsed, the interpreter
//!   dispatched to the right code paths, and the only failure was
//!   "no bytes to consume".
//!
//! The probe also collects every identifier called as a function in
//! a parse-tree walk and flags the ones that are *not* in our
//! builtin dispatch table (`UndefinedName` would surface them at
//! run time too, but a static call sweep is faster and reaches
//! arms the run never visits).

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use hxy_010_lang::Interpreter;
use hxy_010_lang::MemorySource;
use hxy_010_lang::RuntimeError;
use hxy_010_lang::ast::Expr;
use hxy_010_lang::ast::FunctionDef;
use hxy_010_lang::ast::Program;
use hxy_010_lang::ast::Stmt;
use hxy_010_lang::ast::TopItem;
use hxy_010_lang::parse;
use hxy_010_lang::tokenize;

/// Names dispatched by `Interpreter::call_builtin` (in `interp.rs`).
/// Update when builtins change. The probe flags any identifier that
/// looks like a function call (`Foo(...)`) and isn't in this set --
/// such calls would either resolve to a user-defined function in the
/// same template, or surface `UndefinedName` at run time.
const KNOWN_BUILTINS: &[&str] = &[
    // Endianness pragmas.
    "LittleEndian",
    "BigEndian",
    // Cursor / file helpers.
    "FTell",
    "FSeek",
    "FSkip",
    "FEof",
    "FileSize",
    // Typed readers.
    "ReadUInt",
    "ReadInt",
    "ReadUShort",
    "ReadShort",
    "ReadUQuad",
    "ReadUInt64",
    "ReadQuad",
    "ReadInt64",
    "ReadByte",
    "ReadUByte",
    "ReadFloat",
    "ReadDouble",
    "ReadBytes",
    "ReadString",
    "ReadStringLength",
    "ReadWString",
    "ReadWStringLength",
    // Diagnostics / control.
    "Warning",
    "Waring",
    "Printf",
    "printf",
    "Exit",
    "Assert",
    "exists",
    "RequiresVersion",
    "FindFirst",
    // Special-form / type-introspection.
    "sizeof",
    "startof",
    // Layout / display pragmas (no-ops in headless).
    "BitfieldRightToLeft",
    "BitfieldLeftToRight",
    "BitfieldDisablePadding",
    "BitfieldEnablePadding",
    "DisplayFormatHex",
    "DisplayFormatDecimal",
    "DisplayFormatBinary",
    "SetForeColor",
    "SetBackColor",
    // String / number conversion + manipulation.
    "Strlen",
    "WStrlen",
    "Strcmp",
    "Stricmp",
    "Strncmp",
    "Strnicmp",
    "WStrcmp",
    "WStricmp",
    "WStrncmp",
    "WStrnicmp",
    "Memcmp",
    "Strncpy",
    "Strcpy",
    "WStrcpy",
    "WStrncpy",
    "Memcpy",
    "SubStr",
    "StrDel",
    "Atoi",
    "Atof",
    "BinaryStrToInt",
    "IntToBinaryStr",
    "Str",
    "str",
    "WStringToString",
    "StringToWString",
    "ToUpper",
    "ToLower",
    "IsCharAlpha",
    "IsCharAlphaNumeric",
    "IsCharNum",
    "IsCharDigit",
    "IsCharWhitespace",
    "SPrintf",
    "EnumToString",
    "SScanf",
    "Checksum",
    "ChecksumAlgArrayStr",
    "ChecksumAlgStr",
    // Math.
    "Min",
    "Max",
    "Abs",
    "Pow",
    "Sqrt",
    "Ceil",
    "Floor",
    "Round",
    // UI state / I/O (headless no-ops).
    "GetCursorPos",
    "GetSelStart",
    "GetSelSize",
    "GetFileName",
    "GetFileNameW",
    "GetFilePath",
    "OutputPaneClear",
    "ClearOutput",
    "StatusMessage",
    "FPrintf",
    "RunTemplate",
    "InputRadioButtonBox",
    "InputDirectory",
    "InputDouble",
    "InputFloat",
    "InputNumber",
    "InputOpenFileName",
    "InputSaveFileName",
    "InputString",
    "MessageBox",
    // Time / GUID stringification.
    "TimeTToString",
    "Time64TToString",
    "FileTimeToString",
    "OleTimeToString",
    "DOSTimeToString",
    "DOSDateToString",
    "GUIDToString",
    // Endian-state queries.
    "IsLittleEndian",
    "IsBigEndian",
    // Buffer mutation (no-ops in headless).
    "InsertBytes",
    "DeleteBytes",
    "OverwriteBytes",
    "WriteString",
    "Memset",
    // Bookmark / file-table introspection.
    "AddBookmark",
    "GetBookmarkArraySize",
    "GetBookmarkName",
    "GetBookmarkPos",
    "GetBookmarkType",
    "GetNumBookmarks",
    "GetArg",
    "GetNumArgs",
    "GetFileNum",
    "FileExists",
    "FindOpenFile",
    "FileSelect",
    "FileOpen",
    "FileClose",
    "FileNameGetBase",
    "FileNameSetExtension",
    // Style / color queries.
    "GetBackColor",
    "GetForeColor",
    "SetColor",
    "SetStyle",
    "DisasmSetMode",
    "ThemeAutoScaleColors",
    "OutputPaneSave",
    // String search / split / concat.
    "Strstr",
    "WStrstr",
    "Strchr",
    "WStrchr",
    "Strcat",
    "WSubStr",
    "FindAll",
    "function_exists",
    "exists_function",
    // Wide / line text helpers.
    "TextGetNumLines",
    "TextGetLineSize",
    "TextAddressToLine",
    "TextLineToAddress",
    "ReadLine",
    "ToColonHexString",
    "RegExMatch",
    "SwapBytes",
    "SScan",
];

#[derive(Default)]
struct Totals {
    files: usize,
    lex_err: usize,
    parse_err: usize,
    run_err: usize,
    run_source: usize, // SourceError -- read past EOF on empty buffer
    ok: usize,
    /// Number of distinct unknown-call names per file (capped to
    /// avoid drowning the report when one template defines many of
    /// its own helpers).
    unknown_calls: BTreeMap<String, usize>,
    /// Count of lex / parse / runtime error messages, normalized so
    /// we can spot the most common failure modes.
    lex_messages: BTreeMap<String, usize>,
    parse_messages: BTreeMap<String, usize>,
    run_messages: BTreeMap<String, usize>,
}

fn main() {
    let dir = env::args().nth(1).expect("usage: probe_corpus <dir>");
    let dir = PathBuf::from(dir);
    let mut totals = Totals::default();
    let mut files: Vec<PathBuf> = match fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("bt"))
            .collect(),
        Err(e) => {
            eprintln!("read_dir({}): {}", dir.display(), e);
            std::process::exit(2);
        }
    };
    files.sort();
    for path in &files {
        probe_one(path, &mut totals);
    }
    print_summary(&totals);
}

fn probe_one(path: &Path, totals: &mut Totals) {
    totals.files += 1;
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => {
            // Treat unreadable as a lex-style failure -- the file
            // never made it into the language pipeline.
            totals.lex_err += 1;
            *totals.lex_messages.entry("io error".into()).or_insert(0) += 1;
            return;
        }
    };
    let tokens = match tokenize(&raw) {
        Ok(t) => t,
        Err(e) => {
            totals.lex_err += 1;
            *totals.lex_messages.entry(normalize_msg(&format!("{e}"))).or_insert(0) += 1;
            return;
        }
    };
    let program = match parse(tokens) {
        Ok(p) => p,
        Err(e) => {
            totals.parse_err += 1;
            *totals.parse_messages.entry(normalize_msg(&format!("{e}"))).or_insert(0) += 1;
            return;
        }
    };
    // Static call-name sweep before running so unknown builtins are
    // surfaced even if execution short-circuits early.
    let mut user_defined = std::collections::BTreeSet::new();
    collect_user_function_names(&program, &mut user_defined);
    let mut called = std::collections::BTreeSet::new();
    collect_call_names(&program, &mut called);
    for name in called {
        if user_defined.contains(&name) || KNOWN_BUILTINS.contains(&name.as_str()) {
            continue;
        }
        *totals.unknown_calls.entry(name).or_insert(0) += 1;
    }
    // Run with a short wall-clock budget. Interpreters that hit
    // `read past EOF` (very common on an empty buffer) report a
    // SourceError; we bucket that separately because parse + dispatch
    // worked and only the data ran out.
    let interp = Interpreter::new(MemorySource::new(Vec::new())).with_timeout(Duration::from_millis(250));
    let result = interp.run(&program);
    match result.terminal_error {
        None => totals.ok += 1,
        Some(RuntimeError::Source(_)) => totals.run_source += 1,
        Some(err) => {
            totals.run_err += 1;
            *totals.run_messages.entry(normalize_msg(&format!("{err}"))).or_insert(0) += 1;
        }
    }
}

/// Walk the AST and collect every name that appears in call position
/// (`Foo(...)`). Misses indirect / member-call patterns
/// (`obj.method(...)`) on purpose -- those don't resolve to builtins.
fn collect_call_names(program: &Program, out: &mut std::collections::BTreeSet<String>) {
    for it in &program.items {
        match it {
            TopItem::Stmt(s) => collect_stmt(s, out),
            TopItem::Function(f) => {
                for s in &f.body {
                    collect_stmt(s, out);
                }
            }
        }
    }
}

fn collect_user_function_names(program: &Program, out: &mut std::collections::BTreeSet<String>) {
    for it in &program.items {
        if let TopItem::Function(FunctionDef { name, .. }) = it {
            out.insert(name.clone());
        }
    }
}

fn collect_stmt(stmt: &Stmt, out: &mut std::collections::BTreeSet<String>) {
    match stmt {
        Stmt::Block { stmts, .. } => {
            for s in stmts {
                collect_stmt(s, out);
            }
        }
        Stmt::Expr { expr, .. } => collect_expr(expr, out),
        Stmt::If { cond, then_branch, else_branch, .. } => {
            collect_expr(cond, out);
            collect_stmt(then_branch, out);
            if let Some(s) = else_branch.as_deref() {
                collect_stmt(s, out);
            }
        }
        Stmt::While { cond, body, .. } | Stmt::DoWhile { cond, body, .. } => {
            collect_expr(cond, out);
            collect_stmt(body, out);
        }
        Stmt::For { init, cond, step, body, .. } => {
            if let Some(s) = init.as_deref() {
                collect_stmt(s, out);
            }
            if let Some(e) = cond.as_ref() {
                collect_expr(e, out);
            }
            if let Some(e) = step.as_ref() {
                collect_expr(e, out);
            }
            collect_stmt(body, out);
        }
        Stmt::Switch { scrutinee, arms, .. } => {
            collect_expr(scrutinee, out);
            for a in arms {
                if let Some(p) = a.pattern.as_ref() {
                    collect_expr(p, out);
                }
                for s in &a.body {
                    collect_stmt(s, out);
                }
            }
        }
        Stmt::FieldDecl { array_size, args, bit_width, init, .. } => {
            if let Some(e) = array_size.as_ref() {
                collect_expr(e, out);
            }
            for a in args {
                collect_expr(a, out);
            }
            if let Some(e) = bit_width.as_ref() {
                collect_expr(e, out);
            }
            if let Some(e) = init.as_ref() {
                collect_expr(e, out);
            }
        }
        Stmt::Return { value, .. } => {
            if let Some(e) = value.as_ref() {
                collect_expr(e, out);
            }
        }
        Stmt::TypedefStruct(s) => {
            for s in &s.body {
                collect_stmt(s, out);
            }
        }
        Stmt::TypedefEnum(e) => {
            for v in &e.variants {
                if let Some(ev) = v.value.as_ref() {
                    collect_expr(ev, out);
                }
            }
        }
        Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::TypedefAlias { .. } => {}
    }
}

fn collect_expr(expr: &Expr, out: &mut std::collections::BTreeSet<String>) {
    match expr {
        Expr::Call { callee, args, .. } => {
            if let Expr::Ident { name, .. } = callee.as_ref() {
                out.insert(name.clone());
            }
            collect_expr(callee, out);
            for a in args {
                collect_expr(a, out);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_expr(lhs, out);
            collect_expr(rhs, out);
        }
        Expr::Unary { operand, .. } => collect_expr(operand, out),
        Expr::Ternary { cond, then_val, else_val, .. } => {
            collect_expr(cond, out);
            collect_expr(then_val, out);
            collect_expr(else_val, out);
        }
        Expr::Assign { target, value, .. } => {
            collect_expr(target, out);
            collect_expr(value, out);
        }
        Expr::Member { target, .. } => collect_expr(target, out),
        Expr::Index { target, index, .. } => {
            collect_expr(target, out);
            collect_expr(index, out);
        }
        Expr::Ident { .. }
        | Expr::IntLit { .. }
        | Expr::FloatLit { .. }
        | Expr::StringLit { .. }
        | Expr::CharLit { .. } => {}
    }
}

/// Strip variable noise (offsets, line numbers, names) so structurally
/// identical errors bucket together in the summary.
fn normalize_msg(msg: &str) -> String {
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
    println!("== probe_corpus summary ==");
    println!("files:      {}", t.files);
    println!("lex_err:    {}", t.lex_err);
    println!("parse_err:  {}", t.parse_err);
    println!("run_err:    {}", t.run_err);
    println!("run_source: {} (read past EOF on empty buffer; benign)", t.run_source);
    println!("ok:         {}", t.ok);
    println!();
    println!("-- top lex errors --");
    print_topk(&t.lex_messages, 15);
    println!("-- top parse errors --");
    print_topk(&t.parse_messages, 15);
    println!("-- top run errors --");
    print_topk(&t.run_messages, 15);
    println!("-- unknown call names (count = files referencing them) --");
    print_topk(&t.unknown_calls, 60);
}

fn print_topk(map: &BTreeMap<String, usize>, k: usize) {
    let mut v: Vec<_> = map.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (msg, count) in v.into_iter().take(k) {
        println!("  {:>4}  {}", count, msg);
    }
    println!();
}
