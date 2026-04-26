//! Tree-walking interpreter for ImHex pattern programs.
//!
//! Implemented (Phase 1 + 2):
//! - Primitive type reads (`u8`/`s8`/.../`u128`/`s128`/`f32`/`f64`/
//!   `bool`/`char`/`char16`).
//! - User type declarations: `struct`, `union`, `enum : T`,
//!   `bitfield`, `using` aliases.
//! - Sequential field reads inside structs.
//! - Arithmetic / comparison / logical / bitwise / unary expressions.
//! - Control flow: `if` / `else` / `while` / `for` / `return` /
//!   `break` / `continue` / `match`.
//! - Local variable scopes (`auto`, `const`).
//! - Array reads with fixed counts, plus `[]` (open-ended) and
//!   `[while(cond)]` (predicate-driven, parsed but no-op runtime).
//! - Function definitions + call evaluation (positional args).
//! - Placement reads (`Type x @ offset;`) with cursor save/restore.
//! - Magic identifiers: `$` (cursor), `parent.field`, `this.field`.
//! - Reflective `sizeof(Type)` / `sizeof(field)` / `addressof(field)`.
//! - `padding[N];` builtin (skip bytes, no emitted leaf).
//! - `[[attrs]]` plumbed through to emitted nodes verbatim.
//!
//! Deferred to later phases:
//! - Pointer types (`Type *p : u32`).
//! - Full template instantiation (`template<T> struct ...`,
//!   `Bytes<N>`).
//! - Namespace lookup / `import` resolution against the patterns
//!   corpus (Phase 3).
//! - Struct inheritance composition (parsed, parent body dropped).
//! - `try` / `catch` exception flow (parsed, catch arm dropped).

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::ast::ArraySize;
use crate::ast::AssignOp;
use crate::ast::BinOp;
use crate::ast::BitfieldDecl;
use crate::ast::EnumDecl;
use crate::ast::Expr;
use crate::ast::FunctionDef;
use crate::ast::MatchArm;
use crate::ast::MatchPattern;
use crate::ast::Program;
use crate::ast::ReflectKind;
use crate::ast::Stmt;
use crate::ast::StructDecl;
use crate::ast::TopItem;
use crate::ast::TypeRef;
use crate::ast::UnaryOp;
use crate::imports::ImportResolver;
use crate::imports::NoImportResolver;
use crate::source::HexSource;
use crate::source::SourceError;
use crate::token::Span;
use crate::value::Endian;
use crate::value::NodeType;
use crate::value::PrimClass;
use crate::value::PrimKind;
use crate::value::ScalarKind;
use crate::value::Value;

/// Index into [`RunResult::nodes`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NodeIdx(pub u32);

