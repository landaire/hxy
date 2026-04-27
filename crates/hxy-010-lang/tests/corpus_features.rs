//! Coverage tests for 010 dialect features surfaced by surveying the
//! `examples/probe_corpus` walk over a real-world corpus of `.bt`
//! templates. Each test is a hand-rolled synthetic template that
//! exercises one feature and locks an expected outcome -- no template
//! content is copied from the corpus, so license posture stays clean.
//!
//! When adding a new builtin or parser-syntax accommodation, drop a
//! test in here so future regressions show up at `cargo test` time
//! rather than the next time someone runs the probe over a
//! freshly-downloaded corpus.

use hxy_010_lang::Interpreter;
use hxy_010_lang::MemorySource;
use hxy_010_lang::RunResult;
use hxy_010_lang::parse;
use hxy_010_lang::tokenize;

fn run(src: &str) -> RunResult {
    run_with(src, Vec::new())
}

fn run_with(src: &str, bytes: Vec<u8>) -> RunResult {
    let tokens = tokenize(src).unwrap_or_else(|e| panic!("lex failed: {e}\n--- src ---\n{src}"));
    let program = parse(tokens).unwrap_or_else(|e| panic!("parse failed: {e}\n--- src ---\n{src}"));
    Interpreter::new(MemorySource::new(bytes)).run(&program)
}

fn assert_no_terminal_error(result: &RunResult) {
    if let Some(err) = result.terminal_error.as_ref() {
        panic!("template hit terminal error: {err:?}\n  diagnostics: {:?}", result.diagnostics);
    }
}

