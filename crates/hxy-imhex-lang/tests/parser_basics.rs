//! Coverage tests for the Phase 1 ImHex lexer + parser. Each test
//! is hand-rolled here -- no template content is copied from the
//! GPL-licensed upstream corpus. Add tests when a feature lands so
//! corpus-driven regressions show up at `cargo test` time.

use hxy_imhex_lang::ast::ArraySize;
use hxy_imhex_lang::ast::Expr;
use hxy_imhex_lang::ast::Stmt;
use hxy_imhex_lang::ast::TopItem;
use hxy_imhex_lang::parse;
use hxy_imhex_lang::tokenize;

fn parse_str(src: &str) -> hxy_imhex_lang::ast::Program {
    let tokens = tokenize(src).unwrap_or_else(|e| panic!("lex failed: {e}\n--- src ---\n{src}"));
    parse(tokens).unwrap_or_else(|e| panic!("parse failed: {e}\n--- src ---\n{src}"))
}

#[test]
fn empty_program_parses() {
    let ast = parse_str("");
    assert!(ast.items.is_empty());
}

#[test]
fn pragma_lines_are_trivia() {
    // Lexer drops `#pragma`, `#include`, `#ifdef`, etc. so the parser
    // never sees them. Only the trailing decl should land in the AST.
    let ast = parse_str(
        "\
#pragma description Foo
#pragma endian little
#include <std/sys.pat>
struct S { u8 b; };
",
    );
    assert_eq!(ast.items.len(), 1);
    matches!(&ast.items[0], TopItem::Stmt(Stmt::StructDecl(_)));
}

#[test]
fn simple_struct_and_field_decl() {
    let ast = parse_str(
        "\
struct Header {
    u32 magic;
    u8 version;
};
",
    );
    let TopItem::Stmt(Stmt::StructDecl(s)) = &ast.items[0] else { panic!() };
    assert_eq!(s.name, "Header");
    assert_eq!(s.body.len(), 2);
    let Stmt::FieldDecl { ty, name, .. } = &s.body[0] else { panic!() };
    assert_eq!(name, "magic");
    assert_eq!(ty.path, vec!["u32".to_owned()]);
}

#[test]
fn enum_with_backing_type() {
    let ast = parse_str("enum Color : u8 { Red = 0, Green, Blue = 2 };");
    let TopItem::Stmt(Stmt::EnumDecl(e)) = &ast.items[0] else { panic!() };
    assert_eq!(e.name, "Color");
    assert_eq!(e.backing.path, vec!["u8".to_owned()]);
    assert_eq!(e.variants.len(), 3);
}

#[test]
fn bitfield_with_explicit_widths() {
    let ast = parse_str(
        "\
bitfield Flags {
    a : 1;
    b : 7;
    padding : 8;
};
",
    );
    let TopItem::Stmt(Stmt::BitfieldDecl(b)) = &ast.items[0] else { panic!() };
    assert_eq!(b.name, "Flags");
    assert_eq!(b.body.len(), 3);
}

#[test]
fn using_alias() {
    let ast = parse_str("using Magic = u32;");
    let TopItem::Stmt(Stmt::UsingAlias { new_name, source, .. }) = &ast.items[0] else { panic!() };
    assert_eq!(new_name, "Magic");
    assert_eq!(source.path, vec!["u32".to_owned()]);
}

#[test]
fn function_definition_with_typed_params() {
    let ast = parse_str("fn add(u32 a, u32 b) { return a + b; }");
    let TopItem::Function(f) = &ast.items[0] else { panic!() };
    assert_eq!(f.name, "add");
    assert_eq!(f.params.len(), 2);
    assert_eq!(f.body.len(), 1);
}

#[test]
fn placement_and_array_size() {
    // `Type x @ offset;` and `Type x[count];` -- both shapes that
    // distinguish the pattern language from straight 010.
    let ast = parse_str("u32 magic @ 0x100;");
    let TopItem::Stmt(Stmt::FieldDecl { name, placement, .. }) = &ast.items[0] else { panic!() };
    assert_eq!(name, "magic");
    assert!(placement.is_some());

    let ast = parse_str("u8 buf[16];");
    let TopItem::Stmt(Stmt::FieldDecl { array, .. }) = &ast.items[0] else { panic!() };
    assert!(matches!(array, Some(ArraySize::Fixed(_))));

    let ast = parse_str("u8 rest[];");
    let TopItem::Stmt(Stmt::FieldDecl { array, .. }) = &ast.items[0] else { panic!() };
    assert!(matches!(array, Some(ArraySize::Open)));
}

