//! Lexer for ImHex pattern language.
//!
//! Built on `winnow` combinators in the same style as the 010 lexer
//! so the two crates feel familiar side by side. Differences worth
//! flagging:
//!
//! - `[[` and `]]` lex as distinct tokens (`LBracketBracket` /
//!   `RBracketBracket`) -- ImHex's attribute syntax `[[name(...)]]`
//!   collides with `arr[[i]]` in C-style array indexing only if the
//!   parser doesn't pre-commit, so making them tokens keeps the
//!   parser dispatch trivial.
//! - `::` and `@` and `$` are first-class tokens (namespace path,
//!   placement, cursor).
//! - `#pragma` / `#include` / `#define` / `#ifdef` / `#endif` and
//!   any other `#`-prefixed line lex as trivia, same as in the 010
//!   lexer.

use thiserror::Error;
use winnow::ModalResult;
use winnow::Parser;
use winnow::ascii::multispace1;
use winnow::combinator::alt;
use winnow::combinator::delimited;
use winnow::combinator::opt;
use winnow::combinator::preceded;
use winnow::token::any;
use winnow::token::one_of;
use winnow::token::take_till;
use winnow::token::take_while;

use crate::token::Keyword;
use crate::token::Span;
use crate::token::Token;
use crate::token::TokenKind;

#[derive(Debug, Error, PartialEq)]
pub enum LexError {
    #[error("unexpected character {ch:?} at offset {offset}")]
    UnexpectedChar { ch: char, offset: usize },

