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
    let ast = parse_str(
        "\
typedef enum <uchar> {
    KIND_NONE = 0,
    KIND_OTHER = 8
} KIND;",
    );
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn typedef_struct_with_attrs() {
    let ast = parse_str(
        "\
typedef struct {
    uchar version;
    ushort flags;
    if ( flags > 0 )
        uint extra;
} HEADER <read=ReadHeader, style=sSection1>;",
    );
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn if_else_chain() {
    let ast = parse_str(
        "\
if ( tag == 1 ) { a; } else if ( tag == 2 ) { b; } else { c; }",
    );
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn while_loop_with_body() {
    let ast = parse_str(
        "\
while ( !FEof() ) {
    tag = ReadUInt(FTell());
    if ( tag == 0 ) break;
}",
    );
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn for_loop() {
    let ast = parse_str("for (i = 0; i < N; i++) { x; }");
    insta::assert_debug_snapshot!(ast);
}

#[test]
fn function_def_with_ref_param() {
    let ast = parse_str(
        "\
string ReadEntry ( ENTRY &e ) {
    if ( exists(e.name) )
        return e.name;
    return \"\";
}",
    );
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

#[test]
#[ignore]
fn probe_external_template() {
    let path = std::env::var("PROBE_BT").expect("set PROBE_BT=path/to/file.bt");
    let raw = std::fs::read_to_string(&path).unwrap();
    let src = match std::env::var("EXPAND_DIR") {
        Ok(base) => expand_includes_for_probe(std::path::Path::new(&path), std::path::Path::new(&base), &raw),
        Err(_) => raw,
    };
    match hxy_010_lang::tokenize(&src) {
        Ok(toks) => {
            eprintln!("tokenized {} tokens", toks.len());
            match hxy_010_lang::parse(toks) {
                Ok(p) => eprintln!("parsed {} items", p.items.len()),
                Err(e) => eprintln!("parse err: {e:#?}"),
            }
        }
        Err(e) => eprintln!("lex err: {e:#?}"),
    }
}

#[test]
#[ignore]
fn probe_execute_template() {
    let bt = std::env::var("PROBE_BT").expect("PROBE_BT=/path/to/template.bt");
    let data = std::env::var("PROBE_DATA").expect("PROBE_DATA=/path/to/data");
    let raw = std::fs::read_to_string(&bt).unwrap();
    let src = match std::env::var("EXPAND_DIR") {
        Ok(base) => expand_includes_for_probe(std::path::Path::new(&bt), std::path::Path::new(&base), &raw),
        Err(_) => raw,
    };
    let bytes = std::fs::read(&data).unwrap();
    let prog = hxy_010_lang::parse(hxy_010_lang::tokenize(&src).expect("tokenize")).expect("parse");
    let source = hxy_010_lang::MemorySource::new(bytes);
    let result = hxy_010_lang::Interpreter::new(source).run(&prog);
    eprintln!("nodes: {}", result.nodes.len());
    eprintln!("diagnostics: {}", result.diagnostics.len());
    for d in result.diagnostics.iter().take(20) {
        eprintln!("  [{:?}] {}", d.severity, d.message);
    }
    for n in result.nodes.iter().take(15) {
        eprintln!("  @{}+{} {} = {:?}", n.offset, n.length, n.name, n.value);
    }
}

fn expand_includes_for_probe(path: &std::path::Path, base_dir: &std::path::Path, raw: &str) -> String {
    // Ad-hoc expansion so this test doesn't need the app crate; good
    // enough for a probe.
    let mut out = String::new();
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    for line in raw.split_inclusive('\n') {
        let trimmed = line.trim_start().trim_end_matches(['\r', '\n']);
        if let Some(rest) = trimmed.strip_prefix("#include")
            && let Some(q) = rest.trim_start().strip_prefix('"')
            && let Some(end) = q.find('"')
        {
            let target = &q[..end];
            let candidate = parent.join(target);
            let resolved = candidate.canonicalize().ok();
            if let Some(r) = resolved
                && r.starts_with(base_dir.canonicalize().unwrap_or_default())
                && let Ok(inner) = std::fs::read_to_string(&r)
            {
                out.push_str(&expand_includes_for_probe(&r, base_dir, &inner));
                continue;
            }
        }
        out.push_str(line);
    }
    out
}
