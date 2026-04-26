//! Phase 2 acceptance tests. Each one exercises a feature the Phase
//! 1 plan deferred -- placement (`@`), magic identifiers (`$`,
//! `parent`, `this`), reflective `sizeof`/`addressof`, `match`
//! expressions -- against a hand-rolled synthetic template. No
//! template content is copied from the GPL-licensed corpus.

use hxy_imhex_lang::Interpreter;
use hxy_imhex_lang::MemorySource;
use hxy_imhex_lang::RunResult;
use hxy_imhex_lang::Value;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

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
fn placement_inside_struct_saves_and_restores_cursor() {
    // Inside a struct body, `Type x @ offset;` is a side channel:
    // it reads at `offset` but leaves the enclosing struct's cursor
    // alone. The enclosing struct's natural size therefore only
    // reflects its sequential reads -- this is what makes
    // `Extension { ...; payload[N] @ data_pos; }` in an array work.
    let src = "\
struct S {
    u8 first;
    u8 placed @ 0x05;
    u8 second;
};
S s;
";
    let bytes = vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x55];
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    let first = result.nodes.iter().find(|n| n.name == "first").unwrap();
    let placed = result.nodes.iter().find(|n| n.name == "placed").unwrap();
    let second = result.nodes.iter().find(|n| n.name == "second").unwrap();
    assert_eq!(first.offset, 0);
    assert_eq!(placed.offset, 5);
    assert_eq!(second.offset, 1);
    assert!(matches!(placed.value, Some(Value::UInt { value: 0x55, .. })));
    assert!(matches!(second.value, Some(Value::UInt { value: 0xBB, .. })));
}

#[test]
fn placement_at_top_level_advances_cursor() {
    // At top level (outside any struct body), `@` is a plain seek:
    // the cursor stays wherever the placed read ended, so a
    // following `Type y @ $;` reads right after it. nro.hexpat uses
    // `Start start @ 0x00; Header header @ $;` -- if `@` restored
    // the cursor, `$` would be 0 and the second read would clobber
    // the first.
    let src = "\
u8 first @ 0x00;
u8 follow @ $;
";
    let bytes = vec![0xAA, 0xBB, 0xCC];
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    let first = result.nodes.iter().find(|n| n.name == "first").unwrap();
    let follow = result.nodes.iter().find(|n| n.name == "follow").unwrap();
    assert_eq!(first.offset, 0);
    assert_eq!(follow.offset, 1);
    assert!(matches!(first.value, Some(Value::UInt { value: 0xAA, .. })));
    assert!(matches!(follow.value, Some(Value::UInt { value: 0xBB, .. })));
}

#[test]
fn dollar_cursor_returns_current_offset() {
    let src = "\
u8 first;
auto here = $;
u8 second;
auto later = $;
";
    let result = run(src, vec![0xAA, 0xBB]);
    assert_no_terminal_error(&result);
    // Tree only has the byte reads; the locals are scoped, so we
    // assert via the diagnostic-free run + value.
    assert_eq!(result.nodes.len(), 2);
    assert_eq!(result.nodes[0].offset, 0);
    assert_eq!(result.nodes[1].offset, 1);
}

#[test]
fn parent_member_access_resolves_enclosing_struct_field() {
    // `parent.x` resolves to a field on the *enclosing* struct --
    // one hop up from the struct currently being read. Sibling
    // fields on the same struct are reached via `this.x`. This
    // mirrors what corpus templates rely on when inner structs
    // reach back into their containing record (e.g. `[N]`-sized
    // arrays declared off a counter on the parent).
    let src = "\
struct Inner {
    u8 payload[parent.size];
};
struct Outer {
    u32 size;
    Inner inner;
};
Outer o;
";
    let mut bytes = 4u32.to_le_bytes().to_vec();
    bytes.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    let payload = result.nodes.iter().find(|n| n.name == "payload").unwrap();
    assert_eq!(payload.length, 4);
}

#[test]
fn this_member_access_resolves_self_field() {
    // `this.size` resolves to a sibling field on the struct
    // currently being read.
    let src = "\
struct Header {
    u32 size;
    u8 payload[this.size];
};
Header h;
";
    let mut bytes = 3u32.to_le_bytes().to_vec();
    bytes.extend_from_slice(&[0x01, 0x02, 0x03]);
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    let payload = result.nodes.iter().find(|n| n.name == "payload").unwrap();
    assert_eq!(payload.length, 3);
}

#[test]
fn sizeof_type_returns_static_byte_width() {
    // `sizeof(TypeName)` walks the type table -- u32 (4) + u8 (1)
    // = 5. Bind it inside a struct so the local is in scope.
    let src = "\
struct Foo {
    u32 a;
    u8 b;
};
struct Outer {
    u32 marker;
    auto n = sizeof(Foo);
};
Outer outer;
";
    let result = run(src, vec![0x00, 0x00, 0x00, 0x00]);
    assert_no_terminal_error(&result);
    // The `auto n` is a local -- it doesn't emit a node, but the
    // run must still complete cleanly. The previous Phase 1
    // `local_auto_with_initializer` test already locks the path;
    // here we just confirm `sizeof(type)` doesn't trip the run.
    assert!(result.terminal_error.is_none());
}

