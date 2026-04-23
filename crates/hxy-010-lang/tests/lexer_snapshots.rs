//! Snapshot tests for the 010 Binary Template lexer. Each test locks
//! the token stream produced by a representative input. When the
//! language surface grows (new operators, keywords), run `cargo insta
//! review` to update snapshots intentionally.

use hxy_010_lang::tokenize;

#[test]
fn empty_input() {
    let tokens = tokenize("").unwrap();
    assert!(tokens.is_empty());
}

#[test]
fn whitespace_only() {
    let tokens = tokenize("  \t\n  \r\n").unwrap();
    assert!(tokens.is_empty());
}

#[test]
fn comments_only_then_eof() {
    let tokens = tokenize("// one\n/* two */\n// three").unwrap();
    assert!(tokens.is_empty());
}

#[test]
fn unexpected_char_reports_offset() {
    let err = tokenize("int x @ y").unwrap_err();
    insta::assert_debug_snapshot!(err);
}

#[test]
fn integer_literals() {
    let src = "0 42 0x1F 0XFF 0b1010 0B11";
    insta::assert_debug_snapshot!(tokenize(src).unwrap());
}

#[test]
fn float_literals() {
    let src = "1.0 0.5 1.5e3 1.5E-3 2.f 1e10";
    insta::assert_debug_snapshot!(tokenize(src).unwrap());
}

#[test]
fn string_and_char_literals() {
    let src = r#""hello" "a\nb" 'x' '\t' '\''"#;
    insta::assert_debug_snapshot!(tokenize(src).unwrap());
}

#[test]
fn identifiers_and_keywords() {
    let src = "typedef struct enum MyType x _private foo123 return";
    insta::assert_debug_snapshot!(tokenize(src).unwrap());
}

#[test]
fn operators_and_punctuation() {
    let src = "+ - * / % == != <= >= && || << >> += -= ++ -- -> ; , . ? : ( ) [ ] { }";
    insta::assert_debug_snapshot!(tokenize(src).unwrap());
}

#[test]
fn line_and_block_comments() {
    let src = "\
// leading comment
int x; /* inline */ int y;
/* multi
   line */
uint z; // trailing
";
    insta::assert_debug_snapshot!(tokenize(src).unwrap());
}

#[test]
fn typedef_enum_block() {
    let src = "\
typedef enum <short> {
    COMP_STORED    = 0,
    COMP_DEFLATE   = 8
} COMPTYPE;";
    insta::assert_debug_snapshot!(tokenize(src).unwrap());
}

#[test]
fn typedef_struct_with_attributes() {
    let src = "\
typedef struct {
    char     frSignature[4] <style=sHeading1Accent>;
    ushort   frFlags;
    if( frFileNameLength > 0 )
        char frFileName[ frFileNameLength ];
} ZIPFILERECORD <read=ReadZIPFILERECORD, style=sHeading1>;";
    insta::assert_debug_snapshot!(tokenize(src).unwrap());
}

#[test]
fn while_loop_with_calls() {
    let src = "\
LittleEndian();
while( !FEof() )
{
    tag = ReadUInt( FTell() );
    if( tag == 0x04034b50 )
        ZIPFILERECORD record;
}";
    insta::assert_debug_snapshot!(tokenize(src).unwrap());
}

#[test]
fn sample_bt_full_file() {
    let src = include_str!("../fixtures/sample.bt");
    let tokens = tokenize(src).unwrap();
    insta::assert_debug_snapshot!(tokens);
}
