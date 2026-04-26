//! Recursive-descent parser for the ImHex pattern language.
//!
//! Same overall structure as the 010 parser: Pratt-style expression
//! climb, straight-line statement / declaration parsing. Surface-
//! syntax differences (placement `@`, `[[attrs]]`, `fn` declarations,
//! `match` arms, namespace paths) are handled inline rather than by
//! sharing code with the 010 crate -- the languages diverge enough
//! that a shared parser would be cluttered with `if language ...`
//! switches.
//!
//! The first cut handles the Phase 1 subset: top-level
//! `struct` / `enum` / `bitfield` / `union` / `using` / `fn` /
//! `namespace` / `import` plus field declarations and the standard
//! control-flow statements. `[[attrs]]` are parsed wherever they're
//! syntactically valid (after a decl, after a struct body) and
//! threaded onto the AST node. Reflective constructs and full
//! template support land in later phases.

use thiserror::Error;

use crate::ast::ArraySize;
use crate::ast::AssignOp;
use crate::ast::Attr;
use crate::ast::Attrs;
use crate::ast::BinOp;
use crate::ast::BitfieldDecl;
use crate::ast::EnumDecl;
use crate::ast::EnumVariant;
use crate::ast::Expr;
use crate::ast::FunctionDef;
use crate::ast::MatchArm;
use crate::ast::MatchPattern;
use crate::ast::Param;
use crate::ast::Program;
use crate::ast::ReflectKind;
use crate::ast::Stmt;
use crate::ast::StructDecl;
use crate::ast::TopItem;
use crate::ast::TypeRef;
use crate::ast::UnaryOp;
use crate::token::Keyword;
use crate::token::Span;
use crate::token::Token;
use crate::token::TokenKind;

