//! Snapshot tests for the 010 Binary Template parser. Each test runs
//! `tokenize` + `parse` and locks the resulting AST. When the grammar
//! grows, run `cargo insta review` to update snapshots deliberately.

use hxy_010_lang::parse;
use hxy_010_lang::tokenize;

fn parse_str(src: &str) -> hxy_010_lang::ast::Program {
    let tokens = tokenize(src).expect("lex");
    parse(tokens).expect("parse")
}

#[test]
fn empty_program() {
    let ast = parse_str("");
    assert!(ast.items.is_empty());
}

#[test]
fn expr_stmt_literal() {
    let ast = parse_str("42;");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn arithmetic_precedence() {
    let ast = parse_str("1 + 2 * 3 - 4;");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn comparison_and_logical() {
    let ast = parse_str("a == 1 && b > 2 || c != 0;");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn unary_and_postfix() {
    let ast = parse_str("-x + !y + ~z + p++ + --q;");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn ternary_right_associative() {
    let ast = parse_str("a ? b : c ? d : e;");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn assignment_right_associative() {
    let ast = parse_str("a = b = c;");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn compound_assign_and_index() {
    let ast = parse_str("arr[i] += f(x, y + 1);");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn member_and_call() {
    let ast = parse_str("obj.field.method(1, 2);");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn field_decl_simple() {
    let ast = parse_str("uchar x;");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn field_decl_array_with_attrs() {
    let ast = parse_str(r#"char sig[4] <style=sHeading1, format=hex>;"#);
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn local_var_with_init() {
    let ast = parse_str("local uint tag = 0;");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn typedef_alias() {
    let ast = parse_str("typedef QWORD ULONG64;");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn typedef_enum_with_backing() {
    let ast = parse_str("\
typedef enum <uchar> {
    KIND_NONE = 0,
    KIND_OTHER = 8
} KIND;");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn typedef_struct_with_attrs() {
    let ast = parse_str("\
typedef struct {
    uchar version;
    ushort flags;
    if ( flags > 0 )
        uint extra;
} HEADER <read=ReadHeader, style=sSection1>;");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn if_else_chain() {
    let ast = parse_str("\
if ( tag == 1 ) { a; } else if ( tag == 2 ) { b; } else { c; }");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn while_loop_with_body() {
    let ast = parse_str("\
while ( !FEof() ) {
    tag = ReadUInt(FTell());
    if ( tag == 0 ) break;
}");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn for_loop() {
    let ast = parse_str("for (i = 0; i < N; i++) { x; }");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn function_def_with_ref_param() {
    let ast = parse_str("\
string ReadEntry ( ENTRY &e ) {
    if ( exists(e.name) )
        return e.name;
    return \"\";
}");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn sample_bt_fixture() {
    let src = include_str!("../fixtures/sample.bt");
    let ast = parse_str(src);
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn assets_bin_bt_fixture() {
    let src = include_str!("../fixtures/assets_bin.bt");
    let ast = parse_str(src);
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn recctrl_bt_fixture() {
    let src = include_str!("../fixtures/recctrl.bt");
    let ast = parse_str(src);
    insta::assert_debug_snapshot!(ast);
}
