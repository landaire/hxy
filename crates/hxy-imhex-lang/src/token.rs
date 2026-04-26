//! Token types emitted by the ImHex pattern lexer.
//!
//! ImHex's surface syntax is C-influenced but has its own keyword
//! shape: `fn` (rather than C-style return-type prefix), `using`
//! (typedef alias), `match`, `import`, `namespace`. Primitive type
//! names (`u8`, `s16`, `u32`, ...) are *not* keywords here -- they
//! lex as plain identifiers and the parser/interpreter resolve them
//! through the type table, mirroring the 010 lang convention.

use std::fmt;

/// Byte offsets into the template source string. Inclusive start,
/// exclusive end. Identical shape to `hxy_010_lang::token::Span` --
/// the two languages don't share the type so the host's two AST
/// crates stay decoupled.
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
        self.start == self.end
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    // Literals.
    Int(u128),
    Float(f64),
    String(String),
    Char(u32),

    // Identifiers + reserved words.
    Ident(String),
    Keyword(Keyword),

    // Grouping.
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    /// `[[` -- ImHex attribute opener. Lexed as a distinct token
    /// instead of two `LBracket`s so the parser can dispatch without
    /// look-ahead.
    LBracketBracket,
    /// `]]` -- ImHex attribute closer.
    RBracketBracket,

    // Separators.
    Semi,
    Comma,
    Dot,
    Colon,
    /// `::` -- namespace path separator.
    ColonColon,
    Question,

    // Arithmetic / unary.
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    PlusPlus,
    MinusMinus,
    Tilde,
    Bang,

    // Bitwise.
    Amp,
    Pipe,
    Caret,
    Shl,
    Shr,

    // Comparison / logical.
    EqEq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    AmpAmp,
    PipePipe,

    // Assignment.
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

    /// `@` -- placement operator: `Type x @ 0x100;` reads `Type`
    /// from the absolute offset on the right.
    At,
    /// `$` -- magic identifier for the current cursor offset.
    Dollar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum Keyword {
    // Type declaration.
    Struct,
    Union,
    Enum,
    Bitfield,
    Using,
    Namespace,
    Template,
    /// `fn` -- function definition keyword (distinct from C-style).
    Fn,
    /// `auto` -- type inference at declaration sites.
    Auto,
    /// `parent` -- magic identifier for the enclosing struct.
    Parent,
    /// `this` -- magic identifier for the current struct.
    This,

    // Control flow.
    If,
    Else,
    While,
    For,
    Match,
    Return,
    Break,
    Continue,

    // Modifiers / qualifiers.
    Const,
    /// Pattern-language access on imported namespaces.
    Import,
    /// Reflection: `addressof(x)` and `sizeof(x)` are keywords here
    /// (distinct from regular function calls -- they accept a type
    /// or l-value rather than an evaluated expression).
    Addressof,
    Sizeof,
    /// `typeof(x)` returns the type of an expression. Used in
    /// generic / `auto` contexts.
    Typeof,

    // Literals.
    True,
    False,
    Null,

    // Error handling (parsed but skipped at run time -- the
    // interpreter doesn't model exceptions yet).
    Try,
    Catch,

    // Miscellaneous.
    Be,
    Le,
    /// Used in `match` arm patterns: `(_): ...`.
    Underscore,
}

impl Keyword {
    pub fn lookup(s: &str) -> Option<Self> {
        Some(match s {
            "struct" => Self::Struct,
            "union" => Self::Union,
            "enum" => Self::Enum,
            "bitfield" => Self::Bitfield,
            "using" => Self::Using,
            "namespace" => Self::Namespace,
            "template" => Self::Template,
            "fn" => Self::Fn,
            "auto" => Self::Auto,
            "parent" => Self::Parent,
            "this" => Self::This,
            "if" => Self::If,
            "else" => Self::Else,
            "while" => Self::While,
            "for" => Self::For,
            "match" => Self::Match,
            "return" => Self::Return,
            "break" => Self::Break,
            "continue" => Self::Continue,
            "const" => Self::Const,
            "import" => Self::Import,
            "addressof" => Self::Addressof,
            "sizeof" => Self::Sizeof,
            "typeof" => Self::Typeof,
            "true" => Self::True,
            "false" => Self::False,
            "null" => Self::Null,
            "try" => Self::Try,
            "catch" => Self::Catch,
            "be" => Self::Be,
            "le" => Self::Le,
            "_" => Self::Underscore,
            _ => return None,
        })
    }
}
