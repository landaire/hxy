//! Lexer for 010 Editor Binary Template files.
//!
//! Built on `winnow` combinators. Input is `&str` and we derive byte
//! offsets from the remaining-length delta against the original source
//! — simpler than wrapping in `LocatingSlice` and sufficient because
//! the lexer only ever consumes from the front.

use thiserror::Error;
use winnow::ModalResult;
use winnow::Parser;
use winnow::ascii::digit1;
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

/// Single token, leaving `input` advanced past it.
fn lex_one(input: &mut &str) -> ModalResult<TokenKind> {
    alt((lex_number, lex_string, lex_char, lex_ident_or_keyword, lex_punct_or_op)).parse_next(input)
}

fn skip_trivia(input: &mut &str) {
    loop {
        let before_len = input.len();
        let _ = opt(multispace1::<_, winnow::error::ContextError>).parse_next(input);
        let _ = opt(line_comment).parse_next(input);
        let _ = opt(block_comment).parse_next(input);
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
            // no more `*` — consume everything and bail. We treat an
            // unterminated block comment as "comment to end of file"
            // for robustness; the parser will flag missing `;` later.
            *input = "";
            return Ok(());
        };
        // Advance past everything up to and including the `*`.
        *input = &input[idx + 1..];
        if input.starts_with('/') {
            *input = &input[1..];
            return Ok(());
        }
    }
}

fn lex_number(input: &mut &str) -> ModalResult<TokenKind> {
    alt((hex_int, bin_int, float_or_int)).parse_next(input)
}

fn hex_int(input: &mut &str) -> ModalResult<TokenKind> {
    preceded(alt(("0x", "0X")), take_while(1.., |c: char| c.is_ascii_hexdigit()))
        .map(|s: &str| TokenKind::Int(u64::from_str_radix(s, 16).unwrap_or(0)))
        .parse_next(input)
}

fn bin_int(input: &mut &str) -> ModalResult<TokenKind> {
    preceded(alt(("0b", "0B")), take_while(1.., |c: char| c == '0' || c == '1'))
        .map(|s: &str| TokenKind::Int(u64::from_str_radix(s, 2).unwrap_or(0)))
        .parse_next(input)
}

fn float_or_int(input: &mut &str) -> ModalResult<TokenKind> {
    // integer [ . fraction ] [ e|E [+-]? digits ] [ f|F|L|l|u|U ]?
    let int_part: &str = digit1.parse_next(input)?;
    let fraction: Option<&str> = opt(preceded(".", take_while(0.., |c: char| c.is_ascii_digit()))).parse_next(input)?;
    let exponent: Option<(&str, Option<&str>, &str)> =
        opt((alt(("e", "E")), opt(alt(("+", "-"))), take_while(1.., |c: char| c.is_ascii_digit())))
            .parse_next(input)?;
    // Type suffixes are consumed but don't change the parsed value.
    let _ = opt(one_of(('f', 'F', 'l', 'L', 'u', 'U'))).parse_next(input)?;

    if fraction.is_none() && exponent.is_none() {
        let v = int_part.parse::<u64>().unwrap_or(0);
        return Ok(TokenKind::Int(v));
    }

    let mut s = int_part.to_string();
    if let Some(f) = fraction {
        s.push('.');
        s.push_str(f);
    }
    if let Some((e, sign, digits)) = exponent {
        s.push_str(e);
        if let Some(sign) = sign {
            s.push_str(sign);
        }
        s.push_str(digits);
    }
    Ok(TokenKind::Float(s.parse::<f64>().unwrap_or(0.0)))
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
        "+=".value(TokenKind::PlusEq),
        "-=".value(TokenKind::MinusEq),
        "*=".value(TokenKind::StarEq),
        "/=".value(TokenKind::SlashEq),
        "%=".value(TokenKind::PercentEq),
        "&=".value(TokenKind::AmpEq),
        "|=".value(TokenKind::PipeEq),
        "^=".value(TokenKind::CaretEq),
        "->".value(TokenKind::Arrow),
    ))
    .parse_next(input)
}

fn single_char_op(input: &mut &str) -> ModalResult<TokenKind> {
    any.verify_map(|c: char| {
        Some(match c {
            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            '{' => TokenKind::LBrace,
            '}' => TokenKind::RBrace,
            '[' => TokenKind::LBracket,
            ']' => TokenKind::RBracket,
            ';' => TokenKind::Semi,
            ',' => TokenKind::Comma,
            '.' => TokenKind::Dot,
            ':' => TokenKind::Colon,
            '?' => TokenKind::Question,
            '+' => TokenKind::Plus,
            '-' => TokenKind::Minus,
            '*' => TokenKind::Star,
            '/' => TokenKind::Slash,
            '%' => TokenKind::Percent,
            '~' => TokenKind::Tilde,
            '!' => TokenKind::Bang,
            '&' => TokenKind::Amp,
            '|' => TokenKind::Pipe,
            '^' => TokenKind::Caret,
            '<' => TokenKind::Lt,
            '>' => TokenKind::Gt,
            '=' => TokenKind::Eq,
            _ => return None,
        })
    })
    .parse_next(input)
}
