//! Recursive-descent parser for 010 Binary Template files.
//!
//! Expressions use a Pratt-style precedence climb; declarations and
//! statements are parsed with straight-line recursive descent.
//! Consumes the [`Vec<Token>`] produced by [`tokenize`](crate::tokenize).

use thiserror::Error;

use crate::ast::Attr;
use crate::ast::Attrs;
use crate::ast::AssignOp;
use crate::ast::BinOp;
use crate::ast::DeclModifier;
use crate::ast::EnumDecl;
use crate::ast::EnumVariant;
use crate::ast::Expr;
use crate::ast::FunctionDef;
use crate::ast::Param;
use crate::ast::Program;
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
    let mut parser = Parser { tokens, pos: 0 };
    let mut items = Vec::new();
    while !parser.at_eof() {
        items.push(parser.parse_top_item()?);
    }
    Ok(Program { items })
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

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
        // Span of the most recently consumed token, used to close a
        // multi-token construct like a function body.
        self.tokens.get(self.pos.saturating_sub(1)).map(|t| t.span).unwrap_or(Span::new(0, 0))
    }

    fn expect_kind(&mut self, want: &TokenKind, expected_msg: &'static str) -> Result<Token, ParseError> {
        match self.peek() {
            Some(t) if &t.kind == want => Ok(self.bump().unwrap()),
            Some(t) => Err(ParseError::Unexpected {
                expected: expected_msg,
                found: format!("{:?}", t.kind),
                span: t.span,
            }),
            None => Err(ParseError::UnexpectedEof { expected: expected_msg }),
        }
    }

    fn expect_ident(&mut self) -> Result<(String, Span), ParseError> {
        match self.peek() {
            Some(Token { kind: TokenKind::Ident(_), .. }) => {
                let tok = self.bump().unwrap();
                let span = tok.span;
                let TokenKind::Ident(name) = tok.kind else { unreachable!() };
                Ok((name, span))
            }
            Some(t) => Err(ParseError::Unexpected {
                expected: "identifier",
                found: format!("{:?}", t.kind),
                span: t.span,
            }),
            None => Err(ParseError::UnexpectedEof { expected: "identifier" }),
        }
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
        if self.peek_kind() == Some(&TokenKind::Keyword(kw)) {
            self.bump();
            true
        } else {
            false
        }
    }


    fn parse_top_item(&mut self) -> Result<TopItem, ParseError> {
        // Function defs are hard to distinguish from variable decls
        // purely from the leading token: both can start with a type
        // name. The discriminator is whether the token after the
        // identifier is `(`.
        if self.looks_like_function_def() {
            return Ok(TopItem::Function(self.parse_function_def()?));
        }
        Ok(TopItem::Stmt(self.parse_stmt()?))
    }

    fn looks_like_function_def(&self) -> bool {
        // `TypeIdent name (` with TypeIdent being a plain Ident or one
        // of a small set of keywords (e.g. `void`). We look ahead
        // without consuming.
        let Some(t0) = self.peek() else { return false };
        let starts_type = matches!(&t0.kind, TokenKind::Ident(_) | TokenKind::Keyword(Keyword::Void));
        if !starts_type {
            return false;
        }
        let Some(t1) = self.peek_at(1) else { return false };
        let is_ident = matches!(&t1.kind, TokenKind::Ident(_));
        if !is_ident {
            return false;
        }
        matches!(self.peek_at(2).map(|t| &t.kind), Some(TokenKind::LParen))
    }

    fn parse_function_def(&mut self) -> Result<FunctionDef, ParseError> {
        let start = self.peek().map(|t| t.span.start).unwrap_or(0);
        let return_type = self.parse_type_ref()?;
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
        let end = self.last_span().end;
        let Stmt::Block { stmts, .. } = body_block else { unreachable!() };
        Ok(FunctionDef { return_type, name, params, body: stmts, span: Span::new(start, end) })
    }

    fn parse_param(&mut self) -> Result<Param, ParseError> {
        let ty = self.parse_type_ref()?;
        let is_ref = self.eat_kind(&TokenKind::Amp);
        let (name, name_span) = self.expect_ident()?;
        let span = Span::new(ty.span.start, name_span.end);
        Ok(Param { ty, is_ref, name, span })
    }


    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        let Some(tok) = self.peek() else {
            return Err(ParseError::UnexpectedEof { expected: "statement" });
        };
        match &tok.kind {
            TokenKind::LBrace => self.parse_block(),
            TokenKind::Keyword(Keyword::If) => self.parse_if(),
            TokenKind::Keyword(Keyword::While) => self.parse_while(),
            TokenKind::Keyword(Keyword::Do) => self.parse_do_while(),
            TokenKind::Keyword(Keyword::For) => self.parse_for(),
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
            TokenKind::Keyword(Keyword::Typedef) => self.parse_typedef(),
            TokenKind::Keyword(Keyword::Local) | TokenKind::Keyword(Keyword::Const) => self.parse_field_decl(None),
            TokenKind::Semi => {
                // Empty statement — consume and treat as a no-op block.
                let t = self.bump().unwrap();
                Ok(Stmt::Block { stmts: Vec::new(), span: t.span })
            }
            TokenKind::Keyword(Keyword::Struct) => self.parse_struct_stmt(),
            TokenKind::Ident(_) | TokenKind::Keyword(Keyword::Enum) => {
                // Field declaration if followed by an identifier; else
                // an expression statement.
                if self.looks_like_field_decl() {
                    self.parse_field_decl(None)
                } else {
                    self.parse_expr_stmt()
                }
            }
            _ => self.parse_expr_stmt(),
        }
    }

    /// Heuristic: a field declaration looks like `Type ident` possibly
    /// followed by array brackets, `=`, or `;`. An expression statement
    /// starts the same way (e.g. `tag = ...`) only when the identifier
    /// is already defined as a variable. The parser can't know that,
    /// so we disambiguate conservatively: we treat two consecutive
    /// identifiers as a field declaration, anything else as an
    /// expression. This matches how 010 itself resolves ambiguity.
    fn looks_like_field_decl(&self) -> bool {
        let Some(t0) = self.peek() else { return false };
        if !matches!(&t0.kind, TokenKind::Ident(_) | TokenKind::Keyword(Keyword::Struct) | TokenKind::Keyword(Keyword::Enum))
        {
            return false;
        }
        matches!(self.peek_at(1).map(|t| &t.kind), Some(TokenKind::Ident(_)))
    }

    fn parse_block(&mut self) -> Result<Stmt, ParseError> {
        let open = self.expect_kind(&TokenKind::LBrace, "{")?;
        let mut stmts = Vec::new();
        while !matches!(self.peek_kind(), Some(TokenKind::RBrace) | None) {
            stmts.push(self.parse_stmt()?);
        }
        let close = self.expect_kind(&TokenKind::RBrace, "}")?;
        Ok(Stmt::Block { stmts, span: Span::new(open.span.start, close.span.end) })
    }

    fn parse_if(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap();
        self.expect_kind(&TokenKind::LParen, "(")?;
        let cond = self.parse_expr()?;
        self.expect_kind(&TokenKind::RParen, ")")?;
        let then_branch = Box::new(self.parse_stmt()?);
        let else_branch = if self.eat_keyword(Keyword::Else) {
            Some(Box::new(self.parse_stmt()?))
        } else {
            None
        };
        let end = else_branch
            .as_ref()
            .map(|s| stmt_span(s).end)
            .unwrap_or_else(|| stmt_span(&then_branch).end);
        Ok(Stmt::If { cond, then_branch, else_branch, span: Span::new(kw.span.start, end) })
    }

    fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap();
        self.expect_kind(&TokenKind::LParen, "(")?;
        let cond = self.parse_expr()?;
        self.expect_kind(&TokenKind::RParen, ")")?;
        let body = Box::new(self.parse_stmt()?);
        let end = stmt_span(&body).end;
        Ok(Stmt::While { cond, body, span: Span::new(kw.span.start, end) })
    }

    fn parse_do_while(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap();
        let body = Box::new(self.parse_stmt()?);
        self.expect_kind(&TokenKind::Keyword(Keyword::While), "while")?;
        self.expect_kind(&TokenKind::LParen, "(")?;
        let cond = self.parse_expr()?;
        self.expect_kind(&TokenKind::RParen, ")")?;
        let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
        Ok(Stmt::DoWhile { body, cond, span: Span::new(kw.span.start, end_tok.span.end) })
    }

    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap();
        self.expect_kind(&TokenKind::LParen, "(")?;
        let init = if self.eat_kind(&TokenKind::Semi) {
            None
        } else {
            // A for-init can be either a declaration or an expression
            // statement. `parse_stmt` handles both and consumes the
            // trailing `;` itself.
            Some(Box::new(self.parse_stmt()?))
        };
        let cond = if matches!(self.peek_kind(), Some(TokenKind::Semi)) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        self.expect_kind(&TokenKind::Semi, ";")?;
        let step = if matches!(self.peek_kind(), Some(TokenKind::RParen)) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        self.expect_kind(&TokenKind::RParen, ")")?;
        let body = Box::new(self.parse_stmt()?);
        let end = stmt_span(&body).end;
        Ok(Stmt::For { init, cond, step, body, span: Span::new(kw.span.start, end) })
    }

    fn parse_return(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap();
        let value = if matches!(self.peek_kind(), Some(TokenKind::Semi)) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
        Ok(Stmt::Return { value, span: Span::new(kw.span.start, end_tok.span.end) })
    }

    fn parse_expr_stmt(&mut self) -> Result<Stmt, ParseError> {
        let expr = self.parse_expr()?;
        let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
        let span = Span::new(expr.span().start, end_tok.span.end);
        Ok(Stmt::Expr { expr, span })
    }


    fn parse_typedef(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `typedef`
        let start = kw.span.start;

        // `typedef enum <backing>? { ... } Name <attrs>?;`
        if self.eat_keyword(Keyword::Enum) {
            let backing = if self.eat_kind(&TokenKind::Lt) {
                let t = self.parse_type_ref()?;
                self.expect_kind(&TokenKind::Gt, ">")?;
                Some(t)
            } else {
                None
            };
            self.expect_kind(&TokenKind::LBrace, "{")?;
            let mut variants = Vec::new();
            if !matches!(self.peek_kind(), Some(TokenKind::RBrace)) {
                loop {
                    variants.push(self.parse_enum_variant()?);
                    if !self.eat_kind(&TokenKind::Comma) {
                        break;
                    }
                    if matches!(self.peek_kind(), Some(TokenKind::RBrace)) {
                        break;
                    }
                }
            }
            self.expect_kind(&TokenKind::RBrace, "}")?;
            let (name, _) = self.expect_ident()?;
            let attrs = self.parse_optional_attrs()?;
            let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
            return Ok(Stmt::TypedefEnum(EnumDecl {
                name,
                backing,
                variants,
                attrs,
                span: Span::new(start, end_tok.span.end),
            }));
        }

        // `typedef struct [Tag] { ... } [Alias] <attrs>?;`
        // The struct can have a tag before the body, an alias after
        // the body, or both — at least one is required. We pick the
        // tag when present, else fall back to the alias.
        if self.eat_keyword(Keyword::Struct) {
            let tag = if matches!(self.peek_kind(), Some(TokenKind::Ident(_))) {
                Some(self.expect_ident()?.0)
            } else {
                None
            };
            let body_block = self.parse_block()?;
            let Stmt::Block { stmts, .. } = body_block else { unreachable!() };
            let alias = if matches!(self.peek_kind(), Some(TokenKind::Ident(_))) {
                Some(self.expect_ident()?.0)
            } else {
                None
            };
            let name = tag.clone().or(alias).ok_or(ParseError::Unexpected {
                expected: "struct tag or typedef alias",
                found: "neither".into(),
                span: Span::new(start, start),
            })?;
            let attrs = self.parse_optional_attrs()?;
            let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
            return Ok(Stmt::TypedefStruct(StructDecl {
                name,
                body: stmts,
                attrs,
                span: Span::new(start, end_tok.span.end),
            }));
        }

        // `typedef SourceType NewName;`
        let source = self.parse_type_ref()?;
        let (new_name, name_span) = self.expect_ident()?;
        let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
        Ok(Stmt::TypedefAlias {
            new_name,
            source,
            span: Span::new(start, end_tok.span.end.max(name_span.end)),
        })
    }

    /// Handle the `struct` keyword outside a `typedef`. Three shapes:
    ///   1. `struct Name { body };` — type declaration
    ///   2. `struct { body } field_name;` — anonymous struct as a field type
    ///   3. `struct Name field_name;` — field whose type is an already-declared struct
    fn parse_struct_stmt(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `struct`
        let start = kw.span.start;

        match self.peek_kind() {
            // `struct Name ...`
            Some(TokenKind::Ident(_)) => {
                let (name, name_span) = self.expect_ident()?;
                if matches!(self.peek_kind(), Some(TokenKind::LBrace)) {
                    // Form 1: `struct Name { body } [<attrs>];`
                    let body_block = self.parse_block()?;
                    let Stmt::Block { stmts, .. } = body_block else { unreachable!() };
                    let attrs = self.parse_optional_attrs()?;
                    let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
                    Ok(Stmt::TypedefStruct(StructDecl {
                        name,
                        body: stmts,
                        attrs,
                        span: Span::new(start, end_tok.span.end),
                    }))
                } else {
                    // Form 3: `struct Name field_name [array] [<attrs>];`
                    let ty = TypeRef { name, span: name_span };
                    self.parse_field_decl(Some(ty))
                }
            }
            // Form 2: `struct { body } field_name ...;`
            Some(TokenKind::LBrace) => {
                // Give the anonymous struct a synthetic name so the
                // AST remains a strict subset of the named form. The
                // name lives inside the declaration scope only.
                let body_block = self.parse_block()?;
                let Stmt::Block { stmts, span: body_span } = body_block else { unreachable!() };
                let anon_name = format!("__anon_struct_{}", body_span.start);
                let struct_attrs = self.parse_optional_attrs()?;
                // Emit a typedef-style struct decl, then fall through
                // to a field declaration using that synthetic name.
                let decl_span = Span::new(start, self.last_span().end);
                let struct_stmt = Stmt::TypedefStruct(StructDecl {
                    name: anon_name.clone(),
                    body: stmts,
                    attrs: struct_attrs,
                    span: decl_span,
                });
                // The field after the anonymous struct body.
                let (field_name, field_name_span) = self.expect_ident()?;
                let array_size = if self.eat_kind(&TokenKind::LBracket) {
                    let expr = self.parse_expr()?;
                    self.expect_kind(&TokenKind::RBracket, "]")?;
                    Some(expr)
                } else {
                    None
                };
                let field_attrs = self.parse_optional_attrs()?;
                let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
                let field_stmt = Stmt::FieldDecl {
                    modifier: DeclModifier::Field,
                    ty: TypeRef { name: anon_name, span: field_name_span },
                    name: field_name,
                    array_size,
                    init: None,
                    attrs: field_attrs,
                    span: Span::new(start, end_tok.span.end),
                };
                // Bundle both into a block so the caller gets a single
                // statement back; the interpreter walks blocks
                // transparently.
                Ok(Stmt::Block {
                    stmts: vec![struct_stmt, field_stmt],
                    span: Span::new(start, end_tok.span.end),
                })
            }
            _ => Err(ParseError::Unexpected {
                expected: "struct name or body",
                found: self.peek().map(|t| format!("{:?}", t.kind)).unwrap_or_else(|| "eof".into()),
                span: self.peek().map(|t| t.span).unwrap_or(Span::new(start, start)),
            }),
        }
    }

    fn parse_enum_variant(&mut self) -> Result<EnumVariant, ParseError> {
        let (name, name_span) = self.expect_ident()?;
        let value = if self.eat_kind(&TokenKind::Eq) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let end = value.as_ref().map(|e| e.span().end).unwrap_or(name_span.end);
        Ok(EnumVariant { name, value, span: Span::new(name_span.start, end) })
    }

    /// Parse a `<key=expr, ...>` attribute list if the next token is
    /// `<`, else return an empty [`Attrs`]. Attribute values parse at
    /// `ATTR_VALUE_BP` so `>` and `,` stay unambiguous closers rather
    /// than getting consumed as comparison / sequence operators.
    fn parse_optional_attrs(&mut self) -> Result<Attrs, ParseError> {
        if !self.eat_kind(&TokenKind::Lt) {
            return Ok(Attrs::default());
        }
        let mut attrs = Vec::new();
        if !matches!(self.peek_kind(), Some(TokenKind::Gt)) {
            loop {
                let (key, key_span) = self.expect_ident()?;
                self.expect_kind(&TokenKind::Eq, "=")?;
                let value = self.parse_expr_bp(ATTR_VALUE_BP)?;
                let span = Span::new(key_span.start, value.span().end);
                attrs.push(Attr { key, value, span });
                if !self.eat_kind(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect_kind(&TokenKind::Gt, ">")?;
        Ok(Attrs(attrs))
    }

    /// Parse a field declaration. The optional `ty_override` argument
    /// is reserved for re-entry from contexts where the parser has
    /// already consumed the type (currently unused; kept for future
    /// nested-struct use).
    fn parse_field_decl(&mut self, ty_override: Option<TypeRef>) -> Result<Stmt, ParseError> {
        let start = self.peek().map(|t| t.span.start).unwrap_or(0);
        let modifier = if self.eat_keyword(Keyword::Local) {
            DeclModifier::Local
        } else if self.eat_keyword(Keyword::Const) {
            DeclModifier::Const
        } else {
            DeclModifier::Field
        };
        let ty = match ty_override {
            Some(t) => t,
            None => self.parse_type_ref()?,
        };
        let (name, _) = self.expect_ident()?;
        let array_size = if self.eat_kind(&TokenKind::LBracket) {
            let expr = self.parse_expr()?;
            self.expect_kind(&TokenKind::RBracket, "]")?;
            Some(expr)
        } else {
            None
        };
        let attrs = self.parse_optional_attrs()?;
        let init = if self.eat_kind(&TokenKind::Eq) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
        Ok(Stmt::FieldDecl {
            modifier,
            ty,
            name,
            array_size,
            init,
            attrs,
            span: Span::new(start, end_tok.span.end),
        })
    }

    fn parse_type_ref(&mut self) -> Result<TypeRef, ParseError> {
        if self.eat_keyword(Keyword::Void) {
            let span = self.tokens[self.pos - 1].span;
            return Ok(TypeRef { name: "void".into(), span });
        }
        let (name, span) = self.expect_ident()?;
        Ok(TypeRef { name, span })
    }


    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_expr_bp(0)
    }

    /// Pratt expression parser. `min_bp` is the minimum binding power
    /// the next operator must beat to bind; a higher `min_bp` means
    /// "return, let a caller bind more tightly". Assignment is
    /// right-associative; other operators left-associative.
    fn parse_expr_bp(&mut self, min_bp: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_prefix()?;

        while let Some(tok) = self.peek().cloned() {
            // Postfix: call, index, member access, post-inc/dec.
            match tok.kind {
                TokenKind::LParen => {
                    self.bump();
                    let args = self.parse_call_args()?;
                    let close = self.expect_kind(&TokenKind::RParen, ")")?;
                    let span = Span::new(lhs.span().start, close.span.end);
                    lhs = Expr::Call { callee: Box::new(lhs), args, span };
                    continue;
                }
                TokenKind::LBracket => {
                    self.bump();
                    let index = self.parse_expr()?;
                    let close = self.expect_kind(&TokenKind::RBracket, "]")?;
                    let span = Span::new(lhs.span().start, close.span.end);
                    lhs = Expr::Index { target: Box::new(lhs), index: Box::new(index), span };
                    continue;
                }
                TokenKind::Dot => {
                    self.bump();
                    let (field, fspan) = self.expect_ident()?;
                    let span = Span::new(lhs.span().start, fspan.end);
                    lhs = Expr::Member { target: Box::new(lhs), field, span };
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

            // Infix operators.
            if let Some((l_bp, r_bp, op)) = infix_binding_power(&tok.kind) {
                if l_bp < min_bp {
                    break;
                }
                self.bump();
                let rhs = self.parse_expr_bp(r_bp)?;
                let span = Span::new(lhs.span().start, rhs.span().end);
                lhs = Expr::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs), span };
                continue;
            }

            // Ternary: cond ? a : b
            if matches!(tok.kind, TokenKind::Question) {
                let q_bp = 5;
                if q_bp < min_bp {
                    break;
                }
                self.bump();
                let then_val = self.parse_expr_bp(0)?;
                self.expect_kind(&TokenKind::Colon, ":")?;
                let else_val = self.parse_expr_bp(q_bp)?;
                let span = Span::new(lhs.span().start, else_val.span().end);
                lhs = Expr::Ternary {
                    cond: Box::new(lhs),
                    then_val: Box::new(then_val),
                    else_val: Box::new(else_val),
                    span,
                };
                continue;
            }

            // Assignment operators (right-associative).
            if let Some(op) = assign_op(&tok.kind) {
                let a_bp = 2;
                if a_bp < min_bp {
                    break;
                }
                self.bump();
                let rhs = self.parse_expr_bp(a_bp)?;
                let span = Span::new(lhs.span().start, rhs.span().end);
                lhs = Expr::Assign { op, target: Box::new(lhs), value: Box::new(rhs), span };
                continue;
            }

            break;
        }

        Ok(lhs)
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
            TokenKind::Ident(name) => {
                self.bump();
                Ok(Expr::Ident { name, span: tok.span })
            }
            TokenKind::Keyword(Keyword::True) => {
                self.bump();
                Ok(Expr::IntLit { value: 1, span: tok.span })
            }
            TokenKind::Keyword(Keyword::False) => {
                self.bump();
                Ok(Expr::IntLit { value: 0, span: tok.span })
            }
            TokenKind::Keyword(Keyword::Sizeof) => {
                self.bump();
                // sizeof ( expr )
                self.expect_kind(&TokenKind::LParen, "(")?;
                let inner = self.parse_expr()?;
                let close = self.expect_kind(&TokenKind::RParen, ")")?;
                // Model as a call to a magic identifier; the
                // interpreter resolves it specially.
                let callee =
                    Expr::Ident { name: "sizeof".into(), span: tok.span };
                Ok(Expr::Call {
                    callee: Box::new(callee),
                    args: vec![inner],
                    span: Span::new(tok.span.start, close.span.end),
                })
            }
            TokenKind::LParen => {
                self.bump();
                let inner = self.parse_expr()?;
                let close = self.expect_kind(&TokenKind::RParen, ")")?;
                // Discard the parens — the AST doesn't need them,
                // precedence is already captured. But widen the span.
                Ok(with_span(inner, Span::new(tok.span.start, close.span.end)))
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
            _ => Err(ParseError::NotAnExpression {
                found: format!("{:?}", tok.kind),
                span: tok.span,
            }),
        }
    }

    fn parse_call_args(&mut self) -> Result<Vec<Expr>, ParseError> {
        if matches!(self.peek_kind(), Some(TokenKind::RParen)) {
            return Ok(Vec::new());
        }
        let mut args = Vec::new();
        loop {
            args.push(self.parse_expr_bp(1)?);
            if !self.eat_kind(&TokenKind::Comma) {
                break;
            }
        }
        Ok(args)
    }
}


/// Binding power of a prefix operator like unary `-` / `!` — higher
/// than any infix so `-a * b` parses as `(-a) * b`.
const PREFIX_BP: u8 = 30;

/// Minimum binding power used when parsing attribute values. Keeps
/// `>` (attr closer) and `,` (attr separator) from being consumed as
/// binary operators. Postfix operators (call, index, member access)
/// still bind because they bypass the `min_bp` check entirely.
const ATTR_VALUE_BP: u8 = 40;

fn infix_binding_power(kind: &TokenKind) -> Option<(u8, u8, BinOp)> {
    let (l, r, op) = match kind {
        TokenKind::PipePipe => (3, 4, BinOp::LogicalOr),
        TokenKind::AmpAmp => (5, 6, BinOp::LogicalAnd),
        TokenKind::Pipe => (7, 8, BinOp::BitOr),
        TokenKind::Caret => (9, 10, BinOp::BitXor),
        TokenKind::Amp => (11, 12, BinOp::BitAnd),
        TokenKind::EqEq => (13, 14, BinOp::Eq),
        TokenKind::NotEq => (13, 14, BinOp::NotEq),
        TokenKind::Lt => (15, 16, BinOp::Lt),
        TokenKind::Gt => (15, 16, BinOp::Gt),
        TokenKind::LtEq => (15, 16, BinOp::LtEq),
        TokenKind::GtEq => (15, 16, BinOp::GtEq),
        TokenKind::Shl => (17, 18, BinOp::Shl),
        TokenKind::Shr => (17, 18, BinOp::Shr),
        TokenKind::Plus => (19, 20, BinOp::Add),
        TokenKind::Minus => (19, 20, BinOp::Sub),
        TokenKind::Star => (21, 22, BinOp::Mul),
        TokenKind::Slash => (21, 22, BinOp::Div),
        TokenKind::Percent => (21, 22, BinOp::Rem),
        _ => return None,
    };
    Some((l, r, op))
}

fn assign_op(kind: &TokenKind) -> Option<AssignOp> {
    Some(match kind {
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

fn stmt_span(s: &Stmt) -> Span {
    match s {
        Stmt::TypedefAlias { span, .. }
        | Stmt::If { span, .. }
        | Stmt::While { span, .. }
        | Stmt::DoWhile { span, .. }
        | Stmt::For { span, .. }
        | Stmt::Return { span, .. }
        | Stmt::Break { span }
        | Stmt::Continue { span }
        | Stmt::Block { span, .. }
        | Stmt::Expr { span, .. }
        | Stmt::FieldDecl { span, .. } => *span,
        Stmt::TypedefEnum(e) => e.span,
        Stmt::TypedefStruct(s) => s.span,
    }
}

fn with_span(expr: Expr, span: Span) -> Expr {
    match expr {
        Expr::IntLit { value, .. } => Expr::IntLit { value, span },
        Expr::FloatLit { value, .. } => Expr::FloatLit { value, span },
        Expr::StringLit { value, .. } => Expr::StringLit { value, span },
        Expr::CharLit { value, .. } => Expr::CharLit { value, span },
        Expr::Ident { name, .. } => Expr::Ident { name, span },
        Expr::Binary { op, lhs, rhs, .. } => Expr::Binary { op, lhs, rhs, span },
        Expr::Unary { op, operand, .. } => Expr::Unary { op, operand, span },
        Expr::Call { callee, args, .. } => Expr::Call { callee, args, span },
        Expr::Index { target, index, .. } => Expr::Index { target, index, span },
        Expr::Member { target, field, .. } => Expr::Member { target, field, span },
        Expr::Assign { op, target, value, .. } => Expr::Assign { op, target, value, span },
        Expr::Ternary { cond, then_val, else_val, .. } => Expr::Ternary { cond, then_val, else_val, span },
    }
}