#[test]
fn while_array_size() {
    let ast = parse_str("u8 chunks[while($ < 10)];");
    let TopItem::Stmt(Stmt::FieldDecl { array, .. }) = &ast.items[0] else { panic!() };
    assert!(matches!(array, Some(ArraySize::While(_))));
}

#[test]
fn double_bracket_attributes() {
    let ast = parse_str("u32 marker [[name(\"M\"), color(\"FF0000\")]];");
    let TopItem::Stmt(Stmt::FieldDecl { attrs, .. }) = &ast.items[0] else { panic!() };
    assert_eq!(attrs.0.len(), 2);
    assert_eq!(attrs.0[0].name, "name");
    assert_eq!(attrs.0[1].name, "color");
}

#[test]
fn namespace_path_in_type_ref() {
    let ast = parse_str("std::mem::Bytes data;");
    let TopItem::Stmt(Stmt::FieldDecl { ty, .. }) = &ast.items[0] else { panic!() };
    assert_eq!(ty.path, vec!["std".to_owned(), "mem".to_owned(), "Bytes".to_owned()]);
}

#[test]
fn template_args_on_type_ref() {
    let ast = parse_str("std::mem::Bytes<16> hash;");
    let TopItem::Stmt(Stmt::FieldDecl { ty, .. }) = &ast.items[0] else { panic!() };
    assert_eq!(ty.template_args.len(), 1);
}

#[test]
fn namespace_block() {
    let ast = parse_str("namespace foo::bar { struct S { u8 b; }; }");
    let TopItem::Stmt(Stmt::Namespace { path, body, is_auto, .. }) = &ast.items[0] else { panic!() };
    assert_eq!(path, &vec!["foo".to_owned(), "bar".to_owned()]);
    assert!(!is_auto);
    assert_eq!(body.len(), 1);
}

#[test]
fn import_dotted_and_path_styles() {
    let ast = parse_str("import std.io;");
    let TopItem::Stmt(Stmt::Import { path, .. }) = &ast.items[0] else { panic!() };
    assert_eq!(path, &vec!["std".to_owned(), "io".to_owned()]);

    let ast = parse_str("import std::io;");
    let TopItem::Stmt(Stmt::Import { path, .. }) = &ast.items[0] else { panic!() };
    assert_eq!(path, &vec!["std".to_owned(), "io".to_owned()]);
}

#[test]
fn match_with_value_range_wildcard() {
    let ast = parse_str(
        "\
fn pick(u8 x) {
    match (x) {
        (1): { return 1; }
        (2, 3): { return 2; }
        (4 ... 8): { return 3; }
        (_): { return 0; }
    }
}
",
    );
    let TopItem::Function(f) = &ast.items[0] else { panic!() };
    let Stmt::Match { arms, .. } = &f.body[0] else { panic!() };
    assert_eq!(arms.len(), 4);
}

#[test]
fn cursor_and_parent_in_expressions() {
    let ast = parse_str("u8 a[parent.size - $];");
    let TopItem::Stmt(Stmt::FieldDecl { array, .. }) = &ast.items[0] else { panic!() };
    assert!(array.is_some());
}

#[test]
fn sizeof_and_addressof_reflect() {
    let ast = parse_str(
        "\
fn check() {
    return sizeof(u32) + addressof(magic);
}
",
    );
    let TopItem::Function(f) = &ast.items[0] else { panic!() };
    // Just verify it parses; the body has a return + call shape we
    // already cover elsewhere.
    assert_eq!(f.body.len(), 1);
}

#[test]
fn integer_literal_underscore_separators() {
    // Lexer should preserve the value -- 1_000 is one million,
    // not "1" followed by an identifier.
    let ast = parse_str("u32 x = 1_000_000;");
    let TopItem::Stmt(Stmt::FieldDecl { init, .. }) = &ast.items[0] else { panic!() };
    let Some(Expr::IntLit { value, .. }) = init else { panic!() };
    assert_eq!(*value, 1_000_000);
}

#[test]
fn integer_literal_apostrophe_separators() {
    // C++14-style digit separators: `0xA000'0002` and `1'000'000`.
    // Several corpus templates use this for hex grouping.
    let ast = parse_str("u32 x = 0xA000'0002;");
    let TopItem::Stmt(Stmt::FieldDecl { init: Some(Expr::IntLit { value, .. }), .. }) = &ast.items[0] else {
        panic!()
    };
    assert_eq!(*value, 0xA000_0002);
    let ast = parse_str("u32 y = 1'000'000;");
    let TopItem::Stmt(Stmt::FieldDecl { init: Some(Expr::IntLit { value, .. }), .. }) = &ast.items[0] else {
        panic!()
    };
    assert_eq!(*value, 1_000_000);
}

