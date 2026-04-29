//! winnow-based parser for the calculator grammar.
//!
//! Grammar (loose to tight precedence):
//!
//! ```text
//! expr        = additive
//! additive    = multiplicative (('+' | '-') multiplicative)*
//! multiplicative = unary (('*' | '/' | '%') unary)*
//! unary       = ('-' | '+') unary | primary
//! primary     = (number unit?) | ('(' expr (')')? unit?) | path
//! number      = '0x' hexdigits | digits         (underscores allowed)
//! unit        = 'B' | 'KB' | 'KiB' | 'MB' | 'MiB' | 'GB' | 'GiB'
//!             | 'TB' | 'TiB'                    (case insensitive)
//! path        = ident ('#' digits)? (('.' ident) | ('[' digits ']'))*
//! ident       = [A-Za-z_][A-Za-z0-9_]*
//! ```
//!
//! `path` defers to the caller-supplied [`crate::PathResolver`] at
//! evaluation time -- the parser only captures structure. The
//! `'#' digits` suffix on the root segment selects a specific run
//! when the same template ran multiple times against the active
//! file (1-indexed; absent = most recent).
//!
//! Closing `)` is parsed via `opt`, so an unclosed paren simply
//! means "the inner expression ends where input ends" -- mirrors
//! Speedcrunch's input affordance for half-typed expressions.
//! Genuine syntax errors (an unmatched `)`, a literal that
//! overflows `i128`, trailing junk) still surface through
//! [`ParseError`].

use thiserror::Error;
use winnow::ModalResult;
use winnow::Parser;
use winnow::ascii::space0;
use winnow::combinator::alt;
use winnow::combinator::opt;
use winnow::combinator::repeat;
use winnow::error::ContextError;
use winnow::error::ErrMode;
use winnow::token::one_of;
use winnow::token::take_while;

/// Parsed calculator expression. The tree is flat -- no source
/// spans, no annotations. Re-parses are cheap (the inputs are
/// short), so we don't bother caching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// A numeric literal already folded with any unit suffix
    /// (`1 KiB` becomes `Literal(1024)`). Storing the folded
    /// value here keeps the evaluator trivial; the original
    /// unit is recoverable from the parse string if a UI ever
    /// needs to show it.
    Literal(i128),
    Unary(UnaryOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// Reference to a template field, resolved at eval time by the
    /// caller-supplied [`crate::PathResolver`]. The parser only
    /// captures structure; whether `png.length` actually exists
    /// (and what its scalar value is) is the resolver's job.
    Path(Path),
    /// Built-in unary function applied to a template field.
    /// `offset(png.signature)` returns the byte offset;
    /// `len(png.signature)` returns the byte length.
    Call(Function, Path),
}

/// Built-in calculator functions. All are unary and take a
/// [`Path`] argument; they project a single integer (offset or
/// length) out of the resolved [`crate::FieldRef`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Function {
    Offset,
    Len,
}

impl Function {
    /// Short canonical spelling. Used in error / display strings
    /// and as the keyword the parser matches.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offset => "offset",
            Self::Len => "len",
        }
    }

    /// Match a lowercased identifier against the known function
    /// names. Returns `None` for anything else so `primary` can
    /// fall through to a path lookup.
    fn from_lowercased(name: &str) -> Option<Self> {
        match name {
            "offset" => Some(Self::Offset),
            "len" => Some(Self::Len),
            _ => None,
        }
    }
}

/// Reference to a template field. `root` is matched against the
/// available templates' display stems (e.g. `png.bt` matches
/// `"png"` case-insensitively); `instance` selects which run to
/// resolve when the same template fired more than once
/// (1-indexed, `None` = most recent). `segments` walks the
/// resulting field tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    pub root: String,
    pub instance: Option<u32>,
    pub segments: Vec<PathSegment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathSegment {
    Name(String),
    Index(u64),
}

