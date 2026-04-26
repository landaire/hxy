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
    assert_eq!(b.fields.len(), 3);
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
fn const_field_decl() {
    let ast = parse_str("const u32 MAGIC = 0xDEADBEEF;");
    let TopItem::Stmt(Stmt::FieldDecl { is_const, init, .. }) = &ast.items[0] else { panic!() };
    assert!(*is_const);
    assert!(init.is_some());
}