#[test]
fn const_field_decl() {
    let ast = parse_str("const u32 MAGIC = 0xDEADBEEF;");
    let TopItem::Stmt(Stmt::FieldDecl { is_const, init, .. }) = &ast.items[0] else { panic!() };
    assert!(*is_const);
    assert!(init.is_some());
}

#[test]
fn bitfield_typed_field_prefix() {
    // ImHex bitfields can prefix a field with a type that projects
    // the bit-slice as that type (typically an enum). The parser
    // must accept both `name : width` and `Type name : width`.
    let ast = parse_str(
        "\
bitfield Flags {
    bool A : 1;
    plain : 3;
    SomeEnum tagged : 4;
};
",
    );
    let TopItem::Stmt(Stmt::BitfieldDecl(decl)) = &ast.items[0] else { panic!() };
    let fields: Vec<_> = decl
        .body
        .iter()
        .filter_map(|s| if let Stmt::BitfieldField { ty, name, .. } = s { Some((name, ty)) } else { None })
        .collect();
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0].0, "A");
    assert!(fields[0].1.as_ref().is_some_and(|t| t.leaf() == "bool"));
    assert_eq!(fields[1].0, "plain");
    assert!(fields[1].1.is_none());
    assert_eq!(fields[2].0, "tagged");
    assert!(fields[2].1.as_ref().is_some_and(|t| t.leaf() == "SomeEnum"));
}

#[test]
fn bitfield_body_supports_if_and_computed() {
    // Bitfield bodies can contain conditional fields and derived /
    // computed values alongside the usual `name : width;` shape.
    let ast = parse_str(
        "\
bitfield Flags {
    a : 4;
    if (a > 0) {
        b : 4;
    }
    auto sum = a + 1;
};
",
    );
    let TopItem::Stmt(Stmt::BitfieldDecl(decl)) = &ast.items[0] else { panic!() };
    assert_eq!(decl.body.len(), 3);
    assert!(matches!(decl.body[0], Stmt::BitfieldField { .. }));
    assert!(matches!(decl.body[1], Stmt::If { .. }));
    assert!(matches!(decl.body[2], Stmt::FieldDecl { .. }));
}

#[test]
fn enum_range_variant() {
    // `Reserved = 0 ... 7` -- the variant covers an inclusive range
    // of integer values. We accept `..` and `...` interchangeably.
    let ast = parse_str(
        "\
enum E : u8 {
    Solo = 1,
    Reserved = 0 ... 7,
    Tail = 8 .. 15
};
",
    );
    let TopItem::Stmt(Stmt::EnumDecl(decl)) = &ast.items[0] else { panic!() };
    assert_eq!(decl.variants.len(), 3);
    assert!(decl.variants[0].value_end.is_none());
    let r = &decl.variants[1];
    assert_eq!(r.name, "Reserved");
    assert!(r.value.is_some() && r.value_end.is_some());
    assert!(decl.variants[2].value_end.is_some());
}

#[test]
fn float_literal_with_c_style_suffix() {
    // `100.f` and `0.5d` are C-style float suffixes used by patterns
    // imported from BT/C sources. The suffix is informational; the
    // parsed value is a regular f64.
    let ast = parse_str("u32 x = 0; auto f = 100.f; auto d = 0.5d;");
    // Three statements; the lexer must not have rejected the suffixes.
    assert_eq!(ast.items.len(), 3);
}

#[test]
fn rbracketbracket_splits_for_inner_index() {
    // `arr[expr]]` -- `]]` is lexed as the attribute closer but here
    // it really is two `]`s (closing the index, then closing the
    // outer array). The parser must split the merged token.
    let ast = parse_str(
        "\
struct S {
    u8 outer[parent.refs[0]];
};
",
    );
    let TopItem::Stmt(Stmt::StructDecl(decl)) = &ast.items[0] else { panic!() };
    let Stmt::FieldDecl { array, .. } = &decl.body[0] else { panic!() };
    matches!(array, Some(ArraySize::Fixed(_)));
}

#[test]
fn whitespace_separated_attribute_close_accepted() {
    // `[[name("x")] ]` -- some hand-edited templates put a stray
    // space inside the closer, which the lexer emits as two `]`s.
    // The parser should accept the pair as `]]`.
    let ast = parse_str("u8 b [[name(\"x\")] ];");
    assert_eq!(ast.items.len(), 1);
}

#[test]
fn template_keyword_in_field_name_position() {
    // `template` is a real ImHex keyword (`template<...> struct ...`)
    // but a few corpus patterns use it as a regular field name --
    // soft-ident accepts it where ambiguity is otherwise resolved.
    let ast = parse_str("DialogItemTemplate template [[inline]];");
    assert_eq!(ast.items.len(), 1);
}

