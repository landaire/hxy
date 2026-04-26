//! AST for ImHex pattern source files.
//!
//! Shape stays close to the surface syntax; semantic concerns
//! (type resolution, namespace lookup, template instantiation)
//! are deferred to the interpreter. Spans are carried per-node so
//! diagnostics + the host's `template::Node` output can refer back
//! to the offending source range.

use crate::token::Span;

/// One pattern file -- a flat list of top-level items (declarations,
/// pragmas the lexer didn't strip, top-level field placements). The
/// interpreter walks them in source order.
#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    pub items: Vec<TopItem>,
}

/// Top-level item. Function, type declaration, or top-level statement
/// (the field placements that drive the actual byte-reading walk).
#[derive(Clone, Debug, PartialEq)]
pub enum TopItem {
    Function(FunctionDef),
    Stmt(Stmt),
}

/// `Type` reference -- a (possibly namespace-qualified) name plus
/// optional template args. The interpreter resolves it against the
/// type table at use time.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeRef {
    /// Namespace path: `std::mem::Bytes` becomes
    /// `["std", "mem", "Bytes"]`.
    pub path: Vec<String>,
    pub template_args: Vec<Expr>,
    pub span: Span,
}

impl TypeRef {
    /// Convenience: the leaf segment of the path. Used by callers
    /// that don't care about the namespace prefix.
    pub fn leaf(&self) -> &str {
        self.path.last().map(String::as_str).unwrap_or("")
    }
}

/// `[[key(arg, ...), key2, ...]]` annotation list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Attrs(pub Vec<Attr>);

#[derive(Clone, Debug, PartialEq)]
pub struct Attr {
    pub name: String,
    pub args: Vec<Expr>,
    pub span: Span,
}

/// `fn name(params) { body }`. Return type is implicit (`auto`)
/// unless a `-> Type` annotation follows the parameter list.
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionDef {
    pub name: String,
    pub params: Vec<Param>,
    pub return_type: Option<TypeRef>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub ty: Option<TypeRef>,
    pub name: String,
    pub span: Span,
}

/// `struct Name { body } [[attrs]];` and `union Name { body };`.
#[derive(Clone, Debug, PartialEq)]
pub struct StructDecl {
    pub name: String,
    /// Template parameter names: `struct Foo<T, auto N> { ... }`
    /// becomes `template_params = ["T", "N"]`. Empty for non-
    /// parametric structs. The interpreter binds each param to
    /// the corresponding [`TypeRef::template_args`] entry when
    /// the struct is read.
    pub template_params: Vec<String>,
    /// Optional parent type for struct inheritance: `struct B : A`.
    /// `None` for non-inheriting structs. The interpreter inlines
    /// the parent's body before the child's at read time.
    pub parent: Option<TypeRef>,
    pub body: Vec<Stmt>,
    pub attrs: Attrs,
    pub is_union: bool,
    pub span: Span,
}

/// `enum Name : Backing { Variant, Variant = expr, ... };`.
#[derive(Clone, Debug, PartialEq)]
pub struct EnumDecl {
    pub name: String,
    pub template_params: Vec<String>,
    pub backing: TypeRef,
    pub variants: Vec<EnumVariant>,
    pub attrs: Attrs,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumVariant {
    pub name: String,
    pub value: Option<Expr>,
    /// Optional upper bound for variants spelled as a range:
    /// `Reserved = 0 ... 7,`. When set, the variant matches every
    /// integer in `[value, value_end]`. The interpreter currently
    /// uses only `value` for naming; range matching is best-effort.
    pub value_end: Option<Expr>,
    pub span: Span,
}

/// `bitfield Name { name : N; padding : N; ... };`.
///
/// The body is a flat list of statements rather than a list of
/// fields-only because ImHex bitfields can include `if`/`match`
/// branches, computed `Type name = expr;` derived values, and
/// byte-aligned regular reads alongside the usual `name : width;`
/// bit-slice fields. The interpreter walks the body in order.
#[derive(Clone, Debug, PartialEq)]
pub struct BitfieldDecl {
    pub name: String,
    pub template_params: Vec<String>,
    pub body: Vec<Stmt>,
    pub attrs: Attrs,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    /// `using Alias = T;`
    UsingAlias {
        new_name: String,
        source: TypeRef,
        span: Span,
    },
    StructDecl(StructDecl),
    EnumDecl(EnumDecl),
    BitfieldDecl(BitfieldDecl),
    /// Nested function: `fn name(...) { ... }` inside a `namespace`
    /// or struct body. The top-level form lives in [`TopItem::Function`].
    FnDecl(FunctionDef),

