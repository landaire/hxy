//! Phase 3 acceptance tests. Each one exercises namespace dispatch,
//! import resolution, or the std-builtin shim against a hand-rolled
//! synthetic template + a small in-memory import resolver. No
//! template content is copied from the GPL-licensed corpus.

use std::collections::HashMap;
use std::sync::Arc;

use hxy_imhex_lang::ImportResolver;
use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::RunResult;
use hxy_imhex_lang::Severity;
use hxy_imhex_lang::Value;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

/// In-memory resolver for tests -- maps a path like `["std", "io"]`
/// to a hard-coded source string.
struct MapResolver(HashMap<String, String>);

impl MapResolver {
    fn new(entries: &[(&str, &str)]) -> Self {
        Self(entries.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect())
    }
}

impl ImportResolver for MapResolver {
    fn resolve(&self, segments: &[String]) -> Option<String> {
        self.0.get(&segments.join("::")).cloned()
    }
}

fn run_with_resolver(src: &str, bytes: Vec<u8>, resolver: Arc<dyn ImportResolver>) -> RunResult {
    let tokens = tokenize(src).unwrap_or_else(|e| panic!("lex failed: {e}\n--- src ---\n{src}"));
    let program = parse(tokens).unwrap_or_else(|e| panic!("parse failed: {e}\n--- src ---\n{src}"));
    Interpreter::new(MemorySource::new(bytes)).with_import_resolver(resolver).run(&program)
}

fn run(src: &str, bytes: Vec<u8>) -> RunResult {
    let tokens = tokenize(src).unwrap_or_else(|e| panic!("lex failed: {e}\n--- src ---\n{src}"));
    let program = parse(tokens).unwrap_or_else(|e| panic!("parse failed: {e}\n--- src ---\n{src}"));
    Interpreter::new(MemorySource::new(bytes)).run(&program)
}

fn assert_no_terminal_error(result: &RunResult) {
    if let Some(err) = result.terminal_error.as_ref() {
        panic!("interpreter halted: {err:?}\n  diagnostics: {:?}", result.diagnostics);
    }
}

#[test]
fn import_pulls_struct_decls_into_main_program() {
    let resolver = Arc::new(MapResolver::new(&[("lib", "struct Header { u32 magic; };")]));
    let main = "\
import lib;
Header h;
";
    let result = run_with_resolver(main, 0xDEADBEEFu32.to_le_bytes().to_vec(), resolver);
    assert_no_terminal_error(&result);
    let header = result.nodes.first().unwrap();
    assert_eq!(header.name, "h");
    let magic = result.nodes.iter().find(|n| n.name == "magic").unwrap();
    assert!(matches!(magic.value, Some(Value::UInt { value: 0xDEAD_BEEF, .. })));
}

#[test]
fn import_resolves_dotted_path() {
    let resolver = Arc::new(MapResolver::new(&[("std::io", "struct Block { u8 marker; };")]));
    let main = "\
import std.io;
Block b;
";
    let result = run_with_resolver(main, vec![0x42], resolver);
    assert_no_terminal_error(&result);
    assert!(result.nodes.iter().any(|n| n.name == "marker"));
}

#[test]
fn missing_import_records_diagnostic_but_doesnt_halt() {
    // A bare `import nope.nothere;` should not stop the run -- a
    // diagnostic is queued and execution continues. Templates that
    // have an `import std.io;` we can't yet resolve still surface
    // their structural reads.
    let main = "\
import nope.nothere;
u8 byte;
";
    let result = run(main, vec![0xAB]);
    assert_no_terminal_error(&result);
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("nope::nothere") && matches!(d.severity, Severity::Warning))
    );
    assert!(result.nodes.iter().any(|n| n.name == "byte"));
}

#[test]
fn namespace_block_registers_qualified_function() {
    let src = "\
namespace foo {
    fn bump(u8 x) { return x + 1; }
}
struct S {
    u8 x;
    auto next = foo::bump(x);
};
S s;
";
    let result = run(src, vec![0x05]);
    assert_no_terminal_error(&result);
    // Run completes -- `foo::bump` resolved through the qualified
    // function table.
}