#[test]
fn anonymous_typed_field_with_attrs() {
    // `Type [[attrs]];` -- a name-less inline read. ImHex emits the
    // value but the field has no bound name. We model it as a
    // FieldDecl with an empty name.
    let ast = parse_str("FDTToken[[hidden]];");
    let TopItem::Stmt(Stmt::FieldDecl { name, ty, .. }) = &ast.items[0] else { panic!() };
    assert!(name.is_empty());
    assert_eq!(ty.leaf(), "FDTToken");
}

#[test]
fn match_pattern_alternation_with_pipe() {
    // `(0 ... 15 | 26 ... 53 | 99): ...` -- corpus templates use
    // `|` and `||` as shorthand for listing alternative patterns
    // inside one arm group. Both must parse without bit-or'ing the
    // upper range bound into the next pattern.
    let ast = parse_str(
        "\
fn classify(u8 x) {
    match (x) {
        (0 ... 15 | 26 ... 53 | 99): { return 1; }
        (1 || 3): { return 2; }
        (_): { return 0; }
    }
}
",
    );
    let TopItem::Function(f) = &ast.items[0] else { panic!() };
    let Stmt::Match { arms, .. } = &f.body[0] else { panic!() };
    assert_eq!(arms[0].patterns.len(), 3);
    assert_eq!(arms[1].patterns.len(), 2);
}

#[test]
fn generic_int_primitive_lookup() {
    // `u24`, `s40`, etc. aren't in the static primitive table but
    // should still resolve as byte-aligned int types at runtime.
    use hxy_imhex_lang::Interpreter;
    use hxy_imhex_lang::MemorySource;
    let src = "u24 v;";
    let tokens = hxy_imhex_lang::tokenize(src).unwrap();
    let prog = hxy_imhex_lang::parse(tokens).unwrap();
    let result = Interpreter::new(MemorySource::new(vec![0x01, 0x02, 0x03])).run(&prog);
    assert!(result.terminal_error.is_none(), "u24 should read 3 bytes: {:?}", result.terminal_error);
    let v = result.nodes.iter().find(|n| n.name == "v").unwrap();
    assert_eq!(v.length, 3);
}

#[test]
fn match_tuple_scrutinee_compat() {
    // Tuple-style scrutinee `match (a, b) { ... }`. Multi-value
    // matching isn't modelled yet, but the syntax must not reject.
    let ast = parse_str(
        "\
fn pick(u8 a, u8 b) {
    match (a, b) {
        (0): { return 0; }
        (_): { return 1; }
    }
}
",
    );
    assert_eq!(ast.items.len(), 1);
}

#[test]
fn define_macro_expands_at_use_site() {
    // `#define NAME body` -- name is replaced by the body's tokens
    // wherever it appears as an identifier. The use-site span is
    // preserved so errors still point at the use, not the define.
    let ast = parse_str(
        "\
#define MAGIC_LEN 4
char magic[MAGIC_LEN];
",
    );
    let TopItem::Stmt(Stmt::FieldDecl { array, .. }) = &ast.items[0] else { panic!() };
    let Some(ArraySize::Fixed(Expr::IntLit { value, .. })) = array else { panic!() };
    assert_eq!(*value, 4);
}

#[test]
fn define_macro_expands_to_string_literal() {
    let ast = parse_str(
        "\
#define MAGIC \"FOO\"
char magic[std::string::length(MAGIC)];
",
    );
    let TopItem::Stmt(Stmt::FieldDecl { array, .. }) = &ast.items[0] else { panic!() };
    let Some(ArraySize::Fixed(_)) = array else { panic!() };
    // The define body itself parsed; further evaluation happens at
    // run time. We just need the tokens to reach the parser.
}

#[test]
fn bitfield_match_arm_field_in_body() {
    // Bitfield bodies can contain `match` blocks whose arms carry
    // additional bit-slice fields. We need both the match-stmt
    // dispatch in bitfield-entry parsing AND the arm body to
    // accept `name : width;` shapes via the regular stmt fallback.
    let ast = parse_str(
        "\
bitfield B {
    op : 4;
    match (op) {
        (1): { a : 4; }
        (2): { b : 4; }
        (_): { rest : 4; }
    }
};
",
    );
    let TopItem::Stmt(Stmt::BitfieldDecl(decl)) = &ast.items[0] else { panic!() };
    assert!(matches!(decl.body[0], Stmt::BitfieldField { .. }));
    assert!(matches!(decl.body[1], Stmt::Match { .. }));
}