#[derive(Debug, Error, PartialEq)]
pub enum ParseError {
    #[error("unexpected end of file, expected {expected}")]
    UnexpectedEof { expected: &'static str },

    #[error("unexpected token at {span}: expected {expected}, found {found}")]
    Unexpected { expected: &'static str, found: String, span: Span },

    #[error("cannot use token as expression at {span}: {found}")]
    NotAnExpression { found: String, span: Span },
}

pub fn parse(tokens: Vec<Token>) -> Result<Program, ParseError> {
    let mut p = Parser { tokens, pos: 0 };
    let mut items = Vec::new();
    while !p.at_eof() {
        items.push(p.parse_top_item()?);
    }
    Ok(Program { items })
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

// ---------------------------------------------------------------------------
// Cursor helpers.
// ---------------------------------------------------------------------------

impl Parser {
    fn at_eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.peek().map(|t| &t.kind)
    }

    fn peek_at(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.pos + offset)
    }

    fn bump(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned()?;
        self.pos += 1;
        Some(t)
    }

    fn last_span(&self) -> Span {
        // Used to close a multi-token construct when the trailing
        // token has already been consumed.
        self.tokens.get(self.pos.saturating_sub(1)).map(|t| t.span).unwrap_or(Span::new(0, 0))
    }

    fn eat_kind(&mut self, want: &TokenKind) -> bool {
        if self.peek_kind() == Some(want) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_keyword(&mut self, kw: Keyword) -> bool {
        matches!(self.peek_kind(), Some(TokenKind::Keyword(k)) if *k == kw) && {
            self.bump();
            true
        }
    }

    fn expect_kind(&mut self, want: &TokenKind, expected_msg: &'static str) -> Result<Token, ParseError> {
        match self.peek() {
            Some(t) if &t.kind == want => Ok(self.bump().unwrap()),
            Some(t) => {
                Err(ParseError::Unexpected { expected: expected_msg, found: format!("{:?}", t.kind), span: t.span })
            }
            None => Err(ParseError::UnexpectedEof { expected: expected_msg }),
        }
    }

    /// Consume a single `]`. The lexer eagerly merges `]]` into one
    /// token to flag attribute closers, so when an inner `[idx]]`
    /// would close both an index and an attribute (or two arrays in
    /// a row), we split the merged token here so each consumer gets
    /// its own `]`.
    fn expect_rbracket(&mut self) -> Result<Token, ParseError> {
        if matches!(self.peek_kind(), Some(TokenKind::RBracketBracket)) {
            let merged = self.tokens[self.pos].clone();
            let mid = merged.span.start + 1;
            self.tokens[self.pos] =
                Token { kind: TokenKind::RBracket, span: Span::new(merged.span.start, mid) };
            self.tokens
                .insert(self.pos + 1, Token { kind: TokenKind::RBracket, span: Span::new(mid, merged.span.end) });
        }
        self.expect_kind(&TokenKind::RBracket, "]")
    }

    /// Consume `]]`. Mirror of [`Self::expect_rbracket`] for the
    /// other direction: when the source has `] ]` (whitespace-
    /// separated), the lexer emits two `RBracket`s and we accept
    /// the pair as an attribute closer.
    fn expect_rbracket_bracket(&mut self) -> Result<Token, ParseError> {
        if matches!(self.peek_kind(), Some(TokenKind::RBracketBracket)) {
            return self.expect_kind(&TokenKind::RBracketBracket, "]]");
        }
        if matches!(self.peek_kind(), Some(TokenKind::RBracket))
            && matches!(self.peek_at(1).map(|t| &t.kind), Some(TokenKind::RBracket))
        {
            let first = self.bump().unwrap();
            let second = self.bump().unwrap();
            return Ok(Token {
                kind: TokenKind::RBracketBracket,
                span: Span::new(first.span.start, second.span.end),
            });
        }
        self.expect_kind(&TokenKind::RBracketBracket, "]]")
    }

    fn expect_ident(&mut self) -> Result<(String, Span), ParseError> {
        match self.peek() {
            Some(Token { kind: TokenKind::Ident(_), .. }) => {
                let tok = self.bump().unwrap();
                let span = tok.span;
                let TokenKind::Ident(name) = tok.kind else { unreachable!() };
                Ok((name, span))
            }
            Some(t) => {
                Err(ParseError::Unexpected { expected: "identifier", found: format!("{:?}", t.kind), span: t.span })
            }
            None => Err(ParseError::UnexpectedEof { expected: "identifier" }),
        }
    }

    /// Variant of [`Self::expect_ident`] that also accepts `auto`
    /// in type position. ImHex lets `auto` stand in for an inferred
    /// type at decl sites; we treat it as a regular ident name here
    /// and let the interpreter decide what to do with it.
    fn expect_type_ident_or_auto(&mut self) -> Result<(String, Span), ParseError> {
        self.expect_soft_ident()
    }

    /// Permissive identifier: also accepts keywords that double as
    /// regular names in some positions (`auto`, `parent`, `this`,
    /// `be`, `le`). Corpus patterns use these as field / param /
    /// member names; treating them as keywords-only would reject a
    /// large slice of the input.
    fn expect_soft_ident(&mut self) -> Result<(String, Span), ParseError> {
        if let Some(tok) = self.peek().cloned()
            && let Some(name) = soft_ident_name(&tok.kind)
        {
            self.bump();
            return Ok((name.to_owned(), tok.span));
        }
        self.expect_ident()
    }
}

/// True when the token at `parser.pos + offset` is something the
/// soft-ident path would accept (an ident or one of the keywords
/// soft-ident treats as a name).
fn is_soft_ident_at(parser: &Parser, offset: usize) -> bool {
    match parser.peek_at(offset).map(|t| &t.kind) {
        Some(TokenKind::Ident(_)) => true,
        Some(k) => soft_ident_name(k).is_some(),
        None => false,
    }
}

/// Keywords that may also appear in identifier position. Returning
/// `Some(name)` lets the soft-ident path accept the token.
fn soft_ident_name(kind: &TokenKind) -> Option<&'static str> {
    Some(match kind {
        TokenKind::Ident(_) => return None, // already an ident; let the regular path handle it
        TokenKind::Keyword(Keyword::Auto) => "auto",
        TokenKind::Keyword(Keyword::Parent) => "parent",
        TokenKind::Keyword(Keyword::This) => "this",
        TokenKind::Keyword(Keyword::Be) => "be",
        TokenKind::Keyword(Keyword::Le) => "le",
        // `template` is a normal field name in a few corpus templates
        // (e.g. `DialogItemTemplate template [[inline]];`). The
        // `template<...>` prefix form uses it in keyword position only
        // at the start of a decl, where this fallback isn't reached.
        TokenKind::Keyword(Keyword::Template) => "template",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Top level.
// ---------------------------------------------------------------------------

impl Parser {
    fn parse_top_item(&mut self) -> Result<TopItem, ParseError> {
        if matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Fn))) {
            return Ok(TopItem::Function(self.parse_function_def()?));
        }
        Ok(TopItem::Stmt(self.parse_stmt()?))
    }

    fn parse_function_def(&mut self) -> Result<FunctionDef, ParseError> {
        let kw = self.bump().unwrap(); // `fn`
        let (name, _) = self.expect_ident()?;
        self.expect_kind(&TokenKind::LParen, "(")?;
        let mut params = Vec::new();
        if !matches!(self.peek_kind(), Some(TokenKind::RParen)) {
            loop {
                params.push(self.parse_param()?);
                if !self.eat_kind(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect_kind(&TokenKind::RParen, ")")?;
        let body_block = self.parse_block()?;
        let Stmt::Block { stmts, .. } = body_block else { unreachable!() };
        Ok(FunctionDef {
            name,
            params,
            return_type: None,
            body: stmts,
            span: Span::new(kw.span.start, self.last_span().end),
        })
    }

    fn parse_param(&mut self) -> Result<Param, ParseError> {
        // ImHex params look like `Type name`, `auto name`, or
        // `ref Type name`. The `ref` keyword marks a by-reference
        // parameter -- semantically meaningful (the function can
        // mutate the bound value), but for Phase 1/2 we just drop
        // the modifier and bind the value normally.
        if let Some(TokenKind::Ident(name)) = self.peek_kind()
            && name == "ref"
        {
            self.bump();
        }
        let ty = self.parse_type_ref()?;
        // Variadic marker `auto ... args`: the type was `auto`, the
        // marker is three dots (lexed as three Dot tokens). We
        // discard the marker; runtime call dispatch will see the
        // remaining args as a normal positional list (no spread).
        if matches!(self.peek_kind(), Some(TokenKind::Dot))
            && matches!(self.peek_at(1).map(|t| &t.kind), Some(TokenKind::Dot))
            && matches!(self.peek_at(2).map(|t| &t.kind), Some(TokenKind::Dot))
        {
            self.bump();
            self.bump();
            self.bump();
        }
        let (name, name_span) = self.expect_soft_ident()?;
        // Default argument: `fn f(str x = \"\")`. Parse and drop --
        // the runtime doesn't honour defaults yet, but rejecting the
        // syntax loses parser coverage.
        if self.eat_kind(&TokenKind::Eq) {
            let _ = self.parse_expr_bp(ATTR_VALUE_BP)?;
        }
        let span = Span::new(ty.span.start, name_span.end);
        Ok(Param { ty: Some(ty), name, span })
    }
}

// ---------------------------------------------------------------------------
// Statements + declarations.
// ---------------------------------------------------------------------------

impl Parser {
    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        let Some(tok) = self.peek().cloned() else {
            return Err(ParseError::UnexpectedEof { expected: "statement" });
        };
        match tok.kind {
            // Empty-statement filler: a stray `;` at any level. ImHex
            // patterns drop them for cosmetic reasons (after an `if`
            // body, between decls). Treat as a zero-stmt block so the
            // surrounding parser path stays uniform.
            TokenKind::Semi => {
                let t = self.bump().unwrap();
                Ok(Stmt::Block { stmts: Vec::new(), span: t.span })
            }
            TokenKind::LBrace => self.parse_block(),
            // `fn` declarations are valid at any scope, not just at
            // the top level (corpus patterns put helpers inside
            // `namespace` blocks).
            TokenKind::Keyword(Keyword::Fn) => Ok(Stmt::FnDecl(self.parse_function_def()?)),
            TokenKind::Keyword(Keyword::Using) => self.parse_using(),
            TokenKind::Keyword(Keyword::Struct) => self.parse_struct_decl(false),
            TokenKind::Keyword(Keyword::Union) => self.parse_struct_decl(true),
            TokenKind::Keyword(Keyword::Enum) => self.parse_enum_decl(),
            TokenKind::Keyword(Keyword::Bitfield) => self.parse_bitfield_decl(),
            TokenKind::Keyword(Keyword::Namespace) => self.parse_namespace(),
            TokenKind::Keyword(Keyword::Import) => self.parse_import(),
            TokenKind::Keyword(Keyword::If) => self.parse_if(),
            TokenKind::Keyword(Keyword::While) => self.parse_while(),
            TokenKind::Keyword(Keyword::For) => self.parse_for(),
            TokenKind::Keyword(Keyword::Match) => self.parse_match(),
            TokenKind::Keyword(Keyword::Try) => self.parse_try(),
            TokenKind::Keyword(Keyword::Return) => self.parse_return(),
            TokenKind::Keyword(Keyword::Break) => {
                let t = self.bump().unwrap();
                self.expect_kind(&TokenKind::Semi, ";")?;
                Ok(Stmt::Break { span: t.span })
            }
            TokenKind::Keyword(Keyword::Continue) => {
                let t = self.bump().unwrap();
                self.expect_kind(&TokenKind::Semi, ";")?;
                Ok(Stmt::Continue { span: t.span })
            }
            // `const Type name = ...;` -- prefix on a regular field
            // decl.
            TokenKind::Keyword(Keyword::Const) => self.parse_field_decl(true),
            // Field-level endian prefix or `auto` type: these are
            // unambiguously the start of a field decl.
            TokenKind::Keyword(Keyword::Be) | TokenKind::Keyword(Keyword::Le) | TokenKind::Keyword(Keyword::Auto) => {
                self.parse_field_decl(false)
            }
            // `template<T> struct ... ;` / `template<T> fn ...`.
            // Skip the parameter list and re-dispatch on the body.
            TokenKind::Keyword(Keyword::Template) => self.parse_templated_decl(),
            // Top-level `[[attr]];` -- ImHex's pragma-style
            // attributes (e.g. `[[hex::spec(...)]]`). Parse and
            // discard for now; renderer integration lands later.
            TokenKind::LBracketBracket => {
                let _ = self.parse_optional_attrs()?;
                let end = self.expect_kind(&TokenKind::Semi, ";")?.span.end;
                Ok(Stmt::Block { stmts: Vec::new(), span: Span::new(tok.span.start, end) })
            }
            // Anything else: try field declaration, fall back to an
            // expression statement on failure.
            _ => self.parse_decl_or_expr_stmt(),
        }
    }

    fn parse_block(&mut self) -> Result<Stmt, ParseError> {
        let open = self.expect_kind(&TokenKind::LBrace, "{")?;
        let mut stmts = Vec::new();
        while !matches!(self.peek_kind(), Some(TokenKind::RBrace)) {
            if self.at_eof() {
                return Err(ParseError::UnexpectedEof { expected: "}" });
            }
            stmts.push(self.parse_stmt()?);
        }
        let close = self.expect_kind(&TokenKind::RBrace, "}")?;
        Ok(Stmt::Block { stmts, span: Span::new(open.span.start, close.span.end) })
    }

    /// Parse a `<T, auto N, ...>` template parameter list if one
    /// starts at the cursor. Each entry yields one binding name --
    /// optional leading kind keywords (`auto`) and type prefixes
    /// (`u8 Count`) are dropped because the interpreter doesn't
    /// enforce param kinds. Returns an empty `Vec` when no `<`
    /// follows.
    fn parse_optional_template_params(&mut self) -> Result<Vec<String>, ParseError> {
        if !self.eat_kind(&TokenKind::Lt) {
            return Ok(Vec::new());
        }
        let mut params = Vec::new();
        if !matches!(self.peek_kind(), Some(TokenKind::Gt)) {
            loop {
                // Optional value-vs-type marker.
                let _ = self.eat_keyword(Keyword::Auto);
                // First soft-ident is either the param name (no
                // type prefix) or a type ref. If another ident
                // follows, the first was the type prefix.
                let (first, _) = self.expect_soft_ident()?;
                let name = if matches!(self.peek_kind(), Some(TokenKind::Ident(_))) {
                    self.expect_ident()?.0
                } else {
                    first
                };
                params.push(name);
                if !self.eat_kind(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect_kind(&TokenKind::Gt, ">")?;
        Ok(params)
    }

    /// `template<T, U, ...> ...` -- parameterised type / function
    /// declaration. The body re-dispatches on `struct` / `union` /
    /// `bitfield` / `fn`. Template params captured here are merged
    /// onto the produced AST node so the interpreter can bind them
    /// to template args at read time.
    fn parse_templated_decl(&mut self) -> Result<Stmt, ParseError> {
        self.bump(); // `template`
        let params = self.parse_optional_template_params()?;
        let inner = self.parse_stmt()?;
        // Merge captured params onto the produced decl. The body's
        // own `<...>` slot might also have populated params; we
        // prepend the `template<...>` form so it wins on conflict.
        Ok(merge_template_params(inner, params))
    }

    fn parse_using(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `using`
        let (new_name, name_span) = self.expect_ident()?;
        // Parameterised alias: `using BlockArray<T> = T[count];`.
        // Drop the template parameter list and treat the alias as
        // a regular non-parametric alias for now.
        let _ = self.parse_optional_template_params()?;
        // `using Foo;` is the corpus's "forward-declare" shape --
        // valid syntax with no aliased target. We model it as an
        // alias to itself so the rest of the pipeline doesn't have
        // to special-case the missing source.
        if matches!(self.peek_kind(), Some(TokenKind::Semi)) {
            let end = self.bump().unwrap().span.end;
            let self_ref = TypeRef { path: vec![new_name.clone()], template_args: Vec::new(), span: name_span };
            return Ok(Stmt::UsingAlias { new_name, source: self_ref, span: Span::new(kw.span.start, end) });
        }
        self.expect_kind(&TokenKind::Eq, "=")?;
        let source = self.parse_type_ref()?;
        // `using Alias = T [[attrs]];` is common in `std/` patterns
        // -- the alias gets a default `[[format]]` or similar.
        // Accept and discard for now; renderer integration lands
        // when attributes are wired through.
        let _ = self.parse_optional_attrs()?;
        let end = self.expect_kind(&TokenKind::Semi, ";")?.span.end;
        Ok(Stmt::UsingAlias { new_name, source, span: Span::new(kw.span.start, end) })
    }

    fn parse_struct_decl(&mut self, is_union: bool) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap();
        let (name, _) = self.expect_ident()?;
        // Template parameters declared on the type itself: `struct
        // Foo<T, auto N>`. The leading `template<...>` form (handled
        // by [`Self::parse_templated_decl`]) routes through this
        // path too, so we accept `<...>` either side of the body.
        let template_params = self.parse_optional_template_params()?;
        // Struct inheritance: `struct B : A { ... }`. We capture
        // the first parent type for body composition; additional
        // parents (multi-inheritance) are accepted but only the
        // first is composed at runtime.
        let parent = if self.eat_kind(&TokenKind::Colon) {
            let p = self.parse_type_ref()?;
            while self.eat_kind(&TokenKind::Comma) {
                let _next = self.parse_type_ref()?;
            }
            Some(p)
        } else {
            None
        };
        let body_block = self.parse_block()?;
        let Stmt::Block { stmts, .. } = body_block else { unreachable!() };
        let attrs = self.parse_optional_attrs()?;
        let end = self.expect_kind(&TokenKind::Semi, ";")?.span.end;
        Ok(Stmt::StructDecl(StructDecl {
            name,
            template_params,
            parent,
            body: stmts,
            attrs,
            is_union,
            span: Span::new(kw.span.start, end),
        }))
    }

    fn parse_enum_decl(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `enum`
        let (name, _) = self.expect_ident()?;
        let template_params = self.parse_optional_template_params()?;
        // Required backing type: `enum Name : u8 { ... }`. The
        // colon doubles up with bitfield-width syntax in field
        // decls, but at the enum-decl site there's no ambiguity.
        self.expect_kind(&TokenKind::Colon, ":")?;
        let backing = self.parse_type_ref()?;
        self.expect_kind(&TokenKind::LBrace, "{")?;
        let mut variants = Vec::new();
        if !matches!(self.peek_kind(), Some(TokenKind::RBrace)) {
            loop {
                variants.push(self.parse_enum_variant()?);
                if !self.eat_kind(&TokenKind::Comma) {
                    break;
                }
                // Trailing comma OK.
                if matches!(self.peek_kind(), Some(TokenKind::RBrace)) {
                    break;
                }
            }
        }
        self.expect_kind(&TokenKind::RBrace, "}")?;
        let attrs = self.parse_optional_attrs()?;
        let end = self.expect_kind(&TokenKind::Semi, ";")?.span.end;
        Ok(Stmt::EnumDecl(EnumDecl {
            name,
            template_params,
            backing,
            variants,
            attrs,
            span: Span::new(kw.span.start, end),
        }))
    }

    fn parse_enum_variant(&mut self) -> Result<EnumVariant, ParseError> {
        let (name, name_span) = self.expect_ident()?;
        let value = if self.eat_kind(&TokenKind::Eq) { Some(self.parse_expr()?) } else { None };
        // Optional `... hi` (or `.. hi`) tail for range variants:
        // `Reserved = 0 ... 7,` matches values 0 through 7 inclusive.
        let value_end = if value.is_some() && self.eat_kind(&TokenKind::Dot) {
            self.expect_kind(&TokenKind::Dot, "..")?;
            let _ = self.eat_kind(&TokenKind::Dot);
            Some(self.parse_expr()?)
        } else {
            None
        };
        let end = value_end
            .as_ref()
            .map(|e| e.span().end)
            .or_else(|| value.as_ref().map(|e| e.span().end))
            .unwrap_or(name_span.end);
        Ok(EnumVariant { name, value, value_end, span: Span::new(name_span.start, end) })
    }

    fn parse_bitfield_decl(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `bitfield`
        let (name, _) = self.expect_ident()?;
        let template_params = self.parse_optional_template_params()?;
        self.expect_kind(&TokenKind::LBrace, "{")?;
        let mut body = Vec::new();
        while !matches!(self.peek_kind(), Some(TokenKind::RBrace) | None) {
            body.push(self.parse_bitfield_entry()?);
        }
        self.expect_kind(&TokenKind::RBrace, "}")?;
        let attrs = self.parse_optional_attrs()?;
        let end = self.expect_kind(&TokenKind::Semi, ";")?.span.end;
        Ok(Stmt::BitfieldDecl(BitfieldDecl {
            name,
            template_params,
            body,
            attrs,
            span: Span::new(kw.span.start, end),
        }))
    }

    /// One entry inside a bitfield body. A bit-slice field
    /// (`[Type] name : width;`) is the common shape, but the body
    /// can also contain `if`/`match` branches, `Type name = expr;`
    /// computed values, and byte-aligned regular reads -- so we
    /// fall back to [`Self::parse_stmt`] when the next tokens don't
    /// look like a bit-slice.
    fn parse_bitfield_entry(&mut self) -> Result<Stmt, ParseError> {
        if self.looks_like_bitfield_field() {
            return self.parse_bitfield_field();
        }
        // Drop a stray `;`.
        if self.eat_kind(&TokenKind::Semi) {
            return Ok(Stmt::Block { stmts: Vec::new(), span: self.last_span() });
        }
        self.parse_stmt()
    }

    /// True when the upcoming tokens look like a `[Type] name : width`
    /// bit-slice field. We need to disambiguate this from regular
    /// `Type name = expr;` derived values and `Type name;` byte reads.
    fn looks_like_bitfield_field(&self) -> bool {
        // Walk past an optional be/le prefix and one or two idents
        // (with optional `::` segments and `<...>` template args)
        // until we either hit `:` (bit-slice) or `=` / `;` / `[` /
        // `@` / `*` (regular field decl).
        let mut i = 0;
        if matches!(
            self.peek_at(i).map(|t| &t.kind),
            Some(TokenKind::Keyword(Keyword::Be) | TokenKind::Keyword(Keyword::Le))
        ) {
            i += 1;
        }
        // First ident.
        if !is_soft_ident_at(self, i) {
            return false;
        }
        i += 1;
        // Optional `::Ident` repeats.
        while matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::ColonColon)) {
            i += 1;
            if !is_soft_ident_at(self, i) {
                return false;
            }
            i += 1;
        }
        // Optional `<...>` template args -- skip with depth tracking.
        if matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::Lt)) {
            let mut depth = 1usize;
            i += 1;
            let limit = i + 64;
            while i < limit && depth > 0 {
                match self.peek_at(i).map(|t| &t.kind) {
                    Some(TokenKind::Lt) => depth += 1,
                    Some(TokenKind::Gt) => depth -= 1,
                    None => return false,
                    _ => {}
                }
                i += 1;
            }
            if depth != 0 {
                return false;
            }
        }
        // After the type (or type-or-name), what follows decides:
        //   `:` -- bit-slice (no type prefix).
        //   `Ident :` -- bit-slice with type prefix.
        //   anything else (`=`, `;`, `[`, `*`, `@`) -- regular decl.
        match self.peek_at(i).map(|t| &t.kind) {
            Some(TokenKind::Colon) => true,
            Some(k) if soft_ident_name(k).is_some() || matches!(k, TokenKind::Ident(_)) => {
                matches!(self.peek_at(i + 1).map(|t| &t.kind), Some(TokenKind::Colon))
            }
            _ => false,
        }
    }

    fn parse_bitfield_field(&mut self) -> Result<Stmt, ParseError> {
        // ImHex bitfields accept either `name : width` or
        // `Type name : width`. We parse a TypeRef speculatively: if
        // the next token is `:`, the "type" was actually the name;
        // else it's a real prefix and we read the name after it.
        let head = self.parse_type_ref()?;
        let (ty, name, name_span) = if matches!(self.peek_kind(), Some(TokenKind::Colon)) {
            let name = head.path.last().cloned().unwrap_or_default();
            (None, name, head.span)
        } else {
            let (n, s) = self.expect_soft_ident()?;
            (Some(head), n, s)
        };
        self.expect_kind(&TokenKind::Colon, ":")?;
        let width = self.parse_expr()?;
        let attrs = self.parse_optional_attrs()?;
        let end = self.expect_kind(&TokenKind::Semi, ";")?.span.end;
        let start = ty.as_ref().map(|t| t.span.start).unwrap_or(name_span.start);
        Ok(Stmt::BitfieldField { ty, name, width, attrs, span: Span::new(start, end) })
    }

    fn parse_namespace(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `namespace`
        // `namespace auto Name { ... }` -- the leading `auto` makes
        // members visible without an explicit `use` / `import`.
        let is_auto = matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Auto))) && {
            self.bump();
            true
        };
        let mut path = Vec::new();
        let (first, _) = self.expect_ident()?;
        path.push(first);
        while self.eat_kind(&TokenKind::ColonColon) {
            let (seg, _) = self.expect_ident()?;
            path.push(seg);
        }
        let body_block = self.parse_block()?;
        let Stmt::Block { stmts, .. } = body_block else { unreachable!() };
        Ok(Stmt::Namespace { path, is_auto, body: stmts, span: Span::new(kw.span.start, self.last_span().end) })
    }

    fn parse_import(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `import`
        // Selective-import shape: `import * from foo.bar as Alias;`
        // ImHex spells re-exports this way; the parser doesn't need
        // to model the alias so we drain to the closing `;` and
        // emit a no-op import statement.
        if matches!(self.peek_kind(), Some(TokenKind::Star)) {
            while let Some(tok) = self.bump() {
                if matches!(tok.kind, TokenKind::Semi) {
                    return Ok(Stmt::Import { path: Vec::new(), span: Span::new(kw.span.start, tok.span.end) });
                }
            }
            return Err(ParseError::UnexpectedEof { expected: ";" });
        }
        // Either `std.io` (dotted) or `std::io` (path-style); accept
        // both because corpus patterns mix them.
        let mut path = Vec::new();
        let (first, _) = self.expect_ident()?;
        path.push(first);
        loop {
            let separator = self.eat_kind(&TokenKind::Dot) || self.eat_kind(&TokenKind::ColonColon);
            if !separator {
                break;
            }
            let (seg, _) = self.expect_ident()?;
            path.push(seg);
        }
        // Trailing `as Alias` -- consume + drop.
        if let Some(TokenKind::Ident(s)) = self.peek_kind()
            && s == "as"
        {
            self.bump();
            let _ = self.expect_ident()?;
        }
        let end = self.expect_kind(&TokenKind::Semi, ";")?.span.end;
        Ok(Stmt::Import { path, span: Span::new(kw.span.start, end) })
    }

    fn parse_if(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `if`
        self.expect_kind(&TokenKind::LParen, "(")?;
        let cond = self.parse_expr()?;
        self.expect_kind(&TokenKind::RParen, ")")?;
        let then_branch = Box::new(self.parse_stmt()?);
        let else_branch = if self.eat_keyword(Keyword::Else) { Some(Box::new(self.parse_stmt()?)) } else { None };
        let end = else_branch.as_ref().map(|s| stmt_span(s).end).unwrap_or_else(|| stmt_span(&then_branch).end);
        Ok(Stmt::If { cond, then_branch, else_branch, span: Span::new(kw.span.start, end) })
    }

    fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `while`
        self.expect_kind(&TokenKind::LParen, "(")?;
        let cond = self.parse_expr()?;
        self.expect_kind(&TokenKind::RParen, ")")?;
        let body = Box::new(self.parse_stmt()?);
        Ok(Stmt::While { cond, body, span: Span::new(kw.span.start, self.last_span().end) })
    }

    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `for`
        self.expect_kind(&TokenKind::LParen, "(")?;
        // ImHex's for-loop uses commas as the init/cond/step
        // separator (C-style uses semicolons). Accept either:
        // peek for a trailing `;` after the init statement, and
        // fall back to comma if not present.
        let init = if self.eat_kind(&TokenKind::Semi) || self.eat_kind(&TokenKind::Comma) {
            None
        } else {
            Some(Box::new(self.parse_stmt_no_semi()?))
        };
        // The init statement consumed its own terminator if it was
        // a regular `parse_stmt`; if it was an init-expression we
        // need to eat the separator ourselves.
        let _ = self.eat_kind(&TokenKind::Comma);
        let cond = if matches!(self.peek_kind(), Some(TokenKind::Semi) | Some(TokenKind::Comma)) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        // Comma or semicolon after the condition.
        let _ = self.eat_kind(&TokenKind::Semi) || self.eat_kind(&TokenKind::Comma);
        let step = if matches!(self.peek_kind(), Some(TokenKind::RParen)) { None } else { Some(self.parse_expr()?) };
        self.expect_kind(&TokenKind::RParen, ")")?;
        let body = Box::new(self.parse_stmt()?);
        Ok(Stmt::For { init, cond, step, body, span: Span::new(kw.span.start, self.last_span().end) })
    }

    /// Like [`Self::parse_stmt`], but without consuming a trailing
    /// `;`. Used by `for` init clauses since the surrounding
    /// `for(...)` already enforces the separator.
    fn parse_stmt_no_semi(&mut self) -> Result<Stmt, ParseError> {
        // For Phase 1 we route field decls through `parse_field_decl`
        // which always consumes its own `;`. Init clauses that use
        // a typed decl carry the `;` inside them, and we'll silently
        // accept it; non-decl forms (assignment expressions) need
        // the comma-as-separator path. Try as expression first.
        if matches!(
            self.peek_kind(),
            Some(TokenKind::Keyword(Keyword::Const))
                | Some(TokenKind::Keyword(Keyword::Auto))
                | Some(TokenKind::Keyword(Keyword::Be))
                | Some(TokenKind::Keyword(Keyword::Le))
        ) || self.looks_like_field_decl()
        {
            return self.parse_field_decl_no_semi();
        }
        let expr = self.parse_expr()?;
        let span = expr.span();
        Ok(Stmt::Expr { expr, span })
    }

    /// Variant of [`Self::parse_field_decl`] that doesn't consume
    /// the trailing `;` -- used by for-loop init clauses where the
    /// separator is owned by the loop syntax.
    fn parse_field_decl_no_semi(&mut self) -> Result<Stmt, ParseError> {
        let start = self.peek().map(|t| t.span.start).unwrap_or(0);
        let is_const = self.eat_keyword(Keyword::Const);
        let ty = self.parse_type_ref()?;
        self.parse_one_declarator(start, is_const, &ty)
    }

    fn parse_match(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `match`
        self.expect_kind(&TokenKind::LParen, "(")?;
        let scrutinee = self.parse_expr()?;
        // Tuple-style scrutinee `match (a, b) { ... }`: the first
        // expression becomes the (single) scrutinee, additional
        // expressions are accepted for round-tripping but currently
        // dropped. Full multi-value matching arrives in a later phase.
        while self.eat_kind(&TokenKind::Comma) {
            let _ = self.parse_expr()?;
        }
        self.expect_kind(&TokenKind::RParen, ")")?;
        self.expect_kind(&TokenKind::LBrace, "{")?;
        let mut arms = Vec::new();
        while !matches!(self.peek_kind(), Some(TokenKind::RBrace) | None) {
            arms.push(self.parse_match_arm()?);
        }
        self.expect_kind(&TokenKind::RBrace, "}")?;
        Ok(Stmt::Match { scrutinee, arms, span: Span::new(kw.span.start, self.last_span().end) })
    }

    fn parse_match_arm(&mut self) -> Result<MatchArm, ParseError> {
        let start = self.peek().map(|t| t.span.start).unwrap_or(0);
        // Pattern groups are wrapped in parens: `(1)`, `(1, 2, 3)`,
        // `(0 ... 5)`, `(_)`. Multiple comma-separated patterns
        // share a single arm body.
        self.expect_kind(&TokenKind::LParen, "(")?;
        let mut patterns = Vec::new();
        loop {
            patterns.push(self.parse_match_pattern()?);
            // Patterns inside one arm group can be separated by `,`,
            // `|` (alternation, C-style), or `||` (alternation,
            // logical-or spelling). Corpus templates use all three
            // shorthand forms; we treat them equivalently.
            if !self.eat_kind(&TokenKind::Comma)
                && !self.eat_kind(&TokenKind::Pipe)
                && !self.eat_kind(&TokenKind::PipePipe)
            {
                break;
            }
        }
        self.expect_kind(&TokenKind::RParen, ")")?;
        self.expect_kind(&TokenKind::Colon, ":")?;
        let body = if matches!(self.peek_kind(), Some(TokenKind::LBrace)) {
            let block = self.parse_block()?;
            let Stmt::Block { stmts, .. } = block else { unreachable!() };
            stmts
        } else {
            vec![self.parse_stmt()?]
        };
        Ok(MatchArm { patterns, body, span: Span::new(start, self.last_span().end) })
    }

    fn parse_match_pattern(&mut self) -> Result<MatchPattern, ParseError> {
        if matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Underscore))) {
            let t = self.bump().unwrap();
            return Ok(MatchPattern::Wildcard { span: t.span });
        }
        // Patterns parse at a binding power above `|` so the
        // alternation separator `(0 ... 15 | 26 ... 53)` doesn't get
        // eaten as a bit-or in the upper bound. Anything above the
        // BitOr Pratt level (12) works; using 13 keeps `&`, `^`, and
        // arithmetic still in scope.
        let lo = self.parse_expr_bp(MATCH_PATTERN_BP)?;
        // Range: `lo .. hi` or `lo ... hi`. We accept either spelling.
        if self.eat_kind(&TokenKind::Dot) {
            self.expect_kind(&TokenKind::Dot, "..")?;
            let _ = self.eat_kind(&TokenKind::Dot); // optional 3rd dot
            let hi = self.parse_expr_bp(MATCH_PATTERN_BP)?;
            let span = Span::new(lo.span().start, hi.span().end);
            return Ok(MatchPattern::Range { lo, hi, span });
        }
        Ok(MatchPattern::Value(lo))
    }

    /// `try { body } catch { handler }`. The interpreter doesn't
    /// model exceptions yet, so we run the try body straight
    /// through and discard the catch arm.
    fn parse_try(&mut self) -> Result<Stmt, ParseError> {
        self.bump(); // `try`
        let body = self.parse_block()?;
        // Optional catch with ignored bind name + body.
        if self.eat_keyword(Keyword::Catch) {
            // Discard the parens-wrapped exception bind, if any.
            if self.eat_kind(&TokenKind::LParen) {
                while !matches!(self.peek_kind(), Some(TokenKind::RParen) | None) {
                    self.bump();
                }
                let _ = self.eat_kind(&TokenKind::RParen);
            }
            let _catch_body = self.parse_block()?;
        }
        Ok(body)
    }

    fn parse_return(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap();
        let value = if matches!(self.peek_kind(), Some(TokenKind::Semi)) { None } else { Some(self.parse_expr()?) };
        let end = self.expect_kind(&TokenKind::Semi, ";")?.span.end;
        Ok(Stmt::Return { value, span: Span::new(kw.span.start, end) })
    }

    /// Decide between a field declaration (`Type name [...] [= ...] [[attrs]];`)
    /// and an expression statement (a bare call or assignment). The
    /// discriminator is "two adjacent identifiers (or path + ident)
    /// at the top": that's a declaration. Otherwise treat the line
    /// as an expression.
    fn parse_decl_or_expr_stmt(&mut self) -> Result<Stmt, ParseError> {
        if self.looks_like_padding() {
            return self.parse_padding_decl();
        }
        // Bitfield-style `[Type] name : width;` -- legal inside
        // bitfield bodies, including their nested `if`/`match`
        // branches. Outside a bitfield context the stmt becomes a
        // no-op at run time (see `Stmt::BitfieldField` handling in
        // exec_stmt), so peeking here is safe.
        if self.looks_like_bitfield_field() {
            return self.parse_bitfield_field();
        }
        if self.looks_like_field_decl() {
            return self.parse_field_decl(false);
        }
        // `AlignTo<4>;` -- bare template-call statement. The `<>`
        // would otherwise be parsed as comparison operators and
        // fail. Detect `Ident<...>;` at expression-statement
        // position and emit a synthetic no-op call so the file
        // parses; semantics land when templates do (Phase 4).
        if self.looks_like_template_call_stmt() {
            return self.parse_template_call_stmt();
        }
        let expr = self.parse_expr()?;
        let end = self.expect_kind(&TokenKind::Semi, ";")?.span.end;
        let span = Span::new(expr.span().start, end);
        Ok(Stmt::Expr { expr, span })
    }

    fn looks_like_template_call_stmt(&self) -> bool {
        let mut i = 0;
        if !matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::Ident(_))) {
            return false;
        }
        i += 1;
        // Optional namespace prefix.
        while matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::ColonColon)) {
            i += 1;
            if !matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::Ident(_))) {
                return false;
            }
            i += 1;
        }
        if !matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::Lt)) {
            return false;
        }
        // Walk to matching `>`. Bail if we leave the template arg
        // list before we balance.
        let mut depth = 1usize;
        i += 1;
        let limit = i + 64;
        while i < limit {
            match self.peek_at(i).map(|t| &t.kind) {
                Some(TokenKind::Lt) => depth += 1,
                Some(TokenKind::Gt) => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                None | Some(TokenKind::Semi) => return false,
                _ => {}
            }
            i += 1;
        }
        if depth != 0 {
            return false;
        }
        // Optional `(args)` trailing the template instantiation.
        if matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::LParen)) {
            let mut p = 1usize;
            i += 1;
            let plimit = i + 256;
            while i < plimit {
                match self.peek_at(i).map(|t| &t.kind) {
                    Some(TokenKind::LParen) => p += 1,
                    Some(TokenKind::RParen) => {
                        p -= 1;
                        if p == 0 {
                            i += 1;
                            break;
                        }
                    }
                    None => return false,
                    _ => {}
                }
                i += 1;
            }
        }
        // Optional `[[...]]` attribute pair.
        if matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::LBracketBracket)) {
            i += 1;
            let alimit = i + 64;
            while i < alimit && !matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::RBracketBracket) | None) {
                i += 1;
            }
            if matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::RBracketBracket)) {
                i += 1;
            }
        }
        matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::Semi))
    }

    /// Consume an `Ident[<...>][(...)];` shape and emit an empty
    /// block. Renderer-side these are usually presentational
    /// helpers (e.g. `AlignTo<4>;` or a free-standing namespaced
    /// template call) so emitting nothing is the closest correct
    /// semantics until template instantiation lands.
    fn parse_template_call_stmt(&mut self) -> Result<Stmt, ParseError> {
        let start = self.peek().map(|t| t.span.start).unwrap_or(0);
        // Drain everything up to and including the trailing `;`.
        while let Some(tok) = self.bump() {
            if matches!(tok.kind, TokenKind::Semi) {
                let end = tok.span.end;
                return Ok(Stmt::Block { stmts: Vec::new(), span: Span::new(start, end) });
            }
        }
        Err(ParseError::UnexpectedEof { expected: ";" })
    }

    /// `padding[N];` is a builtin field-decl shape that skips N
    /// bytes without naming a target. Recognise it as a
    /// `padding`-typed array decl.
    fn looks_like_padding(&self) -> bool {
        matches!(self.peek_kind(), Some(TokenKind::Ident(name)) if name == "padding")
            && matches!(self.peek_at(1).map(|t| &t.kind), Some(TokenKind::LBracket))
    }

    fn parse_padding_decl(&mut self) -> Result<Stmt, ParseError> {
        let start = self.peek().map(|t| t.span.start).unwrap_or(0);
        let (name, name_span) = self.expect_ident()?; // "padding"
        let array = self.parse_optional_array_size()?;
        let attrs = self.parse_optional_attrs()?;
        let end = self.expect_kind(&TokenKind::Semi, ";")?.span.end;
        // Synthesise a `Type` ref pointing at the padding builtin.
        // The interpreter recognises `padding` and skips the
        // requested number of bytes without emitting a typed leaf.
        let ty = TypeRef { path: vec!["padding".to_owned()], template_args: Vec::new(), span: name_span };
        Ok(Stmt::FieldDecl {
            is_const: false,
            ty,
            name,
            array,
            placement: None,
            init: None,
            attrs,
            pointer_width: None,
            span: Span::new(start, end),
        })
    }

    fn looks_like_field_decl(&self) -> bool {
        // `Ident Ident` -- simple decl.
        // `Ident :: Ident ... Ident` -- namespace-prefixed decl.
        // `Ident < ... > Ident` -- template-instantiated decl
        // (e.g. `std::mem::Bytes<16> data;`). Phase 1 handles
        // `Ident < expr > Ident` as a fast path; deeper template
        // parsing arrives in a later phase.
        let mut i = 0;
        // Optional field-level endian prefix.
        if matches!(
            self.peek_at(i).map(|t| &t.kind),
            Some(TokenKind::Keyword(Keyword::Be)) | Some(TokenKind::Keyword(Keyword::Le))
        ) {
            i += 1;
        }
        if !matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::Ident(_))) {
            return false;
        }
        i += 1;
        while matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::ColonColon)) {
            i += 1;
            if !matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::Ident(_))) {
                return false;
            }
            i += 1;
        }
        // Optional template args: skip to the matching `>`. Bail
        // out if depth doesn't balance within a small budget.
        if matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::Lt)) {
            let mut depth = 1usize;
            i += 1;
            let limit = i + 64;
            while i < limit {
                match self.peek_at(i).map(|t| &t.kind) {
                    Some(TokenKind::Lt) => depth += 1,
                    Some(TokenKind::Gt) => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    None => return false,
                    _ => {}
                }
                i += 1;
            }
            if depth != 0 {
                return false;
            }
        }
        // Optional `*` for pointer types: `Foo *p : u32;`. We accept
        // it here so `looks_like_field_decl` doesn't reject the
        // shape; the parser will reject the body as unimplemented
        // until phase 4.
        if matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::Star)) {
            i += 1;
        }
        // After the type (and optional `*`), the declarator must
        // start with either an identifier (regular field name) or a
        // `[[` / `;` (anonymous field). We also accept soft-ident
        // keywords (`auto`, `parent`, `this`, `template`, ...) since
        // some corpus templates use those words as field names.
        match self.peek_at(i).map(|t| &t.kind) {
            Some(TokenKind::Ident(_)) | Some(TokenKind::LBracketBracket) | Some(TokenKind::Semi) => true,
            Some(k) => soft_ident_name(k).is_some(),
            None => false,
        }
    }

    fn parse_field_decl(&mut self, is_const_prefix: bool) -> Result<Stmt, ParseError> {
        let start = self.peek().map(|t| t.span.start).unwrap_or(0);
        if is_const_prefix {
            self.bump(); // `const`
        }
        let ty = self.parse_type_ref()?;
        let first = self.parse_one_declarator(start, is_const_prefix, &ty)?;
        if !self.eat_kind(&TokenKind::Comma) {
            let end = self.expect_kind(&TokenKind::Semi, ";")?.span.end;
            return Ok(extend_decl_span(first, end));
        }
        // Comma-separated declarators share the type prefix:
        // `u32 a, b[2], c = 1;` -- common in the corpus. Each one
        // becomes its own [`Stmt::FieldDecl`]; we bundle them in a
        // [`Stmt::Block`] so the caller still gets a single AST node.
        let mut decls = vec![first];
        loop {
            decls.push(self.parse_one_declarator(start, is_const_prefix, &ty)?);
            if !self.eat_kind(&TokenKind::Comma) {
                break;
            }
        }
        let end = self.expect_kind(&TokenKind::Semi, ";")?.span.end;
        let decls: Vec<Stmt> = decls.into_iter().map(|d| extend_decl_span(d, end)).collect();
        Ok(Stmt::Block { stmts: decls, span: Span::new(start, end) })
    }

    fn parse_one_declarator(&mut self, start: usize, is_const_prefix: bool, ty: &TypeRef) -> Result<Stmt, ParseError> {
        // Pointer-decl shape: `Type *p : u32 @ base;`. The `*`
        // marker, the `: <addr-width>` pointer-size annotation, and
        // the optional `@ pointer_base(...)` are all parsed and
        // dropped for now -- modelling pointer reads requires
        // address-resolution work that lands in phase 4.
        let is_pointer = self.eat_kind(&TokenKind::Star);
        // Anonymous fields: `Type [[attrs]];` and `Type;` apply the
        // type as an inline read with no bound name. We synthesise a
        // skipping placeholder so the interpreter still emits a node
        // but the renderer / scripts won't see a meaningful name.
        let (name, _) = if !is_pointer
            && matches!(self.peek_kind(), Some(TokenKind::LBracketBracket | TokenKind::Semi))
        {
            (String::new(), Span::new(start, start))
        } else {
            self.expect_soft_ident()?
        };
        let array = self.parse_optional_array_size()?;
        // Pointer width: `: u32` -- captured so the interpreter can
        // do an address-indirected read.
        let pointer_width = if is_pointer && self.eat_kind(&TokenKind::Colon) {
            Some(self.parse_type_ref()?)
        } else {
            None
        };
        let placement = if self.eat_kind(&TokenKind::At) { Some(self.parse_expr()?) } else { None };
        // ImHex's `in` / `out` modifiers come in two flavours:
        // (a) `bool x in;` -- declare an input/output variable.
        // (b) `Type x[N] @ offset in section;` -- bind the
        //     placement to a named memory section.
        // For Phase 2 we drop both; the optional section name
        // following `in` is consumed if present.
        if let Some(TokenKind::Ident(ident)) = self.peek_kind()
            && (ident == "in" || ident == "out")
        {
            self.bump();
            // The section after `in` / `out` can be either an
            // identifier (`in mem_section`) or an expression
            // (`in 0`, `in std::mem::sections::main`). Parse and
            // drop it for now; the renderer doesn't model named
            // memory sections yet.
            if !matches!(self.peek_kind(), Some(TokenKind::Semi | TokenKind::LBracketBracket | TokenKind::Eq)) {
                let _ = self.parse_expr()?;
            }
        }
        let init = if self.eat_kind(&TokenKind::Eq) { Some(self.parse_expr()?) } else { None };
        let attrs = self.parse_optional_attrs()?;
        Ok(Stmt::FieldDecl {
            is_const: is_const_prefix,
            ty: ty.clone(),
            name,
            array,
            placement,
            init,
            attrs,
            pointer_width,
            span: Span::new(start, self.last_span().end),
        })
    }

    fn parse_optional_array_size(&mut self) -> Result<Option<ArraySize>, ParseError> {
        if !self.eat_kind(&TokenKind::LBracket) {
            return Ok(None);
        }
        // `[]` -- open-ended.
        if matches!(self.peek_kind(), Some(TokenKind::RBracket)) {
            self.bump();
            return Ok(Some(ArraySize::Open));
        }
        // `[while(...)]` -- predicate-driven.
        if matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::While))) {
            self.bump();
            self.expect_kind(&TokenKind::LParen, "(")?;
            let cond = self.parse_expr()?;
            self.expect_kind(&TokenKind::RParen, ")")?;
            self.expect_rbracket()?;
            return Ok(Some(ArraySize::While(cond)));
        }
        let expr = self.parse_expr()?;
        self.expect_rbracket()?;
        Ok(Some(ArraySize::Fixed(expr)))
    }
}

