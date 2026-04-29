//! Snapshot coverage for the calculator's parsed AST and evaluated
//! values. The cases here pin behaviour of the user-facing surface
//! ("typed in the palette") so future changes to precedence,
//! unit handling, or implicit-close-paren behaviour show up as
//! readable diffs in `cargo insta review`.

use hxy_calculator::FieldRef;
use hxy_calculator::PathResolver;
use hxy_calculator::ResolveError;
use hxy_calculator::evaluate;
use hxy_calculator::evaluate_str;
use hxy_calculator::evaluate_str_with;
use hxy_calculator::parse;

fn snap_name(case: &str) -> String {
    case.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_owned()
}

#[test]
fn parse_snapshots() {
    let cases = [
        "42",
        "0x100",
        "1 + 2 * 3",
        "(1 + 2) * 3",
        "0x100 + 1MiB",
        "10 MiB",
        "10MiB",
        "(5 * (5 + 10",
        "1KB + 1KiB",
        "-(0x10 + 0x20)",
        "1_000_000",
        "(1 + 1) MiB",
        "png.length",
        "0x1 + png.length",
        "png#2.chunks[0].length",
    ];
    for case in cases {
        let parsed = parse(case).expect(case);
        insta::assert_debug_snapshot!(format!("parse_{}", snap_name(case)), parsed);
    }
}

#[test]
fn evaluate_snapshots() {
    let cases = [
        "42",
        "0x100",
        "1 + 2 * 3",
        "(1 + 2) * 3",
        "0x100 + 1MiB",
        "10 MiB",
        "10MiB",
        "(5 * (5 + 10",
        "1KB + 1KiB",
        "-(0x10 + 0x20)",
        "1_000_000",
        "(1 + 1) MiB",
        "1 GiB / 1 MiB",
        "1 TiB - 1",
    ];
    for case in cases {
        let value = evaluate(&parse(case).expect(case)).expect(case);
        insta::assert_snapshot!(format!("eval_{}", snap_name(case)), format!("{} = {}", case, value.raw()));
    }
}

/// Stub resolver: returns canned values for a couple of paths so
/// the snapshots cover the path branch end-to-end without needing
/// a real template runtime in the test.
struct StubResolver;

impl PathResolver for StubResolver {
    fn lookup(&self, path: &hxy_calculator::Path) -> Result<FieldRef, ResolveError> {
        match (path.root.as_str(), path.instance, path.segments.as_slice()) {
            ("png", None, [hxy_calculator::PathSegment::Name(n)]) if n == "length" => {
                Ok(FieldRef { offset: 0x100, length: 8, value: Some(8192) })
            }
            ("png", Some(2), [hxy_calculator::PathSegment::Name(n)]) if n == "length" => {
                Ok(FieldRef { offset: 0x200, length: 8, value: Some(4096) })
            }
            ("png", None, [hxy_calculator::PathSegment::Name(n)]) if n == "signature" => {
                Ok(FieldRef { offset: 0, length: 8, value: None })
            }
            _ => Err(ResolveError::UnknownTemplate { name: path.root.clone() }),
        }
    }
}

#[test]
fn evaluate_with_resolver_snapshots() {
    let cases = [
        "png.length",
        "0x1 + png.length",
        "png#2.length",
        "png.length / 1KiB",
        "offset(png.signature)",
        "len(png.signature)",
        "sizeof(png.signature)",
        "offset(png.length) + len(png.length)",
    ];
    for case in cases {
        let value = evaluate_str_with(case, &StubResolver).expect(case);
        insta::assert_snapshot!(format!("resolver_{}", snap_name(case)), format!("{} = {}", case, value.raw()));
    }
}

#[test]
fn error_snapshots() {
    let cases = ["", "abc", "1 +", "1 / 0", "5 + 3)", "5 + 3 abc"];
    for case in cases {
        let result: Result<_, _> = evaluate_str(case);
        let rendered = match result {
            Ok(v) => format!("ok({})", v.raw()),
            Err(e) => format!("err({e})"),
        };
        insta::assert_snapshot!(format!("err_{}", snap_name(case)), rendered);
    }
}
