//! Tiny arithmetic calculator for byte-offset / length expressions.
//!
//! Parses hex (`0x...`) and decimal numbers, optionally followed by
//! a unit suffix (`B`, `KB` / `KiB`, `MB` / `MiB`, `GB` / `GiB`,
//! `TB` / `TiB`), combined with the four basic operators
//! (`+ - * /`) and `%`. Parentheses group sub-expressions; an
//! unclosed `(` is treated as if it closed at end-of-input -- a
//! Speedcrunch-style affordance so the user can keep typing
//! without finishing every nested paren.
//!
//! Template field paths (`png.length`, `png#2.chunks[0].length`)
//! are recognised by the parser and resolved at evaluation time
//! via the caller-supplied [`PathResolver`]. The crate ships only
//! a [`NullResolver`] -- the host (hxy) implements a real one
//! against the active file's parsed templates and routes
//! ambiguity (`png` vs `png` from two runs) by picking the most
//! recent run unless the expression spells out `png#N`.
//!
//! Values are evaluated as `i128` internally to leave headroom for
//! intermediate negatives and large multiplies; the public
//! [`Value`] newtype exposes [`Value::raw`] (the raw `i128`) and
//! [`Value::as_u64`] (typed conversion that surfaces underflow /
//! overflow as [`ValueError`]). The intended use site is the hxy
//! command palette's "go to offset" entry.

mod eval;
mod parser;

pub use eval::EvalError;
pub use eval::evaluate;
pub use eval::evaluate_with;
pub use parser::Base;
pub use parser::BinOp;
pub use parser::Expr;
pub use parser::MetaKind;
pub use parser::ParseError;
pub use parser::Path;
pub use parser::PathSegment;
pub use parser::UnaryOp;
pub use parser::Unit;
pub use parser::parse;

use thiserror::Error;

/// Plug-point for resolving template-field paths that appear in
/// calculator expressions. The host is responsible for looking
/// up `path.root` against its template instances, picking a run
/// based on `path.instance` (or "most recent" when absent), and
/// walking `path.segments` against the resulting field tree.
///
/// The single [`Self::lookup`] entry point hands back a
/// [`FieldRef`] that bundles the field's byte span with its
/// integer value (when the field is a scalar). The evaluator
/// chooses which slot to read: bare paths read `value`,
/// `offset(p)` reads `offset`, `len(p)` / `sizeof(p)` read
/// `length`. That way the resolver only has to find the field
/// once regardless of which view the expression demands.
pub trait PathResolver {
    fn lookup(&self, path: &Path) -> Result<FieldRef, ResolveError>;

    /// Distinct template-stem names available for completion.
    /// Returns an empty list by default so resolvers that don't
    /// support multi-template lookup don't have to think about
    /// completion. Order is up to the implementation; the host
    /// typically sorts alphabetically before showing them.
    fn template_stems(&self) -> Vec<String> {
        Vec::new()
    }

    /// Names of `path`'s direct children, used for completion of
    /// `path.<segment>` queries. The default returns an empty list
    /// (no completion). Implementations should treat the same
    /// "auto-descend through a single root" rule as [`Self::lookup`]
    /// so the user's mental path matches what completion offers.
    fn list_children(&self, _path: &Path) -> Vec<String> {
        Vec::new()
    }
}

/// What a [`PathResolver`] returns for a successfully-located
/// field. `offset` / `length` come from the parser's span (always
/// available); `value` is `Some` only when the field is a scalar
/// the resolver could coerce to an integer. Struct / array nodes
/// have `value: None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldRef {
    pub offset: u64,
    pub length: u64,
    pub value: Option<i128>,
}

/// Resolver that refuses every path. Used by the no-context
/// [`evaluate`] / [`evaluate_str`] entry points so an expression
/// containing `png.length` typed in a context without templates
/// errors loudly instead of returning zero.
pub struct NullResolver;

impl PathResolver for NullResolver {
    fn lookup(&self, _path: &Path) -> Result<FieldRef, ResolveError> {
        Err(ResolveError::NoContext)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResolveError {
    #[error("no template named {name:?}")]
    UnknownTemplate { name: String },
    #[error("template {name:?} has only {available} run(s); #{requested} not found")]
    InstanceOutOfRange { name: String, requested: u32, available: u32 },
    #[error("multiple templates match {name:?} -- pick one with #N")]
    AmbiguousTemplate { name: String, count: u32 },
    #[error("field {component:?} not found under {parent:?}")]
    FieldNotFound { parent: String, component: String },
    #[error("array index {index} out of bounds (len {len}) under {parent:?}")]
    IndexOutOfBounds { parent: String, index: u64, len: u64 },
    #[error("{path:?} is not a scalar field")]
    NotAScalar { path: String },
    #[error("scalar at {path:?} is not an integer")]
    NotAnInteger { path: String },
    #[error("path resolution not supported in this context")]
    NoContext,
}

/// Result of evaluating a calculator expression. Held as `i128`
/// so intermediate subtraction below zero (`0x10 - 0x20`) doesn't
/// silently wrap; callers downstream can demand a non-negative
/// `u64` via [`Value::as_u64`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Value(i128);

impl Value {
    /// Borrow the raw signed 128-bit value. Useful when the
    /// caller wants to display both decimal and hex forms.
    pub fn raw(self) -> i128 {
        self.0
    }

    /// Coerce the result into a `u64`. Errors when the value is
    /// negative or exceeds `u64::MAX` so the caller can route
    /// the failure into a UI error rather than silently clamping.
    pub fn as_u64(self) -> Result<u64, ValueError> {
        if self.0 < 0 {
            return Err(ValueError::Negative(self.0));
        }
        u64::try_from(self.0).map_err(|_| ValueError::Overflow(self.0))
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValueError {
    #[error("value is negative: {0}")]
    Negative(i128),
    #[error("value exceeds u64 range: {0}")]
    Overflow(i128),
}

/// Combined parse + evaluate failure. The `#[from]` impls let
/// callers `?` either layer's error into the same return type.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CalcError {
    #[error("{0}")]
    Parse(#[from] ParseError),
    #[error("{0}")]
    Eval(#[from] EvalError),
}

/// One-shot "string in, value out". Equivalent to calling
/// [`parse`] followed by [`evaluate`]; provided so callers that
/// don't need the AST can stay on a single call. Path references
/// fail with [`ResolveError::NoContext`] -- use
/// [`evaluate_str_with`] when the host has a resolver wired up.
pub fn evaluate_str(input: &str) -> Result<Value, CalcError> {
    evaluate_str_with(input, &NullResolver)
}

/// Same as [`evaluate_str`] but routes [`Expr::Path`] references
/// through the supplied resolver.
pub fn evaluate_str_with(input: &str, resolver: &dyn PathResolver) -> Result<Value, CalcError> {
    let expr = parse(input)?;
    Ok(evaluate_with(&expr, resolver)?)
}