impl Path {
    /// Render the path back into a display string for use in error
    /// messages and palette subtitles. Round-trips well enough for
    /// human reading; not guaranteed to byte-equal the user's
    /// original input (whitespace and case are normalised).
    pub fn display(&self) -> String {
        let mut s = self.root.clone();
        if let Some(n) = self.instance {
            s.push('#');
            s.push_str(&n.to_string());
        }
        for seg in &self.segments {
            match seg {
                PathSegment::Name(n) => {
                    s.push('.');
                    s.push_str(n);
                }
                PathSegment::Index(i) => {
                    s.push('[');
                    s.push_str(&i.to_string());
                    s.push(']');
                }
            }
        }
        s
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Pos,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// IEC vs SI prefix discriminator. Encoded as its own enum
/// rather than a `binary: bool` flag so the call sites read as
/// `Base::Binary` / `Base::Decimal` instead of `true` / `false`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
    /// Powers of 10 (KB = 1000).
    Decimal,
    /// Powers of 1024 (KiB = 1024).
    Binary,
}

/// Recognised unit suffix. The `Base` carried by each scale
/// distinguishes `KB` (1000) from `KiB` (1024).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Bytes,
    Kilo(Base),
    Mega(Base),
    Giga(Base),
    Tera(Base),
}

impl Unit {
    /// Multiplier the unit applies to its preceding number.
    /// `Bytes` is `1`; the rest fan out from `1000` / `1024`
    /// per [`Base`].
    pub fn multiplier(self) -> i128 {
        match self {
            Self::Bytes => 1,
            Self::Kilo(Base::Decimal) => 1_000,
            Self::Kilo(Base::Binary) => 1_024,
            Self::Mega(Base::Decimal) => 1_000_000,
            Self::Mega(Base::Binary) => 1_048_576,
            Self::Giga(Base::Decimal) => 1_000_000_000,
            Self::Giga(Base::Binary) => 1_073_741_824,
            Self::Tera(Base::Decimal) => 1_000_000_000_000,
            Self::Tera(Base::Binary) => 1_099_511_627_776,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("empty input")]
    Empty,
    #[error("syntax error at position {pos}")]
    Syntax { pos: usize },
    #[error("number {literal:?} does not fit in a 128-bit value")]
    NumberOverflow { literal: String },
    #[error("trailing input at position {pos}: {tail:?}")]
    TrailingInput { pos: usize, tail: String },
}

/// Parse `input` into an [`Expr`]. Whitespace is permitted
/// anywhere between tokens; leading and trailing whitespace are
/// stripped before parsing.
pub fn parse(input: &str) -> Result<Expr, ParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ParseError::Empty);
    }
    let original = trimmed;
    let mut cursor: &str = trimmed;
    let expr = match additive(&mut cursor) {
        Ok(e) => e,
        Err(_) => {
            return Err(ParseError::Syntax { pos: original.len() - cursor.len() });
        }
    };
    skip_ws(&mut cursor);
    if !cursor.is_empty() {
        return Err(ParseError::TrailingInput {
            pos: original.len() - cursor.len(),
            tail: cursor.to_owned(),
        });
    }
    Ok(expr)
}

fn skip_ws(input: &mut &str) {
    let _ = space0::<_, ContextError>.parse_next(input);
}

