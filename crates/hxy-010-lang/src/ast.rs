//! AST for 010 Editor Binary Template sources.
//!
//! Every node carries a [`Span`] pointing back into the original
//! source so diagnostics and the interpreter-emitted tree can refer
//! to the offending text. The AST stays close to the surface syntax
//! — semantic distinctions like "is this a type name or a variable"
//! are deferred to later passes.

use crate::token::Span;

/// One 010 template file.
#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    pub items: Vec<TopItem>,
}

/// Top-level item in a template: either a statement (including decls)
/// or a function definition. Templates execute their top-level
/// statements sequentially, so the split exists only to keep function
/// definitions — which don't execute inline — out of the main stream.
#[derive(Clone, Debug, PartialEq)]
pub enum TopItem {
    Stmt(Stmt),
    Function(FunctionDef),
}

/// A `type-name[<attrs>]` reference. 010 distinguishes signed/unsigned
/// primitive names by spelling; we don't interpret them here.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeRef {
    pub name: String,
    pub span: Span,
}

/// Trailing `<key=expr, ...>` attribute list on a declaration or
/// type. The keys are arbitrary identifiers; we don't validate them
/// at parse time because 010 doesn't either — unknown attributes are
/// harmless, they just don't drive any display behaviour.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Attrs(pub Vec<Attr>);

#[derive(Clone, Debug, PartialEq)]
pub struct Attr {
    pub key: String,
    pub value: Expr,
    pub span: Span,
}

/// `typedef enum <backing> { Variants } Name <attrs>;`
#[derive(Clone, Debug, PartialEq)]
pub struct EnumDecl {
    pub name: String,
    pub backing: Option<TypeRef>,
    pub variants: Vec<EnumVariant>,
    pub attrs: Attrs,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumVariant {
    pub name: String,
    pub value: Option<Expr>,
    pub span: Span,
}

/// `typedef struct { body } Name <attrs>;` or an inline `struct Name { body }`.
///
/// `params` is populated for parameterised structs — the form
/// `struct Name (int32 len) { ... }` — and empty otherwise. Each field
/// declaration that references a parameterised struct must pass a
/// matching positional arg list; see [`FieldDecl::args`].
#[derive(Clone, Debug, PartialEq)]
pub struct StructDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub attrs: Attrs,
    /// `true` when the declaration was a `union`. Unions share the
    /// struct-decl shape because their syntax is identical; the
    /// interpreter treats fields as overlapping at the start offset.
    pub is_union: bool,
    pub span: Span,
}

/// Named function: `ret-type Name ( params ) { body }`.
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionDef {
    pub return_type: TypeRef,
    pub name: String,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub ty: TypeRef,
    pub is_ref: bool,
    pub name: String,
    pub span: Span,
}

/// A statement. Declarations are statements in 010 just like in C.
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    TypedefAlias {
        new_name: String,
        source: TypeRef,
        span: Span,
    },
    TypedefEnum(EnumDecl),
    TypedefStruct(StructDecl),

    /// A variable-or-field declaration, e.g. `uint x;`, `char buf[N];`,
    /// `local int i = 0;`, `const uint MAX = 10;`. Whether this reads
    /// bytes from the source or allocates an ephemeral variable is
    /// decided by `modifier` — the interpreter's job, not the parser's.
    ///
    /// `args` carries the positional arguments to a parameterised
    /// struct: `PNG_CHUNK_PLTE plte(length);` → `args = [length]`.
    /// `bit_width` is set when the declaration uses C-style bitfield
    /// syntax: `DWORD flag : 1;` packs successive fields into the
    /// same underlying integer.
    FieldDecl {
        modifier: DeclModifier,
        ty: TypeRef,
        name: String,
        array_size: Option<Expr>,
        args: Vec<Expr>,
        bit_width: Option<Expr>,
        init: Option<Expr>,
        attrs: Attrs,
        span: Span,
    },

    /// `switch (scrutinee) { case A: ...; case B: ...; default: ...; }`
    ///
    /// Case bodies are sequences of statements; fall-through is
    /// permitted (and common in 010's `switch`es). The lack of an
    /// enclosing `Block` means the interpreter can drop into the next
    /// arm when no `break` is encountered.
    Switch {
        scrutinee: Expr,
        arms: Vec<SwitchArm>,
        span: Span,
    },

    If {
        cond: Expr,
        then_branch: Box<Stmt>,
        else_branch: Option<Box<Stmt>>,
        span: Span,
    },
    While {
        cond: Expr,
        body: Box<Stmt>,
        span: Span,
    },
    DoWhile {
        body: Box<Stmt>,
        cond: Expr,
        span: Span,
    },
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        step: Option<Expr>,
        body: Box<Stmt>,
        span: Span,
    },

    Return {
        value: Option<Expr>,
        span: Span,
    },
    Break {
        span: Span,
    },
    Continue {
        span: Span,
    },

    Block {
        stmts: Vec<Stmt>,
        span: Span,
    },
    Expr {
        expr: Expr,
        span: Span,
    },
}

/// One `case <value>:` or `default:` arm within a [`Stmt::Switch`].
/// An arm with `None` pattern is the `default` branch; there is at
/// most one per switch (the parser doesn't enforce that today since
/// 010 itself is forgiving).
#[derive(Clone, Debug, PartialEq)]
pub struct SwitchArm {
    pub pattern: Option<Expr>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclModifier {
    /// Normal field — reads from the byte source and emits a node.
    Field,
    /// `local` — ephemeral variable, not materialised in the tree.
    Local,
    /// `const` — like `local` but immutable after init.
    Const,
}

/// Expression tree. `span` is on the outer Expr enum via
/// [`Expr::span`] rather than duplicated on every variant.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    IntLit { value: u64, span: Span },
    FloatLit { value: f64, span: Span },
    StringLit { value: String, span: Span },
    CharLit { value: u32, span: Span },
    Ident { name: String, span: Span },

    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr>, span: Span },
    Unary { op: UnaryOp, operand: Box<Expr>, span: Span },

    Call { callee: Box<Expr>, args: Vec<Expr>, span: Span },
    Index { target: Box<Expr>, index: Box<Expr>, span: Span },
    Member { target: Box<Expr>, field: String, span: Span },

    Assign { op: AssignOp, target: Box<Expr>, value: Box<Expr>, span: Span },
    Ternary { cond: Box<Expr>, then_val: Box<Expr>, else_val: Box<Expr>, span: Span },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::IntLit { span, .. }
            | Expr::FloatLit { span, .. }
            | Expr::StringLit { span, .. }
            | Expr::CharLit { span, .. }
            | Expr::Ident { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Call { span, .. }
            | Expr::Index { span, .. }
            | Expr::Member { span, .. }
            | Expr::Assign { span, .. }
            | Expr::Ternary { span, .. } => *span,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    LogicalAnd,
    LogicalOr,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,     // -
    Pos,     // +
    Not,     // !
    BitNot,  // ~
    PreInc,  // ++x
    PreDec,  // --x
    PostInc, // x++
    PostDec, // x--
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    RemAssign,
    AndAssign,
    OrAssign,
    XorAssign,
    ShlAssign,
    ShrAssign,
}
