//! Structured-fuzzing helpers, compiled in when the `arbitrary`
//! feature is on.
//!
//! [`FuzzProgram`] derives [`arbitrary::Arbitrary`] and emits valid
//! 010 source via [`FuzzProgram::emit`]. Fuzz targets feed the
//! emitted string through tokenize / parse / interpret — this
//! explores interpreter paths a byte-oriented fuzzer can't reach in
//! reasonable time (most random inputs don't even tokenize into
//! valid keywords).

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use std::fmt::Write as _;

/// Pool of legal-but-generic identifier names the fuzzer draws from.
/// Small, fixed set so generated programs reference each other
/// (e.g. `if (x > 0)` where `x` was declared earlier in the same
/// program) instead of every name being unique and unused.
const IDENT_POOL: &[&str] = &["a", "b", "c", "d", "x", "y", "z", "n", "m", "foo", "bar", "baz"];

const TYPE_POOL: &[&str] = &["uchar", "ushort", "uint", "uint64", "char", "short", "int", "int64", "float", "double"];

const BUILTIN_POOL: &[&str] = &["FTell", "FEof", "FileSize"];

/// A single fuzz-generated input: a body of top-level statements
/// plus the byte buffer the template reads from.
#[derive(Debug, Arbitrary)]
pub struct FuzzProgram {
    pub stmts: Vec<FuzzStmt>,
    pub data: Vec<u8>,
}

#[derive(Debug, Arbitrary)]
pub enum FuzzStmt {
    Field(FuzzField),
    Local { name: FuzzIdent, init: Option<FuzzExpr> },
    If { cond: FuzzExpr, then_body: Vec<FuzzStmt>, else_body: Option<Vec<FuzzStmt>> },
    While { cond: FuzzExpr, body: Vec<FuzzStmt> },
    Expr(FuzzExpr),
    LittleEndian,
    BigEndian,
}

#[derive(Debug, Arbitrary)]
pub struct FuzzField {
    pub ty: FuzzType,
    pub name: FuzzIdent,
    pub array_size: Option<u8>,
}

#[derive(Debug, Arbitrary)]
pub struct FuzzType(pub u8);

#[derive(Debug, Arbitrary)]
pub struct FuzzIdent(pub u8);

#[derive(Debug, Arbitrary)]
pub enum FuzzExpr {
    IntLit(i32),
    Var(FuzzIdent),
    Add(Box<FuzzExpr>, Box<FuzzExpr>),
    Sub(Box<FuzzExpr>, Box<FuzzExpr>),
    Lt(Box<FuzzExpr>, Box<FuzzExpr>),
    Eq(Box<FuzzExpr>, Box<FuzzExpr>),
    Not(Box<FuzzExpr>),
    Call(FuzzBuiltin),
}

#[derive(Debug, Arbitrary)]
pub struct FuzzBuiltin(pub u8);

impl FuzzIdent {
    fn name(&self) -> &'static str {
        IDENT_POOL[(self.0 as usize) % IDENT_POOL.len()]
    }
}

impl FuzzType {
    fn name(&self) -> &'static str {
        TYPE_POOL[(self.0 as usize) % TYPE_POOL.len()]
    }
}

impl FuzzBuiltin {
    fn name(&self) -> &'static str {
        BUILTIN_POOL[(self.0 as usize) % BUILTIN_POOL.len()]
    }
}

impl FuzzProgram {
    /// Render the program to 010 source. The output is guaranteed to
    /// tokenize; it's not guaranteed to type-check in the real 010
    /// sense, but well within what our interpreter is expected to
    /// handle without panicking.
    pub fn emit(&self) -> String {
        let mut out = String::new();
        for stmt in &self.stmts {
            emit_stmt(&mut out, stmt, 0);
        }
        out
    }
}

fn emit_stmt(out: &mut String, stmt: &FuzzStmt, depth: usize) {
    indent(out, depth);
    match stmt {
        FuzzStmt::Field(f) => {
            let _ = write!(out, "{} {}", f.ty.name(), f.name.name());
            if let Some(size) = f.array_size {
                let _ = write!(out, "[{}]", (size as u32) % 16);
            }
            out.push_str(";\n");
        }
        FuzzStmt::Local { name, init } => {
            let _ = write!(out, "local int {}", name.name());
            if let Some(init) = init {
                out.push_str(" = ");
                emit_expr(out, init);
            }
            out.push_str(";\n");
        }
        FuzzStmt::If { cond, then_body, else_body } => {
            out.push_str("if (");
            emit_expr(out, cond);
            out.push_str(") {\n");
            for s in then_body {
                emit_stmt(out, s, depth + 1);
            }
            indent(out, depth);
            out.push('}');
            if let Some(else_body) = else_body {
                out.push_str(" else {\n");
                for s in else_body {
                    emit_stmt(out, s, depth + 1);
                }
                indent(out, depth);
                out.push('}');
            }
            out.push('\n');
        }
        FuzzStmt::While { cond, body } => {
            out.push_str("while (");
            emit_expr(out, cond);
            out.push_str(") {\n");
            for s in body {
                emit_stmt(out, s, depth + 1);
            }
            indent(out, depth);
            out.push_str("}\n");
        }
        FuzzStmt::Expr(e) => {
            emit_expr(out, e);
            out.push_str(";\n");
        }
        FuzzStmt::LittleEndian => out.push_str("LittleEndian();\n"),
        FuzzStmt::BigEndian => out.push_str("BigEndian();\n"),
    }
}

fn emit_expr(out: &mut String, expr: &FuzzExpr) {
    match expr {
        FuzzExpr::IntLit(v) => {
            let _ = write!(out, "{v}");
        }
        FuzzExpr::Var(n) => out.push_str(n.name()),
        FuzzExpr::Add(l, r) => {
            out.push('(');
            emit_expr(out, l);
            out.push_str(" + ");
            emit_expr(out, r);
            out.push(')');
        }
        FuzzExpr::Sub(l, r) => {
            out.push('(');
            emit_expr(out, l);
            out.push_str(" - ");
            emit_expr(out, r);
            out.push(')');
        }
        FuzzExpr::Lt(l, r) => {
            out.push('(');
            emit_expr(out, l);
            out.push_str(" < ");
            emit_expr(out, r);
            out.push(')');
        }
        FuzzExpr::Eq(l, r) => {
            out.push('(');
            emit_expr(out, l);
            out.push_str(" == ");
            emit_expr(out, r);
            out.push(')');
        }
        FuzzExpr::Not(e) => {
            out.push('!');
            emit_expr(out, e);
        }
        FuzzExpr::Call(b) => {
            let _ = write!(out, "{}()", b.name());
        }
    }
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("    ");
    }
}

/// `FuzzProgram` with a ready-to-consume `Arbitrary` bridge for
/// libFuzzer — useful when the caller wants a one-liner.
pub fn program_from_unstructured(u: &mut Unstructured<'_>) -> arbitrary::Result<FuzzProgram> {
    FuzzProgram::arbitrary(u)
}