// ---------------------------------------------------------------------------
// Type references + attribute lists.
// ---------------------------------------------------------------------------

impl Parser {
    fn parse_type_ref(&mut self) -> Result<TypeRef, ParseError> {
        // Field-level endian prefixes: `be u32 x;` / `le u32 x;`.
        // ImHex lets you override `#pragma endian` on individual
        // fields. We drop the prefix here -- the interpreter reads
        // the canonical `[[hxy_endian]]` attribute to pick a byte
        // order, and a future phase can wire the prefix through to
        // an attribute on the resulting node.
        let _ = self.eat_keyword(Keyword::Be) || self.eat_keyword(Keyword::Le);
        let (head, head_span) = self.expect_type_ident_or_auto()?;
        let mut path = vec![head];
        let mut end = head_span.end;
        while self.eat_kind(&TokenKind::ColonColon) {
            let (seg, span) = self.expect_ident()?;
            path.push(seg);
            end = span.end;
        }
        let template_args = if self.eat_kind(&TokenKind::Lt) {
            // Template instantiation: `Bytes<16>`, `Optional<u32>`.
            // Args parse at a binding power *above* the comparison
            // ops so the closing `>` doesn't get eaten as a "greater
            // than" operator. Phase 1 only supports literal /
            // identifier args; nested template instantiations will
            // need a richer expression form, but those are rare in
            // the corpus and can land later.
            let mut args = Vec::new();
            if !matches!(self.peek_kind(), Some(TokenKind::Gt)) {
                loop {
                    args.push(self.parse_expr_bp(TEMPLATE_ARG_BP)?);
                    if !self.eat_kind(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            let close = self.expect_kind(&TokenKind::Gt, ">")?;
            end = close.span.end;
            args
        } else {
            Vec::new()
        };
        Ok(TypeRef { path, template_args, span: Span::new(head_span.start, end) })
    }

    fn parse_optional_attrs(&mut self) -> Result<Attrs, ParseError> {
        if !self.eat_kind(&TokenKind::LBracketBracket) {
            return Ok(Attrs::default());
        }
        let mut attrs = Vec::new();
        if !matches!(self.peek_kind(), Some(TokenKind::RBracketBracket)) {
            loop {
                attrs.push(self.parse_attr()?);
                if !self.eat_kind(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect_rbracket_bracket()?;
        Ok(Attrs(attrs))
    }

    fn parse_attr(&mut self) -> Result<Attr, ParseError> {
        // Attribute names can be namespace-qualified:
        // `[[hex::spec("...")]]`, `[[std::name("...")]]`. Join the
        // path back into a single string so the renderer can pick
        // it up uniformly with the simple-name variants.
        let (head, head_span) = self.expect_ident()?;
        let mut name = head;
        let mut end = head_span.end;
        while self.eat_kind(&TokenKind::ColonColon) {
            let (seg, span) = self.expect_ident()?;
            name.push_str("::");
            name.push_str(&seg);
            end = span.end;
        }
        let name_span = Span::new(head_span.start, end);
        let mut args = Vec::new();
        if self.eat_kind(&TokenKind::LParen) {
            if !matches!(self.peek_kind(), Some(TokenKind::RParen)) {
                loop {
                    args.push(self.parse_expr_bp(1)?);
                    if !self.eat_kind(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            end = self.expect_kind(&TokenKind::RParen, ")")?.span.end;
        }
        Ok(Attr { name, args, span: Span::new(name_span.start, end) })
    }
}

// ---------------------------------------------------------------------------
// Expressions (Pratt-style precedence climb).
// ---------------------------------------------------------------------------

const PREFIX_BP: u8 = 30;
const ATTR_VALUE_BP: u8 = 1;
/// Binding power used inside `<...>` template-arg lists. Higher than
/// any comparison op so the closing `>` doesn't get eaten. Lower than
/// prefix/postfix so identifiers + integer literals + `(...)` /
/// arithmetic still parse correctly.
const TEMPLATE_ARG_BP: u8 = 21;
/// Bound for expressions used inside a match arm pattern. Has to sit
/// above `|` (BitOr, `(11, 12)`) so `(a | b)` reads as alternation
/// between two patterns, not a bit-or'd single value.
const MATCH_PATTERN_BP: u8 = 13;

fn infix_bp(op: &TokenKind) -> Option<(u8, u8)> {
    Some(match op {
        TokenKind::Eq
        | TokenKind::PlusEq
        | TokenKind::MinusEq
        | TokenKind::StarEq
        | TokenKind::SlashEq
        | TokenKind::PercentEq
        | TokenKind::AmpEq
        | TokenKind::PipeEq
        | TokenKind::CaretEq
        | TokenKind::ShlEq
        | TokenKind::ShrEq => (3, 2), // right-associative
        TokenKind::Question => (5, 4),
        TokenKind::PipePipe => (7, 8),
        TokenKind::AmpAmp => (9, 10),
        TokenKind::Pipe => (11, 12),
        TokenKind::Caret => (13, 14),
        TokenKind::Amp => (15, 16),
        TokenKind::EqEq | TokenKind::NotEq => (17, 18),
        TokenKind::Lt | TokenKind::Gt | TokenKind::LtEq | TokenKind::GtEq => (19, 20),
        TokenKind::Shl | TokenKind::Shr => (21, 22),
        TokenKind::Plus | TokenKind::Minus => (23, 24),
        TokenKind::Star | TokenKind::Slash | TokenKind::Percent => (25, 26),
        _ => return None,
    })
}

fn assign_op(op: &TokenKind) -> Option<AssignOp> {
    Some(match op {
        TokenKind::Eq => AssignOp::Assign,
        TokenKind::PlusEq => AssignOp::AddAssign,
        TokenKind::MinusEq => AssignOp::SubAssign,
        TokenKind::StarEq => AssignOp::MulAssign,
        TokenKind::SlashEq => AssignOp::DivAssign,
        TokenKind::PercentEq => AssignOp::RemAssign,
        TokenKind::AmpEq => AssignOp::AndAssign,
        TokenKind::PipeEq => AssignOp::OrAssign,
        TokenKind::CaretEq => AssignOp::XorAssign,
        TokenKind::ShlEq => AssignOp::ShlAssign,
        TokenKind::ShrEq => AssignOp::ShrAssign,
        _ => return None,
    })
}

fn arith_op(op: &TokenKind) -> Option<BinOp> {
    Some(match op {
        TokenKind::Plus => BinOp::Add,
        TokenKind::Minus => BinOp::Sub,
        TokenKind::Star => BinOp::Mul,
        TokenKind::Slash => BinOp::Div,
        TokenKind::Percent => BinOp::Rem,
        TokenKind::EqEq => BinOp::Eq,
        TokenKind::NotEq => BinOp::NotEq,
        TokenKind::Lt => BinOp::Lt,
        TokenKind::Gt => BinOp::Gt,
        TokenKind::LtEq => BinOp::LtEq,
        TokenKind::GtEq => BinOp::GtEq,
        TokenKind::AmpAmp => BinOp::LogicalAnd,
        TokenKind::PipePipe => BinOp::LogicalOr,
        TokenKind::Amp => BinOp::BitAnd,
        TokenKind::Pipe => BinOp::BitOr,
        TokenKind::Caret => BinOp::BitXor,
        TokenKind::Shl => BinOp::Shl,
        TokenKind::Shr => BinOp::Shr,
        _ => return None,
    })
}

impl Parser {
    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_expr_bp(0)
    }

    #[allow(clippy::while_let_loop)] // multiple unrelated `break` exits
    fn parse_expr_bp(&mut self, min_bp: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_prefix()?;
        loop {
            let Some(tok) = self.peek().cloned() else { break };
            // Postfix forms (call, index, member, post-inc/dec).
            match &tok.kind {
                TokenKind::LParen => {
                    self.bump();
                    let args = self.parse_call_args()?;
                    let close = self.expect_kind(&TokenKind::RParen, ")")?;
                    lhs = Expr::Call {
                        callee: Box::new(lhs.clone()),
                        args,
                        span: Span::new(lhs.span().start, close.span.end),
                    };
                    continue;
                }
                TokenKind::LBracket => {
                    self.bump();
                    let index = self.parse_expr()?;
                    let close = self.expect_rbracket()?;
                    lhs = Expr::Index {
                        target: Box::new(lhs.clone()),
                        index: Box::new(index),
                        span: Span::new(lhs.span().start, close.span.end),
                    };
                    continue;
                }
                TokenKind::Dot => {
                    // `a..b` and `a...b` (used in match-arm ranges)
                    // would otherwise be eaten as `a.<member>` here;
                    // peek for a second dot and bail out so the
                    // surrounding context handles the range.
                    if matches!(self.peek_at(1).map(|t| &t.kind), Some(TokenKind::Dot)) {
                        break;
                    }
                    self.bump();
                    // Soft ident -- corpus patterns use `parent`,
                    // `this`, `auto` as struct member names.
                    let (field, fspan) = self.expect_soft_ident()?;
                    lhs = Expr::Member {
                        target: Box::new(lhs.clone()),
                        field,
                        span: Span::new(lhs.span().start, fspan.end),
                    };
                    continue;
                }
                TokenKind::PlusPlus => {
                    let t = self.bump().unwrap();
                    lhs = Expr::Unary {
                        op: UnaryOp::PostInc,
                        operand: Box::new(lhs.clone()),
                        span: Span::new(lhs.span().start, t.span.end),
                    };
                    continue;
                }
                TokenKind::MinusMinus => {
                    let t = self.bump().unwrap();
                    lhs = Expr::Unary {
                        op: UnaryOp::PostDec,
                        operand: Box::new(lhs.clone()),
                        span: Span::new(lhs.span().start, t.span.end),
                    };
                    continue;
                }
                _ => {}
            }
            // Infix.
            let Some((lbp, rbp)) = infix_bp(&tok.kind) else { break };
            if lbp < min_bp {
                break;
            }
            self.bump();
            // Ternary `?:` -- middle then `:` then else.
            if matches!(tok.kind, TokenKind::Question) {
                let then_val = self.parse_expr_bp(0)?;
                self.expect_kind(&TokenKind::Colon, ":")?;
                let else_val = self.parse_expr_bp(rbp)?;
                lhs = Expr::Ternary {
                    cond: Box::new(lhs.clone()),
                    then_val: Box::new(then_val),
                    else_val: Box::new(else_val.clone()),
                    span: Span::new(lhs.span().start, else_val.span().end),
                };
                continue;
            }
            // Assignment.
            if let Some(op) = assign_op(&tok.kind) {
                let value = self.parse_expr_bp(rbp)?;
                lhs = Expr::Assign {
                    op,
                    target: Box::new(lhs.clone()),
                    value: Box::new(value.clone()),
                    span: Span::new(lhs.span().start, value.span().end),
                };
                continue;
            }
            // Generic arithmetic / comparison.
            let bin = arith_op(&tok.kind).expect("infix_bp returned for non-arithmetic op");
            let rhs = self.parse_expr_bp(rbp)?;
            lhs = Expr::Binary {
                op: bin,
                lhs: Box::new(lhs.clone()),
                rhs: Box::new(rhs.clone()),
                span: Span::new(lhs.span().start, rhs.span().end),
            };
        }
        Ok(lhs)
    }

    fn parse_call_args(&mut self) -> Result<Vec<Expr>, ParseError> {
        let mut out = Vec::new();
        if matches!(self.peek_kind(), Some(TokenKind::RParen)) {
            return Ok(out);
        }
        loop {
            out.push(self.parse_expr_bp(ATTR_VALUE_BP)?);
            if !self.eat_kind(&TokenKind::Comma) {
                break;
            }
        }
        Ok(out)
    }

    fn parse_prefix(&mut self) -> Result<Expr, ParseError> {
        let Some(tok) = self.peek().cloned() else {
            return Err(ParseError::UnexpectedEof { expected: "expression" });
        };
        match tok.kind {
            TokenKind::Int(v) => {
                self.bump();
                Ok(Expr::IntLit { value: v, span: tok.span })
            }
            TokenKind::Float(v) => {
                self.bump();
                Ok(Expr::FloatLit { value: v, span: tok.span })
            }
            TokenKind::String(s) => {
                self.bump();
                Ok(Expr::StringLit { value: s, span: tok.span })
            }
            TokenKind::Char(c) => {
                self.bump();
                Ok(Expr::CharLit { value: c, span: tok.span })
            }
            TokenKind::Keyword(Keyword::True) => {
                self.bump();
                Ok(Expr::BoolLit { value: true, span: tok.span })
            }
            TokenKind::Keyword(Keyword::False) => {
                self.bump();
                Ok(Expr::BoolLit { value: false, span: tok.span })
            }
            TokenKind::Keyword(Keyword::Null) => {
                self.bump();
                Ok(Expr::NullLit { span: tok.span })
            }
            TokenKind::Keyword(Keyword::Sizeof) => self.parse_reflect(ReflectKind::Sizeof),
            TokenKind::Keyword(Keyword::Addressof) => self.parse_reflect(ReflectKind::Addressof),
            TokenKind::Keyword(Keyword::Typeof) => self.parse_reflect(ReflectKind::Typeof),
            TokenKind::Keyword(Keyword::Parent) => {
                self.bump();
                Ok(Expr::Ident { name: "parent".into(), span: tok.span })
            }
            TokenKind::Keyword(Keyword::This) => {
                self.bump();
                Ok(Expr::Ident { name: "this".into(), span: tok.span })
            }
            TokenKind::Dollar => {
                self.bump();
                Ok(Expr::Ident { name: "$".into(), span: tok.span })
            }
            TokenKind::Ident(_) => self.parse_ident_or_path(),
            // `be u16(x)` / `le u16(x)` -- field-level endian cast in
            // expression position. Drop the keyword and let the rest
            // of the expression carry on; the call that follows
            // produces the same numeric value (we don't model the
            // endian override at runtime yet).
            TokenKind::Keyword(Keyword::Be) | TokenKind::Keyword(Keyword::Le) => {
                self.bump();
                self.parse_prefix()
            }
            // `{1, 2, 3}` -- brace-list initialiser as expression.
            // Collapse to the first element since the AST has no
            // dedicated array-literal node; this lets corpus
            // patterns like `local int xs[N] = { ... };` parse.
            TokenKind::LBrace => {
                self.bump();
                let mut first: Option<Expr> = None;
                if !matches!(self.peek_kind(), Some(TokenKind::RBrace)) {
                    first = Some(self.parse_expr_bp(ATTR_VALUE_BP)?);
                    while self.eat_kind(&TokenKind::Comma) {
                        if matches!(self.peek_kind(), Some(TokenKind::RBrace)) {
                            break;
                        }
                        let _ = self.parse_expr_bp(ATTR_VALUE_BP)?;
                    }
                }
                let close = self.expect_kind(&TokenKind::RBrace, "}")?;
                let span = Span::new(tok.span.start, close.span.end);
                Ok(first.map(|e| with_span(e, span)).unwrap_or(Expr::IntLit { value: 0, span }))
            }
            TokenKind::LParen => {
                self.bump();
                let inner = self.parse_expr()?;
                let close = self.expect_kind(&TokenKind::RParen, ")")?;
                let span = Span::new(tok.span.start, close.span.end);
                Ok(with_span(inner, span))
            }
            TokenKind::Minus | TokenKind::Plus | TokenKind::Bang | TokenKind::Tilde => {
                self.bump();
                let op = match tok.kind {
                    TokenKind::Minus => UnaryOp::Neg,
                    TokenKind::Plus => UnaryOp::Pos,
                    TokenKind::Bang => UnaryOp::Not,
                    TokenKind::Tilde => UnaryOp::BitNot,
                    _ => unreachable!(),
                };
                let operand = self.parse_expr_bp(PREFIX_BP)?;
                let span = Span::new(tok.span.start, operand.span().end);
                Ok(Expr::Unary { op, operand: Box::new(operand), span })
            }
            TokenKind::PlusPlus => {
                self.bump();
                let operand = self.parse_expr_bp(PREFIX_BP)?;
                let span = Span::new(tok.span.start, operand.span().end);
                Ok(Expr::Unary { op: UnaryOp::PreInc, operand: Box::new(operand), span })
            }
            TokenKind::MinusMinus => {
                self.bump();
                let operand = self.parse_expr_bp(PREFIX_BP)?;
                let span = Span::new(tok.span.start, operand.span().end);
                Ok(Expr::Unary { op: UnaryOp::PreDec, operand: Box::new(operand), span })
            }
            other => Err(ParseError::NotAnExpression { found: format!("{other:?}"), span: tok.span }),
        }
    }

    fn parse_reflect(&mut self, kind: ReflectKind) -> Result<Expr, ParseError> {
        let kw = self.bump().unwrap();
        self.expect_kind(&TokenKind::LParen, "(")?;
        let operand = self.parse_expr()?;
        let close = self.expect_kind(&TokenKind::RParen, ")")?;
        Ok(Expr::Reflect { kind, operand: Box::new(operand), span: Span::new(kw.span.start, close.span.end) })
    }

    fn parse_ident_or_path(&mut self) -> Result<Expr, ParseError> {
        let (head, head_span) = self.expect_ident()?;
        if !matches!(self.peek_kind(), Some(TokenKind::ColonColon)) {
            return Ok(Expr::Ident { name: head, span: head_span });
        }
        let mut segments = vec![head];
        let mut end = head_span.end;
        while self.eat_kind(&TokenKind::ColonColon) {
            let (seg, span) = self.expect_ident()?;
            segments.push(seg);
            end = span.end;
        }
        Ok(Expr::Path { segments, span: Span::new(head_span.start, end) })
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn stmt_span(stmt: &Stmt) -> Span {
    match stmt {
        Stmt::UsingAlias { span, .. }
        | Stmt::Namespace { span, .. }
        | Stmt::Import { span, .. }
        | Stmt::Block { span, .. }
        | Stmt::Expr { span, .. }
        | Stmt::If { span, .. }
        | Stmt::While { span, .. }
        | Stmt::For { span, .. }
        | Stmt::Match { span, .. }
        | Stmt::Return { span, .. }
        | Stmt::Break { span }
        | Stmt::Continue { span }
        | Stmt::FieldDecl { span, .. }
        | Stmt::BitfieldField { span, .. } => *span,
        Stmt::StructDecl(s) => s.span,
        Stmt::EnumDecl(e) => e.span,
        Stmt::BitfieldDecl(b) => b.span,
        Stmt::FnDecl(f) => f.span,
    }
}

/// Merge a leading `template<...>` parameter list onto whatever
/// declaration the body re-dispatches to. `template<T> struct
/// Foo<T>` and `struct Foo<T>` should produce the same AST; this
/// helper makes both shapes equivalent. If the body decl doesn't
/// support template parameters (e.g. a regular field decl), the
/// captured params are dropped silently.
fn merge_template_params(stmt: Stmt, params: Vec<String>) -> Stmt {
    if params.is_empty() {
        return stmt;
    }
    match stmt {
        Stmt::StructDecl(mut d) => {
            if d.template_params.is_empty() {
                d.template_params = params;
            }
            Stmt::StructDecl(d)
        }
        Stmt::EnumDecl(mut d) => {
            if d.template_params.is_empty() {
                d.template_params = params;
            }
            Stmt::EnumDecl(d)
        }
        Stmt::BitfieldDecl(mut d) => {
            if d.template_params.is_empty() {
                d.template_params = params;
            }
            Stmt::BitfieldDecl(d)
        }
        other => other,
    }
}

/// Replace the span end on a freshly-built field decl. Used by the
/// comma-separated declarator path so each decl's span runs to the
/// trailing `;` -- otherwise the early decls would be missing their
/// terminator in span output.
fn extend_decl_span(stmt: Stmt, end: usize) -> Stmt {
    match stmt {
        Stmt::FieldDecl { is_const, ty, name, array, placement, init, attrs, pointer_width, span } => Stmt::FieldDecl {
            is_const,
            ty,
            name,
            array,
            placement,
            init,
            attrs,
            pointer_width,
            span: Span::new(span.start, end),
        },
        other => other,
    }
}

/// Replace the span on an expression so the parens-wrapper case can
/// widen the span without unwrapping the variant.
fn with_span(expr: Expr, span: Span) -> Expr {
    match expr {
        Expr::IntLit { value, .. } => Expr::IntLit { value, span },
        Expr::FloatLit { value, .. } => Expr::FloatLit { value, span },
        Expr::StringLit { value, .. } => Expr::StringLit { value, span },
        Expr::CharLit { value, .. } => Expr::CharLit { value, span },
        Expr::BoolLit { value, .. } => Expr::BoolLit { value, span },
        Expr::NullLit { .. } => Expr::NullLit { span },
        Expr::Ident { name, .. } => Expr::Ident { name, span },
        Expr::Path { segments, .. } => Expr::Path { segments, span },
        Expr::Binary { op, lhs, rhs, .. } => Expr::Binary { op, lhs, rhs, span },
        Expr::Unary { op, operand, .. } => Expr::Unary { op, operand, span },
        Expr::Call { callee, args, .. } => Expr::Call { callee, args, span },
        Expr::Index { target, index, .. } => Expr::Index { target, index, span },
        Expr::Member { target, field, .. } => Expr::Member { target, field, span },
        Expr::Assign { op, target, value, .. } => Expr::Assign { op, target, value, span },
        Expr::Ternary { cond, then_val, else_val, .. } => Expr::Ternary { cond, then_val, else_val, span },
        Expr::Reflect { kind, operand, .. } => Expr::Reflect { kind, operand, span },
    }
}
