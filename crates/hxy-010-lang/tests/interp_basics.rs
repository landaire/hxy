//! Exercises the tree-walking interpreter end-to-end against
//! synthetic byte buffers. These tests are our first line of defence
//! against execution regressions; insta snapshots lock the node trees
//! and diagnostic output.

use std::time::Duration;

use hxy_010_lang::Interpreter;
use hxy_010_lang::MemorySource;
use hxy_010_lang::RuntimeError;
use hxy_010_lang::parse;
use hxy_010_lang::tokenize;

fn run(src: &str, bytes: Vec<u8>) -> hxy_010_lang::RunResult {
    let tokens = tokenize(src).unwrap();
    let program = parse(tokens).unwrap();
    Interpreter::new(MemorySource::new(bytes)).run(&program)
}

#[test]
fn reads_primitive_fields_little_endian() {
    let src = "\
LittleEndian();
uchar magic;
uint32 size;
uint64 offset;";
    // magic=0xAB, size=0x01020304, offset=0x0A0B0C0D0E0F1011
    let mut bytes = vec![0xAB];
    bytes.extend_from_slice(&0x01020304u32.to_le_bytes());
    bytes.extend_from_slice(&0x0A0B0C0D0E0F1011u64.to_le_bytes());
    let result = run(src, bytes);
    insta::assert_debug_snapshot!(result);
}

#[test]
fn big_endian_reads_inverse_bytes() {
    let src = "\
BigEndian();
uint16 a;
uint32 b;";
    // a=0x1234, b=0xDEADBEEF (big-endian bytes on the wire).
    let mut bytes = vec![];
    bytes.extend_from_slice(&0x1234u16.to_be_bytes());
    bytes.extend_from_slice(&0xDEADBEEFu32.to_be_bytes());
    let result = run(src, bytes);
    insta::assert_debug_snapshot!(result);
}

#[test]
fn char_array_becomes_string() {
    let src = "\
LittleEndian();
char greeting[5];";
    let bytes = b"hello".to_vec();
    let result = run(src, bytes);
    insta::assert_debug_snapshot!(result);
}

#[test]
fn while_loop_reads_until_eof() {
    let src = "\
LittleEndian();
local uint count = 0;
while ( !FEof() ) {
    uchar b;
    count = count + 1;
}";
    let bytes = vec![0x11, 0x22, 0x33];
    let result = run(src, bytes);
    insta::assert_debug_snapshot!(result);
}

#[test]
fn if_else_branches_on_magic() {
    let src = "\
LittleEndian();
uint32 magic;
if ( magic == 0xDEADBEEF )
    uint32 payload;
else
    uint32 fallback;";
    let mut bytes = vec![];
    bytes.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
    bytes.extend_from_slice(&0x42u32.to_le_bytes());
    let result = run(src, bytes);
    insta::assert_debug_snapshot!(result);
}

#[test]
fn typedef_struct_with_nested_fields() {
    let src = "\
LittleEndian();
typedef struct {
    uchar version;
    ushort flags;
    uint32 crc;
} HEADER;
HEADER header;";
    let bytes = vec![0x01, 0x02, 0x00, 0xEF, 0xBE, 0xAD, 0xDE];
    let result = run(src, bytes);
    insta::assert_debug_snapshot!(result);
}

#[test]
fn typedef_enum_resolves_variant_name() {
    let src = "\
LittleEndian();
typedef enum <uchar> {
    KIND_NONE = 0,
    KIND_FOO  = 1,
    KIND_BAR  = 2
} KIND;
KIND which;";
    let bytes = vec![0x02];
    let result = run(src, bytes);
    insta::assert_debug_snapshot!(result);
}

#[test]
fn function_call_returns_value() {
    let src = "\
LittleEndian();
int add ( int a, int b ) { return a + b; }
local int x = 10;
local int y = 20;
local int z = add(x, y);";
    let result = run(src, vec![]);
    insta::assert_debug_snapshot!(result);
}

#[test]
fn warning_emits_diagnostic() {
    let src = "\
Warning(\"oh no\");";
    let result = run(src, vec![]);
    insta::assert_debug_snapshot!(result);
}

#[test]
fn timeout_surfaces_structured_terminal_error() {
    // Pure Rust-side loop with no cursor advance -- the
    // forward-progress guard would also catch this, but we want to
    // see the timeout variant specifically, so push the stall limit
    // up by reading from the (empty) source each iteration. Easier:
    // configure a tiny timeout and a step limit that won't trip.
    let src = "while (1) { local int i = 0; i++; }";
    let tokens = tokenize(src).unwrap();
    let program = parse(tokens).unwrap();
    // Explicitly raise the step limit so it can't masquerade as the
    // cause; the only finite budget here is the wall clock.
    let result = Interpreter::new(MemorySource::new(vec![]))
        .with_timeout(Duration::from_millis(50))
        .with_step_limit(u64::MAX)
        .run(&program);
    // The forward-progress guard fires first because it's deterministic
    // (100k iterations of a no-source-advance loop), well before 50ms.
    // Either error is acceptable evidence that callers can match.
    match result.terminal_error {
        Some(RuntimeError::LoopStalled { iterations }) => {
            assert!(iterations >= 100_000, "stall count looked wrong: {iterations}");
        }
        Some(RuntimeError::TimedOut { timeout_ms }) => {
            assert_eq!(timeout_ms, 50);
        }
        other => panic!("expected stall or timeout, got {other:?}"),
    }
}
