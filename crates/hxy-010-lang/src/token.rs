//! Token types emitted by the 010 Editor Binary Template lexer.
//!
//! The lexer is language-aware only in so far as it distinguishes
//! keywords from identifiers. Primitive type names (`uchar`, `int`,
//! etc.) are plain identifiers -- the parser decides at bind time
//! whether a name refers to a built-in type, a typedef, a variable,
//! or a function.

use std::fmt;

/// Byte offsets into the template source string. Inclusive start,
/// exclusive end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

/// A single token and its source location.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    // Literals
    Int(u64),
    Float(f64),
    String(String),
    Char(u32),

    // Identifiers & keywords
    Ident(String),
    Keyword(Keyword),

    // Grouping
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,

    // Separators
    Semi,
    Comma,
    Dot,
    Colon,
    Question,
    Arrow, // `->`

    // Operators -- arithmetic / unary
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusPlus,
    MinusMinus,
    Tilde, // bitwise not
    Bang,  // logical not

    // Operators -- bitwise
    Amp,
    Pipe,
    Caret,
    Shl,
    Shr,

    // Operators -- comparison / logical
    EqEq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    AmpAmp,
    PipePipe,

    // Operators -- assignment
    Eq,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AmpEq,
    PipeEq,
    CaretEq,
    ShlEq,
    ShrEq,
}

/// Reserved words. Primitive type names are *not* included -- see the
/// module comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum Keyword {
    Typedef,
    Struct,
    Enum,
    Union,
    If,
    Else,
    While,
    For,
    Do,
    Switch,
    Case,
    Default,
    Break,
    Continue,
    Return,
    Local,
    Const,
    Void,
    Sizeof,
    True,
    False,
}

impl Keyword {
    pub fn lookup(s: &str) -> Option<Self> {
        Some(match s {
            "typedef" => Self::Typedef,
            "struct" => Self::Struct,
            "enum" => Self::Enum,
            "union" => Self::Union,
            "if" => Self::If,
            "else" => Self::Else,
            "while" => Self::While,
            "for" => Self::For,
            "do" => Self::Do,
            "switch" => Self::Switch,
            "case" => Self::Case,
            "default" => Self::Default,
            "break" => Self::Break,
            "continue" => Self::Continue,
            "return" => Self::Return,
            "local" => Self::Local,
            "const" => Self::Const,
            "void" => Self::Void,
            "sizeof" => Self::Sizeof,
            "true" => Self::True,
            "false" => Self::False,
            _ => return None,
        })
    }
}