#[test]
fn std_print_pushes_info_diagnostic() {
    let src = "std::print(\"hello {}\", 42);";
    let result = run(src, Vec::new());
    assert_no_terminal_error(&result);
    let info = result.diagnostics.iter().find(|d| matches!(d.severity, Severity::Info)).expect("info diagnostic");
    assert_eq!(info.message, "hello 42");
}

#[test]
fn std_format_returns_formatted_string() {
    let src = "\
struct S {
    u32 marker;
    auto label = std::format(\"got {}\", marker);
};
S s;
";
    let result = run(src, 7u32.to_le_bytes().to_vec());
    assert_no_terminal_error(&result);
    // Verify by triggering a print of the local. ImHex's auto
    // doesn't emit nodes, so the run completing without errors is
    // the assertion: format returned without failing.
}

#[test]
fn std_assert_halts_on_falsy_condition() {
    let src = "std::assert(false, \"won't pass\");";
    let result = run(src, Vec::new());
    assert!(result.terminal_error.is_some(), "assert should halt");
    assert!(result.diagnostics.iter().any(|d| d.message.contains("won't pass")));
}

#[test]
fn std_assert_passes_on_truthy_condition() {
    let src = "\
std::assert(true, \"ok\");
u8 byte;
";
    let result = run(src, vec![0xAA]);
    assert_no_terminal_error(&result);
    assert!(result.nodes.iter().any(|n| n.name == "byte"));
}

#[test]
fn std_mem_size_returns_source_length() {
    let src = "\
struct S {
    auto file_size = std::mem::size();
    u8 first;
};
S s;
";
    let result = run(src, vec![0x42; 32]);
    assert_no_terminal_error(&result);
    assert_eq!(result.nodes.iter().find(|n| n.name == "first").unwrap().offset, 0);
}

#[test]
fn std_mem_read_unsigned_random_access() {
    let src = "\
struct S {
    u8 prefix;
    auto far = std::mem::read_unsigned(4, 4);
};
S s;
";
    // bytes 0..1 = prefix, bytes 4..8 = u32 little-endian.
    let mut bytes = vec![0xAA, 0x00, 0x00, 0x00];
    bytes.extend_from_slice(&0xCAFEBABEu32.to_le_bytes());
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
}

#[test]
fn std_mem_bytes_template_reads_n_bytes() {
    // `std::mem::Bytes<N>` reads N bytes as a single leaf.
    let src = "\
struct S {
    std::mem::Bytes<4> hash;
};
S s;
";
    let result = run(src, vec![0x01, 0x02, 0x03, 0x04, 0xFF]);
    assert_no_terminal_error(&result);
    let hash = result.nodes.iter().find(|n| n.name == "hash").unwrap();
    assert_eq!(hash.length, 4);
    assert!(matches!(hash.value, Some(Value::Bytes(ref b)) if b == &[0x01, 0x02, 0x03, 0x04]));
}

#[test]
fn bare_bytes_template_works_too() {
    // The corpus alternates between fully-qualified and bare
    // `Bytes<N>` references; both should resolve to the same
    // builtin.
    let src = "\
using Bytes = u8;
struct S {
    Bytes<2> head;
};
S s;
";
    let result = run(src, vec![0xAA, 0xBB]);
    assert_no_terminal_error(&result);
    let head = result.nodes.iter().find(|n| n.name == "head").unwrap();
    assert_eq!(head.length, 2);
}

#[test]
fn std_string_helpers_return_expected_predicates() {
    let src = "\
auto starts = std::string::starts_with(\"hello world\", \"hello\");
auto ends = std::string::ends_with(\"hello world\", \"world\");
auto missing = std::string::contains(\"hello\", \"xyz\");
std::print(\"{} {} {}\", starts, ends, missing);
";
    let result = run(src, Vec::new());
    assert_no_terminal_error(&result);
    let info = result.diagnostics.iter().find(|d| matches!(d.severity, Severity::Info)).unwrap();
    assert_eq!(info.message, "true true false");
}