    /// `[Type] name : width [[attrs]];` -- a bit-slice field, only
    /// legal inside a [`BitfieldDecl`] body. The optional type
    /// projects the bit-slice as a typed value (typically an enum);
    /// when `None`, the bits are read as a raw unsigned integer.
    BitfieldField {
        ty: Option<TypeRef>,
        name: String,
        width: Expr,
        attrs: Attrs,
        span: Span,
    },

    /// `[const] Type name [array] [@ offset] [= init] [[attrs]];`.
    /// One declaration -- comma-separated declarators are unrolled
    /// into multiple of these by the parser.
    FieldDecl {
        is_const: bool,
        ty: TypeRef,
        name: String,
        array: Option<ArraySize>,
        placement: Option<Expr>,
        init: Option<Expr>,
        attrs: Attrs,
        /// `Type *p : u32` -- pointer field. The type after `:` is
        /// the pointer-width (read as the address). When set, the
        /// interpreter reads `pointer_width`, seeks to that offset,
        /// reads `ty` there, and restores the cursor.
        pointer_width: Option<TypeRef>,
        span: Span,
    },

    /// `namespace foo { ... }` (regular) or `namespace auto std { ... }`
    /// (auto-imports its members into the global scope).
    Namespace {
        path: Vec<String>,
        is_auto: bool,
        body: Vec<Stmt>,
        span: Span,
    },

    /// `import std.io;` -- resolves to a file load + parse.
    Import {
        path: Vec<String>,
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
    For {
        init: Option<Box<Stmt>>,
        cond: Option<Expr>,
        step: Option<Expr>,
        body: Box<Stmt>,
        span: Span,
    },
    Match {
        scrutinee: Expr,
        arms: Vec<MatchArm>,
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
}

/// Array size: a fixed expression, `[]` (open-ended), or
/// `[while(cond)]` (loop until predicate false).
#[derive(Clone, Debug, PartialEq)]
pub enum ArraySize {
    Fixed(Expr),
    Open,
    While(Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MatchArm {
    pub patterns: Vec<MatchPattern>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MatchPattern {
    /// Single value: `(1)`, `("foo")`.
    Value(Expr),
    /// Range: `(0 ... 5)`. ImHex uses `..` and `...` interchangeably
    /// in match arm patterns; we model both as the same node.
    Range { lo: Expr, hi: Expr, span: Span },
    /// `(_)` -- match anything.
    Wildcard { span: Span },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    IntLit {
        value: u128,
        span: Span,
    },
    FloatLit {
        value: f64,
        span: Span,
    },
    StringLit {
        value: String,
        span: Span,
    },
    CharLit {
        value: u32,
        span: Span,
    },
    BoolLit {
        value: bool,
        span: Span,
    },
    NullLit {
        span: Span,
    },
    /// `parent`, `this`, `$`, or a regular identifier.
    Ident {
        name: String,
        span: Span,
    },
    /// `std::print` -- a namespace-qualified identifier. Rendered
    /// as the joined path so the interpreter can use it as a
    /// lookup key.
    Path {
        segments: Vec<String>,
        span: Span,
    },

    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
        span: Span,
    },

    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        span: Span,
    },
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
        span: Span,
    },
    Member {
        target: Box<Expr>,
        field: String,
        span: Span,
    },

    Assign {
        op: AssignOp,
        target: Box<Expr>,
        value: Box<Expr>,
        span: Span,
    },
    Ternary {
        cond: Box<Expr>,
        then_val: Box<Expr>,
        else_val: Box<Expr>,
        span: Span,
    },

    /// `sizeof(x)` / `addressof(x)` / `typeof(x)`. Modelled as a
    /// distinct expression node since the operand can be a *type*
    /// (not just a value), and treating it as a function call would
    /// require resolving types at parse time.
    Reflect {
        kind: ReflectKind,
        operand: Box<Expr>,
        span: Span,
    },

    /// Carries a nested [`TypeRef`] in expression position. Comes up
    /// in template-arg lists when an arg is itself a type
    /// instantiation (`Foo<Bar<u32>>`). The interpreter resolves
    /// it as a type-valued operand at use time.
    TypeRefExpr {
        ty: Box<TypeRef>,
        span: Span,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReflectKind {
    Sizeof,
    Addressof,
    Typeof,
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::IntLit { span, .. }
            | Expr::FloatLit { span, .. }
            | Expr::StringLit { span, .. }
            | Expr::CharLit { span, .. }
            | Expr::BoolLit { span, .. }
            | Expr::NullLit { span }
            | Expr::Ident { span, .. }
            | Expr::Path { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Call { span, .. }
            | Expr::Index { span, .. }
            | Expr::Member { span, .. }
            | Expr::Assign { span, .. }
            | Expr::Ternary { span, .. }
            | Expr::Reflect { span, .. }
            | Expr::TypeRefExpr { span, .. } => *span,
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
    Neg,
    Pos,
    Not,
    BitNot,
    PreInc,
    PreDec,
    PostInc,
    PostDec,
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
