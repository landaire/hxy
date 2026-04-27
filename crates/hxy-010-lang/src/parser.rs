//! Recursive-descent parser for 010 Binary Template files.
//!
//! Expressions use a Pratt-style precedence climb; declarations and
//! statements are parsed with straight-line recursive descent.
//! Consumes the [`Vec<Token>`] produced by [`tokenize`](crate::tokenize).

use thiserror::Error;

use crate::ast::AssignOp;
use crate::ast::Attr;
use crate::ast::Attrs;
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
            Some(t) => {
                Err(ParseError::Unexpected { expected: expected_msg, found: format!("{:?}", t.kind), span: t.span })
            }
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
            Some(t) => {
                Err(ParseError::Unexpected { expected: "identifier", found: format!("{:?}", t.kind), span: t.span })
            }
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

    /// Detect a C-style cast `(typename) ...` starting at the position
    /// just after a `(`. Returns the number of tokens to skip (1 for
    /// `(IDENT)`, 2 for `(IDENT IDENT)` like `unsigned long`) before
    /// the closing `)`. Returns `None` when the parens look like a
    /// regular grouping expression.
    ///
    /// The next token after the `)` must look like an expression
    /// starter (identifier, literal, `(`, prefix operator). That keeps
    /// `(x) + y` parsing as `(x) + y` rather than getting reinterpreted
    /// as a cast.
    fn detect_cast_type(&self) -> Option<usize> {
        let t0 = self.peek_at(0)?;
        // Single `(IDENT)` cast.
        let single = matches!(&t0.kind, TokenKind::Ident(_));
        // Compound `(unsigned long)` style: first ident is signed/
        // unsigned, second is the actual type. We check spelling
        // because `unsigned`/`signed` aren't keywords.
        let compound = match &t0.kind {
            TokenKind::Ident(name) if name == "unsigned" || name == "signed" => {
                matches!(self.peek_at(1).map(|t| &t.kind), Some(TokenKind::Ident(_)))
            }
            _ => false,
        };
        let skip = if compound {
            2
        } else if single {
            1
        } else {
            return None;
        };
        // After the type tokens: must be `)` followed by an
        // expression-starter.
        let close = self.peek_at(skip)?;
        if !matches!(close.kind, TokenKind::RParen) {
            return None;
        }
        let next = self.peek_at(skip + 1)?;
        let starts_expr = matches!(
            &next.kind,
            TokenKind::Ident(_)
                | TokenKind::Int(_)
                | TokenKind::Float(_)
                | TokenKind::String(_)
                | TokenKind::Char(_)
                | TokenKind::LParen
                | TokenKind::Minus
                | TokenKind::Plus
                | TokenKind::Bang
                | TokenKind::Tilde
                | TokenKind::PlusPlus
                | TokenKind::MinusMinus
                | TokenKind::Keyword(Keyword::True | Keyword::False | Keyword::Sizeof)
        );
        if !starts_expr {
            return None;
        }
        Some(skip)
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
        if !matches!(self.peek_at(2).map(|t| &t.kind), Some(TokenKind::LParen)) {
            return false;
        }
        // Discriminate function def / prototype from a field decl
        // that uses parameterised-struct args: `Type name(args) { ... }`
        // is a function definition, `Type name(args);` is a function
        // *prototype* (we treat it as a no-op decl), and
        // `Type name(args)[count];` is a parameterised-struct field.
        // Walk to the matching `)` and inspect the next token.
        let mut depth = 1usize;
        let mut idx = 3usize;
        while let Some(tok) = self.peek_at(idx) {
            match tok.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        idx += 1;
                        break;
                    }
                }
                _ => {}
            }
            idx += 1;
        }
        if depth != 0 {
            return false;
        }
        let after = self.peek_at(idx).map(|t| &t.kind);
        let is_def_body = matches!(after, Some(TokenKind::LBrace));
        // `void` return + `;` is unambiguously a function prototype
        // (a field decl can't have type `void`). Non-void prototypes
        // collide with parameterised-struct field decls (`Record
        // items(head.fileLen);`); for those we only commit to the
        // function path when the args look like a typed parameter
        // list -- the heuristic is "first arg is two adjacent
        // identifiers, optionally separated by `&`", matching
        // `Type name` and `Type &name`.
        let is_void_return = matches!(&self.peek_at(0).map(|t| &t.kind), Some(TokenKind::Keyword(Keyword::Void)));
        let is_proto = matches!(after, Some(TokenKind::Semi)) && (is_void_return || self.first_arg_looks_like_param());
        is_def_body || is_proto
    }

    /// Tail of the function-def heuristic: starting just inside the
    /// parameter list `(`, does the first argument shape look like a
    /// typed parameter (`Type name`, `Type &name`, `const Type &name`)
    /// rather than an expression? Used to distinguish
    /// `int SeekBlock(uint64 block_id);` (prototype) from
    /// `Record items(head.fileLen);` (parameterised-struct field decl).
    fn first_arg_looks_like_param(&self) -> bool {
        let mut i = 3; // tokens after `Type name (`
        // Optional `local` / `const` qualifier on the first param.
        if matches!(
            self.peek_at(i).map(|t| &t.kind),
            Some(TokenKind::Keyword(Keyword::Local)) | Some(TokenKind::Keyword(Keyword::Const))
        ) {
            i += 1;
        }
        // Optional `struct` / `union` / `enum` tag on the type.
        if matches!(
            self.peek_at(i).map(|t| &t.kind),
            Some(TokenKind::Keyword(Keyword::Struct))
                | Some(TokenKind::Keyword(Keyword::Union))
                | Some(TokenKind::Keyword(Keyword::Enum))
        ) {
            i += 1;
        }
        // The type identifier itself, with the same `unsigned T` /
        // `signed T` slack we accept in the field-decl path.
        if let Some(TokenKind::Ident(name)) = self.peek_at(i).map(|t| &t.kind)
            && (name == "unsigned" || name == "signed")
        {
            i += 1;
        }
        if !matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::Ident(_))) {
            return false;
        }
        i += 1;
        // Optional reference marker.
        if matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::Amp)) {
            i += 1;
        }
        // Param name.
        if !matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::Ident(_))) {
            return false;
        }
        i += 1;
        // After the param: comma (more params) or `)` (end of list).
        matches!(self.peek_at(i).map(|t| &t.kind), Some(TokenKind::Comma) | Some(TokenKind::RParen))
    }

    fn parse_function_def(&mut self) -> Result<FunctionDef, ParseError> {
        let start = self.peek().map(|t| t.span.start).unwrap_or(0);
        let return_type = self.parse_type_ref()?;
        let (name, _) = self.expect_ident()?;
        self.expect_kind(&TokenKind::LParen, "(")?;
        let mut params = Vec::new();
        // C convention: `f(void)` declares no params.
        let void_only = matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Void)))
            && matches!(self.peek_at(1).map(|t| &t.kind), Some(TokenKind::RParen));
        if !void_only && !matches!(self.peek_kind(), Some(TokenKind::RParen)) {
            loop {
                params.push(self.parse_param()?);
                if !self.eat_kind(&TokenKind::Comma) {
                    break;
                }
            }
        }
        if void_only {
            self.bump(); // skip the `void`
        }
        self.expect_kind(&TokenKind::RParen, ")")?;
        // Function prototype: `void f(int x);`. Consume the trailing
        // `;` and emit an empty body. The interpreter will resolve
        // calls against whichever prototype OR definition shows up
        // last in the source -- a bare prototype just means the
        // identifier is registered but never executes anything.
        if matches!(self.peek_kind(), Some(TokenKind::Semi)) {
            let end_tok = self.bump().unwrap();
            return Ok(FunctionDef {
                return_type,
                name,
                params,
                body: Vec::new(),
                span: Span::new(start, end_tok.span.end),
            });
        }
        let body_block = self.parse_block()?;
        let end = self.last_span().end;
        let Stmt::Block { stmts, .. } = body_block else { unreachable!() };
        Ok(FunctionDef { return_type, name, params, body: stmts, span: Span::new(start, end) })
    }

    fn parse_param(&mut self) -> Result<Param, ParseError> {
        // 010 lets function params carry a `local` / `const` qualifier
        // ahead of the type -- e.g. `string readFoo(local CTYPE &t)`.
        // The qualifier is semantic sugar for us; accept and discard
        // either one.
        let _ = self.eat_keyword(Keyword::Local) || self.eat_keyword(Keyword::Const);
        // `struct ar_file &f` / `enum KIND k` -- skip the leading
        // tag keyword; the identifier that follows is the actual
        // type name. (010 templates inherit the C convention.)
        let _ =
            self.eat_keyword(Keyword::Struct) || self.eat_keyword(Keyword::Union) || self.eat_keyword(Keyword::Enum);
        let ty = self.parse_type_ref()?;
        let is_ref = self.eat_kind(&TokenKind::Amp);
        let (name, name_span) = self.expect_ident()?;
        // Trailing `[]` on a param: C array-decay. Templates write
        // `void f(char buf[])` to mean "buf is a pointer/array";
        // there's nothing for the interpreter to do with it.
        if self.eat_kind(&TokenKind::LBracket) {
            // Optional length expression -- `char buf[N]` is the
            // same shape as a regular array param. Drop whatever
            // dimension specifier the template gave.
            if !matches!(self.peek_kind(), Some(TokenKind::RBracket)) {
                let _ = self.parse_expr()?;
            }
            self.expect_kind(&TokenKind::RBracket, "]")?;
        }
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
            TokenKind::Keyword(Keyword::Switch) => self.parse_switch(),
            TokenKind::Keyword(Keyword::Union) => self.parse_struct_stmt(),
            TokenKind::Keyword(Keyword::Local) | TokenKind::Keyword(Keyword::Const) => self.parse_field_decl(None),
            TokenKind::Semi => {
                // Empty statement -- consume and treat as a no-op block.
                let t = self.bump().unwrap();
                Ok(Stmt::Block { stmts: Vec::new(), span: t.span })
            }
            TokenKind::Keyword(Keyword::Struct) => self.parse_struct_stmt(),
            TokenKind::Keyword(Keyword::Enum) => {
                // Bare `enum` outside a typedef has three shapes:
                //   1. `enum Name { ... };` -- pure type declaration
                //   2. `enum <backing>? Name? { ... } ident;` -- inline
                //       enum used as a field type
                //   3. `enum Name field;` -- field whose type is a
                //       previously-declared enum
                //
                // Peek past the optional tag: if the next significant
                // token is `{` or `<`, it's form 1 or 2; otherwise
                // we're looking at a field declaration.
                if self.is_enum_type_decl_start() {
                    self.parse_inline_enum_field()
                } else if self.looks_like_field_decl() {
                    self.parse_field_decl(None)
                } else {
                    self.parse_expr_stmt()
                }
            }
            TokenKind::Ident(_) => {
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
    /// Token lookahead for `enum [<backing>] [Tag] {`. Distinguishes
    /// an enum type declaration from `enum Name fieldName;`.
    fn is_enum_type_decl_start(&self) -> bool {
        // We're positioned on `enum`. Skip the keyword, then an
        // optional `< ... >` backing clause, then an optional tag
        // identifier, and check for `{`.
        let mut offset = 1;
        if matches!(self.peek_at(offset).map(|t| &t.kind), Some(TokenKind::Lt)) {
            // Scan to the matching `>` -- backing type refs are simple
            // (one identifier), so the next-next token is the closer.
            offset += 1;
            while let Some(t) = self.peek_at(offset) {
                offset += 1;
                if matches!(t.kind, TokenKind::Gt) {
                    break;
                }
                if offset > 8 {
                    return false;
                }
            }
        }
        if matches!(self.peek_at(offset).map(|t| &t.kind), Some(TokenKind::Ident(_))) {
            offset += 1;
        }
        matches!(self.peek_at(offset).map(|t| &t.kind), Some(TokenKind::LBrace))
    }

    fn looks_like_field_decl(&self) -> bool {
        let Some(t0) = self.peek() else { return false };
        if !matches!(
            &t0.kind,
            TokenKind::Ident(_) | TokenKind::Keyword(Keyword::Struct) | TokenKind::Keyword(Keyword::Enum)
        ) {
            return false;
        }
        // `Type ident ...` -- normal declaration.
        // `Type : N;`     -- anonymous bitfield padding.
        matches!(self.peek_at(1).map(|t| &t.kind), Some(TokenKind::Ident(_) | TokenKind::Colon))
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
        let else_branch = if self.eat_keyword(Keyword::Else) { Some(Box::new(self.parse_stmt()?)) } else { None };
        let end = else_branch.as_ref().map(|s| stmt_span(s).end).unwrap_or_else(|| stmt_span(&then_branch).end);
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
        let cond = if matches!(self.peek_kind(), Some(TokenKind::Semi)) { None } else { Some(self.parse_expr()?) };
        self.expect_kind(&TokenKind::Semi, ";")?;
        let step = if matches!(self.peek_kind(), Some(TokenKind::RParen)) { None } else { Some(self.parse_expr()?) };
        self.expect_kind(&TokenKind::RParen, ")")?;
        let body = Box::new(self.parse_stmt()?);
        let end = stmt_span(&body).end;
        Ok(Stmt::For { init, cond, step, body, span: Span::new(kw.span.start, end) })
    }

    fn parse_return(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap();
        let value = if matches!(self.peek_kind(), Some(TokenKind::Semi)) { None } else { Some(self.parse_expr()?) };
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

        // `typedef enum <backing>? [Tag] { ... } [Alias] <attrs>?;`
        //
        // Either tag-before-body or alias-after-body is accepted;
        // when both are given we prefer the tag because that's the
        // name by which the type is referred to in the rest of the
        // source. Anonymous + alias also works (`typedef enum { ... } X;`).
        if self.eat_keyword(Keyword::Enum) {
            let backing = if self.eat_kind(&TokenKind::Lt) {
                let t = self.parse_type_ref()?;
                self.expect_kind(&TokenKind::Gt, ">")?;
                Some(t)
            } else {
                None
            };
            let tag =
                if matches!(self.peek_kind(), Some(TokenKind::Ident(_))) { Some(self.expect_ident()?.0) } else { None };
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
            let alias =
                if matches!(self.peek_kind(), Some(TokenKind::Ident(_))) { Some(self.expect_ident()?.0) } else { None };
            let (name, alias_for_extra) = match (tag.clone(), alias.clone()) {
                (Some(t), alias_opt) => (t, alias_opt),
                (None, Some(a)) => (a, None),
                (None, None) => {
                    return Err(ParseError::Unexpected {
                        expected: "enum tag or typedef alias",
                        found: "neither".into(),
                        span: Span::new(start, start),
                    });
                }
            };
            let attrs = self.parse_optional_attrs()?;
            let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
            let span = Span::new(start, end_tok.span.end);
            let main = Stmt::TypedefEnum(EnumDecl { name: name.clone(), backing, variants, attrs, span });
            // Both the tag and the typedef alias should resolve to
            // the same type. We register the tag as the primary
            // declaration and emit an alias stmt for each extra
            // name so the interpreter's type registry picks up both.
            if let Some(extra) = alias_for_extra {
                return Ok(Stmt::Block {
                    stmts: vec![main, Stmt::TypedefAlias { new_name: extra, source: TypeRef { name, span }, array_size: None, span }],
                    span,
                });
            }
            return Ok(main);
        }

        // `typedef struct [Tag] [(params)] { ... } [Alias] <attrs>?;`
        // `typedef union  [Tag] { ... } [Alias] <attrs>?;`
        if matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Struct) | TokenKind::Keyword(Keyword::Union))) {
            let is_union = matches!(self.peek_kind(), Some(TokenKind::Keyword(Keyword::Union)));
            self.bump();
            let tag =
                if matches!(self.peek_kind(), Some(TokenKind::Ident(_))) { Some(self.expect_ident()?.0) } else { None };
            // Forward declaration -- `typedef struct Name;` with no body.
            // Templates use this to allow recursive types (e.g. msgpack
            // values that nest themselves). Treat as a no-op decl: the
            // real definition appears later in the file.
            if let Some(t) = &tag
                && matches!(self.peek_kind(), Some(TokenKind::Semi))
            {
                let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
                let span = Span::new(start, end_tok.span.end);
                return Ok(Stmt::TypedefStruct(StructDecl {
                    name: t.clone(),
                    params: Vec::new(),
                    body: Vec::new(),
                    attrs: Default::default(),
                    is_union,
                    span,
                }));
            }
            // `typedef struct TAG NAME;` (no body) -- alias `NAME` to
            // the previously-declared struct `TAG`. Used by templates
            // that introduce a thin renaming for a shared underlying
            // shape (e.g. dex's `typedef struct uleb128 uleb128p1;`).
            if let Some(t) = &tag
                && matches!(self.peek_kind(), Some(TokenKind::Ident(_)))
            {
                let alias_name = self.expect_ident()?.0;
                let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
                let span = Span::new(start, end_tok.span.end);
                return Ok(Stmt::TypedefAlias {
                    new_name: alias_name,
                    source: TypeRef { name: t.clone(), span },
                    array_size: None,
                    span,
                });
            }
            let params = self.parse_optional_struct_params()?;
            let body_block = self.parse_block()?;
            let Stmt::Block { stmts: body, .. } = body_block else { unreachable!() };
            let alias =
                if matches!(self.peek_kind(), Some(TokenKind::Ident(_))) { Some(self.expect_ident()?.0) } else { None };
            let (name, alias_for_extra) = match (tag.clone(), alias.clone()) {
                (Some(t), alias_opt) => (t, alias_opt),
                (None, Some(a)) => (a, None),
                (None, None) => {
                    return Err(ParseError::Unexpected {
                        expected: "struct tag or typedef alias",
                        found: "neither".into(),
                        span: Span::new(start, start),
                    });
                }
            };
            let attrs = self.parse_optional_attrs()?;
            let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
            let span = Span::new(start, end_tok.span.end);
            let main = Stmt::TypedefStruct(StructDecl { name: name.clone(), params, body, attrs, is_union, span });
            if let Some(extra) = alias_for_extra {
                return Ok(Stmt::Block {
                    stmts: vec![main, Stmt::TypedefAlias { new_name: extra, source: TypeRef { name, span }, array_size: None, span }],
                    span,
                });
            }
            return Ok(main);
        }

        // `typedef SourceType NewName [array_size]? <attrs>?;`
        //
        // 010 allows a typedef alias to carry an optional array size
        // and / or attribute list -- e.g.
        // `typedef CHAR DIGEST[20] <read=formatDigest>;`. The array
        // size matters: `DIGEST x;` reads 20 chars, not one. We
        // capture the size into the AST so the interpreter can
        // re-attach it when the alias is used as a field type.
        let source = self.parse_type_ref()?;
        let (new_name, name_span) = self.expect_ident()?;
        let array_size = if self.eat_kind(&TokenKind::LBracket) {
            let expr = self.parse_expr()?;
            self.expect_kind(&TokenKind::RBracket, "]")?;
            Some(expr)
        } else {
            None
        };
        let _ = self.parse_optional_attrs()?;
        let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
        Ok(Stmt::TypedefAlias {
            new_name,
            source,
            array_size,
            span: Span::new(start, end_tok.span.end.max(name_span.end)),
        })
    }

    /// Parse `[N]` if the next token is `[`. Returns `None` when no
    /// bracket follows; returns `Some(IntLit 0)` for the empty `[]`
    /// idiom (open-ended array). Otherwise the bracket holds an
    /// arbitrary expression.
    fn parse_optional_array_dim(&mut self) -> Result<Option<Expr>, ParseError> {
        if !self.eat_kind(&TokenKind::LBracket) {
            return Ok(None);
        }
        if matches!(self.peek_kind(), Some(TokenKind::RBracket)) {
            let close = self.bump().unwrap();
            return Ok(Some(Expr::IntLit { value: 0, span: close.span }));
        }
        let expr = self.parse_expr()?;
        self.expect_kind(&TokenKind::RBracket, "]")?;
        Ok(Some(expr))
    }

    /// Parse an optional parenthesised parameter list on a struct
    /// definition: `struct Name (int32 len, int32 kind) { ... }`.
    /// Returns an empty vector when no `(` follows the struct name.
    fn parse_optional_struct_params(&mut self) -> Result<Vec<Param>, ParseError> {
        if !self.eat_kind(&TokenKind::LParen) {
            return Ok(Vec::new());
        }
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
        Ok(params)
    }

    /// Parse `enum <backing>? [Tag] { variants } ident [<attrs>];` when
    /// it appears outside a `typedef`. Emits a synthetic
    /// [`Stmt::TypedefEnum`] + [`Stmt::FieldDecl`] bundled in a
    /// [`Stmt::Block`] so the interpreter registers the type before
    /// reading the field.
    fn parse_inline_enum_field(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `enum`
        let start = kw.span.start;
        let backing = if self.eat_kind(&TokenKind::Lt) {
            let t = self.parse_type_ref()?;
            self.expect_kind(&TokenKind::Gt, ">")?;
            Some(t)
        } else {
            None
        };
        let tag = if matches!(self.peek_kind(), Some(TokenKind::Ident(_))) {
            // Peek further to tell a tag apart from the field name:
            // if the next-next token is `{`, the ident is a tag.
            let next_is_brace = matches!(self.peek_at(1).map(|t| &t.kind), Some(TokenKind::LBrace));
            if next_is_brace { Some(self.expect_ident()?.0) } else { None }
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
        let close = self.expect_kind(&TokenKind::RBrace, "}")?;
        let has_tag = tag.is_some();
        let anon_name = tag.unwrap_or_else(|| format!("__anon_enum_{}", close.span.end));
        let enum_stmt = Stmt::TypedefEnum(EnumDecl {
            name: anon_name.clone(),
            backing,
            variants,
            attrs: Attrs::default(),
            span: Span::new(start, close.span.end),
        });
        // `enum Name { ... };` (or even `enum { ... };` with no tag,
        // which is rare but legal) is a pure type declaration.
        // No field is emitted in either case; the tag, if any, is
        // what the interpreter registers.
        if matches!(self.peek_kind(), Some(TokenKind::Semi)) {
            let end_tok = self.bump().unwrap();
            return Ok(Stmt::Block { stmts: vec![enum_stmt], span: Span::new(start, end_tok.span.end) });
        }
        let _ = has_tag;
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
            args: Vec::new(),
            bit_width: None,
            init: None,
            attrs: field_attrs,
            span: Span::new(start, end_tok.span.end),
        };
        Ok(Stmt::Block { stmts: vec![enum_stmt, field_stmt], span: Span::new(start, end_tok.span.end) })
    }

    fn parse_switch(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `switch`
        self.expect_kind(&TokenKind::LParen, "(")?;
        let scrutinee = self.parse_expr()?;
        self.expect_kind(&TokenKind::RParen, ")")?;
        self.expect_kind(&TokenKind::LBrace, "{")?;
        let mut arms: Vec<crate::ast::SwitchArm> = Vec::new();
        while !matches!(self.peek_kind(), Some(TokenKind::RBrace) | None) {
            let arm_start = self.peek().map(|t| t.span.start).unwrap_or(0);
            let pattern = if self.eat_keyword(Keyword::Case) {
                let pat = self.parse_expr()?;
                self.expect_kind(&TokenKind::Colon, ":")?;
                Some(pat)
            } else if self.eat_keyword(Keyword::Default) {
                self.expect_kind(&TokenKind::Colon, ":")?;
                None
            } else {
                return Err(ParseError::Unexpected {
                    expected: "case or default",
                    found: self.peek().map(|t| format!("{:?}", t.kind)).unwrap_or_else(|| "eof".into()),
                    span: self.peek().map(|t| t.span).unwrap_or(Span::new(arm_start, arm_start)),
                });
            };
            let mut body = Vec::new();
            while !matches!(
                self.peek_kind(),
                Some(TokenKind::Keyword(Keyword::Case))
                    | Some(TokenKind::Keyword(Keyword::Default))
                    | Some(TokenKind::RBrace)
                    | None
            ) {
                body.push(self.parse_stmt()?);
            }
            let end = self.last_span().end;
            arms.push(crate::ast::SwitchArm { pattern, body, span: Span::new(arm_start, end) });
        }
        let close = self.expect_kind(&TokenKind::RBrace, "}")?;
        Ok(Stmt::Switch { scrutinee, arms, span: Span::new(kw.span.start, close.span.end) })
    }

    /// Handle the `struct` / `union` keywords outside a `typedef`.
    /// Four shapes:
    ///   1. `struct Name { body };` -- type declaration
    ///   2. `struct Name (params) { body };` -- parameterised type decl
    ///   3. `struct { body } field_name;` -- anonymous struct as a field type
    ///   4. `struct Name field_name;` -- field whose type is an already-declared struct
    fn parse_struct_stmt(&mut self) -> Result<Stmt, ParseError> {
        let kw = self.bump().unwrap(); // `struct` or `union`
        let is_union = matches!(kw.kind, TokenKind::Keyword(Keyword::Union));
        let start = kw.span.start;

        match self.peek_kind() {
            // `struct Name ...`
            Some(TokenKind::Ident(_)) => {
                let (name, name_span) = self.expect_ident()?;
                // A `(` after the name is always a param list on a
                // definition -- field decls can't take args on a bare
                // `struct Name x(args)` form because the struct type
                // itself would already need to have been parsed.
                // Forward declaration: `struct Name;` introduces the
                // tag without a body. Templates use this as a hint to
                // the editor; for our purposes it's a no-op (the
                // matching `typedef struct { ... } Name` later in the
                // file is what registers the type).
                if matches!(self.peek_kind(), Some(TokenKind::Semi)) {
                    let end_tok = self.bump().unwrap();
                    return Ok(Stmt::Block { stmts: Vec::new(), span: Span::new(start, end_tok.span.end) });
                }
                if matches!(self.peek_kind(), Some(TokenKind::LParen)) {
                    let params = self.parse_optional_struct_params()?;
                    let body_block = self.parse_block()?;
                    let Stmt::Block { stmts, .. } = body_block else { unreachable!() };
                    let attrs = self.parse_optional_attrs()?;
                    let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
                    return Ok(Stmt::TypedefStruct(StructDecl {
                        name,
                        params,
                        body: stmts,
                        attrs,
                        is_union,
                        span: Span::new(start, end_tok.span.end),
                    }));
                }
                if matches!(self.peek_kind(), Some(TokenKind::LBrace)) {
                    // Form 1: `struct Name { body } [<attrs>];`
                    // Form 1b: `struct Name { body } field [array] [<attrs>];`
                    //          -- inline def + instance (common in
                    //          XEX2Headers.bt).
                    let body_block = self.parse_block()?;
                    let Stmt::Block { stmts, span: body_span } = body_block else { unreachable!() };
                    // If an identifier follows the body, this is
                    // Form 1b: define the type, then immediately
                    // declare a field of that type.
                    if matches!(self.peek_kind(), Some(TokenKind::Ident(_))) {
                        let struct_stmt = Stmt::TypedefStruct(StructDecl {
                            name: name.clone(),
                            params: Vec::new(),
                            body: stmts,
                            attrs: Attrs::default(),
                            is_union,
                            span: Span::new(start, body_span.end),
                        });
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
                            ty: TypeRef { name, span: field_name_span },
                            name: field_name,
                            array_size,
                            args: Vec::new(),
                            bit_width: None,
                            init: None,
                            attrs: field_attrs,
                            span: Span::new(start, end_tok.span.end),
                        };
                        return Ok(Stmt::Block {
                            stmts: vec![struct_stmt, field_stmt],
                            span: Span::new(start, end_tok.span.end),
                        });
                    }
                    let attrs = self.parse_optional_attrs()?;
                    let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
                    Ok(Stmt::TypedefStruct(StructDecl {
                        name,
                        params: Vec::new(),
                        body: stmts,
                        attrs,
                        is_union,
                        span: Span::new(start, end_tok.span.end),
                    }))
                } else {
                    // Form 4: `struct Name field_name [array] [<attrs>];`
                    let ty = TypeRef { name, span: name_span };
                    self.parse_field_decl(Some(ty))
                }
            }
            // Form 3: `struct { body } field_name ...;`
            Some(TokenKind::LBrace) => {
                // Give the anonymous struct a synthetic name so the
                // AST remains a strict subset of the named form. The
                // name lives inside the declaration scope only.
                let body_block = self.parse_block()?;
                let Stmt::Block { stmts, span: body_span } = body_block else { unreachable!() };
                let anon_name = format!("__anon_struct_{}", body_span.start);
                let struct_attrs = self.parse_optional_attrs()?;
                let decl_span = Span::new(start, self.last_span().end);
                let struct_stmt = Stmt::TypedefStruct(StructDecl {
                    name: anon_name.clone(),
                    params: Vec::new(),
                    body: stmts,
                    attrs: struct_attrs,
                    is_union,
                    span: decl_span,
                });
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
                    args: Vec::new(),
                    bit_width: None,
                    init: None,
                    attrs: field_attrs,
                    span: Span::new(start, end_tok.span.end),
                };
                Ok(Stmt::Block { stmts: vec![struct_stmt, field_stmt], span: Span::new(start, end_tok.span.end) })
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
        let value = if self.eat_kind(&TokenKind::Eq) { Some(self.parse_expr()?) } else { None };
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
    ///
    /// Supports parameterised struct instantiation:
    /// `PNG_CHUNK_PLTE plte(length);` -- the args after the field name
    /// are bound to the struct's declared params at execute time.
    fn parse_field_decl(&mut self, ty_override: Option<TypeRef>) -> Result<Stmt, ParseError> {
        let start = self.peek().map(|t| t.span.start).unwrap_or(0);
        // `local`, `const`, or both (in either order). 010 templates
        // routinely combine the two -- e.g. `local const double X`,
        // `const local DWORD Y` -- so accept any prefix sequence,
        // collapsing to the most-restrictive modifier seen.
        let mut modifier = DeclModifier::Field;
        loop {
            if self.eat_keyword(Keyword::Local) {
                if !matches!(modifier, DeclModifier::Const) {
                    modifier = DeclModifier::Local;
                }
            } else if self.eat_keyword(Keyword::Const) {
                modifier = DeclModifier::Const;
            } else {
                break;
            }
        }
        let ty = match ty_override {
            Some(t) => t,
            None => self.parse_type_ref()?,
        };
        // Anonymous bitfield: `DWORD : 22;` reserves bits without
        // giving them a name. Accept it here and synthesize a unique
        // internal name so the rest of the decl pipeline stays simple.
        let (name, _) = if matches!(self.peek_kind(), Some(TokenKind::Colon)) {
            let anon_span = self.peek().map(|t| t.span).unwrap_or_else(|| Span::new(start, start));
            (format!("__anon_bitfield_{}", anon_span.start), anon_span)
        } else {
            self.expect_ident()?
        };
        let mut array_size = self.parse_optional_array_dim()?;
        let args = if self.eat_kind(&TokenKind::LParen) {
            let mut out = Vec::new();
            if !matches!(self.peek_kind(), Some(TokenKind::RParen)) {
                loop {
                    out.push(self.parse_expr_bp(1)?);
                    if !self.eat_kind(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect_kind(&TokenKind::RParen, ")")?;
            out
        } else {
            Vec::new()
        };
        // Trailing array dim after parameterised-struct args:
        // `EFI_PARTITION_ENTRY partitions(size)[count];`.
        if array_size.is_none() {
            array_size = self.parse_optional_array_dim()?;
        }
        // Multi-dimensional arrays: `int x[5][3];`. We only model a
        // single dim, so the inner dim is parsed and dropped (the
        // outer count is what the field stream reads).
        while matches!(self.peek_kind(), Some(TokenKind::LBracket)) {
            let _ = self.parse_optional_array_dim()?;
        }
        // C-style bitfield: `DWORD flag : 3;` packs `flag` into the
        // low / high 3 bits of the next shared DWORD slot.
        let bit_width = if self.eat_kind(&TokenKind::Colon) { Some(self.parse_expr_bp(BITFIELD_BP)?) } else { None };
        let attrs = self.parse_optional_attrs()?;
        let init = if self.eat_kind(&TokenKind::Eq) { Some(self.parse_expr()?) } else { None };

        let first_decl = Stmt::FieldDecl {
            modifier,
            ty: ty.clone(),
            name,
            array_size,
            args,
            bit_width,
            init,
            attrs,
            span: Span::new(start, self.last_span().end),
        };

        // C-style comma-separated declarators: `local int x, headerLen;`
        // shares the type prefix across names. Each additional name
        // parses its own optional array / attrs / initializer and
        // becomes its own [`Stmt::FieldDecl`]; the caller gets a
        // [`Stmt::Block`] bundle so a single call site hands back a
        // single AST node.
        let mut decls = vec![first_decl];
        while self.eat_kind(&TokenKind::Comma) {
            let decl_start = self.peek().map(|t| t.span.start).unwrap_or(start);
            let (next_name, _) = if matches!(self.peek_kind(), Some(TokenKind::Colon)) {
                let sp = self.peek().map(|t| t.span).unwrap_or_else(|| Span::new(decl_start, decl_start));
                (format!("__anon_bitfield_{}", sp.start), sp)
            } else {
                self.expect_ident()?
            };
            let next_array_size = self.parse_optional_array_dim()?;
            // Drop additional dims for multi-dim declarators here too.
            while matches!(self.peek_kind(), Some(TokenKind::LBracket)) {
                let _ = self.parse_optional_array_dim()?;
            }
            let next_args = if self.eat_kind(&TokenKind::LParen) {
                let mut out = Vec::new();
                if !matches!(self.peek_kind(), Some(TokenKind::RParen)) {
                    loop {
                        out.push(self.parse_expr_bp(1)?);
                        if !self.eat_kind(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect_kind(&TokenKind::RParen, ")")?;
                out
            } else {
                Vec::new()
            };
            let next_bit_width =
                if self.eat_kind(&TokenKind::Colon) { Some(self.parse_expr_bp(BITFIELD_BP)?) } else { None };
            let next_attrs = self.parse_optional_attrs()?;
            let next_init = if self.eat_kind(&TokenKind::Eq) { Some(self.parse_expr()?) } else { None };
            decls.push(Stmt::FieldDecl {
                modifier,
                ty: ty.clone(),
                name: next_name,
                array_size: next_array_size,
                args: next_args,
                bit_width: next_bit_width,
                init: next_init,
                attrs: next_attrs,
                span: Span::new(decl_start, self.last_span().end),
            });
        }

        let end_tok = self.expect_kind(&TokenKind::Semi, ";")?;
        if decls.len() == 1 {
            return Ok(decls.into_iter().next().unwrap());
        }
        Ok(Stmt::Block { stmts: decls, span: Span::new(start, end_tok.span.end) })
    }

    fn parse_type_ref(&mut self) -> Result<TypeRef, ParseError> {
        if self.eat_keyword(Keyword::Void) {
            let span = self.tokens[self.pos - 1].span;
            return Ok(TypeRef { name: "void".into(), span });
        }
        // C-style sign modifier: `unsigned T` / `signed T`. 010
        // doesn't have a true `unsigned` keyword; templates that
        // write it expect us to look at `T`. Drop the modifier and
        // trust the underlying type name.
        if let Some(TokenKind::Ident(name)) = self.peek_kind()
            && (name == "unsigned" || name == "signed")
            && matches!(self.peek_at(1).map(|t| &t.kind), Some(TokenKind::Ident(_)))
        {
            self.bump();
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
                // sizeof ( expr | typename ). Templates write
                // `sizeof(struct FIELD)` or `sizeof(union U)` with
                // a tag keyword; skip it -- the inner ident is the
                // type name we want.
                self.expect_kind(&TokenKind::LParen, "(")?;
                let _ = self.eat_keyword(Keyword::Struct)
                    || self.eat_keyword(Keyword::Union)
                    || self.eat_keyword(Keyword::Enum);
                let inner = self.parse_expr()?;
                let close = self.expect_kind(&TokenKind::RParen, ")")?;
                // Model as a call to a magic identifier; the
                // interpreter resolves it specially.
                let callee = Expr::Ident { name: "sizeof".into(), span: tok.span };
                Ok(Expr::Call {
                    callee: Box::new(callee),
                    args: vec![inner],
                    span: Span::new(tok.span.start, close.span.end),
                })
            }
            TokenKind::LBrace => {
                // Brace-list initializer: `{1, 2, 3}` for a fixed
                // local array. We don't have a dedicated array
                // literal AST node, so collapse to the first element
                // for now -- templates that branch on the array
                // contents see only the first value. The rest of the
                // declaration parses cleanly and the field shows up
                // in the tree.
                self.bump();
                let mut first: Option<Expr> = None;
                if !matches!(self.peek_kind(), Some(TokenKind::RBrace)) {
                    first = Some(self.parse_expr_bp(1)?);
                    while self.eat_kind(&TokenKind::Comma) {
                        if matches!(self.peek_kind(), Some(TokenKind::RBrace)) {
                            break;
                        }
                        let _ = self.parse_expr_bp(1)?;
                    }
                }
                let close = self.expect_kind(&TokenKind::RBrace, "}")?;
                let span = Span::new(tok.span.start, close.span.end);
                Ok(first.map(|e| with_span(e, span)).unwrap_or(Expr::IntLit { value: 0, span }))
            }
            TokenKind::LParen => {
                self.bump();
                // C-style cast detection: `(typename) expr`. Loose
                // heuristic -- look for an identifier (or
                // `unsigned`/`signed` modifier + identifier) inside
                // the parens, followed immediately by `)` and an
                // expression-starter token. The cast is dropped (we
                // emit just the inner expression); 010's interpreter
                // is loose enough about types that `(uint64)x` and
                // `x` produce the same numeric value for the cases
                // templates actually care about.
                if let Some(skip) = self.detect_cast_type() {
                    for _ in 0..skip {
                        self.bump();
                    }
                    self.expect_kind(&TokenKind::RParen, ")")?;
                    let target = self.parse_expr_bp(PREFIX_BP)?;
                    let span = Span::new(tok.span.start, target.span().end);
                    return Ok(with_span(target, span));
                }
                let inner = self.parse_expr()?;
                let close = self.expect_kind(&TokenKind::RParen, ")")?;
                // Discard the parens -- the AST doesn't need them,
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
            _ => Err(ParseError::NotAnExpression { found: format!("{:?}", tok.kind), span: tok.span }),
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

/// Binding power of a prefix operator like unary `-` / `!` -- higher
/// than any infix so `-a * b` parses as `(-a) * b`.
const PREFIX_BP: u8 = 30;

/// Minimum binding power used when parsing attribute values. Keeps
/// `>` (attr closer) and `,` (attr separator) from being consumed as
/// binary operators. Postfix operators (call, index, member access)
/// still bind because they bypass the `min_bp` check entirely.
const ATTR_VALUE_BP: u8 = 40;

/// Min binding power for parsing a bitfield width expression. Lower
/// than `ATTR_VALUE_BP` so arithmetic like `: NumBits + 2` is folded
/// into the width, but high enough to leave the comparison level
/// (`<` / `>`) alone so a trailing `<attrs>` keeps its meaning.
const BITFIELD_BP: u8 = 17;

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
        | Stmt::Switch { span, .. }
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
