//! Tree-walking evaluator for parsed [`Expr`]s.
//!
//! Uses checked `i128` arithmetic so over/underflow surfaces as
//! [`EvalError::Overflow`] rather than wrapping silently. Division
//! / modulo by zero are surfaced as [`EvalError::DivByZero`].
//! [`Expr::Path`] is delegated to the caller-supplied
//! [`crate::PathResolver`]; resolution failures bubble up as
//! [`EvalError::Resolve`] so the calling UI can render a single
//! "Invalid: ..." row regardless of which layer failed.

use thiserror::Error;

use crate::NullResolver;
use crate::PathResolver;
use crate::ResolveError;
use crate::Value;
use crate::parser::BinOp;
use crate::parser::Expr;
use crate::parser::Function;
use crate::parser::Path;
use crate::parser::UnaryOp;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EvalError {
    #[error("division by zero")]
    DivByZero,
    #[error("arithmetic overflow")]
    Overflow,
    #[error("{0}")]
    Resolve(#[from] ResolveError),
}

/// Walk `expr` and produce the resulting [`Value`]. Path
/// references error with [`ResolveError::NoContext`]; use
/// [`evaluate_with`] to supply a resolver.
pub fn evaluate(expr: &Expr) -> Result<Value, EvalError> {
    evaluate_with(expr, &NullResolver)
}

/// Walk `expr` and produce the resulting [`Value`], delegating
/// [`Expr::Path`] nodes to `resolver`.
pub fn evaluate_with(expr: &Expr, resolver: &dyn PathResolver) -> Result<Value, EvalError> {
    eval_inner(expr, resolver).map(Value)
}

fn eval_function(func: Function, path: &Path, resolver: &dyn PathResolver) -> Result<i128, EvalError> {
    let field = resolver.lookup(path)?;
    let raw: u64 = match func {
        Function::Offset => field.offset,
        // `Len` and `Sizeof` both pull the byte length. The
        // distinction is preserved in the AST so a debug print
        // shows what the user typed; numerically they're equal.
        Function::Len | Function::Sizeof => field.length,
    };
    Ok(i128::from(raw))
}

fn eval_inner(expr: &Expr, resolver: &dyn PathResolver) -> Result<i128, EvalError> {
    match expr {
        Expr::Literal(v) => Ok(*v),
        Expr::Path(p) => {
            let field = resolver.lookup(p)?;
            field.value.ok_or_else(|| EvalError::Resolve(ResolveError::NotAScalar { path: p.display() }))
        }
        Expr::Call(func, p) => Ok(eval_function(*func, p, resolver)?),
        Expr::Unary(op, inner) => {
            let v = eval_inner(inner, resolver)?;
            match op {
                UnaryOp::Neg => v.checked_neg().ok_or(EvalError::Overflow),
                UnaryOp::Pos => Ok(v),
            }
        }
        Expr::Binary(op, l, r) => {
            let lv = eval_inner(l, resolver)?;
            let rv = eval_inner(r, resolver)?;
            match op {
                BinOp::Add => lv.checked_add(rv).ok_or(EvalError::Overflow),
                BinOp::Sub => lv.checked_sub(rv).ok_or(EvalError::Overflow),
                BinOp::Mul => lv.checked_mul(rv).ok_or(EvalError::Overflow),
                BinOp::Div => {
                    if rv == 0 {
                        return Err(EvalError::DivByZero);
                    }
                    lv.checked_div(rv).ok_or(EvalError::Overflow)
                }
                BinOp::Mod => {
                    if rv == 0 {
                        return Err(EvalError::DivByZero);
                    }
                    lv.checked_rem(rv).ok_or(EvalError::Overflow)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Path;
    use crate::parser::PathSegment;
    use crate::parser::parse;

    fn ev(input: &str) -> i128 {
        evaluate(&parse(input).expect("parse")).expect("eval").raw()
    }

    #[test]
    fn arithmetic() {
        assert_eq!(ev("1 + 2 * 3"), 7);
        assert_eq!(ev("(1 + 2) * 3"), 9);
        assert_eq!(ev("10 - 3 - 2"), 5);
        assert_eq!(ev("100 / 4"), 25);
        assert_eq!(ev("10 % 3"), 1);
    }

    #[test]
    fn hex_with_unit() {
        assert_eq!(ev("0x100 + 1MiB"), 0x100 + 1024 * 1024);
    }

    #[test]
    fn implicit_close() {
        assert_eq!(ev("(5 * (5 + 10"), 75);
    }

    #[test]
    fn negative_intermediate() {
        assert_eq!(ev("0x10 - 0x20"), -16);
    }

    #[test]
    fn div_zero_errors() {
        assert_eq!(evaluate(&parse("1 / 0").unwrap()), Err(EvalError::DivByZero));
        assert_eq!(evaluate(&parse("1 % 0").unwrap()), Err(EvalError::DivByZero));
    }

    #[test]
    fn overflow_errors() {
        let huge = format!("{}", i128::MAX);
        let expr = parse(&format!("{huge} + 1")).unwrap();
        assert_eq!(evaluate(&expr), Err(EvalError::Overflow));
    }

    #[test]
    fn null_resolver_rejects_paths() {
        let err = evaluate(&parse("png.length").unwrap()).unwrap_err();
        assert!(matches!(err, EvalError::Resolve(ResolveError::NoContext)));
    }

    /// Tiny stub resolver that fakes a `png.length` field at
    /// offset `0x100` with span `8` bytes and scalar value `42`.
    /// Lets the eval tests cover both the bare-path and the
    /// function-call paths without spinning up a real template.
    struct FixedResolver;
    impl PathResolver for FixedResolver {
        fn lookup(&self, path: &Path) -> Result<crate::FieldRef, ResolveError> {
            if path.root == "png"
                && path.instance.is_none()
                && path.segments == [PathSegment::Name("length".into())]
            {
                Ok(crate::FieldRef { offset: 0x100, length: 8, value: Some(42) })
            } else {
                Err(ResolveError::UnknownTemplate { name: path.root.clone() })
            }
        }
    }

    #[test]
    fn resolver_provides_path_value() {
        let expr = parse("0x1 + png.length").unwrap();
        let value = evaluate_with(&expr, &FixedResolver).unwrap();
        assert_eq!(value.raw(), 1 + 42);
    }

    #[test]
    fn function_offset_returns_span_offset() {
        let expr = parse("offset(png.length)").unwrap();
        let value = evaluate_with(&expr, &FixedResolver).unwrap();
        assert_eq!(value.raw(), 0x100);
    }

    #[test]
    fn function_len_returns_span_length() {
        let expr = parse("len(png.length)").unwrap();
        let value = evaluate_with(&expr, &FixedResolver).unwrap();
        assert_eq!(value.raw(), 8);
    }

    #[test]
    fn function_sizeof_alias_of_len() {
        let len_v = evaluate_with(&parse("len(png.length)").unwrap(), &FixedResolver).unwrap();
        let size_v = evaluate_with(&parse("sizeof(png.length)").unwrap(), &FixedResolver).unwrap();
        assert_eq!(len_v.raw(), size_v.raw());
    }

    #[test]
    fn functions_compose_with_arithmetic() {
        // offset(png.length) + len(png.length) = 0x100 + 8
        let expr = parse("offset(png.length) + len(png.length)").unwrap();
        let value = evaluate_with(&expr, &FixedResolver).unwrap();
        assert_eq!(value.raw(), 0x100 + 8);
    }
}