fn additive(input: &mut &str) -> ModalResult<Expr> {
    skip_ws(input);
    let mut left = multiplicative(input)?;
    loop {
        skip_ws(input);
        let op = match opt(alt(("+".value(BinOp::Add), "-".value(BinOp::Sub)))).parse_next(input)? {
            Some(o) => o,
            None => break,
        };
        skip_ws(input);
        let right = multiplicative(input)?;
        left = Expr::Binary(op, Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn multiplicative(input: &mut &str) -> ModalResult<Expr> {
    skip_ws(input);
    let mut left = unary(input)?;
    loop {
        skip_ws(input);
        let op = match opt(alt((
            "*".value(BinOp::Mul),
            "/".value(BinOp::Div),
            "%".value(BinOp::Mod),
        )))
        .parse_next(input)?
        {
            Some(o) => o,
            None => break,
        };
        skip_ws(input);
        let right = unary(input)?;
        left = Expr::Binary(op, Box::new(left), Box::new(right));
    }
    Ok(left)
}

fn unary(input: &mut &str) -> ModalResult<Expr> {
    skip_ws(input);
    if let Some(op) = opt(alt(("-".value(UnaryOp::Neg), "+".value(UnaryOp::Pos)))).parse_next(input)? {
        skip_ws(input);
        let inner = unary(input)?;
        return Ok(Expr::Unary(op, Box::new(inner)));
    }
    primary(input)
}

fn primary(input: &mut &str) -> ModalResult<Expr> {
    skip_ws(input);
    if input.starts_with('(') {
        return paren_expr(input);
    }
    if input.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return number_with_unit(input);
    }
    // An identifier followed by `(` is a built-in function call.
    // Anything else is a path. We commit to the function-call
    // branch only after the `(` shows up so a real path whose root
    // happens to be `offset` / `len` / `sizeof` (e.g. a template
    // field literally named `len`) still parses as a path lookup.
    let head = identifier(input)?;
    skip_ws(input);
    if input.starts_with('(')
        && let Some(func) = Function::from_lowercased(&head.to_ascii_lowercase())
    {
        "(".parse_next(input)?;
        skip_ws(input);
        let arg = path(input)?;
        skip_ws(input);
        ")".parse_next(input)?;
        return Ok(Expr::Call(func, arg));
    }
    // Fall through: build a path starting with the already-consumed
    // identifier. Reusing `path()` directly would re-parse the head,
    // so we splice the suffix in by hand.
    let instance = opt(instance_suffix).parse_next(input)?;
    let segments: Vec<PathSegment> = repeat(0.., path_segment).parse_next(input)?;
    Ok(Expr::Path(Path { root: head, instance, segments }))
}

fn paren_expr(input: &mut &str) -> ModalResult<Expr> {
    "(".parse_next(input)?;
    skip_ws(input);
    let inner = additive(input)?;
    skip_ws(input);
    // `)` is intentionally optional. Missing closer = "ends here";
    // it lets the user type `(5 * (5 + 10` and still get a value.
    let _: Option<&str> = opt(")").parse_next(input)?;
    skip_ws(input);
    if let Some(unit) = opt(unit_suffix).parse_next(input)? {
        return Ok(Expr::Binary(
            BinOp::Mul,
            Box::new(inner),
            Box::new(Expr::Literal(unit.multiplier())),
        ));
    }
    Ok(inner)
}

fn number_with_unit(input: &mut &str) -> ModalResult<Expr> {
    let value = number(input)?;
    skip_ws(input);
    if let Some(unit) = opt(unit_suffix).parse_next(input)? {
        let folded = value.checked_mul(unit.multiplier()).ok_or_else(|| ErrMode::Cut(ContextError::new()))?;
        return Ok(Expr::Literal(folded));
    }
    Ok(Expr::Literal(value))
}

fn number(input: &mut &str) -> ModalResult<i128> {
    alt((hex_number, decimal_number)).parse_next(input)
}

fn hex_number(input: &mut &str) -> ModalResult<i128> {
    alt(("0x", "0X")).parse_next(input)?;
    let digits: &str = take_while(1.., |c: char| c.is_ascii_hexdigit() || c == '_').parse_next(input)?;
    if !digits.chars().any(|c| c.is_ascii_hexdigit()) {
        return Err(ErrMode::Backtrack(ContextError::new()));
    }
    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    i128::from_str_radix(&cleaned, 16).map_err(|_| ErrMode::Cut(ContextError::new()))
}

fn decimal_number(input: &mut &str) -> ModalResult<i128> {
    let digits: &str = take_while(1.., |c: char| c.is_ascii_digit() || c == '_').parse_next(input)?;
    if !digits.chars().any(|c| c.is_ascii_digit()) {
        return Err(ErrMode::Backtrack(ContextError::new()));
    }
    let cleaned: String = digits.chars().filter(|c| *c != '_').collect();
    cleaned.parse::<i128>().map_err(|_| ErrMode::Cut(ContextError::new()))
}

fn path(input: &mut &str) -> ModalResult<Path> {
    let root = identifier(input)?;
    let instance = opt(instance_suffix).parse_next(input)?;
    let segments: Vec<PathSegment> = repeat(0.., path_segment).parse_next(input)?;
    Ok(Path { root, instance, segments })
}

fn instance_suffix(input: &mut &str) -> ModalResult<u32> {
    "#".parse_next(input)?;
    let digits: &str = take_while(1.., |c: char| c.is_ascii_digit()).parse_next(input)?;
    digits.parse::<u32>().map_err(|_| ErrMode::Cut(ContextError::new()))
}

fn path_segment(input: &mut &str) -> ModalResult<PathSegment> {
    alt((dotted_segment, indexed_segment)).parse_next(input)
}

fn dotted_segment(input: &mut &str) -> ModalResult<PathSegment> {
    ".".parse_next(input)?;
    let name = identifier(input)?;
    Ok(PathSegment::Name(name))
}

fn indexed_segment(input: &mut &str) -> ModalResult<PathSegment> {
    "[".parse_next(input)?;
    let digits: &str = take_while(1.., |c: char| c.is_ascii_digit()).parse_next(input)?;
    "]".parse_next(input)?;
    let n = digits.parse::<u64>().map_err(|_| ErrMode::Cut(ContextError::new()))?;
    Ok(PathSegment::Index(n))
}

fn identifier(input: &mut &str) -> ModalResult<String> {
    let head: char = one_of(|c: char| c.is_ascii_alphabetic() || c == '_').parse_next(input)?;
    let tail: &str = take_while(0.., |c: char| c.is_ascii_alphanumeric() || c == '_').parse_next(input)?;
    let mut s = String::with_capacity(1 + tail.len());
    s.push(head);
    s.push_str(tail);
    Ok(s)
}

fn unit_suffix(input: &mut &str) -> ModalResult<Unit> {
    let token: &str = take_while(1..=4, |c: char| c.is_ascii_alphabetic()).parse_next(input)?;
    match token.to_ascii_lowercase().as_str() {
        "b" => Ok(Unit::Bytes),
        "kb" => Ok(Unit::Kilo(Base::Decimal)),
        "kib" => Ok(Unit::Kilo(Base::Binary)),
        "mb" => Ok(Unit::Mega(Base::Decimal)),
        "mib" => Ok(Unit::Mega(Base::Binary)),
        "gb" => Ok(Unit::Giga(Base::Decimal)),
        "gib" => Ok(Unit::Giga(Base::Binary)),
        "tb" => Ok(Unit::Tera(Base::Decimal)),
        "tib" => Ok(Unit::Tera(Base::Binary)),
        _ => Err(ErrMode::Backtrack(ContextError::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(v: i128) -> Expr {
        Expr::Literal(v)
    }

    fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary(op, Box::new(l), Box::new(r))
    }

    #[test]
    fn empty_input_errors() {
        assert_eq!(parse(""), Err(ParseError::Empty));
        assert_eq!(parse("   "), Err(ParseError::Empty));
    }

    #[test]
    fn decimal_literal() {
        assert_eq!(parse("42"), Ok(lit(42)));
    }

    #[test]
    fn decimal_with_underscores() {
        assert_eq!(parse("1_000_000"), Ok(lit(1_000_000)));
    }

    #[test]
    fn hex_literal() {
        assert_eq!(parse("0x100"), Ok(lit(0x100)));
        assert_eq!(parse("0XFF"), Ok(lit(0xFF)));
    }

    #[test]
    fn unit_suffix_attached() {
        assert_eq!(parse("1KiB"), Ok(lit(1024)));
        assert_eq!(parse("1 KiB"), Ok(lit(1024)));
        assert_eq!(parse("1 mib"), Ok(lit(1024 * 1024)));
        assert_eq!(parse("10 MiB"), Ok(lit(10 * 1024 * 1024)));
        assert_eq!(parse("1 KB"), Ok(lit(1000)));
    }

    #[test]
    fn precedence_left_associative() {
        // 1 + 2 * 3  = 1 + (2 * 3)
        assert_eq!(parse("1 + 2 * 3"), Ok(bin(BinOp::Add, lit(1), bin(BinOp::Mul, lit(2), lit(3)))));
    }

    #[test]
    fn parens_force_grouping() {
        // (1 + 2) * 3
        assert_eq!(parse("(1 + 2) * 3"), Ok(bin(BinOp::Mul, bin(BinOp::Add, lit(1), lit(2)), lit(3))));
    }

    #[test]
    fn implicit_close_paren() {
        // (5 * (5 + 10  -- both inner and outer close at EOF
        assert_eq!(
            parse("(5 * (5 + 10"),
            Ok(bin(BinOp::Mul, lit(5), bin(BinOp::Add, lit(5), lit(10)))),
        );
    }

    #[test]
    fn implicit_close_with_unit() {
        // 0x100 + 1MiB
        assert_eq!(parse("0x100 + 1MiB"), Ok(bin(BinOp::Add, lit(0x100), lit(1024 * 1024))));
    }

    #[test]
    fn unary_neg() {
        assert_eq!(parse("-5"), Ok(Expr::Unary(UnaryOp::Neg, Box::new(lit(5)))));
        assert_eq!(parse("- -5"), Ok(Expr::Unary(UnaryOp::Neg, Box::new(Expr::Unary(UnaryOp::Neg, Box::new(lit(5)))))));
    }

    #[test]
    fn unmatched_close_paren_is_error() {
        assert!(matches!(parse("5 + 3)"), Err(ParseError::TrailingInput { .. })));
    }

    #[test]
    fn trailing_junk_is_error() {
        assert!(matches!(parse("5 + 3 abc"), Err(ParseError::TrailingInput { .. })));
    }

    #[test]
    fn paren_unit_suffix() {
        // (1 + 1) MiB == 2 * 1MiB
        assert_eq!(
            parse("(1 + 1) MiB"),
            Ok(bin(BinOp::Mul, bin(BinOp::Add, lit(1), lit(1)), lit(1024 * 1024))),
        );
    }

    fn path_expr(p: Path) -> Expr {
        Expr::Path(p)
    }

    #[test]
    fn bare_identifier_is_path() {
        assert_eq!(
            parse("png"),
            Ok(path_expr(Path { root: "png".into(), instance: None, segments: vec![] })),
        );
    }

    #[test]
    fn dotted_path() {
        assert_eq!(
            parse("png.length"),
            Ok(path_expr(Path {
                root: "png".into(),
                instance: None,
                segments: vec![PathSegment::Name("length".into())],
            })),
        );
    }

    #[test]
    fn indexed_path() {
        assert_eq!(
            parse("png.chunks[0].length"),
            Ok(path_expr(Path {
                root: "png".into(),
                instance: None,
                segments: vec![
                    PathSegment::Name("chunks".into()),
                    PathSegment::Index(0),
                    PathSegment::Name("length".into()),
                ],
            })),
        );
    }

    #[test]
    fn instance_suffix_on_root() {
        assert_eq!(
            parse("png#2.length"),
            Ok(path_expr(Path {
                root: "png".into(),
                instance: Some(2),
                segments: vec![PathSegment::Name("length".into())],
            })),
        );
    }

    #[test]
    fn arithmetic_with_path() {
        let lhs = lit(1);
        let rhs = path_expr(Path {
            root: "png".into(),
            instance: None,
            segments: vec![PathSegment::Name("length".into())],
        });
        assert_eq!(parse("0x1 + png.length"), Ok(bin(BinOp::Add, lhs, rhs)));
    }

    #[test]
    fn function_call_offset() {
        assert_eq!(
            parse("offset(png.signature)"),
            Ok(Expr::Call(
                Function::Offset,
                Path {
                    root: "png".into(),
                    instance: None,
                    segments: vec![PathSegment::Name("signature".into())],
                },
            )),
        );
    }

    #[test]
    fn function_calls_compose_with_arithmetic() {
        // 0x1 + len(png.IDAT) -> Add(1, Call(Len, png.IDAT))
        let inner = Path {
            root: "png".into(),
            instance: None,
            segments: vec![PathSegment::Name("IDAT".into())],
        };
        assert_eq!(
            parse("0x1 + len(png.IDAT)"),
            Ok(bin(BinOp::Add, lit(1), Expr::Call(Function::Len, inner))),
        );
    }

    #[test]
    fn function_call_case_insensitive() {
        // OFFSET and Offset both pick the Offset variant.
        let arg = Path { root: "png".into(), instance: None, segments: vec![] };
        assert_eq!(parse("OFFSET(png)"), Ok(Expr::Call(Function::Offset, arg.clone())));
        assert_eq!(parse("Offset(png)"), Ok(Expr::Call(Function::Offset, arg)));
    }

    #[test]
    fn ident_named_like_function_but_no_parens_is_path() {
        // `len` standalone is a bare path -- the resolver may or
        // may not have a top-level field by that name. The parser
        // doesn't commit to the function branch until it sees `(`.
        assert_eq!(
            parse("len"),
            Ok(Expr::Path(Path { root: "len".into(), instance: None, segments: vec![] })),
        );
    }

    #[test]
    fn function_call_whitespace_inside_parens() {
        let arg = Path {
            root: "png".into(),
            instance: None,
            segments: vec![PathSegment::Name("length".into())],
        };
        assert_eq!(
            parse("offset(  png.length  )"),
            Ok(Expr::Call(Function::Offset, arg)),
        );
    }

    #[test]
    fn path_display_round_trip() {
        let p = Path {
            root: "png".into(),
            instance: Some(2),
            segments: vec![
                PathSegment::Name("chunks".into()),
                PathSegment::Index(3),
                PathSegment::Name("length".into()),
            ],
        };
        assert_eq!(p.display(), "png#2.chunks[3].length");
    }
}