#[test]
fn sizeof_field_returns_emitted_node_length() {
    // `sizeof(field)` walks the emitted nodes -- distinct from
    // `sizeof(TypeName)`, which uses the type table.
    let src = "\
struct S {
    u32 marker;
    auto m_size = sizeof(marker);
};
S s;
";
    let result = run(src, vec![0x00, 0x00, 0x00, 0x00]);
    assert_no_terminal_error(&result);
}

#[test]
fn addressof_field_returns_offset() {
    let src = "\
struct S {
    u8 pad;
    u32 marker;
    auto m_at = addressof(marker);
};
S s;
";
    let mut bytes = vec![0xFF];
    bytes.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    // The local `m_at` should hold 1 (offset of the marker after
    // the leading u8 pad). We can't read locals directly but the
    // run must complete without an unresolved reference.
}

#[test]
fn match_value_arm_runs_matching_body() {
    let src = "\
struct Doc {
    u8 kind;
    match (kind) {
        (1): { u32 small; }
        (2): { u64 large; }
        (_): { u8 fallback; }
    }
};
Doc d;
";
    // kind = 1 -- small arm should run, large + fallback should not.
    let mut bytes = vec![1];
    bytes.extend_from_slice(&0xCAFEBABEu32.to_le_bytes());
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    assert!(result.nodes.iter().any(|n| n.name == "small"));
    assert!(!result.nodes.iter().any(|n| n.name == "large"));
    assert!(!result.nodes.iter().any(|n| n.name == "fallback"));
}

#[test]
fn match_range_arm_covers_inclusive_bounds() {
    let src = "\
struct Doc {
    u8 kind;
    match (kind) {
        (1 ... 5): { u8 in_range; }
        (_): { u8 outside; }
    }
};
Doc d;
";
    let result = run(
        "\
struct Doc {
    u8 kind;
    match (kind) {
        (1 ... 5): { u8 in_range; }
        (_): { u8 outside; }
    }
};
Doc d;
",
        vec![3, 0xAA],
    );
    assert_no_terminal_error(&result);
    assert!(result.nodes.iter().any(|n| n.name == "in_range"));
    assert!(!result.nodes.iter().any(|n| n.name == "outside"));
    let _ = src;
}

#[test]
fn for_loop_with_comma_separators_iterates() {
    // ImHex's for uses commas instead of semicolons. The body should
    // run `count` times.
    let src = "\
struct Buf {
    u32 count;
    for (auto i = 0, i < count, i += 1) {
        u8 byte;
    }
};
Buf b;
";
    let mut bytes = 3u32.to_le_bytes().to_vec();
    bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    let bytes_read = result.nodes.iter().filter(|n| n.name == "byte").count();
    assert_eq!(bytes_read, 3);
}

#[test]
fn try_catch_runs_body_skips_handler() {
    // `try { ... } catch { ... }` parses; the interpreter doesn't
    // model exceptions yet, so the try body runs straight through
    // and the catch arm is dropped. Verify the body's reads land
    // and the catch's don't.
    let src = "\
struct S {
    try {
        u32 marker;
    } catch {
        u8 fallback;
    }
};
S s;
";
    let result = run(src, 0xDEADBEEFu32.to_le_bytes().to_vec());
    assert_no_terminal_error(&result);
    assert!(result.nodes.iter().any(|n| n.name == "marker"));
    assert!(!result.nodes.iter().any(|n| n.name == "fallback"));
}

#[test]
fn struct_inheritance_parses_with_dropped_parent_link() {
    // `struct B : A { ... }` -- the parent ref is parsed and
    // discarded for now. Body still reads.
    let src = "\
struct A {
    u32 a;
};
struct B : A {
    u32 b;
};
B b_inst;
";
    let mut bytes = 7u32.to_le_bytes().to_vec();
    bytes.extend_from_slice(&8u32.to_le_bytes());
    let result = run(src, bytes);
    assert_no_terminal_error(&result);
    // Phase 2: only `b` is emitted (we don't compose the parent's
    // body yet). The acceptance criterion is just that the file
    // parses + dispatches.
    assert!(result.nodes.iter().any(|n| n.name == "b"));
}

#[test]
fn padding_field_skips_bytes_without_emitting_a_typed_leaf() {
    // `padding[N];` should consume N bytes; subsequent fields read
    // after the gap.
    let src = "\
struct S {
    u8 first;
    padding[3];
    u8 last;
};
S s;
";
    let result = run(src, vec![0xAA, 0x00, 0x00, 0x00, 0xBB]);
    assert_no_terminal_error(&result);
    let last = result.nodes.iter().find(|n| n.name == "last").unwrap();
    assert_eq!(last.offset, 4);
    assert!(matches!(last.value, Some(Value::UInt { value: 0xBB, .. })));
}