    #[error("unterminated {what} starting at offset {offset}")]
    Unterminated { what: &'static str, offset: usize },
}

pub fn tokenize(source: &str) -> Result<Vec<Token>, LexError> {
    let mut input: &str = source;
    let mut tokens = Vec::new();
    loop {
        skip_trivia(&mut input);
        if input.is_empty() {
            break;
        }
        let start = source.len() - input.len();
        let kind = lex_one(&mut input).map_err(|_| {
            let offset = source.len() - input.len();
            let ch = input.chars().next().unwrap_or('?');
            LexError::UnexpectedChar { ch, offset }
        })?;
        let end = source.len() - input.len();
        tokens.push(Token { kind, span: Span::new(start, end) });
    }
    Ok(tokens)
}

fn lex_one(input: &mut &str) -> ModalResult<TokenKind> {
    alt((lex_number, lex_string, lex_char, lex_ident_or_keyword, lex_punct_or_op)).parse_next(input)
}

fn skip_trivia(input: &mut &str) {
    loop {
        let before_len = input.len();
        let _ = opt(multispace1::<_, winnow::error::ContextError>).parse_next(input);
        let _ = opt(line_comment).parse_next(input);
        let _ = opt(block_comment).parse_next(input);
        let _ = opt(preprocessor_line).parse_next(input);
        if input.len() == before_len {
            break;
        }
    }
}

fn line_comment(input: &mut &str) -> ModalResult<()> {
    ("//", take_till(0.., |c| c == '\n')).void().parse_next(input)
}

fn block_comment(input: &mut &str) -> ModalResult<()> {
    "/*".parse_next(input)?;
    loop {
        let Some(idx) = input.find('*') else {
            // Unterminated -- consume the rest. The parser will
            // complain about whatever's missing afterwards.
            *input = "";
            return Ok(());
        };
        *input = &input[idx + 1..];
        if input.starts_with('/') {
            *input = &input[1..];
            return Ok(());
        }
    }
}

/// Skip `#pragma`, `#include`, `#define`, `#ifdef`, `#endif`, etc.
/// as trivia. ImHex uses both `import std.sys;` (true language-level
/// imports) and `#include <std/sys.pat>` (preprocessor-style); only
/// the latter shape needs the lexer to skip a `#`-line, so this
/// covers both `#pragma` directives and `#include` references that
/// the host's import resolver hasn't already expanded.
///
/// Honours line continuations so multi-line `#define MACRO X \` text
/// stays attached to the same directive.
fn preprocessor_line(input: &mut &str) -> ModalResult<()> {
    if !input.starts_with('#') {
        return Err(winnow::error::ErrMode::Backtrack(winnow::error::ContextError::new()));
    }
    *input = &input[1..];
    loop {
        match input.find('\n') {
            None => {
                *input = "";
                return Ok(());
            }
            Some(idx) => {
                let line = &input[..idx];
                let continued = line.trim_end_matches(['\r']).ends_with('\\');
                *input = &input[idx + 1..];
                if !continued {
                    return Ok(());
                }
            }
        }
    }
}

fn lex_number(input: &mut &str) -> ModalResult<TokenKind> {
    alt((hex_int, bin_int, oct_int, float_or_int)).parse_next(input)
}

fn hex_int(input: &mut &str) -> ModalResult<TokenKind> {
    let s: &str =
        preceded(alt(("0x", "0X")), take_while(1.., |c: char| c.is_ascii_hexdigit() || c == '_')).parse_next(input)?;
    consume_int_suffixes(input)?;
    Ok(TokenKind::Int(parse_int(s, 16)))
}

fn bin_int(input: &mut &str) -> ModalResult<TokenKind> {
    let s: &str =
        preceded(alt(("0b", "0B")), take_while(1.., |c: char| c == '0' || c == '1' || c == '_')).parse_next(input)?;
    consume_int_suffixes(input)?;
    Ok(TokenKind::Int(parse_int(s, 2)))
}

/// `0o777`-style octal. ImHex docs list octal literals; we accept
/// both `0o`/`0O` and the C-historical leading-zero form (`0123`)
/// when the digits are all 0-7. Defaulting the leading-zero form
/// to decimal would silently miscompile a few patterns; ImHex's
/// reference says the leading-zero form is octal so we match.
fn oct_int(input: &mut &str) -> ModalResult<TokenKind> {
    let s: &str = preceded(alt(("0o", "0O")), take_while(1.., |c: char| ('0'..='7').contains(&c) || c == '_'))
        .parse_next(input)?;
    consume_int_suffixes(input)?;
    Ok(TokenKind::Int(parse_int(s, 8)))
}

fn float_or_int(input: &mut &str) -> ModalResult<TokenKind> {
    // Accept `_` as a digit-group separator anywhere in the
    // integer / fraction / exponent. We reject leading underscores
    // by requiring the first char to be a digit.
    let _first_digit: char = one_of(|c: char| c.is_ascii_digit()).parse_next(input)?;
    let rest_int: &str = take_while(0.., |c: char| c.is_ascii_digit() || c == '_').parse_next(input)?;
    let mut int_part = String::with_capacity(1 + rest_int.len());
    int_part.push(_first_digit);
    int_part.push_str(rest_int);
    let fraction: Option<&str> =
        opt(preceded(".", take_while(0.., |c: char| c.is_ascii_digit() || c == '_'))).parse_next(input)?;
    let exponent: Option<(&str, Option<&str>, &str)> =
        opt((alt(("e", "E")), opt(alt(("+", "-"))), take_while(1.., |c: char| c.is_ascii_digit() || c == '_')))
            .parse_next(input)?;
    consume_int_suffixes(input)?;

    if fraction.is_none() && exponent.is_none() {
        return Ok(TokenKind::Int(parse_int(&int_part, 10)));
    }
    let mut s = int_part.replace('_', "");
    if let Some(f) = fraction {
        s.push('.');
        s.push_str(&f.replace('_', ""));
    }
    if let Some((e, sign, digits)) = exponent {
        s.push_str(e);
        if let Some(sign) = sign {
            s.push_str(sign);
        }
        s.push_str(&digits.replace('_', ""));
    }
    // C-style `f`/`F`/`d`/`D` suffix on float literals (`100.f`,
    // `0.5d`). The width is informational; we always store as f64.
    if input.chars().next().is_some_and(|c| matches!(c, 'f' | 'F' | 'd' | 'D')) {
        any.parse_next(input)?;
    }
    Ok(TokenKind::Float(s.parse::<f64>().unwrap_or(0.0)))
}

/// Consume any trailing run of `u`/`U`/`l`/`L`. Same C-suffix
/// laxity as the 010 lexer; the actual numeric width is whatever
/// the parsed literal fits into.
fn consume_int_suffixes(input: &mut &str) -> ModalResult<()> {
    while input.chars().next().is_some_and(|c| matches!(c, 'u' | 'U' | 'l' | 'L')) {
        any.parse_next(input)?;
    }
    Ok(())
}

fn parse_int(s: &str, radix: u32) -> u128 {
    let cleaned: String = s.chars().filter(|c| *c != '_').collect();
    u128::from_str_radix(&cleaned, radix).unwrap_or(0)
}

fn lex_string(input: &mut &str) -> ModalResult<TokenKind> {
    delimited("\"", string_inner, "\"").map(TokenKind::String).parse_next(input)
}

fn string_inner(input: &mut &str) -> ModalResult<String> {
    let mut out = String::new();
    loop {
        match input.chars().next() {
            Some('"') | None => return Ok(out),
            Some('\\') => {
                any.parse_next(input)?;
                let esc = any.parse_next(input)?;
                out.push(decode_escape(esc));
            }
            Some(c) => {
                any.parse_next(input)?;
                out.push(c);
            }
        }
    }
}

fn lex_char(input: &mut &str) -> ModalResult<TokenKind> {
    delimited("'", char_inner, "'").map(TokenKind::Char).parse_next(input)
}

fn char_inner(input: &mut &str) -> ModalResult<u32> {
    match input.chars().next() {
        Some('\\') => {
            any.parse_next(input)?;
            let esc = any.parse_next(input)?;
            Ok(decode_escape(esc) as u32)
        }
        Some(c) => {
            any.parse_next(input)?;
            Ok(c as u32)
        }
        None => Err(winnow::error::ErrMode::Backtrack(winnow::error::ContextError::new())),
    }
}

fn decode_escape(c: char) -> char {
    match c {
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        '0' => '\0',
        '\\' => '\\',
        '\'' => '\'',
        '"' => '"',
        other => other,
    }
}

fn lex_ident_or_keyword(input: &mut &str) -> ModalResult<TokenKind> {
    let first = one_of(|c: char| c.is_ascii_alphabetic() || c == '_').parse_next(input)?;
    let rest: &str = take_while(0.., |c: char| c.is_ascii_alphanumeric() || c == '_').parse_next(input)?;
    let mut s = String::with_capacity(1 + rest.len());
    s.push(first);
    s.push_str(rest);
    Ok(match Keyword::lookup(&s) {
        Some(k) => TokenKind::Keyword(k),
        None => TokenKind::Ident(s),
    })
}

fn lex_punct_or_op(input: &mut &str) -> ModalResult<TokenKind> {
    alt((three_char_op, two_char_op, single_char_op)).parse_next(input)
}

fn three_char_op(input: &mut &str) -> ModalResult<TokenKind> {
    alt(("<<=".value(TokenKind::ShlEq), ">>=".value(TokenKind::ShrEq))).parse_next(input)
}

fn two_char_op(input: &mut &str) -> ModalResult<TokenKind> {
    // Split into two `alt` groups -- winnow's tuple impl tops out at
    // 21 branches per `alt` call.
    alt((
        alt((
            "==".value(TokenKind::EqEq),
            "!=".value(TokenKind::NotEq),
            "<=".value(TokenKind::LtEq),
            ">=".value(TokenKind::GtEq),
            "&&".value(TokenKind::AmpAmp),
            "||".value(TokenKind::PipePipe),
            "<<".value(TokenKind::Shl),
            ">>".value(TokenKind::Shr),
            "++".value(TokenKind::PlusPlus),
            "--".value(TokenKind::MinusMinus),
        )),
        alt((
            "+=".value(TokenKind::PlusEq),
            "-=".value(TokenKind::MinusEq),
            "*=".value(TokenKind::StarEq),
            "/=".value(TokenKind::SlashEq),
            "%=".value(TokenKind::PercentEq),
            "&=".value(TokenKind::AmpEq),
            "|=".value(TokenKind::PipeEq),
            "^=".value(TokenKind::CaretEq),
            "[[".value(TokenKind::LBracketBracket),
            "]]".value(TokenKind::RBracketBracket),
            "::".value(TokenKind::ColonColon),
        )),
    ))
    .parse_next(input)
}

fn single_char_op(input: &mut &str) -> ModalResult<TokenKind> {
    alt((
        alt((
            "(".value(TokenKind::LParen),
            ")".value(TokenKind::RParen),
            "{".value(TokenKind::LBrace),
            "}".value(TokenKind::RBrace),
            "[".value(TokenKind::LBracket),
            "]".value(TokenKind::RBracket),
            ";".value(TokenKind::Semi),
            ",".value(TokenKind::Comma),
            ".".value(TokenKind::Dot),
            ":".value(TokenKind::Colon),
            "?".value(TokenKind::Question),
            "+".value(TokenKind::Plus),
            "-".value(TokenKind::Minus),
            "*".value(TokenKind::Star),
            "/".value(TokenKind::Slash),
            "%".value(TokenKind::Percent),
            "~".value(TokenKind::Tilde),
            "!".value(TokenKind::Bang),
            "&".value(TokenKind::Amp),
            "|".value(TokenKind::Pipe),
        )),
        alt((
            "^".value(TokenKind::Caret),
            "<".value(TokenKind::Lt),
            ">".value(TokenKind::Gt),
            "=".value(TokenKind::Eq),
            "@".value(TokenKind::At),
            "$".value(TokenKind::Dollar),
        )),
    ))
    .parse_next(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(src: &str) -> Vec<TokenKind> {
        tokenize(src).expect("lex").into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn skips_pragma_and_import_directives() {
        let src = "#pragma description Foo\n#pragma endian little\n#include <std/sys.pat>\nu32 magic;";
        let toks = lex(src);
        assert!(
            matches!(toks.as_slice(), [TokenKind::Ident(t1), TokenKind::Ident(t2), TokenKind::Semi] if t1 == "u32" && t2 == "magic")
        );
    }

    #[test]
    fn lexes_double_bracket_attribute_pair() {
        let src = "[[name(\"hello\")]]";
        let toks = lex(src);
        assert!(matches!(toks.first(), Some(TokenKind::LBracketBracket)));
        assert!(matches!(toks.last(), Some(TokenKind::RBracketBracket)));
    }

    #[test]
    fn lexes_namespace_path_separator() {
        let src = "std::print";
        let toks = lex(src);
        assert!(matches!(toks.as_slice(), [
            TokenKind::Ident(a),
            TokenKind::ColonColon,
            TokenKind::Ident(b),
        ] if a == "std" && b == "print"));
    }

    #[test]
    fn lexes_placement_and_cursor_tokens() {
        let src = "u32 x @ $ + 4;";
        let toks = lex(src);
        // u32, x, @, $, +, 4, ;
        assert_eq!(toks.len(), 7);
        assert!(matches!(toks[2], TokenKind::At));
        assert!(matches!(toks[3], TokenKind::Dollar));
    }

    #[test]
    fn integer_underscore_separators() {
        let src = "1_000_000 0xFF_FF_00 0b1010_0101";
        let toks = lex(src);
        assert_eq!(toks, vec![TokenKind::Int(1_000_000), TokenKind::Int(0xFF_FF_00), TokenKind::Int(0b1010_0101),]);
    }

    #[test]
    fn keywords_and_idents() {
        let src = "fn foo() { return 1; } struct S { u8 b; } using A = u8;";
        let toks = lex(src);
        assert_eq!(toks[0], TokenKind::Keyword(Keyword::Fn));
        assert!(toks.iter().any(|t| matches!(t, TokenKind::Keyword(Keyword::Struct))));
        assert!(toks.iter().any(|t| matches!(t, TokenKind::Keyword(Keyword::Using))));
        assert!(toks.iter().any(|t| matches!(t, TokenKind::Keyword(Keyword::Return))));
    }
}