#[test]
fn lexer_skips_preprocessor_directives() {
    // `#define`, `#ifdef`, `#endif`, `#error`, line continuations on
    // `#define`, and arbitrary `#whatever` lines all need to lex away
    // as trivia. If the lexer treats `#` as an unknown char the
    // template chokes before parsing even starts.
    let src = "\
#define MAGIC 0xDEADBEEF\n\
#ifdef SOMETHING\n\
#define WIDE \\\n\
            EXTRA\n\
#endif\n\
#pragma once\n\
local int x = 1;\n\
";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn lexer_consumes_integer_literal_suffixes() {
    // C-style `U`, `L`, `UL`, `ULL` suffixes are stripped without
    // changing the parsed value. Floating-point `f` / `F` likewise.
    let src = "\
local uint64 a = 0xFFFFFFFFUL;
local uint64 b = 0x80U;
local uint64 c = 100ULL;
local int d = 7L;
local double e = 1.5f;
";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn lexer_accepts_wide_string_prefix() {
    // `L"..."` is the same string token as `"..."` -- we don't model
    // wide strings as a distinct value type, so the prefix is purely
    // lexical.
    let src = r#"Printf(L"hello %d", 42);"#;
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn parser_accepts_empty_bracket_array() {
    // 010's open-ended array idiom: `byte data[];`. We synthesise a
    // zero-length so the field parses; the interpreter emits an
    // empty array node which downstream code is happy to ignore.
    let src = "byte data[];";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn parser_accepts_multi_dim_array() {
    // `int grid[2][3];` -- only the outer dim is honoured (we don't
    // model 2D arrays); the inner is parsed and dropped. The
    // template just needs to parse without erroring.
    let src = "local int grid[2][3] = 0;";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn parser_accepts_parameterised_struct_with_trailing_array() {
    // `Block items(size)[count];` -- args first, array second. The
    // reverse order (`items[count]`) was already supported; this
    // checks the corpus-found ordering parses too.
    let src = "\
typedef struct (int len) {
    local int n = len;
} Block;
Block xs(2)[3];
";
    let result = run(src);
    // Empty source means the read fails with SourceError; we don't
    // care about that, only that parse + dispatch worked.
    assert!(matches!(result.terminal_error, None | Some(hxy_010_lang::RuntimeError::Source(_))));
}

#[test]
fn parser_accepts_void_no_arg_function() {
    // `f(void)` is the C-style "no params" spelling. Must not parse
    // `void` as a real parameter type.
    let src = "\
int answer(void) { return 42; }
local int x = answer();
";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn parser_accepts_function_prototype() {
    // `void f(int x);` is a forward declaration; the body lands
    // later. We accept it as a no-op def so the source parses.
    let src = "\
void noop(int x);
void noop(int x) { }
noop(0);
";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn parser_accepts_c_style_cast() {
    // `(int) expr` and `(unsigned long) expr` both drop the type
    // and keep the inner expression. 010's interpreter is loose
    // enough about types that the cast is identity at run time.
    let src = "\
local int a = (int) 7;
local int b = (unsigned long) 0xFFFFu;
local int c = (uint64) (a + b);
";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn parser_accepts_brace_list_initializer() {
    // `local int xs[3] = {1, 2, 3};`. We collapse the brace list to
    // its first element (no array-literal AST node); downstream code
    // doesn't rely on the rest. Just verify parse + run succeed.
    let src = "local int xs[3] = {1, 2, 3};";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn parser_accepts_local_const_in_either_order() {
    // Both `local const T x` and `const local T x` show up in the
    // corpus. Either ordering should parse.
    let src = "\
local const int A = 1;
const local int B = 2;
local int sum = A + B;
";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn parser_accepts_struct_forward_declaration() {
    // `struct Tag;` at top level is a no-op; the matching
    // `typedef struct ... Tag;` later registers the type.
    let src = "\
struct Tag;
typedef struct {
    int field;
} Tag;
";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn parser_accepts_unsigned_signed_compound_type() {
    // `unsigned int` and `signed char` appear in the corpus. The
    // modifier is dropped; the underlying type drives the read.
    let src = "\
local unsigned int u = 7;
local signed int s = -3;
local int sum = u + s;
";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn parser_accepts_sizeof_with_struct_keyword() {
    // `sizeof(struct Foo)` -- the `struct` tag inside `sizeof` is
    // stripped at parse time so the inner ident lookups against
    // the type table.
    let src = "\
typedef struct {
    int x;
    int y;
} Foo;
local int n = sizeof(struct Foo);
";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn builtin_assert_passes_when_truthy() {
    let src = "Assert(1, \"won't fire\");";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn builtin_assert_fails_on_falsy_condition() {
    // Failure is reported as a terminal RuntimeError + a diagnostic.
    let src = "Assert(0, \"boom\");";
    let result = run(src);
    assert!(result.terminal_error.is_some(), "assert should halt the run");
    assert!(
        result.diagnostics.iter().any(|d| d.message.contains("boom")),
        "diagnostic should carry the assert message: {:?}",
        result.diagnostics
    );
}

#[test]
fn builtin_fskip_advances_cursor() {
    // FSkip(n) moves the cursor forward by n bytes, so a subsequent
    // FTell() reports the new position.
    let src = "\
FSkip(8);
local int64 pos = FTell();
Printf(\"pos=%d\", pos);
";
    let result = run(src);
    assert_no_terminal_error(&result);
    assert!(
        result.diagnostics.iter().any(|d| d.message.contains("pos=8")),
        "FSkip(8) should land FTell at 8: {:?}",
        result.diagnostics
    );
}

#[test]
fn builtin_sizeof_on_field_returns_byte_length() {
    // `sizeof(field)` reports the byte length of the most recent
    // emitted node by that name. Distinct from `sizeof(TypeName)`.
    let src = "\
LittleEndian();
uint32 magic;
local uint64 n = sizeof(magic);
Printf(\"%d\", n);
";
    let result = run_with(src, 0xDEADBEEFu32.to_le_bytes().to_vec());
    assert_no_terminal_error(&result);
    assert!(result.diagnostics.iter().any(|d| d.message.contains('4')));
}

#[test]
fn builtin_startof_returns_node_offset() {
    // `startof(field)` reports where the field landed in the source.
    let src = "\
LittleEndian();
uchar pad;
uint32 marker;
local uint64 off = startof(marker);
Printf(\"%d\", off);
";
    let mut bytes = vec![0xAA];
    bytes.extend_from_slice(&0x12345678u32.to_le_bytes());
    let result = run_with(src, bytes);
    assert_no_terminal_error(&result);
    assert!(result.diagnostics.iter().any(|d| d.message.contains('1')));
}

#[test]
fn builtin_strcmp_returns_ordering() {
    // libc-style: negative if a < b, zero on equal, positive
    // otherwise. The interpreter only needs to be consistent with
    // sign; magnitudes are unspecified.
    let src = "\
Printf(\"%d %d %d\", Strcmp(\"abc\", \"abc\"), Strcmp(\"abc\", \"abd\"), Strcmp(\"abd\", \"abc\"));
";
    let result = run(src);
    assert_no_terminal_error(&result);
    let line = result
        .diagnostics
        .iter()
        .find_map(|d| if d.message.contains(' ') { Some(d.message.clone()) } else { None })
        .expect("printf produced a diagnostic");
    let parts: Vec<&str> = line.split_whitespace().collect();
    assert_eq!(parts[0], "0", "abc vs abc should be equal: {line}");
    assert!(parts[1].starts_with('-'), "abc < abd should be negative: {line}");
    assert!(!parts[2].starts_with('-') && parts[2] != "0", "abd > abc should be positive: {line}");
}

#[test]
fn builtin_substr_slices_string() {
    // `SubStr(str, start, len)` works on char counts, not bytes.
    let src = "\
local string s = SubStr(\"abcdef\", 1, 3);
Printf(\"%s\", s);
";
    let result = run(src);
    assert_no_terminal_error(&result);
    assert!(
        result.diagnostics.iter().any(|d| d.message == "bcd"),
        "expected 'bcd' diagnostic, got {:?}",
        result.diagnostics
    );
}

#[test]
fn builtin_atoi_parses_decimal() {
    // `Atoi("42")` -> 42. Non-numeric input falls back to 0.
    let src = "\
local int64 a = Atoi(\"42\");
local int64 b = Atoi(\"   bogus  \");
Printf(\"%d %d\", a, b);
";
    let result = run(src);
    assert_no_terminal_error(&result);
    assert!(result.diagnostics.iter().any(|d| d.message.contains("42 0")));
}

#[test]
fn builtin_min_max_pick_extremes() {
    // Min / Max compare numerically and return the appropriate side.
    let src = "\
Printf(\"%d %d\", Min(7, 3), Max(7, 3));
";
    let result = run(src);
    assert_no_terminal_error(&result);
    assert!(result.diagnostics.iter().any(|d| d.message.contains("3 7")));
}

#[test]
fn builtin_is_char_alpha_is_predicate() {
    // ASCII fold; first char of the arg is what's tested. We branch
    // on the result rather than printing the bool directly so the
    // test isn't sensitive to how `Value::Bool` renders.
    let src = "\
local int alpha = 0;
local int digit = 0;
if (IsCharAlpha(\"x\")) alpha = 1;
if (IsCharAlpha(\"7\")) digit = 1;
Printf(\"alpha=%d digit=%d\", alpha, digit);
";
    let result = run(src);
    assert_no_terminal_error(&result);
    assert!(
        result.diagnostics.iter().any(|d| d.message.contains("alpha=1 digit=0")),
        "alpha char should be true, digit char false: {:?}",
        result.diagnostics
    );
}

#[test]
fn builtin_bitfield_padding_pragmas_are_noops() {
    // `BitfieldDisablePadding` / `BitfieldEnablePadding` are
    // presentational pragmas. Calls should just succeed.
    let src = "\
BitfieldDisablePadding();
BitfieldEnablePadding();
";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn builtin_lowercase_printf_aliases_printf() {
    // Templates in the wild typo `printf` (lowercase). Treat it as
    // an alias so a one-character typo doesn't break the run.
    let src = r#"printf("hello %d", 1);"#;
    let result = run(src);
    assert_no_terminal_error(&result);
    assert!(result.diagnostics.iter().any(|d| d.message.contains("hello 1")));
}

#[test]
fn builtin_string_type_alias_reads_char() {
    // 010's `string` type is NUL-terminated; we model it as a
    // 1-byte primitive so templates that declare `string` typed
    // fields don't fail with an unknown-type error. The byte read
    // is whatever's at the cursor.
    let src = "string ch;";
    let result = run_with(src, vec![b'A']);
    assert_no_terminal_error(&result);
    assert_eq!(result.nodes.len(), 1);
}

#[test]
fn builtin_read_string_decodes_until_nul() {
    // ReadString(offset) reads until NUL (or the default cap).
    let src = "\
local string s = ReadString(0);
Printf(\"%s\", s);
";
    let mut bytes = b"hello".to_vec();
    bytes.push(0);
    let result = run_with(src, bytes);
    assert_no_terminal_error(&result);
    assert!(result.diagnostics.iter().any(|d| d.message == "hello"));
}

#[test]
fn builtin_int_to_binary_str_formats_bits() {
    // `IntToBinaryStr(value, bits)` is zero-padded.
    let src = "\
local string s = IntToBinaryStr(5, 8);
Printf(\"%s\", s);
";
    let result = run(src);
    assert_no_terminal_error(&result);
    assert!(result.diagnostics.iter().any(|d| d.message == "00000101"));
}

#[test]
fn parser_accepts_struct_keyword_in_param_type() {
    // `void f(struct Foo &arg)` -- `struct` tag in a parameter type
    // is dropped; the inner ident is the type name. Mirrors the
    // permissive type-ref parsing in field decls.
    let src = "\
typedef struct { int x; } Foo;
void take(struct Foo &f);
take(0);
";
    let result = run(src);
    assert_no_terminal_error(&result);
}

#[test]
fn parser_accepts_array_in_param() {
    // `void f(char buf[])` -- C array-decay; the bracket pair is
    // dropped at parse time so the param shape stays simple.
    let src = "\
void take(char buf[]);
take(0);
";
    let result = run(src);
    assert_no_terminal_error(&result);
}