impl NodeIdx {
    pub fn new(i: u32) -> Self {
        Self(i)
    }
    pub fn as_u32(self) -> u32 {
        self.0
    }
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NodeOut {
    pub name: String,
    pub ty: NodeType,
    pub offset: u64,
    pub length: u64,
    pub value: Option<Value>,
    pub parent: Option<NodeIdx>,
    pub attrs: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub message: String,
    pub severity: Severity,
    pub file_offset: Option<u64>,
    pub template_line: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunResult {
    pub nodes: Vec<NodeOut>,
    pub diagnostics: Vec<Diagnostic>,
    pub return_value: Option<Value>,
    pub terminal_error: Option<RuntimeError>,
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum RuntimeError {
    #[error("undefined name `{name}`")]
    UndefinedName { name: String },

    #[error("unknown type `{name}`")]
    UnknownType { name: String },

    #[error("type error: {0}")]
    Type(String),

    #[error("source read failed: {0}")]
    Source(#[from] SourceError),

    /// Wall-clock budget exceeded.
    #[error("template execution exceeded timeout of {timeout_ms} ms")]
    TimedOut { timeout_ms: u64 },

    /// Step limit hit (catches runaway recursion / loops that don't
    /// rely on cursor-progress to terminate).
    #[error("template execution exceeded {step_limit} statements")]
    StepLimitHit { step_limit: u64 },
}

#[derive(Clone, Debug)]
enum TypeDef {
    Primitive(PrimKind),
    /// Type alias: another name in the type table. `params` carries
    /// the template parameter names from `using Foo<T> = T;`; when
    /// non-empty, the lookup site substitutes the use-site template
    /// args before continuing resolution.
    Alias { params: Vec<String>, target: TypeRef },
    Struct(StructDecl),
    Enum(EnumDecl),
    Bitfield(BitfieldDecl),
}

#[derive(Clone, Default)]
struct Scope {
    vars: HashMap<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Next,
    Break,
    Continue,
    Return,
}

/// Top-level driver. The `Interpreter` owns the runtime state -- type
/// table, function table, scope stack, output node list, and the
/// running cursor offset -- and walks the AST against an owned
/// [`HexSource`]. The cursor is just a `u64` offset on `self`; reads
/// go through `self.source.read(offset, len)` so we never need a
/// borrow that conflicts with `&mut self` execution.
pub struct Interpreter<S: HexSource> {
    source: S,
    pos: u64,
    types: HashMap<String, TypeDef>,
    functions: HashMap<String, FunctionDef>,
    scopes: Vec<Scope>,
    nodes: Vec<NodeOut>,
    /// Name -> indices of every emitted node with that name, in
    /// emission order. Built incrementally to keep `parent.x.y.z`
    /// chains close to O(name-collisions) per hop instead of O(N).
    /// The big device-tree-shaped corpus templates (fdt, pck, ...)
    /// time out without this index.
    nodes_by_name: HashMap<String, Vec<NodeIdx>>,
    diagnostics: Vec<Diagnostic>,
    endian: Endian,
    steps: u64,
    step_limit: u64,
    return_value: Option<Value>,
    /// Tracks how many top-level field reads have produced a parent
    /// span. Wired through to nested node `parent` indexes.
    current_parent: Option<NodeIdx>,
    /// Resolves `import a.b.c;` paths to source bytes. Defaults to
    /// [`NoImportResolver`]; the host wires up a real resolver
    /// pointing at the cloned ImHex patterns repo.
    resolver: Arc<dyn ImportResolver>,
    /// Imports already expanded -- skip re-parsing on repeat
    /// imports of the same path. Keyed by the joined path so
    /// `import std.io;` and `import std::io;` collapse.
    imported: HashSet<String>,
    /// Active namespace prefix while collecting decls inside a
    /// `namespace foo::bar { ... }` block. Empty at the top level.
    namespace_stack: Vec<String>,
    /// External cancel flag. When set, every step bails out with
    /// [`RuntimeError::TimedOut`]. Hosts wire this up to a wall-clock
    /// deadline so a runaway template can't stall the renderer or
    /// the corpus probe.
    interrupt: Arc<AtomicBool>,
}

impl<S: HexSource> Interpreter<S> {
    pub fn new(source: S) -> Self {
        let mut me = Self {
            source,
            pos: 0,
            types: HashMap::new(),
            functions: HashMap::new(),
            scopes: vec![Scope::default()],
            nodes: Vec::new(),
            nodes_by_name: HashMap::new(),
            diagnostics: Vec::new(),
            endian: Endian::Little,
            steps: 0,
            step_limit: 5_000_000,
            return_value: None,
            current_parent: None,
            resolver: Arc::new(NoImportResolver),
            imported: HashSet::new(),
            namespace_stack: Vec::new(),
            interrupt: Arc::new(AtomicBool::new(false)),
        };
        me.register_primitives();
        me
    }

    /// Plug in an external cancel flag. The interpreter polls it on
    /// every statement step and bails out with
    /// [`RuntimeError::TimedOut`] when the flag flips to `true`.
    /// Hosts use this to enforce a wall-clock deadline (the
    /// in-process step counter can't bound CPU when individual
    /// steps walk a large node tree).
    pub fn with_interrupt(mut self, interrupt: Arc<AtomicBool>) -> Self {
        self.interrupt = interrupt;
        self
    }

    /// Plug an import resolver so the interpreter can pull in
    /// `import std.io;` and friends. Without this, imports are
    /// parsed but produce no decls.
    pub fn with_import_resolver(mut self, resolver: Arc<dyn ImportResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    fn cursor_tell(&self) -> u64 {
        self.pos
    }

    fn cursor_seek(&mut self, pos: u64) {
        self.pos = pos.min(self.source.len());
    }

    fn cursor_advance(&mut self, length: u64) -> Result<Vec<u8>, RuntimeError> {
        let bytes = self.source.read(self.pos, length).map_err(RuntimeError::from)?;
        self.pos = self.pos.saturating_add(length);
        Ok(bytes)
    }

    pub fn with_step_limit(mut self, limit: u64) -> Self {
        self.step_limit = limit;
        self
    }

    pub fn run(mut self, program: &Program) -> RunResult {
        let result = self.exec_program(program);
        let terminal_error = result.err();
        RunResult { nodes: self.nodes, diagnostics: self.diagnostics, return_value: self.return_value, terminal_error }
    }

    /// Drive every top-level item in source order. Function defs go
    /// into the function table; statements execute against the
    /// running cursor.
    fn exec_program(&mut self, program: &Program) -> Result<(), RuntimeError> {
        // First pass: collect declarations (types + functions) so
        // forward references work (`fn a() -> b()` where `b` is
        // declared later).
        for it in &program.items {
            self.collect_decl(it);
        }
        // Second pass: execute statements in source order.

        for it in &program.items {
            if let TopItem::Stmt(s) = it {
                let _ = self.exec_stmt(s, None)?;
            }
        }
        Ok(())
    }

    fn collect_decl(&mut self, it: &TopItem) {
        match it {
            TopItem::Function(f) => {
                self.register_function(&f.name, f.clone());
            }
            TopItem::Stmt(s) => self.collect_stmt_decl(s),
        }
    }

    fn collect_stmt_decl(&mut self, s: &Stmt) {
        match s {
            Stmt::StructDecl(d) => {
                self.register_type(&d.name, TypeDef::Struct(d.clone()));
            }
            Stmt::EnumDecl(d) => {
                self.register_type(&d.name, TypeDef::Enum(d.clone()));
            }
            Stmt::BitfieldDecl(d) => {
                self.register_type(&d.name, TypeDef::Bitfield(d.clone()));
            }
            Stmt::FnDecl(f) => {
                self.register_function(&f.name, f.clone());
            }
            Stmt::UsingAlias { new_name, template_params, source, .. } => {
                self.register_type(
                    new_name,
                    TypeDef::Alias { params: template_params.clone(), target: source.clone() },
                );
            }
            Stmt::Namespace { path, body, .. } => {
                // Push the namespace prefix while collecting the
                // body so qualified names register too. Both the
                // bare leaf and the fully-qualified spelling end
                // up as keys -- the leaf so corpus patterns that
                // rely on `using namespace` semantics still find
                // names without prefixes, the qualified spelling
                // for `foo::bar(...)` call sites.
                for seg in path {
                    self.namespace_stack.push(seg.clone());
                }
                for s in body {
                    self.collect_stmt_decl(s);
                }
                for _ in path {
                    self.namespace_stack.pop();
                }
            }
            Stmt::Import { path, .. } => self.expand_import(path),
            Stmt::Block { stmts, .. } => {
                for s in stmts {
                    self.collect_stmt_decl(s);
                }
            }
            _ => {}
        }
    }

    fn register_type(&mut self, name: &str, def: TypeDef) {
        // Always register the bare name so un-prefixed lookups
        // find it. If we're inside a namespace, also register the
        // fully-qualified spelling for `foo::Bar` references.
        self.types.insert(name.to_owned(), def.clone());
        if !self.namespace_stack.is_empty() {
            let qualified = format!("{}::{}", self.namespace_stack.join("::"), name);
            self.types.insert(qualified, def);
        }
    }

    fn register_function(&mut self, name: &str, def: FunctionDef) {
        self.functions.insert(name.to_owned(), def.clone());
        if !self.namespace_stack.is_empty() {
            let qualified = format!("{}::{}", self.namespace_stack.join("::"), name);
            self.functions.insert(qualified, def);
        }
    }

    fn expand_import(&mut self, path: &[String]) {
        if path.is_empty() {
            return;
        }
        let key = path.join("::");
        if !self.imported.insert(key.clone()) {
            return; // already expanded
        }
        let Some(source) = self.resolver.resolve(path) else {
            // Surface as a diagnostic rather than a hard error so a
            // missing import doesn't abort the whole run.
            self.diagnostics.push(Diagnostic {
                message: format!("import `{key}` -- no resolver match"),
                severity: Severity::Warning,
                file_offset: None,
                template_line: None,
            });
            return;
        };
        let tokens = match crate::tokenize(&source) {
            Ok(t) => t,
            Err(e) => {
                self.diagnostics.push(Diagnostic {
                    message: format!("import `{key}` lex: {e}"),
                    severity: Severity::Error,
                    file_offset: None,
                    template_line: None,
                });
                return;
            }
        };
        let program = match crate::parse(tokens) {
            Ok(p) => p,
            Err(e) => {
                self.diagnostics.push(Diagnostic {
                    message: format!("import `{key}` parse: {e}"),
                    severity: Severity::Error,
                    file_offset: None,
                    template_line: None,
                });
                return;
            }
        };
        // Collect decls from the imported program. Imported files
        // are expected to be declaration-only; any top-level
        // statements they carry won't execute (we don't replay
        // imports under the running cursor).
        for it in &program.items {
            self.collect_decl(it);
        }
    }

    fn register_primitives(&mut self) {
        use PrimKind as P;
        let table: &[(&str, PrimKind)] = &[
            ("u8", P::u8()),
            ("u16", P::u16()),
            ("u32", P::u32()),
            ("u64", P::u64()),
            ("u128", P::u128()),
            ("s8", P::s8()),
            ("s16", P::s16()),
            ("s32", P::s32()),
            ("s64", P::s64()),
            ("s128", P::s128()),
            // ImHex's older `i*` aliases for signed ints. Common in
            // patterns ported from C-style sources.
            ("i8", P::s8()),
            ("i16", P::s16()),
            ("i32", P::s32()),
            ("i64", P::s64()),
            ("i128", P::s128()),
            ("float", P::f32()),
            ("double", P::f64()),
            ("f32", P::f32()),
            ("f64", P::f64()),
            ("char", P::char()),
            ("char16", P::char16()),
            ("bool", P::bool()),
            // `str` is modelled as a single-byte placeholder for
            // Phase 1; richer (NUL-terminated, length-prefixed)
            // string types arrive when the std library lands.
            ("str", P::char()),
        ];
        for (name, kind) in table {
            self.types.insert((*name).to_owned(), TypeDef::Primitive(*kind));
        }
    }

    fn lookup_type(&self, ty: &TypeRef) -> Result<TypeDef, RuntimeError> {
        // Try fully-qualified first, then the bare leaf. The decl
        // collector registers both spellings, but typedef'd
        // aliases land on the bare-leaf side so we fall through.
        let qualified = ty.path.join("::");
        let mut current: TypeRef = if self.types.contains_key(&qualified) {
            TypeRef { path: vec![qualified], template_args: ty.template_args.clone(), span: ty.span }
        } else {
            TypeRef { path: vec![ty.leaf().to_owned()], template_args: ty.template_args.clone(), span: ty.span }
        };
        for _ in 0..32 {
            let name = current.leaf().to_owned();
            match self.types.get(&name) {
                Some(TypeDef::Alias { params, target }) => {
                    // Templated alias: substitute use-site args
                    // (`Size<u32>` -> `T` becomes `u32` inside the
                    // alias body) and continue resolving. A bare
                    // alias with no params is a simple rename.
                    if !params.is_empty() {
                        let mut subs: HashMap<String, TypeRef> = HashMap::with_capacity(params.len());
                        for (param, arg) in params.iter().zip(current.template_args.iter()) {
                            if let Some(arg_ty) = expr_as_typeref(arg) {
                                subs.insert(param.clone(), arg_ty);
                            }
                        }
                        current = substitute_typeref(target, &subs);
                    } else {
                        current = TypeRef {
                            path: target.path.clone(),
                            template_args: target.template_args.clone(),
                            span: target.span,
                        };
                    }
                }
                Some(def) => return Ok(def.clone()),
                None => {
                    if let Some(prim) = generic_int_primitive(&name) {
                        return Ok(TypeDef::Primitive(prim));
                    }
                    return Err(RuntimeError::UnknownType { name: ty.leaf().to_owned() });
                }
            }
        }
        Err(RuntimeError::UnknownType { name: ty.leaf().to_owned() })
    }
}

/// Recognise ImHex's arbitrary-width integer primitives spelled as
/// `uN` / `sN` for any bit-count that is a multiple of 8 between 8
/// and 128 inclusive (`u24`, `s40`, `u56`, ...). The byte-aligned
/// variants come up in real corpus templates (DTED elevation data
/// uses `s24`, for example) but aren't worth listing exhaustively
/// in the static primitive table.
fn generic_int_primitive(name: &str) -> Option<PrimKind> {
    let (signed, rest) = match name.as_bytes().first()? {
        b'u' => (false, &name[1..]),
        b's' => (true, &name[1..]),
        _ => return None,
    };
    let bits: u32 = rest.parse().ok()?;
    if bits == 0 || bits > 128 || !bits.is_multiple_of(8) {
        return None;
    }
    Some(PrimKind { class: PrimClass::Int, width: (bits / 8) as u8, signed })
}

/// Best-effort coercion of a template-arg expression into a
/// [`TypeRef`] for use in alias substitution. `Foo<u32>` parses the
/// arg as `Expr::Ident("u32")`, `Foo<Bar<X>>` parses as
/// `Expr::TypeRefExpr`, and namespaced names land as `Expr::Path`.
/// Other expression kinds (literals, member access, ...) don't make
/// sense as a type substitution and return `None`.
fn expr_as_typeref(expr: &Expr) -> Option<TypeRef> {
    match expr {
        Expr::TypeRefExpr { ty, .. } => Some((**ty).clone()),
        Expr::Ident { name, span } => {
            Some(TypeRef { path: vec![name.clone()], template_args: Vec::new(), span: *span })
        }
        Expr::Path { segments, span } => {
            Some(TypeRef { path: segments.clone(), template_args: Vec::new(), span: *span })
        }
        _ => None,
    }
}

/// Walk a [`TypeRef`] and replace any single-segment path that
/// matches a key in `subs` with the corresponding type. Nested
/// template args are walked recursively. Used by [`Interpreter::lookup_type`]
/// when resolving a templated `using` alias.
fn substitute_typeref(ty: &TypeRef, subs: &HashMap<String, TypeRef>) -> TypeRef {
    if ty.path.len() == 1
        && let Some(replacement) = subs.get(&ty.path[0])
    {
        // Carry the substituted target's path and template args; if
        // the original referenced T<args>, fold our use-site args
        // through into the substituted path (rare in the corpus but
        // legal: `using Foo<T> = T<u32>;`).
        let mut out = replacement.clone();
        if !ty.template_args.is_empty() {
            out.template_args =
                ty.template_args.iter().map(|a| substitute_expr(a, subs)).collect();
        }
        return out;
    }
    TypeRef {
        path: ty.path.clone(),
        template_args: ty.template_args.iter().map(|a| substitute_expr(a, subs)).collect(),
        span: ty.span,
    }
}

fn substitute_expr(expr: &Expr, subs: &HashMap<String, TypeRef>) -> Expr {
    match expr {
        Expr::Ident { name, span } if subs.contains_key(name) => {
            let ty = subs.get(name).cloned().unwrap();
            Expr::TypeRefExpr { ty: Box::new(ty), span: *span }
        }
        Expr::TypeRefExpr { ty, span } => {
            Expr::TypeRefExpr { ty: Box::new(substitute_typeref(ty, subs)), span: *span }
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Statement execution.
// ---------------------------------------------------------------------------

impl<S: HexSource> Interpreter<S> {
    fn exec_stmt(&mut self, stmt: &Stmt, parent: Option<NodeIdx>) -> Result<Flow, RuntimeError> {
        self.steps = self.steps.saturating_add(1);
        if self.steps > self.step_limit {
            return Err(RuntimeError::StepLimitHit { step_limit: self.step_limit });
        }
        // Poll the external cancel flag on every step. A relaxed
        // atomic load is cheap (single instruction) so the bound on
        // worst-case cancel latency is one statement.
        if self.interrupt.load(Ordering::Relaxed) {
            return Err(RuntimeError::TimedOut { timeout_ms: 0 });
        }
        match stmt {
            Stmt::Block { stmts, .. } => {
                self.scopes.push(Scope::default());
                let mut flow = Flow::Next;
                for s in stmts {
                    flow = self.exec_stmt(s, parent)?;
                    if !matches!(flow, Flow::Next) {
                        break;
                    }
                }
                self.scopes.pop();
                Ok(flow)
            }
            Stmt::Expr { expr, .. } => {
                self.eval(expr)?;
                Ok(Flow::Next)
            }
            Stmt::If { cond, then_branch, else_branch, .. } => {
                if self.eval(cond)?.is_truthy() {
                    self.exec_stmt(then_branch, parent)
                } else if let Some(e) = else_branch.as_deref() {
                    self.exec_stmt(e, parent)
                } else {
                    Ok(Flow::Next)
                }
            }
            Stmt::While { cond, body, .. } => {
                let start = self.cursor_tell();
                let mut last_pos = start;
                let mut stalled = 0u32;
                while self.eval(cond)?.is_truthy() {
                    let flow = self.exec_stmt(body, parent)?;
                    match flow {
                        Flow::Break => break,
                        Flow::Return => return Ok(Flow::Return),
                        Flow::Continue | Flow::Next => {}
                    }
                    if self.cursor_tell() == last_pos {
                        stalled += 1;
                        if stalled >= LOOP_STALL_LIMIT {
                            return Err(RuntimeError::Type(format!(
                                "while loop stalled at offset {} for {} iterations",
                                self.cursor_tell(),
                                stalled
                            )));
                        }
                    } else {
                        stalled = 0;
                        last_pos = self.cursor_tell();
                    }
                }
                Ok(Flow::Next)
            }
            Stmt::For { init, cond, step, body, .. } => {
                self.scopes.push(Scope::default());
                if let Some(s) = init.as_deref() {
                    self.exec_stmt(s, parent)?;
                }
                let mut stalled = 0u32;
                let mut last_pos = self.cursor_tell();
                loop {
                    let go = match cond.as_ref() {
                        Some(e) => self.eval(e)?.is_truthy(),
                        None => true,
                    };
                    if !go {
                        break;
                    }
                    let flow = self.exec_stmt(body, parent)?;
                    match flow {
                        Flow::Break => break,
                        Flow::Return => {
                            self.scopes.pop();
                            return Ok(Flow::Return);
                        }
                        Flow::Continue | Flow::Next => {}
                    }
                    if let Some(e) = step.as_ref() {
                        self.eval(e)?;
                    }
                    if self.cursor_tell() == last_pos {
                        stalled += 1;
                        if stalled >= LOOP_STALL_LIMIT {
                            self.scopes.pop();
                            return Err(RuntimeError::Type(format!(
                                "for loop stalled at offset {} for {} iterations",
                                self.cursor_tell(),
                                stalled
                            )));
                        }
                    } else {
                        stalled = 0;
                        last_pos = self.cursor_tell();
                    }
                }
                self.scopes.pop();
                Ok(Flow::Next)
            }
            Stmt::Return { value, .. } => {
                self.return_value = match value {
                    Some(e) => Some(self.eval(e)?),
                    None => Some(Value::Void),
                };
                Ok(Flow::Return)
            }
            Stmt::Break { .. } => Ok(Flow::Break),
            Stmt::Continue { .. } => Ok(Flow::Continue),
            Stmt::FieldDecl { .. } => self.exec_field_decl(stmt, parent),
            // Bitfield-only stmts only show up inside a bitfield body
            // walked by `read_bitfield`. If one slips through here
            // (e.g. mistakenly placed at top level), treat it as a
            // no-op rather than crashing the run.
            Stmt::BitfieldField { .. } => Ok(Flow::Next),
            Stmt::Match { scrutinee, arms, .. } => self.exec_match(scrutinee, arms, parent),
            // The remaining variants are declarations the prepass
            // already collected -- nothing to execute.
            Stmt::StructDecl(_)
            | Stmt::EnumDecl(_)
            | Stmt::BitfieldDecl(_)
            | Stmt::FnDecl(_)
            | Stmt::UsingAlias { .. }
            | Stmt::Namespace { .. }
            | Stmt::Import { .. } => Ok(Flow::Next),
        }
    }

    fn exec_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        parent: Option<NodeIdx>,
    ) -> Result<Flow, RuntimeError> {
        let value = self.eval(scrutinee)?;
        for arm in arms {
            if self.arm_matches(&value, &arm.patterns)? {
                let mut flow = Flow::Next;
                for s in &arm.body {
                    flow = self.exec_stmt(s, parent)?;
                    if !matches!(flow, Flow::Next) {
                        break;
                    }
                }
                return Ok(flow);
            }
        }
        Ok(Flow::Next)
    }

    fn arm_matches(&mut self, value: &Value, patterns: &[MatchPattern]) -> Result<bool, RuntimeError> {
        for p in patterns {
            match p {
                MatchPattern::Wildcard { .. } => return Ok(true),
                MatchPattern::Value(e) => {
                    let pv = self.eval(e)?;
                    if values_equal(value, &pv) {
                        return Ok(true);
                    }
                }
                MatchPattern::Range { lo, hi, .. } => {
                    let lo_v = self.eval(lo)?;
                    let hi_v = self.eval(hi)?;
                    if let (Some(v), Some(l), Some(h)) = (value.to_i128(), lo_v.to_i128(), hi_v.to_i128())
                        && (l..=h).contains(&v)
                    {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    fn exec_field_decl(&mut self, stmt: &Stmt, parent: Option<NodeIdx>) -> Result<Flow, RuntimeError> {
        let Stmt::FieldDecl { is_const, ty, name, array, placement, init, attrs, pointer_width, .. } = stmt
        else {
            unreachable!();
        };
        let _ = is_const;
        // `auto` typed locals don't read from the source. A bare
        // `auto x = expr;` evaluates the initializer and binds the
        // result into the current scope.
        if ty.leaf() == "auto" {
            let value = match init {
                Some(e) => self.eval(e)?,
                None => Value::Void,
            };
            self.current_scope_mut().vars.insert(name.clone(), value);
            return Ok(Flow::Next);
        }
        let mut all_attrs = attrs_to_pairs(attrs);
        // `[[no_unique_address]]` -- the field reads at the current
        // cursor but doesn't advance it. Common idiom for inspecting
        // bytes without consuming them (e.g. read the whole file as
        // a sibling `data` field that visualisations hook off, then
        // continue parsing from offset 0). Treated as an implicit
        // placement at the current cursor.
        let no_unique_address =
            attrs.0.iter().any(|a| a.name == "no_unique_address");
        // `Type x @ offset;` -- save the running cursor, jump to
        // the placed address, read, then restore. The save/restore
        // is critical: ImHex placement reads don't advance the
        // surrounding sequential cursor.
        let saved_pos = if placement.is_some() || no_unique_address {
            Some(self.cursor_tell())
        } else {
            None
        };
        if let Some(p) = placement {
            let offset_value = self.eval(p)?;
            let offset = offset_value
                .to_i128()
                .ok_or_else(|| RuntimeError::Type(format!("placement offset not numeric: {offset_value}")))?
                .max(0) as u64;
            self.cursor_seek(offset);
            // Surface the requested address as an attribute so the
            // renderer can show "this field was placed".
            all_attrs.push(("hxy_placement".into(), offset.to_string()));
        }
        let count = match array {
            Some(ArraySize::Fixed(e)) => {
                let v = self.eval(e)?;
                v.to_i128().ok_or_else(|| RuntimeError::Type(format!("array size is not numeric: {v}")))? as u64
            }
            Some(ArraySize::Open) => 0,
            Some(ArraySize::While(_)) => {
                // Predicate-driven arrays. Phase 1 reads zero
                // elements (we don't have the parent-span scaffold to
                // bind the predicate's read context); a future phase
                // wires the cursor + condition through.
                0
            }
            None => 0,
        };
        // Pointer field: `Type *p : u32;`. Read the pointer-width
        // type at the cursor to get an address, save the cursor
        // (so the surrounding sequential walk continues from the
        // byte after the pointer), seek to the address, read the
        // target, restore. Indistinguishable to the renderer from
        // a placed read of the target type plus a small leaf
        // marking the pointer site.
        if let Some(addr_ty) = pointer_width {
            let addr_offset = self.cursor_tell();
            let addr_value = self.read_pointer_address(addr_ty)?;
            let post_addr = self.cursor_tell();
            // Mark the address slot as a leaf so the renderer can
            // show "pointer here".
            self.push_node(NodeOut {
                name: format!("{name}__ptr"),
                ty: NodeType::Unknown(format!("ptr<{}>", ty.leaf())),
                offset: addr_offset,
                length: post_addr - addr_offset,
                value: Some(Value::UInt { value: addr_value as u128, kind: PrimKind::u64() }),
                parent,
                attrs: vec![("hxy_pointer_target".into(), addr_value.to_string())],
            });
            // Read the target at the resolved address; restore.
            self.cursor_seek(addr_value);
            self.read_scalar(name, ty, parent, &all_attrs, init.as_ref())?;
            self.cursor_seek(post_addr);
        } else if ty.leaf() == "padding" {
            // `padding[N];` is a builtin: skip `N` bytes without
            // emitting a typed leaf. Matches the parser's
            // `parse_padding_decl` shape (the type ref's leaf is
            // literally `"padding"`).
            self.cursor_seek(self.cursor_tell().saturating_add(count));
        } else if array.is_some() {
            self.read_array(name, ty, count, parent, &all_attrs)?;
        } else {
            self.read_scalar(name, ty, parent, &all_attrs, init.as_ref())?;
        }
        // Restore the sequential cursor after a placement read so
        // subsequent fields keep reading from where we were before
        // the `@`.
        if let Some(saved) = saved_pos {
            self.cursor_seek(saved);
        }
        Ok(Flow::Next)
    }
}

// ---------------------------------------------------------------------------
// Reads.
// ---------------------------------------------------------------------------

const LOOP_STALL_LIMIT: u32 = 100_000;

impl<S: HexSource> Interpreter<S> {
    fn current_scope_mut(&mut self) -> &mut Scope {
        self.scopes.last_mut().expect("scope stack empty")
    }

    /// Push a node onto the emitted list and keep the by-name index
    /// in sync. Always use this instead of touching `self.nodes`
    /// directly so the lookup path stays consistent.
    fn push_node(&mut self, node: NodeOut) -> NodeIdx {
        let idx = NodeIdx::new(self.nodes.len() as u32);
        self.nodes_by_name.entry(node.name.clone()).or_default().push(idx);
        self.nodes.push(node);
        idx
    }

    /// Read the address of a pointer field. `addr_ty` must resolve
    /// to an integer primitive (typical: `u32`, `u64`); other shapes
    /// fall back to a u32 read since templates do that 90% of the
    /// time.
    fn read_pointer_address(&mut self, addr_ty: &TypeRef) -> Result<u64, RuntimeError> {
        let prim = match self.lookup_type(addr_ty) {
            Ok(TypeDef::Primitive(p)) if matches!(p.class, PrimClass::Int | PrimClass::Char) => p,
            _ => PrimKind::u32(),
        };
        let bytes = self.cursor_advance(prim.width as u64)?;
        let value = decode_prim(&bytes, prim, self.endian)?;
        Ok(value.to_i128().unwrap_or(0).max(0) as u64)
    }

    fn read_scalar(
        &mut self,
        name: &str,
        ty: &TypeRef,

        parent: Option<NodeIdx>,
        attrs: &[(String, String)],
        init: Option<&Expr>,
    ) -> Result<Value, RuntimeError> {
        // `std::mem::Bytes<N>` -- an N-byte read with no internal
        // structure. Surfaces as a single bytes-typed leaf. Match
        // either the qualified spelling or the bare `Bytes` leaf
        // (the corpus uses both).
        if matches!(ty.leaf(), "Bytes")
            && let Some(arg) = ty.template_args.first()
        {
            let v = self.eval(arg)?;
            let n = v.to_i128().ok_or_else(|| RuntimeError::Type(format!("Bytes<N>: N is not numeric: {v}")))?.max(0)
                as u64;
            let offset = self.cursor_tell();
            let bytes = self.cursor_advance(n)?;
            self.push_node(NodeOut {
                name: name.to_owned(),
                ty: NodeType::ScalarArray(ScalarKind::U8, n),
                offset,
                length: n,
                value: Some(Value::Bytes(bytes.clone())),
                parent,
                attrs: attrs.to_vec(),
            });
            return Ok(Value::Bytes(bytes));
        }
        let def = self.lookup_type(ty)?;
        match def {
            TypeDef::Primitive(p) => {
                let offset = self.cursor_tell();
                let bytes = self.cursor_advance(p.width as u64)?;
                let value = decode_prim(&bytes, p, self.endian)?;
                let kind_for_node = ScalarKind::from_prim(p);
                self.push_node(NodeOut {
                    name: name.to_owned(),
                    ty: NodeType::Scalar(kind_for_node),
                    offset,
                    length: p.width as u64,
                    value: Some(value.clone()),
                    parent,
                    attrs: attrs.to_vec(),
                });
                self.current_scope_mut().vars.insert(name.to_owned(), value.clone());
                let _ = init; // primitives don't take initializers in Phase 1
                Ok(value)
            }
            TypeDef::Enum(e) => self.read_enum(name, &e, parent, attrs),
            TypeDef::Struct(s) => self.read_struct(name, &s, ty, parent, attrs),
            TypeDef::Bitfield(b) => self.read_bitfield(name, &b, parent, attrs),
            TypeDef::Alias { .. } => unreachable!("lookup_type follows aliases"),
        }
    }

    fn read_struct(
        &mut self,
        name: &str,
        decl: &StructDecl,
        ty: &TypeRef,
        parent: Option<NodeIdx>,
        attrs: &[(String, String)],
    ) -> Result<Value, RuntimeError> {
        let offset = self.cursor_tell();
        let idx = NodeIdx::new(self.nodes.len() as u32);
        self.push_node(NodeOut {
            name: name.to_owned(),
            ty: NodeType::StructType(decl.name.clone()),
            offset,
            length: 0,
            value: None,
            parent,
            attrs: attrs.to_vec(),
        });
        // `self.current_parent` tracks the *enclosing* struct so the
        // magic `parent` identifier resolves through one hop. Inside
        // this struct's body the enclosing struct is whatever was
        // passed in as `parent` (the struct that contained this
        // field decl); it's NOT this struct itself -- that's `this`.
        let prev_parent = self.current_parent;
        self.current_parent = parent;
        self.scopes.push(Scope::default());

        // Bind template args to template params first, so the body
        // (and any inherited parent body) can see them. Pairs by
        // position; missing args resolve to `Void` / 0.
        //
        // Args that look type-shaped (`Ident`, `Path`, `TypeRefExpr`)
        // also register as a temporary alias under the param's name
        // so the body can use the param in *type* position
        // (`SizeType size;`). The aliases are removed when the body
        // finishes so they don't leak to surrounding reads.
        let mut template_type_aliases: Vec<String> = Vec::new();
        if !decl.template_params.is_empty() {
            for (i, param_name) in decl.template_params.iter().enumerate() {
                let arg_expr = ty.template_args.get(i);
                if let Some(arg) = arg_expr
                    && let Some(arg_ty) = expr_as_typeref(arg)
                {
                    self.types
                        .insert(param_name.clone(), TypeDef::Alias { params: Vec::new(), target: arg_ty });
                    template_type_aliases.push(param_name.clone());
                }
                let value = match arg_expr {
                    Some(e) => self.eval(e)?,
                    None => Value::Void,
                };
                self.current_scope_mut().vars.insert(param_name.clone(), value);
            }
        }

        // Compose any parent struct's body before this one's. ImHex
        // inheritance reads the parent fields first, then the child;
        // we look the parent type up in the same registry and inline
        // its statements. Multi-inheritance is not modelled.
        let parent_body: Vec<Stmt> = if let Some(parent_ty) = decl.parent.as_ref() {
            match self.lookup_type(parent_ty) {
                Ok(TypeDef::Struct(p)) => p.body.clone(),
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };

        if !decl.is_union {
            for s in parent_body.iter().chain(decl.body.iter()) {
                self.exec_stmt(s, Some(idx))?;
            }
            self.nodes[idx.as_usize()].length = self.cursor_tell() - offset;
        } else {
            // Union: every field starts at the same offset; the union's
            // own length is the widest child's. Cursor lands at the
            // farthest advance.
            let mut max_end = offset;
            for s in parent_body.iter().chain(decl.body.iter()) {
                self.cursor_seek(offset);
                self.exec_stmt(s, Some(idx))?;
                max_end = max_end.max(self.cursor_tell());
            }
            self.cursor_seek(max_end);
            self.nodes[idx.as_usize()].length = max_end - offset;
        }
        self.scopes.pop();
        self.current_parent = prev_parent;
        // Drop the temporary template-param aliases we registered on
        // entry so they don't shadow types in surrounding scopes.
        for alias in template_type_aliases {
            self.types.remove(&alias);
        }
        Ok(Value::Void)
    }

    fn read_enum(
        &mut self,
        name: &str,
        decl: &EnumDecl,

        parent: Option<NodeIdx>,
        attrs: &[(String, String)],
    ) -> Result<Value, RuntimeError> {
        let backing_def = self.lookup_type(&decl.backing)?;
        let prim = match backing_def {
            TypeDef::Primitive(p) if matches!(p.class, PrimClass::Int | PrimClass::Char) => p,
            _ => {
                return Err(RuntimeError::Type(format!("enum `{}` backing must be an integer primitive", decl.name)));
            }
        };
        let offset = self.cursor_tell();
        let bytes = self.cursor_advance(prim.width as u64)?;
        let raw = decode_prim(&bytes, prim, self.endian)?;
        let raw_u = raw.to_i128().unwrap_or(0) as u128;
        // Match the value against declared variants. ImHex variants
        // can have arbitrary expressions; Phase 1 evaluates them in a
        // fresh side scope and falls back to the numeric form when no
        // match is found.
        let mut found_name: Option<String> = None;
        let mut auto_counter: i128 = 0;
        for v in &decl.variants {
            let target: i128 = match v.value.as_ref() {
                Some(e) => {
                    let val = self.eval(e)?;
                    val.to_i128().unwrap_or(0)
                }
                None => auto_counter,
            };
            auto_counter = target.wrapping_add(1);
            if (target as u128) == raw_u {
                found_name = Some(v.name.clone());
                break;
            }
        }
        let display_value = match &found_name {
            Some(n) => Value::Str(n.clone()),
            None => Value::Str(format!("{raw_u}")),
        };
        let value_for_node = display_value.clone();
        self.push_node(NodeOut {
            name: name.to_owned(),
            ty: NodeType::EnumType(decl.name.clone()),
            offset,
            length: prim.width as u64,
            value: Some(value_for_node),
            parent,
            attrs: attrs.to_vec(),
        });
        // Bind the raw numeric so later expressions like `if (kind ==
        // 1)` keep working.
        let raw_value = Value::UInt { value: raw_u, kind: PrimKind::u64() };
        self.current_scope_mut().vars.insert(name.to_owned(), raw_value.clone());
        Ok(raw_value)
    }

    fn read_bitfield(
        &mut self,
        name: &str,
        decl: &BitfieldDecl,

        parent: Option<NodeIdx>,
        attrs: &[(String, String)],
    ) -> Result<Value, RuntimeError> {
        // Compute total bits by collecting every BitfieldField in the
        // body, recursing into conditionals so nested fields still
        // contribute to the slot width. We pessimistically include
        // both branches of any if/match; the runtime walk only
        // emits / binds the fields whose enclosing branch actually
        // executes.
        let mut total_bits: u32 = 0;
        self.bitfield_body_total_bits(&decl.body, &mut total_bits)?;
        let width_bytes = total_bits.div_ceil(8).max(1).next_power_of_two().min(16) as u8;
        let width_bytes = match width_bytes {
            1..=2 => width_bytes,
            3..=4 => 4,
            5..=8 => 8,
            _ => 16,
        };
        let offset = self.cursor_tell();
        let bytes = self.cursor_advance(width_bytes as u64)?;
        let raw =
            decode_prim(&bytes, PrimKind { class: PrimClass::Int, width: width_bytes, signed: false }, self.endian)?;
        let raw_u = raw.to_i128().unwrap_or(0) as u128;
        let bf_idx = NodeIdx::new(self.nodes.len() as u32);
        self.push_node(NodeOut {
            name: name.to_owned(),
            ty: NodeType::BitfieldType(decl.name.clone()),
            offset,
            length: width_bytes as u64,
            value: Some(Value::UInt { value: raw_u, kind: PrimKind::u128() }),
            parent,
            attrs: attrs.to_vec(),
        });
        // Per-field extraction. Bits run low-to-high in declaration
        // order -- matches the ImHex default; the `bitfield_order`
        // attribute can override at a future phase.
        let mut consumed: u32 = 0;
        self.exec_bitfield_body(&decl.body, raw_u, offset, bf_idx, &mut consumed)?;
        Ok(Value::UInt { value: raw_u, kind: PrimKind::u128() })
    }

    fn bitfield_body_total_bits(&mut self, body: &[Stmt], total: &mut u32) -> Result<(), RuntimeError> {
        for stmt in body {
            match stmt {
                Stmt::BitfieldField { width, .. } => {
                    let bits = self.eval(width)?.to_i128().unwrap_or(0).max(0) as u32;
                    *total = total.saturating_add(bits);
                }
                Stmt::If { then_branch, else_branch, .. } => {
                    if let Stmt::Block { stmts, .. } = then_branch.as_ref() {
                        self.bitfield_body_total_bits(stmts, total)?;
                    } else {
                        self.bitfield_body_total_bits(std::slice::from_ref(then_branch.as_ref()), total)?;
                    }
                    if let Some(e) = else_branch.as_deref() {
                        if let Stmt::Block { stmts, .. } = e {
                            self.bitfield_body_total_bits(stmts, total)?;
                        } else {
                            self.bitfield_body_total_bits(std::slice::from_ref(e), total)?;
                        }
                    }
                }
                Stmt::Match { arms, .. } => {
                    for arm in arms {
                        self.bitfield_body_total_bits(&arm.body, total)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn exec_bitfield_body(
        &mut self,
        body: &[Stmt],
        raw_u: u128,
        slot_offset: u64,
        bf_idx: NodeIdx,
        consumed: &mut u32,
    ) -> Result<(), RuntimeError> {
        for stmt in body {
            match stmt {
                Stmt::BitfieldField { name, width, .. } => {
                    let bits = self.eval(width)?.to_i128().unwrap_or(0).max(0) as u32;
                    if bits == 0 {
                        continue;
                    }
                    let mask: u128 = if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 };
                    let value_u = (raw_u >> *consumed) & mask;
                    *consumed = consumed.saturating_add(bits);
                    let field_attrs = vec![("hxy_bits".into(), bits.to_string())];
                    self.push_node(NodeOut {
                        name: name.clone(),
                        ty: NodeType::Scalar(ScalarKind::U64),
                        offset: slot_offset,
                        length: 0,
                        value: Some(Value::UInt { value: value_u, kind: PrimKind::u64() }),
                        parent: Some(bf_idx),
                        attrs: field_attrs,
                    });
                    self.current_scope_mut()
                        .vars
                        .insert(name.clone(), Value::UInt { value: value_u, kind: PrimKind::u64() });
                }
                Stmt::If { cond, then_branch, else_branch, .. } => {
                    let take_then = self.eval(cond)?.is_truthy();
                    let branch: &Stmt = if take_then {
                        then_branch.as_ref()
                    } else if let Some(e) = else_branch.as_deref() {
                        e
                    } else {
                        continue;
                    };
                    if let Stmt::Block { stmts, .. } = branch {
                        self.exec_bitfield_body(stmts, raw_u, slot_offset, bf_idx, consumed)?;
                    } else {
                        self.exec_bitfield_body(std::slice::from_ref(branch), raw_u, slot_offset, bf_idx, consumed)?;
                    }
                }
                Stmt::Match { scrutinee, arms, .. } => {
                    let value = self.eval(scrutinee)?;
                    for arm in arms {
                        if self.arm_matches(&value, &arm.patterns)? {
                            self.exec_bitfield_body(&arm.body, raw_u, slot_offset, bf_idx, consumed)?;
                            break;
                        }
                    }
                }
                Stmt::Expr { expr, .. } => {
                    self.eval(expr)?;
                }
                // Computed value: `Type name = expr;`. ImHex bitfield
                // bodies use this to expose derived fields without
                // consuming bits. We bind the initializer so later
                // expressions can see it but emit no node.
                Stmt::FieldDecl { name, init: Some(e), .. } => {
                    let v = self.eval(e)?;
                    self.current_scope_mut().vars.insert(name.clone(), v);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn read_array(
        &mut self,
        name: &str,
        ty: &TypeRef,
        count: u64,

        parent: Option<NodeIdx>,
        attrs: &[(String, String)],
    ) -> Result<Value, RuntimeError> {
        let def = self.lookup_type(ty)?;
        if let TypeDef::Primitive(p) = def
            && matches!(p.class, PrimClass::Char)
        {
            // `char[N]` -> read a contiguous string. Common pattern in
            // both 010 and ImHex.
            let total = (p.width as u64).saturating_mul(count);
            let offset = self.cursor_tell();
            let bytes = self.cursor_advance(total)?;
            let s = String::from_utf8_lossy(&bytes).into_owned();
            self.push_node(NodeOut {
                name: name.to_owned(),
                ty: NodeType::ScalarArray(ScalarKind::Char, count),
                offset,
                length: total,
                value: Some(Value::Str(s.clone())),
                parent,
                attrs: attrs.to_vec(),
            });
            self.current_scope_mut().vars.insert(name.to_owned(), Value::Str(s));
            return Ok(Value::Void);
        }
        // Generic array: emit one parent node, then `count` children.
        let offset = self.cursor_tell();
        let parent_idx = NodeIdx::new(self.nodes.len() as u32);
        let arr_ty_label = ty.leaf().to_owned();
        self.push_node(NodeOut {
            name: name.to_owned(),
            ty: NodeType::Unknown(format!("{arr_ty_label}[{count}]")),
            offset,
            length: 0,
            value: None,
            parent,
            attrs: attrs.to_vec(),
        });
        for i in 0..count {
            self.read_scalar(&format!("[{i}]"), ty, Some(parent_idx), &[], None)?;
        }
        self.nodes[parent_idx.as_usize()].length = self.cursor_tell() - offset;
        Ok(Value::Void)
    }
}

// ---------------------------------------------------------------------------
// Expression evaluation.
// ---------------------------------------------------------------------------

impl<S: HexSource> Interpreter<S> {
    fn eval(&mut self, expr: &Expr) -> Result<Value, RuntimeError> {
        match expr {
            Expr::IntLit { value, .. } => Ok(Value::UInt { value: *value, kind: PrimKind::u64() }),
            Expr::FloatLit { value, .. } => Ok(Value::Float { value: *value, kind: PrimKind::f64() }),
            Expr::StringLit { value, .. } => Ok(Value::Str(value.clone())),
            Expr::CharLit { value, .. } => Ok(Value::Char { value: *value, kind: PrimKind::char() }),
            Expr::BoolLit { value, .. } => Ok(Value::Bool(*value)),
            Expr::NullLit { .. } => Ok(Value::UInt { value: 0, kind: PrimKind::u64() }),
            Expr::Ident { name, .. } => match name.as_str() {
                // Magic identifiers resolved against runtime state
                // before the regular scope-stack walk. `$` is the
                // current cursor offset, `parent` and `this` are
                // handled inline in member-access lookups.
                "$" => Ok(Value::UInt { value: self.cursor_tell() as u128, kind: PrimKind::u64() }),
                _ => self.lookup_ident(name),
            },
            Expr::Path { segments, .. } => {
                // `Foo::...::Enum::Variant` -- treat the trailing
                // segment as a variant name and walk the prefix
                // looking for a registered enum. We try every prefix
                // length so both `Tag::Variant` and the namespaced
                // `std::mem::Endian::Big` shape land. The interpreter
                // registers types under both their bare and fully-
                // qualified spellings, so the lookup needs only one
                // attempt per prefix length.
                if segments.len() >= 2 {
                    let variant_name = segments.last().cloned().unwrap_or_default();
                    for prefix_len in (1..segments.len()).rev() {
                        let key: String = segments[..prefix_len].join("::");
                        if let Some(TypeDef::Enum(decl)) = self.types.get(&key).cloned() {
                            let mut running: i128 = 0;
                            for variant in &decl.variants {
                                if let Some(value_expr) = variant.value.as_ref() {
                                    running = self.eval(value_expr)?.to_i128().unwrap_or(running);
                                }
                                if variant.name == variant_name {
                                    return Ok(Value::UInt {
                                        value: running as u128,
                                        kind: PrimKind::u64(),
                                    });
                                }
                                running = running.saturating_add(1);
                            }
                            // Found the enum but not the variant --
                            // fall through to the leaf-ident path,
                            // which will surface a useful error.
                            break;
                        }
                    }
                }
                // Fall back to the bare leaf for `std::print` and
                // similar where the namespace prefix is decorative.
                let leaf = segments.last().cloned().unwrap_or_default();
                self.lookup_ident(&leaf)
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                let l = self.eval(lhs)?;
                let r = self.eval(rhs)?;
                eval_binary(*op, &l, &r)
            }
            Expr::Unary { op, operand, .. } => {
                if matches!(op, UnaryOp::Pos | UnaryOp::Neg | UnaryOp::Not | UnaryOp::BitNot) {
                    let v = self.eval(operand)?;
                    return eval_unary(*op, &v);
                }
                // Pre/post inc/dec require an l-value; without one
                // we evaluate, increment, but don't rebind. Fine for
                // Phase 1 since the corpus rarely depends on the
                // mutation.
                let v = self.eval(operand)?;
                eval_unary(*op, &v)
            }
            Expr::Ternary { cond, then_val, else_val, .. } => {
                if self.eval(cond)?.is_truthy() {
                    self.eval(then_val)
                } else {
                    self.eval(else_val)
                }
            }
            Expr::Assign { op, target, value, .. } => {
                let new_val = match op {
                    AssignOp::Assign => self.eval(value)?,
                    other => {
                        let cur = self.eval(target)?;
                        let rhs = self.eval(value)?;
                        eval_binary(compound_to_bin(*other), &cur, &rhs)?
                    }
                };
                if let Expr::Ident { name, .. } = target.as_ref() {
                    self.store_ident(name, new_val.clone());
                }
                Ok(new_val)
            }
            Expr::Member { target, field, .. } => {
                // Resolve the chain by walking left-to-right and
                // tracking which emitted node we're currently
                // pointing at. The chain accepts magic identifiers
                // (`parent` / `this`), bare struct names, indexed
                // accesses (`lumps[i]`), and member hops, so
                // expressions like `file_header.lumps[i].fileofs`
                // resolve in one pass.
                if let Some(idx) = self.resolve_node_chain(target)?
                    && let Some(v) = self.lookup_member_under(idx, field)
                {
                    return Ok(v);
                }
                Err(RuntimeError::Type(format!("unresolved member `.{field}`")))
            }
            Expr::Index { target, index, .. } => {
                // Phase 1: `arr[i]` returns the raw index value. The
                // renderer doesn't need indexed reads to drive its
                // tree; later phases hook this up to lazy primitive-
                // array decoding.
                let _ = self.eval(target)?;
                self.eval(index)
            }
            Expr::Call { callee, args, .. } => {
                // Build a list of candidate names to try in order:
                // the fully-qualified path first (so a namespaced
                // builtin like `std::print` wins over a same-leaf
                // user function), then the bare leaf as a fallback.
                let names: Vec<String> = match callee.as_ref() {
                    Expr::Ident { name, .. } => vec![name.clone()],
                    Expr::Path { segments, .. } => {
                        let mut v = vec![segments.join("::")];
                        if let Some(leaf) = segments.last() {
                            v.push(leaf.clone());
                        }
                        v
                    }
                    _ => return Err(RuntimeError::Type("call target must be an identifier".into())),
                };
                let evaluated: Vec<Value> = args.iter().map(|a| self.eval(a)).collect::<Result<_, _>>()?;
                let mut last_err: Option<RuntimeError> = None;
                for name in &names {
                    match self.call_named(name, &evaluated) {
                        Ok(v) => return Ok(v),
                        Err(RuntimeError::UndefinedName { .. }) => continue,
                        Err(e) => last_err = Some(e),
                    }
                }
                Err(last_err
                    .unwrap_or(RuntimeError::UndefinedName { name: names.first().cloned().unwrap_or_default() }))
            }
            Expr::Reflect { kind, operand, .. } => self.eval_reflect(*kind, operand),
            // A nested type reference in expression position: only
            // shows up inside template-arg lists (`Foo<Bar<X>>`).
            // We don't have a first-class "type value" yet -- emit
            // the type name as a string so callers that read it as a
            // tag (e.g. via `std::format`) get something useful.
            Expr::TypeRefExpr { ty, .. } => Ok(Value::Str(ty.path.join("::"))),
        }
    }

    /// `sizeof(x)`, `addressof(x)`, `typeof(x)`. The operand can be a
    /// type name (resolved against the type table for `sizeof`) or a
    /// value reference (resolved against the most-recently-emitted
    /// node for `addressof`/`sizeof of a field`).
    fn eval_reflect(&mut self, kind: ReflectKind, operand: &Expr) -> Result<Value, RuntimeError> {
        // Identifier-as-type or identifier-as-field: peek directly
        // to avoid running through `lookup_ident`, which would fail
        // on type names.
        if let Expr::Ident { name, .. } = operand {
            // Try type table first for `sizeof`.
            if matches!(kind, ReflectKind::Sizeof)
                && let Ok(bytes) = self.sizeof_type_name(name)
            {
                return Ok(Value::UInt { value: bytes as u128, kind: PrimKind::u64() });
            }
            // Then look up the field by name.
            for n in self.nodes.iter().rev() {
                if n.name == *name {
                    return Ok(match kind {
                        ReflectKind::Sizeof => Value::UInt { value: n.length as u128, kind: PrimKind::u64() },
                        ReflectKind::Addressof => Value::UInt { value: n.offset as u128, kind: PrimKind::u64() },
                        ReflectKind::Typeof => Value::Str(format!("{:?}", n.ty)),
                    });
                }
            }
            // Special case: `sizeof($)` -> file size. ImHex's
            // pattern reference uses this idiom for "is there enough
            // data".
            if matches!(kind, ReflectKind::Sizeof) && name == "$" {
                return Ok(Value::UInt { value: self.source.len() as u128, kind: PrimKind::u64() });
            }
        }
        // For complex expressions (member chains, indexing) we
        // evaluate and use the dynamic value's apparent size. This
        // is degraded relative to ImHex's compile-time sizeof but
        // covers the common runtime case.
        let v = self.eval(operand)?;
        Ok(match kind {
            ReflectKind::Sizeof => Value::UInt { value: value_byte_size(&v) as u128, kind: PrimKind::u64() },
            ReflectKind::Addressof => Value::UInt { value: 0, kind: PrimKind::u64() },
            ReflectKind::Typeof => Value::Str(format!("{v}")),
        })
    }

    /// Static size of a registered type, in bytes. Walks aliases,
    /// sums struct field widths, picks the widest union member, and
    /// uses the backing primitive for enums. Returns 0 for unknown
    /// types so `sizeof(Unknown)` doesn't trap the run.
    fn sizeof_type_name(&self, name: &str) -> Result<u64, RuntimeError> {
        let mut cur = name.to_owned();
        for _ in 0..32 {
            match self.types.get(&cur) {
                Some(TypeDef::Primitive(p)) => return Ok(p.width as u64),
                Some(TypeDef::Alias { target, .. }) => cur = target.leaf().to_owned(),
                Some(TypeDef::Enum(e)) => return self.sizeof_type_name(e.backing.leaf()),
                Some(TypeDef::Bitfield(_)) => return Ok(0), // dynamic; renderer-only
                Some(TypeDef::Struct(s)) => {
                    let body = s.body.clone();
                    let is_union = s.is_union;
                    let mut total: u64 = 0;
                    for stmt in &body {
                        if let Stmt::FieldDecl { ty: field_ty, array, .. } = stmt {
                            let elem = self.sizeof_type_name(field_ty.leaf()).unwrap_or(0);
                            let len = match array {
                                None => elem,
                                Some(_) => 0, // dynamic; can't size statically
                            };
                            if is_union {
                                total = total.max(len);
                            } else {
                                total = total.saturating_add(len);
                            }
                        }
                    }
                    return Ok(total);
                }
                None => return Ok(0),
            }
        }
        Ok(0)
    }

    fn lookup_ident(&self, name: &str) -> Result<Value, RuntimeError> {
        for scope in self.scopes.iter().rev() {
            if let Some(v) = scope.vars.get(name) {
                return Ok(v.clone());
            }
        }
        // `$` -- current cursor offset. Phase 1 wires it in here so
        // simple uses (`u8 a[16 - $]`) work; the deeper magic ident
        // story lands in Phase 2.
        if name == "$" {
            // We don't have a borrow on the cursor in `lookup_ident`,
            // so callers that need the live cursor should plumb it
            // through. Returning 0 is the conservative fallback.
            return Ok(Value::UInt { value: 0, kind: PrimKind::u64() });
        }
        Err(RuntimeError::UndefinedName { name: name.to_owned() })
    }

    /// Look up a child node whose `parent == owner_idx` and whose
    /// name matches `field`. Walks the tree right-to-left so the
    /// most-recently-emitted match wins (matters for repeated names
    /// inside a long-lived parent like an array).
    fn lookup_member_under(&self, owner_idx: NodeIdx, field: &str) -> Option<Value> {
        // Walk the by-name index right-to-left so the most-recently-
        // emitted child wins when `field` shows up on more than one
        // sibling (matters for repeated names inside a long-lived
        // parent like an array).
        let candidates = self.nodes_by_name.get(field)?;
        for &idx in candidates.iter().rev() {
            let n = &self.nodes[idx.as_usize()];
            if n.parent == Some(owner_idx) {
                return n.value.clone();
            }
        }
        None
    }

    /// Walk a chain of `Ident` / `Member` / `Index` expressions and
    /// return the [`NodeIdx`] it ultimately points at. Returns
    /// `Ok(None)` when the chain doesn't refer to an emitted node
    /// (e.g. it bottoms out at a literal). Used by the [`Expr::Member`]
    /// arm so nested accesses like `file_header.lumps[i].fileofs`
    /// resolve through the same logic as `parent.foo`.
    fn resolve_node_chain(&mut self, expr: &Expr) -> Result<Option<NodeIdx>, RuntimeError> {
        match expr {
            Expr::Ident { name, .. } => Ok(self.find_node_idx_for_ident(name)),
            Expr::Member { target, field, .. } => {
                let Some(parent_idx) = self.resolve_node_chain(target)? else {
                    return Ok(None);
                };
                Ok(self.find_first_child_idx(parent_idx, field))
            }
            Expr::Index { target, index, .. } => {
                // `arr[i]` -- the array node has the name `arr` and
                // emits N children named `[0]`, `[1]`, ... so we
                // first resolve the array node itself, then look up
                // its i-th child.
                let i = self.eval(index)?.to_i128().unwrap_or(0).max(0) as usize;
                let Some(arr_idx) = self.resolve_node_chain(target)? else {
                    return Ok(None);
                };
                let element_name = format!("[{i}]");
                Ok(self.find_first_child_idx(arr_idx, &element_name))
            }
            _ => Ok(None),
        }
    }

    /// Map an identifier in node-chain position to the most plausible
    /// emitted node: magic identifiers (`parent`, `this`) resolve to
    /// the active enclosing struct, bare names take the most recent
    /// node with that name from the by-name index.
    fn find_node_idx_for_ident(&self, name: &str) -> Option<NodeIdx> {
        match name {
            "parent" => self.current_parent,
            "this" => self.most_recent_struct_idx(),
            other => self.nodes_by_name.get(other).and_then(|v| v.last().copied()),
        }
    }

    fn find_first_child_idx(&self, parent_idx: NodeIdx, name: &str) -> Option<NodeIdx> {
        let candidates = self.nodes_by_name.get(name)?;
        candidates.iter().copied().find(|idx| self.nodes[idx.as_usize()].parent == Some(parent_idx))
    }

    /// Index of the most recently emitted struct / union / bitfield
    /// node. Used to back `this.field` lookups when no scope-stack
    /// pointer is available (the runtime doesn't carry an explicit
    /// "current struct" idx; the most-recent struct in the emitted
    /// list is the one we just entered).
    fn most_recent_struct_idx(&self) -> Option<NodeIdx> {
        for (i, n) in self.nodes.iter().enumerate().rev() {
            if matches!(n.ty, NodeType::StructType(_) | NodeType::BitfieldType(_)) {
                return Some(NodeIdx::new(i as u32));
            }
        }
        None
    }

    fn store_ident(&mut self, name: &str, value: Value) {
        for scope in self.scopes.iter_mut().rev() {
            if scope.vars.contains_key(name) {
                scope.vars.insert(name.to_owned(), value);
                return;
            }
        }
        // Auto-declare in the current scope.
        self.current_scope_mut().vars.insert(name.to_owned(), value);
    }

    fn call_named(&mut self, name: &str, args: &[Value]) -> Result<Value, RuntimeError> {
        if let Some(v) = self.call_builtin(name, args)? {
            return Ok(v);
        }
        let Some(func) = self.functions.get(name).cloned() else {
            return Err(RuntimeError::UndefinedName { name: name.to_owned() });
        };
        if func.params.len() != args.len() {
            return Err(RuntimeError::Type(format!("{} expects {} args, got {}", name, func.params.len(), args.len())));
        }
        self.scopes.push(Scope::default());
        for (p, v) in func.params.iter().zip(args.iter()) {
            self.current_scope_mut().vars.insert(p.name.clone(), v.clone());
        }
        let saved_return = self.return_value.take();
        let mut result = Value::Void;
        for s in &func.body {
            match self.exec_stmt(s, None)? {
                Flow::Return => {
                    if let Some(v) = self.return_value.take() {
                        result = v;
                    }
                    break;
                }
                Flow::Break | Flow::Continue | Flow::Next => {}
            }
        }
        self.return_value = saved_return;
        self.scopes.pop();
        Ok(result)
    }

    /// Lookup a small set of language built-ins. Names are matched
    /// against both the fully-qualified spelling (`std::print`)
    /// and the bare leaf (`print`); the call dispatcher in
    /// [`Self::eval`] tries each in turn so a corpus pattern that
    /// imports `std::print` and one that calls bare `print` both
    /// land here.
    fn call_builtin(&mut self, name: &str, args: &[Value]) -> Result<Option<Value>, RuntimeError> {
        Ok(Some(match name {
            "set_endian" | "std::core::set_endian" | "std::mem::set_endian" => {
                if matches!(args.first(), Some(Value::Bool(true))) {
                    self.endian = Endian::Big;
                } else if matches!(args.first(), Some(Value::Bool(false))) {
                    self.endian = Endian::Little;
                }
                Value::Void
            }
            // Logging / diagnostics.
            "print" | "std::print" => {
                self.emit_diag(Severity::Info, args);
                Value::Void
            }
            "error" | "std::error" => {
                self.emit_diag(Severity::Error, args);
                Value::Void
            }
            // `std::format(fmt, args...)` returns the formatted
            // string instead of pushing a diagnostic.
            "format" | "std::format" => Value::Str(format_args(args)),
            // `std::assert(cond, msg)` -- emit a diagnostic +
            // halt the run on a falsy condition.
            "assert" | "std::assert" => {
                if args.first().is_some_and(Value::is_truthy) {
                    return Ok(Some(Value::Void));
                }
                let msg = args.get(1).map(|v| format!("{v}")).unwrap_or_else(|| "assertion failed".to_owned());
                self.diagnostics.push(Diagnostic {
                    message: format!("assert: {msg}"),
                    severity: Severity::Error,
                    file_offset: Some(self.cursor_tell()),
                    template_line: None,
                });
                return Err(RuntimeError::Type(format!("assert failed: {msg}")));
            }
            // File-level queries.
            "std::mem::eof" => Value::Bool(self.cursor_tell() >= self.source.len()),
            "std::mem::size" | "std::mem::file_size" => {
                Value::UInt { value: self.source.len() as u128, kind: PrimKind::u64() }
            }
            "std::mem::current_offset" => Value::UInt { value: self.cursor_tell() as u128, kind: PrimKind::u64() },
            // Random-access reads. `std::mem::read_unsigned(off, n)`
            // / `read_signed(off, n)` return n-byte ints from the
            // source. `read_string(off, n)` returns n bytes lossy
            // utf-8.
            "std::mem::read_unsigned" | "read_unsigned" => self.std_read_int(args, false)?,
            "std::mem::read_signed" | "read_signed" => self.std_read_int(args, true)?,
            "std::mem::read_string" | "read_string" => self.std_read_string(args)?,
            // String helpers. `length` works on chars; the rest
            // operate on raw byte content. Approximations only.
            "std::string::length" | "std::string::strlen" => {
                let s = string_arg(args, 0);
                Value::UInt { value: s.chars().count() as u128, kind: PrimKind::u64() }
            }
            "std::string::starts_with" => Value::Bool(string_arg(args, 0).starts_with(&string_arg(args, 1))),
            "std::string::ends_with" => Value::Bool(string_arg(args, 0).ends_with(&string_arg(args, 1))),
            "std::string::contains" => Value::Bool(string_arg(args, 0).contains(&string_arg(args, 1))),
            // Math helpers.
            "std::math::abs" => {
                let v = args.first().and_then(|v| v.to_i128()).unwrap_or(0);
                Value::SInt { value: v.abs(), kind: PrimKind::s64() }
            }
            "std::math::min" => min_max_value(args, true),
            "std::math::max" => min_max_value(args, false),
            // Default: no-op acknowledgement for `std::core::*`
            // pragma-style calls (set_display_name, set_pattern_color,
            // etc.) so corpus templates that decorate with these
            // don't trip the interpreter. Returning `Void` for any
            // unrecognised `std::core::` prefix keeps the run going.
            _ if name.starts_with("std::core::") => Value::Void,
            _ => return Ok(None),
        }))
    }

    fn emit_diag(&mut self, severity: Severity, args: &[Value]) {
        self.diagnostics.push(Diagnostic {
            message: format_args(args),
            severity,
            file_offset: Some(self.cursor_tell()),
            template_line: None,
        });
    }

    fn std_read_int(&self, args: &[Value], signed: bool) -> Result<Value, RuntimeError> {
        let offset = args.first().and_then(|v| v.to_i128()).unwrap_or(0).max(0) as u64;
        let size = args.get(1).and_then(|v| v.to_i128()).unwrap_or(0).clamp(0, 8) as u8;
        if size == 0 {
            return Ok(Value::UInt { value: 0, kind: PrimKind::u64() });
        }
        let bytes = self.source.read(offset, size as u64).map_err(RuntimeError::from)?;
        let kind = PrimKind { class: PrimClass::Int, width: size, signed };
        decode_prim(&bytes, kind, self.endian)
    }

    fn std_read_string(&self, args: &[Value]) -> Result<Value, RuntimeError> {
        let offset = args.first().and_then(|v| v.to_i128()).unwrap_or(0).max(0) as u64;
        let size = args.get(1).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as u64;
        let bytes = self.source.read(offset, size).map_err(RuntimeError::from)?;
        Ok(Value::Str(String::from_utf8_lossy(&bytes).into_owned()))
    }
}

// ---------------------------------------------------------------------------
// Primitive decode + arithmetic.
// ---------------------------------------------------------------------------

fn decode_prim(bytes: &[u8], kind: PrimKind, endian: Endian) -> Result<Value, RuntimeError> {
    if bytes.len() as u8 != kind.width {
        return Err(RuntimeError::Type(format!("short read: got {} bytes, need {}", bytes.len(), kind.width)));
    }
    let to_arr = |buf: &mut [u8; 16]| {
        if matches!(endian, Endian::Little) {
            buf[..bytes.len()].copy_from_slice(bytes);
        } else {
            buf[16 - bytes.len()..].copy_from_slice(bytes);
        }
    };
    match kind.class {
        PrimClass::Bool => Ok(Value::Bool(bytes[0] != 0)),
        PrimClass::Char => Ok(Value::Char { value: bytes[0] as u32, kind }),
        PrimClass::Int => {
            let mut buf = [0u8; 16];
            to_arr(&mut buf);
            let raw = match endian {
                Endian::Little => u128::from_le_bytes(buf),
                Endian::Big => u128::from_be_bytes(buf),
            };
            if kind.signed {
                let bits = (kind.width as u32) * 8;
                let shift = 128 - bits;
                let signed = ((raw << shift) as i128) >> shift;
                Ok(Value::SInt { value: signed, kind })
            } else {
                Ok(Value::UInt { value: raw, kind })
            }
        }
        PrimClass::Float => {
            if kind.width == 4 {
                let arr: [u8; 4] = bytes.try_into().map_err(|_| RuntimeError::Type("bad f32".into()))?;
                let v = match endian {
                    Endian::Little => f32::from_le_bytes(arr),
                    Endian::Big => f32::from_be_bytes(arr),
                };
                Ok(Value::Float { value: v as f64, kind })
            } else {
                let arr: [u8; 8] = bytes.try_into().map_err(|_| RuntimeError::Type("bad f64".into()))?;
                let v = match endian {
                    Endian::Little => f64::from_le_bytes(arr),
                    Endian::Big => f64::from_be_bytes(arr),
                };
                Ok(Value::Float { value: v, kind })
            }
        }
    }
}

fn eval_binary(op: BinOp, l: &Value, r: &Value) -> Result<Value, RuntimeError> {
    // String concat / equality: keep strings on the string path,
    // everything else widens to numeric.
    if let (Value::Str(a), Value::Str(b)) = (l, r) {
        return Ok(match op {
            BinOp::Add => Value::Str(format!("{a}{b}")),
            BinOp::Eq => Value::Bool(a == b),
            BinOp::NotEq => Value::Bool(a != b),
            _ => return Err(RuntimeError::Type(format!("string operand not supported for {op:?}"))),
        });
    }
    if l.to_f64().is_some()
        && r.to_f64().is_some()
        && (matches!(l, Value::Float { .. }) || matches!(r, Value::Float { .. }))
    {
        let lf = l.to_f64().unwrap();
        let rf = r.to_f64().unwrap();
        return Ok(match op {
            BinOp::Add => Value::Float { value: lf + rf, kind: PrimKind::f64() },
            BinOp::Sub => Value::Float { value: lf - rf, kind: PrimKind::f64() },
            BinOp::Mul => Value::Float { value: lf * rf, kind: PrimKind::f64() },
            BinOp::Div => Value::Float { value: lf / rf, kind: PrimKind::f64() },
            BinOp::Rem => Value::Float { value: lf % rf, kind: PrimKind::f64() },
            BinOp::Eq => Value::Bool(lf == rf),
            BinOp::NotEq => Value::Bool(lf != rf),
            BinOp::Lt => Value::Bool(lf < rf),
            BinOp::Gt => Value::Bool(lf > rf),
            BinOp::LtEq => Value::Bool(lf <= rf),
            BinOp::GtEq => Value::Bool(lf >= rf),
            BinOp::LogicalAnd => Value::Bool(lf != 0.0 && rf != 0.0),
            BinOp::LogicalOr => Value::Bool(lf != 0.0 || rf != 0.0),
            _ => return Err(RuntimeError::Type(format!("float operand not supported for {op:?}"))),
        });
    }
    let li = l.to_i128().ok_or_else(|| RuntimeError::Type(format!("not numeric: {l}")))?;
    let ri = r.to_i128().ok_or_else(|| RuntimeError::Type(format!("not numeric: {r}")))?;
    Ok(match op {
        BinOp::Add => Value::SInt { value: li.wrapping_add(ri), kind: PrimKind::s64() },
        BinOp::Sub => Value::SInt { value: li.wrapping_sub(ri), kind: PrimKind::s64() },
        BinOp::Mul => Value::SInt { value: li.wrapping_mul(ri), kind: PrimKind::s64() },
        BinOp::Div => Value::SInt {
            value: li.checked_div(ri).ok_or_else(|| RuntimeError::Type("integer divide by zero".into()))?,
            kind: PrimKind::s64(),
        },
        BinOp::Rem => Value::SInt {
            value: li.checked_rem(ri).ok_or_else(|| RuntimeError::Type("integer remainder by zero".into()))?,
            kind: PrimKind::s64(),
        },
        BinOp::BitAnd => Value::SInt { value: li & ri, kind: PrimKind::s64() },
        BinOp::BitOr => Value::SInt { value: li | ri, kind: PrimKind::s64() },
        BinOp::BitXor => Value::SInt { value: li ^ ri, kind: PrimKind::s64() },
        BinOp::Shl => Value::SInt { value: li.wrapping_shl(ri as u32 & 127), kind: PrimKind::s64() },
        BinOp::Shr => Value::SInt { value: li.wrapping_shr(ri as u32 & 127), kind: PrimKind::s64() },
        BinOp::Eq => Value::Bool(li == ri),
        BinOp::NotEq => Value::Bool(li != ri),
        BinOp::Lt => Value::Bool(li < ri),
        BinOp::Gt => Value::Bool(li > ri),
        BinOp::LtEq => Value::Bool(li <= ri),
        BinOp::GtEq => Value::Bool(li >= ri),
        BinOp::LogicalAnd => Value::Bool(li != 0 && ri != 0),
        BinOp::LogicalOr => Value::Bool(li != 0 || ri != 0),
    })
}

fn eval_unary(op: UnaryOp, v: &Value) -> Result<Value, RuntimeError> {
    Ok(match op {
        UnaryOp::Neg => {
            if let (Some(f), true) = (v.to_f64(), matches!(v, Value::Float { .. })) {
                Value::Float { value: -f, kind: PrimKind::f64() }
            } else {
                let i = v.to_i128().ok_or_else(|| RuntimeError::Type(format!("not numeric: {v}")))?;
                Value::SInt { value: -i, kind: PrimKind::s64() }
            }
        }
        UnaryOp::Pos => v.clone(),
        UnaryOp::Not => Value::Bool(!v.is_truthy()),
        UnaryOp::BitNot => {
            let i = v.to_i128().ok_or_else(|| RuntimeError::Type(format!("not numeric: {v}")))?;
            Value::SInt { value: !i, kind: PrimKind::s64() }
        }
        UnaryOp::PreInc | UnaryOp::PostInc => {
            let i = v.to_i128().ok_or_else(|| RuntimeError::Type(format!("not numeric: {v}")))?;
            Value::SInt { value: i + 1, kind: PrimKind::s64() }
        }
        UnaryOp::PreDec | UnaryOp::PostDec => {
            let i = v.to_i128().ok_or_else(|| RuntimeError::Type(format!("not numeric: {v}")))?;
            Value::SInt { value: i - 1, kind: PrimKind::s64() }
        }
    })
}

fn compound_to_bin(op: AssignOp) -> BinOp {
    match op {
        AssignOp::Assign => BinOp::Add, // unreachable; caller filters
        AssignOp::AddAssign => BinOp::Add,
        AssignOp::SubAssign => BinOp::Sub,
        AssignOp::MulAssign => BinOp::Mul,
        AssignOp::DivAssign => BinOp::Div,
        AssignOp::RemAssign => BinOp::Rem,
        AssignOp::AndAssign => BinOp::BitAnd,
        AssignOp::OrAssign => BinOp::BitOr,
        AssignOp::XorAssign => BinOp::BitXor,
        AssignOp::ShlAssign => BinOp::Shl,
        AssignOp::ShrAssign => BinOp::Shr,
    }
}

/// Convenience: format the first argument as a string and use the
/// remaining args to fill `{}` placeholders ImHex-style. When no
/// `{}` shows up we fall back to space-joined Display of every
/// arg, which matches the corpus's `std::print(a, b, c)` shape.
fn format_args(args: &[Value]) -> String {
    let Some(fmt_v) = args.first() else { return String::new() };
    let fmt = match fmt_v {
        Value::Str(s) => s.clone(),
        other => format!("{other}"),
    };
    if !fmt.contains("{}") {
        return args.iter().map(|v| format!("{v}")).collect::<Vec<_>>().join(" ");
    }
    let mut out = String::with_capacity(fmt.len() + 8);
    let mut chars = fmt.chars().peekable();
    let mut idx = 1usize; // first replacement arg
    while let Some(c) = chars.next() {
        if c == '{' && chars.peek() == Some(&'}') {
            chars.next();
            if let Some(v) = args.get(idx) {
                let pretty = match v {
                    Value::Str(s) => s.clone(),
                    other => format!("{other}"),
                };
                out.push_str(&pretty);
            }
            idx += 1;
        } else {
            out.push(c);
        }
    }
    out
}

/// Pull the Nth arg as a string (for `starts_with` / `contains` /
/// etc.). Falls back to the empty string -- matches the corpus's
/// "if you don't supply it, the predicate is trivially false".
fn string_arg(args: &[Value], n: usize) -> String {
    args.get(n)
        .map(|v| match v {
            Value::Str(s) => s.clone(),
            other => format!("{other}"),
        })
        .unwrap_or_default()
}

/// Pick the smaller (when `pick_min`) or larger of two args. Falls
/// through to numeric comparison; non-numeric inputs return the
/// first arg unchanged.
fn min_max_value(args: &[Value], pick_min: bool) -> Value {
    let Some(a) = args.first() else { return Value::SInt { value: 0, kind: PrimKind::s64() } };
    let Some(b) = args.get(1) else { return a.clone() };
    let ord = match (a.to_f64(), b.to_f64()) {
        (Some(x), Some(y)) => x.partial_cmp(&y),
        _ => a.to_i128().zip(b.to_i128()).map(|(x, y)| x.cmp(&y)),
    };
    match (ord, pick_min) {
        (Some(std::cmp::Ordering::Less), true) | (Some(std::cmp::Ordering::Greater), false) => a.clone(),
        (Some(_), _) => b.clone(),
        (None, _) => a.clone(),
    }
}

/// Loose equality across mixed numeric types -- mirrors how match
/// arms compare against the scrutinee. Strings compare by content;
/// everything else widens to `i128`.
fn values_equal(a: &Value, b: &Value) -> bool {
    if let (Value::Str(x), Value::Str(y)) = (a, b) {
        return x == y;
    }
    match (a.to_i128(), b.to_i128()) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Best-effort byte-size of a runtime value. `sizeof(expr)` falls
/// back to this when the operand isn't a registered type or
/// emitted node. Sentinel zeros for `Void` so the math doesn't
/// blow up.
fn value_byte_size(v: &Value) -> u64 {
    match v {
        Value::UInt { kind, .. } | Value::SInt { kind, .. } => kind.width as u64,
        Value::Float { kind, .. } => kind.width as u64,
        Value::Bool(_) => 1,
        Value::Char { kind, .. } => kind.width as u64,
        Value::Str(s) => s.len() as u64,
        Value::Bytes(b) => b.len() as u64,
        Value::Void => 0,
    }
}

fn attrs_to_pairs(attrs: &crate::ast::Attrs) -> Vec<(String, String)> {
    attrs
        .0
        .iter()
        .map(|a| {
            let value = a.args.first().map(format_attr_arg).unwrap_or_default();
            (a.name.clone(), value)
        })
        .collect()
}

fn format_attr_arg(e: &Expr) -> String {
    match e {
        Expr::IntLit { value, .. } => value.to_string(),
        Expr::StringLit { value, .. } => value.clone(),
        Expr::BoolLit { value, .. } => value.to_string(),
        Expr::Ident { name, .. } => name.clone(),
        _ => format!("{:?}", e.span()),
    }
}

// Suppress unused imports until later phases hook them up.
#[allow(dead_code)]
const _SPAN: Span = Span { start: 0, end: 0 };
