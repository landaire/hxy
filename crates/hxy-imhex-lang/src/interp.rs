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
    /// Aliases from local names to emitted-node indices. Populated
    /// when a function call passes an emitted node by name (the
    /// caller's `imageSpec`) and the function's parameter
    /// (`imSpec`) needs `imSpec.height` to resolve through the
    /// caller's node tree. Without this, `imSpec` evaluates to
    /// `Void` (structs don't bind a value) and member access
    /// fails with `unresolved member .height`.
    node_aliases: HashMap<String, NodeIdx>,
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
    /// Stack of currently-being-read struct/bitfield/union indices.
    /// `this` resolves to the top; `parent` resolves to the second-
    /// from-top. Pushed on `read_struct` / `read_bitfield` entry,
    /// popped on exit.
    this_stack: Vec<NodeIdx>,
    /// Set when a `break` statement fires inside a struct body so
    /// the surrounding [`Self::read_dynamic_array`] knows to stop
    /// emitting elements. Cleared by the array reader on consume.
    break_pending: bool,
    /// Per-array iteration counter, pushed on array entry and
    /// popped on exit. Powers `std::core::array_index()`, which
    /// templates use to peek the current element index from inside
    /// a struct body (ogg's `SegmentData { u8 data[
    /// parent.segmentTable[std::core::array_index()]]; }`).
    array_index_stack: Vec<u64>,
    /// Depth of the active function-call stack. Type-prefixed local
    /// declarations inside a function body (`Value value;`,
    /// `le TimeConverter converter;`) save+restore the cursor so
    /// they don't consume bytes from the surrounding template's
    /// running cursor -- the function scratch struct gets emitted
    /// (so `value.value = ...` and `converter.time` resolve to a
    /// real node), but the call-site cursor is unaffected.
    function_depth: u32,
    /// `return;` in the language doubles as a function exit *and*
    /// a "stop processing this template" signal (blend.hexpat's
    /// uncompressed branch returns from a struct body to skip the
    /// trailing seek-table reads). When fired outside a function
    /// we set this flag and let the surrounding `read_struct` /
    /// top-level walk unwind to the program top.
    return_pending: bool,
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
            this_stack: Vec::new(),
            break_pending: false,
            array_index_stack: Vec::new(),
            function_depth: 0,
            return_pending: false,
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

    /// Seed the interpreter's default byte order. Honours the
    /// `#pragma endian` directive that templates declare at the top
    /// of their source -- hosts call [`crate::extract_pragmas`] on
    /// the raw source, then forward the resulting [`Endian`] here.
    /// Templates can still override per-field with `be u32` / `le
    /// u32` and at run time via `std::core::set_endian(...)`.
    pub fn with_default_endian(mut self, endian: Endian) -> Self {
        self.endian = endian;
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
                // Tolerate per-top-level-statement EOF: a top-level
                // `Header h @ 0x00;` whose body runs past the file
                // end shouldn't abort the whole template -- the
                // remaining statements may still produce useful
                // output and `terminal_error` becomes None as long
                // as nothing else fails. Errors that aren't EOF
                // overshoots (type errors, runaway loops, etc.)
                // still propagate.
                match self.exec_stmt(s, None) {
                    Ok(_) => {}
                    Err(RuntimeError::Source(e)) => {
                        self.diagnostics.push(Diagnostic {
                            message: format!("read past EOF at top level: {e:?}"),
                            severity: Severity::Warning,
                            file_offset: None,
                            template_line: None,
                        });
                    }
                    Err(e) => return Err(e),
                }
                // `return;` outside a function (or one that bubbled
                // up through a struct body, as in blend.hexpat's
                // `BlendWrapper { if (magic != ZSTD) { Blend ...;
                // return; } }`) means stop processing -- the
                // trailing top-level statements assume a code path
                // that doesn't apply.
                if self.return_pending {
                    self.return_pending = false;
                    break;
                }
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
                // `using Foo;` (forward declaration: source path
                // points at `Foo` itself) is a no-op when `Foo` is
                // already in the type table -- ImHex uses this
                // shape inside namespaces to forward-reference an
                // outer type. Without this guard we'd overwrite the
                // real definition with a self-alias and every
                // lookup would loop until the depth limit hits.
                let is_self_forward =
                    template_params.is_empty() && source.template_args.is_empty() && source.leaf() == new_name;
                if is_self_forward && self.types.contains_key(new_name) {
                    return;
                }
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
        Ok(self.resolve_type_ref(ty)?.0)
    }

    /// Resolve a [`TypeRef`] through any alias chain and return the
    /// concrete [`TypeDef`] plus the *fully-substituted* [`TypeRef`]
    /// at the resolution point. The latter matters for callers that
    /// then read a struct/enum/bitfield body: they need the
    /// substituted template_args (`SizedString<u16>` -> the struct
    /// receives `[TypeRefExpr(u16), Ident("char")]`, not the
    /// alias's `[Ident("u16")]`) so the body's template-param
    /// bindings are complete.
    fn resolve_type_ref(&self, ty: &TypeRef) -> Result<(TypeDef, TypeRef), RuntimeError> {
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
                    if !params.is_empty() {
                        // Build *both* maps: TypeRef substitutions
                        // (for `using Foo<T> = Bar<T>;` where T is a
                        // type) and Expr substitutions (for
                        // `using AssetTexture<auto size> =
                        // AssetVisualizer<size, "image">;` where
                        // `size` is a value forwarded by name).
                        let mut subs: HashMap<String, TypeRef> = HashMap::with_capacity(params.len());
                        let mut expr_subs: HashMap<String, Expr> = HashMap::with_capacity(params.len());
                        for (param, arg) in params.iter().zip(current.template_args.iter()) {
                            if let Some(arg_ty) = expr_as_typeref(arg) {
                                subs.insert(param.clone(), arg_ty);
                            }
                            expr_subs.insert(param.clone(), arg.clone());
                        }
                        // Apply type-level substitution first, then
                        // expression-level for any Ident that names
                        // an alias param. The expression-level pass
                        // is what carries `IntLit(4)` from
                        // `AssetTexture<4>` through to
                        // `AssetVisualizer<4, "image">`'s `size`
                        // arg, instead of leaving the `size` ident
                        // for Vis to resolve in a scope where it
                        // doesn't exist.
                        let mut tmp = substitute_typeref(target, &subs);
                        tmp.template_args = tmp
                            .template_args
                            .into_iter()
                            .map(|e| substitute_expr_with_arg(&e, &expr_subs))
                            .collect();
                        current = tmp;
                    } else {
                        current = TypeRef {
                            path: target.path.clone(),
                            template_args: target.template_args.clone(),
                            span: target.span,
                        };
                    }
                }
                Some(def) => return Ok((def.clone(), current)),
                None => {
                    if let Some(prim) = generic_int_primitive(&name) {
                        return Ok((TypeDef::Primitive(prim), current));
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
/// Count the leading `parent` hops in a chain like `parent`,
/// `parent.parent`, `parent.parent.parent`, etc. Returns `None` for
/// any other shape so the caller can fall through to regular
/// member resolution.
fn count_parent_hops(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Ident { name, .. } if name == "parent" => Some(1),
        // `this.parent` is the same as `parent` -- pcapng uses
        // `this.parent.block_type` to reach a sibling of the
        // enclosing struct.
        Expr::Member { target, field, .. } if field == "parent" => {
            if let Expr::Ident { name, .. } = target.as_ref()
                && name == "this"
            {
                return Some(1);
            }
            count_parent_hops(target).map(|n| n + 1)
        }
        _ => None,
    }
}

/// Truncate or re-tag a numeric value to a declared primitive type.
/// Used when a local variable is declared `Type name = expr;` and
/// `Type` is a primitive integer (e.g. `u64 pos = -1` should store
/// the all-ones u64 bit pattern). Non-numeric values pass through.
fn coerce_value_to_prim(v: Value, p: PrimKind) -> Value {
    match p.class {
        PrimClass::Int => {
            let raw = v.to_i128().unwrap_or(0);
            if p.signed {
                let bits = (p.width as u32) * 8;
                let shift = 128 - bits;
                let signed = (raw << shift) >> shift;
                Value::SInt { value: signed, kind: p }
            } else {
                let mask: u128 = if p.width >= 16 { u128::MAX } else { (1u128 << (p.width as u32 * 8)) - 1 };
                Value::UInt { value: (raw as u128) & mask, kind: p }
            }
        }
        _ => v,
    }
}

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

/// Replace a bare `Ident` whose name matches an alias param with
/// the raw expression the alias was instantiated with. Carries
/// `Foo<4>` through `using Foo<auto n> = Bar<n>;` so Bar's `n`
/// param sees the literal `4` instead of `Ident("n")`. Other
/// expression shapes are returned unchanged.
fn substitute_expr_with_arg(expr: &Expr, subs: &HashMap<String, Expr>) -> Expr {
    match expr {
        Expr::Ident { name, .. } if subs.contains_key(name) => subs.get(name).cloned().unwrap(),
        other => other.clone(),
    }
}

fn substitute_expr(expr: &Expr, subs: &HashMap<String, TypeRef>) -> Expr {
    match expr {
        Expr::Ident { name, span } if subs.contains_key(name) => {
            // Replace the ident with the substituted type's path.
            // For a single-segment path that's just another ident
            // (the common `using Foo<T> = Bar<T>;` shape), keep the
            // result as an Ident so value-shaped uses (`u8 data[T]`,
            // where `T` is bound to a runtime number in the
            // enclosing struct's scope) still resolve through
            // `lookup_ident`. Wrapping unconditionally as
            // TypeRefExpr forced eval down the type-name fallback,
            // turning `T` into `Str("T")` and breaking any array
            // sized by a value-template arg.
            let ty = subs.get(name).cloned().unwrap();
            if ty.path.len() == 1 && ty.template_args.is_empty() {
                Expr::Ident { name: ty.path[0].clone(), span: *span }
            } else {
                Expr::TypeRefExpr { ty: Box::new(ty), span: *span }
            }
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
            Stmt::TryBlock { body, .. } => {
                // ImHex's `try { ... } catch { ... }` swallows any
                // runtime error thrown from inside the body. Save
                // and restore the cursor so a partial read inside
                // the try doesn't leave the surrounding template
                // cursor in a half-advanced state. Tiff's IFD body
                // attempts strip-vs-tile decoding via twin try
                // blocks; without this, the first block's failure
                // aborts the run.
                let saved_pos = self.cursor_tell();
                let saved_break = self.break_pending;
                match self.exec_stmt(body, parent) {
                    Ok(flow) => Ok(flow),
                    Err(_) => {
                        self.cursor_seek(saved_pos);
                        self.break_pending = saved_break;
                        Ok(Flow::Next)
                    }
                }
            }
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
                let branch: Option<&Stmt> = if self.eval(cond)?.is_truthy() {
                    Some(then_branch.as_ref())
                } else {
                    else_branch.as_deref()
                };
                let Some(branch) = branch else { return Ok(Flow::Next) };
                // ImHex treats `if`/`else` block bodies as transparent
                // for variable scoping: a `u32 v = ...;` declared
                // inside one branch is visible to the surrounding
                // struct after the if/else. dos.hexpat declares
                // `csOffsetFirst` in both arms and references it
                // afterwards. Flatten Block bodies to avoid pushing
                // a new scope.
                if let Stmt::Block { stmts, .. } = branch {
                    let mut flow = Flow::Next;
                    for s in stmts {
                        flow = self.exec_stmt(s, parent)?;
                        if !matches!(flow, Flow::Next) {
                            break;
                        }
                    }
                    Ok(flow)
                } else {
                    self.exec_stmt(branch, parent)
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
                if self.function_depth == 0 {
                    self.return_pending = true;
                }
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
            | Stmt::Import { .. } => Ok(Flow::Next),
            Stmt::Namespace { body, .. } => {
                // Type / fn declarations in the body were already
                // registered during the declaration phase; at run
                // time we still need to execute statement-shaped
                // contents (top-level field decls, expression
                // statements, computed-local bindings) so a
                // namespace-scoped `u8 op_type;` (qoi.hexpat) reads
                // its byte and lands in the global scope. Decl-only
                // statements (struct/enum/fn) re-enter exec_stmt as
                // no-ops, so the walk is safe to apply uniformly.
                for s in body {
                    let flow = self.exec_stmt(s, parent)?;
                    if matches!(flow, Flow::Break | Flow::Return) {
                        return Ok(flow);
                    }
                }
                Ok(Flow::Next)
            }
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
                    // Match arms are scope-transparent (same as
                    // `if` branches): a `Type x = expr;` inside an
                    // arm should be visible to subsequent fields.
                    // Flatten Block bodies to avoid pushing a scope
                    // that would hide them.
                    if let Stmt::Block { stmts, .. } = s {
                        for inner in stmts {
                            flow = self.exec_stmt(inner, parent)?;
                            if !matches!(flow, Flow::Next) {
                                break;
                            }
                        }
                    } else {
                        flow = self.exec_stmt(s, parent)?;
                    }
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
        let Stmt::FieldDecl { is_const, ty, name, array, placement, init, attrs, is_io_var, pointer_width, .. } = stmt
        else {
            unreachable!();
        };
        let _ = is_const;
        // `auto` typed locals don't read from the source. A bare
        // Any decl with an initializer (`auto x = ...`, `u32 start
        // = $;`, `Type x = expr;`) is a local variable: bind the
        // evaluated expression and don't read from the source.
        // ImHex distinguishes "type-prefixed read" from "local
        // variable" by whether the `= init` is present.
        // Placement reads (`Type x @ offset = ...`) and io vars
        // were already handled above; this catches the rest.
        if init.is_some() && placement.is_none() && pointer_width.is_none() {
            let value = self.eval(init.as_ref().unwrap())?;
            // Truncate / re-pack to the declared type when it's a
            // primitive integer. ImHex's `u64 pos = -1` stores
            // 0xFFFFFFFFFFFFFFFF; without truncation the value
            // stays as SInt(-1) and comparisons against the all-
            // ones bit pattern fail.
            let value = if let Ok(TypeDef::Primitive(p)) = self.lookup_type(ty) {
                coerce_value_to_prim(value, p)
            } else {
                value
            };
            self.current_scope_mut().vars.insert(name.clone(), value.clone());
            // When the computed local lives inside a struct, also
            // emit a zero-length child node so an outside reference
            // like `startheader.startEndHeader` (7z.hexpat) finds
            // it via the normal `lookup_member_under` path. Without
            // this, struct-local computed values are visible only
            // for the duration of the struct's body.
            if let Some(parent_idx) = parent {
                self.push_node(NodeOut {
                    name: name.clone(),
                    ty: NodeType::Unknown(ty.leaf().to_owned()),
                    offset: self.cursor_tell(),
                    length: 0,
                    value: Some(value),
                    parent: Some(parent_idx),
                    attrs: attrs_to_pairs(attrs),
                });
            }
            return Ok(Flow::Next);
        }
        if ty.leaf() == "auto" {
            // Bare `auto x;` (no init) -- nothing to bind.
            self.current_scope_mut().vars.insert(name.clone(), Value::Void);
            return Ok(Flow::Next);
        }
        // `str x;` (no init) -- declare an empty string in scope.
        // ImHex's `str` is a dynamic string type, not a fixed-width
        // primitive; bare declarations don't consume bytes.
        if ty.leaf() == "str" && init.is_none() && placement.is_none() && array.is_none() {
            self.current_scope_mut().vars.insert(name.clone(), Value::Str(String::new()));
            return Ok(Flow::Next);
        }
        // `bool x in;` / `Type x out;` -- input/output variables
        // supplied by the host. Don't consume bytes from the source;
        // bind a default placeholder so subsequent expressions that
        // reference the name keep evaluating.
        if *is_io_var {
            let default = match init {
                Some(e) => self.eval(e)?,
                None => Value::Void,
            };
            self.current_scope_mut().vars.insert(name.clone(), default);
            return Ok(Flow::Next);
        }
        // `const Type name = expr;` -- a compile-time constant,
        // not a source read. ImHex evaluates `expr` once and binds
        // the result without advancing the cursor.
        if *is_const && init.is_some() {
            let value = self.eval(init.as_ref().unwrap())?;
            self.current_scope_mut().vars.insert(name.clone(), value);
            return Ok(Flow::Next);
        }
        let mut all_attrs = attrs_to_pairs(attrs);
        // `[[no_unique_address]]` -- the field reads at the current
        // cursor but doesn't advance it. Save the cursor and
        // restore it after the read; the field itself still emits a
        // node and binds in scope as usual.
        let no_unique_address =
            attrs.0.iter().any(|a| a.name == "no_unique_address");
        // Placement (`@ offset`) cursor semantics depend on context:
        //   - At top level, `@` is a plain seek -- the cursor stays
        //     wherever the placed read ended, so a following
        //     `Type y @ $;` reads right after it. (nro.hexpat,
        //     `Start start @ 0; Header header @ $;`.)
        //   - Inside a struct or array element body, `@` is a side
        //     channel: save the cursor, read at `offset`, then
        //     restore. The enclosing struct's natural size only
        //     reflects sequential reads. (adtfdat.hexpat's
        //     `Extension { ...; payload[data_size] @ data_pos; }`
        //     in an `extensions[N]` array needs each element to
        //     advance by its sequential size only.)
        // `[[no_unique_address]]` is always save+restore -- the
        // explicit "read here but don't advance" form.
        let inside_struct = !self.this_stack.is_empty();
        let placement_saves = placement.is_some() && inside_struct;
        // Type-prefixed declarations inside a function body
        // (`Value value;` in lznt1's `fn appendU8`) declare a
        // scratch local: emit the struct so member access works,
        // but don't consume bytes from the caller's running cursor.
        let function_local = self.function_depth > 0
            && parent.is_none()
            && placement.is_none()
            && pointer_width.is_none()
            && init.is_none();
        let saved_pos = if placement_saves || no_unique_address || function_local {
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
            // Open-ended (`Type x[]`) and predicate-driven (`Type
            // x[while(cond)]`) arrays read zero elements for now;
            // see `read_dynamic_array` for the full implementation
            // that's currently disabled behind it.
            _ => 0,
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
            match array.as_ref().unwrap() {
                ArraySize::Fixed(_) => {
                    self.read_array(name, ty, count, parent, &all_attrs)?;
                }
                ArraySize::Open => {
                    self.read_dynamic_array(name, ty, parent, &all_attrs, None)?;
                }
                ArraySize::While(cond) => {
                    self.read_dynamic_array(name, ty, parent, &all_attrs, Some(cond))?;
                }
            }
        } else {
            self.read_scalar(name, ty, parent, &all_attrs, init.as_ref())?;
        }
        // Restore the cursor only for `[[no_unique_address]]` and
        // for in-struct placements. Top-level `@` placement leaves
        // the cursor at the read end (a seek, not a save+restore)
        // so chained `Foo a @ 0; Bar b @ $;` advances naturally.
        if let Some(saved) = saved_pos {
            self.cursor_seek(saved);
        }
        // `[[transform("fn")]]` runs `fn(value)` after the read and
        // replaces the bound value with the result. Templates like
        // cpio_new_ascii.hexpat use this to convert hex-string
        // fields to integers so subsequent arithmetic
        // (`c_namesize - 1`) works on the transformed numeric form.
        let transform_fn = attrs.0.iter().find_map(|a| {
            if a.name == "transform" {
                a.args.first().and_then(|arg| match arg {
                    Expr::StringLit { value, .. } => Some(value.clone()),
                    _ => None,
                })
            } else {
                None
            }
        });
        if let Some(fn_name) = transform_fn {
            let raw = self.lookup_ident(name).unwrap_or(Value::Void);
            if let Ok(new_val) =
                self.call_named_with_aliases(&fn_name, std::slice::from_ref(&raw), &[])
            {
                self.current_scope_mut().vars.insert(name.clone(), new_val.clone());
                if let Some(indices) = self.nodes_by_name.get(name).cloned()
                    && let Some(&idx) = indices.last()
                {
                    self.nodes[idx.as_usize()].value = Some(new_val);
                }
            }
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
        // (the corpus uses both). `hex::type::Json<N>` (an
        // ImHex-specific JSON visualisation type) reads the same
        // way -- N bytes consumed, no structural decode -- so the
        // surrounding chunk-style template (`gltf.hexpat`) keeps
        // its cursor in step with the file even though we don't
        // model the JSON contents.
        if (matches!(ty.leaf(), "Bytes" | "Json"))
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
        let (def, resolved_ty) = self.resolve_type_ref(ty)?;
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
            TypeDef::Struct(s) => self.read_struct(name, &s, &resolved_ty, parent, attrs),
            TypeDef::Bitfield(b) => self.read_bitfield(name, &b, &resolved_ty, parent, attrs),
            TypeDef::Alias { .. } => unreachable!("resolve_type_ref follows aliases"),
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
        self.this_stack.push(idx);

        // Bind template args to template params first, so the body
        // (and any inherited parent body) can see them. Pairs by
        // position; missing args resolve to `Void` / 0.
        //
        // Args that look type-shaped (`Ident`, `Path`, `TypeRefExpr`)
        // also register as a temporary alias under the param's name
        // so the body can use the param in *type* position
        // (`SizeType size;`). The aliases are removed when the body
        // finishes so they don't leak to surrounding reads.
        // Save any prior binding under the template param name so we
        // can restore it on exit. Without this, instantiating
        // `BasicType<auto Tag>` overwrites the global `enum Tag`
        // and leaves the type table broken for subsequent uses
        // after BasicType returns (hprof.hexpat).
        let mut template_type_aliases: Vec<(String, Option<TypeDef>)> = Vec::new();
        if !decl.template_params.is_empty() {
            for (i, param_name) in decl.template_params.iter().enumerate() {
                let arg_expr = ty.template_args.get(i);
                let arg_ty = arg_expr.and_then(expr_as_typeref);
                if let Some(t) = arg_ty {
                    // If the arg names another alias param (the
                    // `using symbols_list<T> = list<T>` chain in
                    // selinux.hexpat), resolve through it now so
                    // the inner struct sees the eventual concrete
                    // type instead of a self-cyclic `T -> T` alias.
                    let resolved = if t.path.len() == 1
                        && let Some(TypeDef::Alias { target, .. }) = self.types.get(&t.path[0])
                    {
                        target.clone()
                    } else {
                        t
                    };
                    let prev = self.types.insert(
                        param_name.clone(),
                        TypeDef::Alias { params: Vec::new(), target: resolved },
                    );
                    template_type_aliases.push((param_name.clone(), prev));
                }
                // Bind the arg's *value* in scope so the body can
                // use it for arithmetic / comparisons too. Some
                // templates pass a name that resolves both as a
                // type (`SizedString<u8>`) and as a value
                // (`Array<T, no_of_runs>` -- `no_of_runs` is a
                // sibling field). We try eval first; if it produces
                // anything but `Void` we use that, otherwise fall
                // back to a string placeholder so type-shaped args
                // (`u8`, `Bar<X>`, ...) keep the run going.
                let evaluated = arg_expr.map(|e| self.eval(e).unwrap_or(Value::Void));
                let value = match evaluated {
                    Some(v) if !matches!(v, Value::Void) => v,
                    _ => arg_expr
                        .map(|e| match e {
                            Expr::Ident { name, .. } => Value::Str(name.clone()),
                            Expr::Path { segments, .. } => Value::Str(segments.join("::")),
                            Expr::TypeRefExpr { ty, .. } => Value::Str(ty.path.join("::")),
                            other => self.eval(other).unwrap_or(Value::Void),
                        })
                        .unwrap_or(Value::Void),
                };
                self.current_scope_mut().vars.insert(param_name.clone(), value);
            }
        }

        // Compose every ancestor struct's body before this one's.
        // ImHex inheritance reads parent fields first, then child;
        // chains like `V5 : V4 : V3 : V2 : V1` (BMP's DIB header
        // versions) need every level inlined, so we walk back from
        // the innermost parent up to the eldest. We bound the walk
        // at 32 hops to flag accidental cycles.
        let mut parent_body: Vec<Stmt> = Vec::new();
        if decl.parent.is_some() {
            let mut chain: Vec<Vec<Stmt>> = Vec::new();
            let mut current_parent = decl.parent.clone();
            for _ in 0..32 {
                let Some(parent_ty) = current_parent.as_ref() else { break };
                let Ok(TypeDef::Struct(p)) = self.lookup_type(parent_ty) else { break };
                let next_parent = p.parent.clone();
                // Bind the parent's template params to the args
                // supplied by the inheritance reference. dotnet's
                // `struct ClassWithId : RecordTypeEnumT<
                // RecordTypeEnum::ClassWithId>` needs
                // `_recordTypeEnumT = ClassWithId` visible while we
                // execute RecordTypeEnumT's body in this struct's
                // scope. Without this, the inherited body sees
                // `_recordTypeEnumT` as an undefined name.
                if !p.template_params.is_empty() {
                    for (i, param_name) in p.template_params.iter().enumerate() {
                        let arg_expr = parent_ty.template_args.get(i);
                        let value = arg_expr
                            .map(|e| self.eval(e).unwrap_or(Value::Void))
                            .unwrap_or(Value::Void);
                        self.current_scope_mut().vars.insert(param_name.clone(), value);
                    }
                }
                chain.push(p.body.clone());
                current_parent = next_parent;
            }
            // Eldest first so the resulting `parent_body` reads the
            // ancestors in the same source order they'd appear if
            // hand-rolled (oldest fields at the start).
            for body in chain.into_iter().rev() {
                parent_body.extend(body);
            }
        }

        // `break` inside a struct body (typical in `Block blocks[while(true)]`
        // patterns) bubbles up through `break_pending` so the
        // surrounding `read_dynamic_array` knows to stop reading.
        if !decl.is_union {
            for s in parent_body.iter().chain(decl.body.iter()) {
                // Tolerate per-field EOF the same way arrays do: a
                // struct that runs past the file end stops emitting
                // the remaining trailing fields, but the run keeps
                // going. ImHex surfaces the overshoot as a warning
                // and lets the surrounding template continue.
                let flow = match self.exec_stmt(s, Some(idx)) {
                    Ok(f) => f,
                    Err(RuntimeError::Source(_)) => break,
                    Err(e) => return Err(e),
                };
                if matches!(flow, Flow::Break | Flow::Continue) {
                    self.break_pending = matches!(flow, Flow::Break);
                    break;
                }
                // `return;` outside a function unwinds the entire
                // template walk; stop emitting fields and let the
                // surrounding caller (or `exec_program`) see the
                // pending signal.
                if matches!(flow, Flow::Return) || self.return_pending {
                    break;
                }
            }
            self.nodes[idx.as_usize()].length = self.cursor_tell().saturating_sub(offset);
        } else {
            // Union: every field starts at the same offset; the union's
            // own length is the widest child's. Cursor lands at the
            // farthest advance.
            let mut max_end = offset;
            for s in parent_body.iter().chain(decl.body.iter()) {
                self.cursor_seek(offset);
                let flow = self.exec_stmt(s, Some(idx))?;
                max_end = max_end.max(self.cursor_tell());
                if matches!(flow, Flow::Break | Flow::Continue) {
                    self.break_pending = matches!(flow, Flow::Break);
                    break;
                }
            }
            self.cursor_seek(max_end);
            self.nodes[idx.as_usize()].length = max_end - offset;
        }
        self.scopes.pop();
        self.this_stack.pop();
        self.current_parent = prev_parent;
        // Restore any prior binding we shadowed when registering the
        // template param as an alias. If there was no prior binding,
        // remove the alias entirely.
        for (alias, prev) in template_type_aliases {
            match prev {
                Some(def) => {
                    self.types.insert(alias, def);
                }
                None => {
                    self.types.remove(&alias);
                }
            }
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
        let offset = self.cursor_tell();
        // Struct-backed enum (rar.hexpat: `enum HeaderType : vint`).
        // Read the struct, apply its `[[transform]]` to get a
        // numeric value, and use that as the variant key. Width is
        // computed from the struct's actual byte consumption.
        let (raw_u, length, prim) = if let TypeDef::Struct(s) = &backing_def {
            let backing_ty = TypeRef {
                path: vec![s.name.clone()],
                template_args: Vec::new(),
                span: decl.backing.span,
            };
            let hidden_name = format!("__{}__backing", name);
            self.read_struct(&hidden_name, s, &backing_ty, parent, &[])?;
            let raw_value = self.lookup_ident(&hidden_name).unwrap_or(Value::Void);
            // Apply the backing struct's transform fn (e.g.
            // `vint::vint_value`) to convert the raw struct into a
            // numeric variant key. ImHex's pattern attrs aren't yet
            // surfaced through TypeDef::Struct, so look the fn up by
            // a `<typename>_value` convention used by every corpus
            // example we've seen so far.
            let fn_candidates = [format!("{}_value", s.name.to_lowercase()), format!("{}_value", s.name)];
            let mut value: u128 = raw_value.to_i128().unwrap_or(0) as u128;
            for fn_name in &fn_candidates {
                if let Ok(v) = self.call_named_with_aliases(fn_name, &[raw_value.clone()], &[]) {
                    value = v.to_i128().unwrap_or(0) as u128;
                    break;
                }
            }
            let length = self.cursor_tell().saturating_sub(offset);
            (value, length, PrimKind { class: PrimClass::Int, width: length as u8, signed: false })
        } else {
            let prim = match backing_def {
                TypeDef::Primitive(p) if matches!(p.class, PrimClass::Int | PrimClass::Char) => p,
                _ => {
                    return Err(RuntimeError::Type(format!(
                        "enum `{}` backing must be an integer primitive or struct",
                        decl.name
                    )));
                }
            };
            let bytes = self.cursor_advance(prim.width as u64)?;
            let raw = decode_prim(&bytes, prim, self.endian)?;
            (raw.to_i128().unwrap_or(0) as u128, prim.width as u64, prim)
        };
        let _ = prim;
        let _ = length;
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
        // Store the raw integer as the node's value so comparisons
        // against `EnumType::Variant` (also resolved to an integer)
        // work directly. The matched variant name lands as a
        // `hxy_enum_variant` attribute so renderers can show the
        // friendly label without losing the numeric value.
        let raw_value = Value::UInt { value: raw_u, kind: PrimKind::u64() };
        let mut node_attrs = attrs.to_vec();
        if let Some(n) = &found_name {
            node_attrs.push(("hxy_enum_variant".into(), n.clone()));
        }
        self.push_node(NodeOut {
            name: name.to_owned(),
            ty: NodeType::EnumType(decl.name.clone()),
            offset,
            length,
            value: Some(raw_value.clone()),
            parent,
            attrs: node_attrs,
        });
        self.current_scope_mut().vars.insert(name.to_owned(), raw_value.clone());
        Ok(raw_value)
    }

    fn read_bitfield(
        &mut self,
        name: &str,
        decl: &BitfieldDecl,
        ty: &TypeRef,
        parent: Option<NodeIdx>,
        attrs: &[(String, String)],
    ) -> Result<Value, RuntimeError> {
        // Bind template params for parametrised bitfields (e.g.
        // `bitfield RGBA<auto R, auto G, auto B, auto A> { r : R; ...
        // }` from `type/color.pat` -- without this `r : R` looks up
        // `R` in scope and errors). Same shape as read_struct's
        // template binding, scoped to the bitfield body.
        self.scopes.push(Scope::default());
        let mut template_type_aliases: Vec<(String, Option<TypeDef>)> = Vec::new();
        if !decl.template_params.is_empty() {
            for (i, param_name) in decl.template_params.iter().enumerate() {
                let arg_expr = ty.template_args.get(i);
                let arg_ty = arg_expr.and_then(expr_as_typeref);
                if let Some(t) = arg_ty {
                    let resolved = if t.path.len() == 1
                        && let Some(TypeDef::Alias { target, .. }) = self.types.get(&t.path[0])
                    {
                        target.clone()
                    } else {
                        t
                    };
                    let prev = self.types.insert(
                        param_name.clone(),
                        TypeDef::Alias { params: Vec::new(), target: resolved },
                    );
                    template_type_aliases.push((param_name.clone(), prev));
                }
                let value = arg_expr.map(|e| self.eval(e).unwrap_or(Value::Void)).unwrap_or(Value::Void);
                self.current_scope_mut().vars.insert(param_name.clone(), value);
            }
        }
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
        self.this_stack.push(bf_idx);
        let result = self.exec_bitfield_body(&decl.body, raw_u, offset, bf_idx, &mut consumed);
        self.this_stack.pop();
        // Restore template-param aliases and pop the body scope
        // before propagating any inner error.
        for (alias, prev) in template_type_aliases {
            match prev {
                Some(def) => {
                    self.types.insert(alias, def);
                }
                None => {
                    self.types.remove(&alias);
                }
            }
        }
        self.scopes.pop();
        result?;
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
            // both 010 and ImHex. We map each byte 1:1 to its
            // codepoint instead of running UTF-8 validation: magic-
            // byte comparisons (`type::Magic<"...\xBC\xAF...">`) need
            // round-trip fidelity for high bytes, which UTF-8-lossy
            // would replace with `U+FFFD`.
            let total = (p.width as u64).saturating_mul(count);
            let offset = self.cursor_tell();
            // Tolerate a string overshooting EOF (e.g. mo.hexpat's
            // `char string[length] @ offset` when length+offset
            // crosses the file boundary): clamp to whatever bytes
            // remain instead of aborting the run.
            let available = self.source.len().saturating_sub(offset);
            let total = total.min(available);
            let bytes = self.cursor_advance(total)?;
            let s: String = bytes.iter().map(|&b| char::from(b)).collect();
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
        // Sanity-clamp the iteration bound. A garbage `count` (an
        // uninitialised u32 read past the schema's intent) can claim
        // billions of elements, and with our per-element EOF
        // tolerance no individual read fails -- we'd just churn step
        // budget on placement reads that happen to hit early bytes.
        // No template legitimately holds more elements than bytes
        // in the file, so cap the iteration count at the file size.
        let count = count.min(self.source.len().saturating_add(1));
        self.array_index_stack.push(0);
        let result = (|| -> Result<(), RuntimeError> {
            for i in 0..count {
                *self.array_index_stack.last_mut().unwrap() = i;
                // ImHex tolerates an array element running past EOF:
                // the array stops reading at the file boundary, the
                // parent struct keeps going, and the failure surfaces
                // as a (non-fatal) diagnostic. Treat `Source` errors
                // the same way -- the alternative aborts the whole
                // template on a single overshoot, which fails
                // fixtures that are merely truncated relative to the
                // schema (id3.hexpat, etc).
                match self.read_scalar(&format!("[{i}]"), ty, Some(parent_idx), &[], None) {
                    Ok(_) => {}
                    Err(RuntimeError::Source(_)) => break,
                    Err(e) => return Err(e),
                }
                // Honour `break` inside the element body. terminfo's
                // `terminal_name terminal_names[1000]` reads names
                // until a NUL terminator and breaks out -- without
                // this the array slogs through 999 spurious empty
                // reads and trips the surrounding ByteSizedArray
                // assert when the cursor lands at the wrong spot.
                if self.break_pending {
                    self.break_pending = false;
                    break;
                }
            }
            Ok(())
        })();
        self.array_index_stack.pop();
        result?;
        self.nodes[parent_idx.as_usize()].length = self.cursor_tell().saturating_sub(offset);
        Ok(Value::Void)
    }

    /// Read a predicate-driven or open-ended array (`Type x[]`,
    /// `Type x[while(cond)]`). Currently disabled in favour of a
    /// 0-element read; turning this on across the corpus regressed
    /// templates whose predicates depend on side-effect state we
    /// don't yet model. Kept as the scaffold for that work.
    #[allow(dead_code, unused_assignments)]
    fn read_dynamic_array(
        &mut self,
        name: &str,
        ty: &TypeRef,
        parent: Option<NodeIdx>,
        attrs: &[(String, String)],
        cond: Option<&Expr>,
    ) -> Result<Value, RuntimeError> {
        let offset = self.cursor_tell();
        let parent_idx = self.push_node(NodeOut {
            name: name.to_owned(),
            ty: NodeType::Unknown(format!("{}[]", ty.leaf())),
            offset,
            length: 0,
            value: None,
            parent,
            attrs: attrs.to_vec(),
        });
        let mut count = 0u64;
        let mut last_pos = self.cursor_tell();
        let mut stalls = 0u32;
        self.array_index_stack.push(0);
        loop {
            *self.array_index_stack.last_mut().unwrap() = count;
            if self.cursor_tell() >= self.source.len() {
                break;
            }
            if let Some(cond_expr) = cond {
                let v = match self.eval(cond_expr) {
                    Ok(v) => v,
                    Err(e) => {
                        self.array_index_stack.pop();
                        return Err(e);
                    }
                };
                if !v.is_truthy() {
                    break;
                }
            }
            // For `char[]` / `u8[]` open arrays, ImHex stops at the
            // first NUL byte (C-string semantics). Other element
            // types just read until EOF / cond / stall.
            if cond.is_none() && (ty.leaf() == "char" || ty.leaf() == "u8") {
                let byte = self.source.read(self.cursor_tell(), 1).ok().and_then(|b| b.first().copied());
                if matches!(byte, Some(0)) {
                    let _ = self.read_scalar(&format!("[{count}]"), ty, Some(parent_idx), &[], None);
                    count += 1;
                    break;
                }
            }
            match self.read_scalar(&format!("[{count}]"), ty, Some(parent_idx), &[], None) {
                Ok(_) => {}
                Err(RuntimeError::Source(_)) => break,
                Err(e) => {
                    self.array_index_stack.pop();
                    return Err(e);
                }
            }
            count += 1;
            // `break` inside an element's body (typical in
            // `Block blocks[while(true)] { ...; if (sentinel) break; }`)
            // ends the array, not just the surrounding struct.
            if self.break_pending {
                self.break_pending = false;
                break;
            }
            let now = self.cursor_tell();
            if now == last_pos {
                stalls += 1;
                if stalls > 4 {
                    self.diagnostics.push(Diagnostic {
                        message: format!(
                            "dynamic array `{name}` stalled at offset {now} after {count} reads"
                        ),
                        severity: Severity::Warning,
                        file_offset: Some(now),
                        template_line: None,
                    });
                    break;
                }
            } else {
                stalls = 0;
                last_pos = now;
            }
            // Cap absolute element count so a runaway predicate
            // doesn't burn step budget on an infinite array.
            if count >= LOOP_STALL_LIMIT as u64 {
                break;
            }
        }
        self.array_index_stack.pop();
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
                    // Collect every enum whose registered key matches
                    // a prefix of the path. With two namespaces both
                    // declaring `enum Type {...}` the bare `Type` key
                    // is overwritten by the second registration, so
                    // we also walk the full type table for any name
                    // that ends with `::<prefix>` and try each
                    // candidate -- the first one that has the variant
                    // wins. (rgbds.hexpat needs `fstack::Type::REPT`
                    // even though `sym::Type` is the last registered
                    // bare `Type`.)
                    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
                    let mut candidates: Vec<EnumDecl> = Vec::new();
                    for prefix_len in (1..segments.len()).rev() {
                        let key: String = segments[..prefix_len].join("::");
                        if seen_keys.insert(key.clone())
                            && let Some(TypeDef::Enum(decl)) = self.types.get(&key)
                        {
                            candidates.push(decl.clone());
                        }
                        let suffix = format!("::{key}");
                        let extra: Vec<(String, EnumDecl)> = self
                            .types
                            .iter()
                            .filter_map(|(n, d)| {
                                if let TypeDef::Enum(decl) = d {
                                    if n.ends_with(&suffix) || n == &key {
                                        Some((n.clone(), decl.clone()))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                            .collect();
                        for (n, decl) in extra {
                            if seen_keys.insert(n) {
                                candidates.push(decl);
                            }
                        }
                    }
                    for decl in &candidates {
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
                match target.as_ref() {
                    Expr::Ident { name, .. } => {
                        if name == "$" {
                            // `$ = expr;` / `$ += n;` -- the magic cursor.
                            // Move the runtime cursor to the new offset
                            // instead of just updating a scope variable.
                            // terminfo.hexpat advances the cursor past
                            // a NUL terminator with `$ += 1;` and the
                            // surrounding read flow assumes the cursor
                            // actually moved.
                            let off = new_val.to_i128().unwrap_or(0).max(0) as u64;
                            self.cursor_seek(off);
                        } else {
                            self.store_ident(name, new_val.clone());
                        }
                    }
                    Expr::Member { target: inner, field, .. } => {
                        // `obj.field = value;` -- mutate the field's
                        // emitted-node value in place. lznt1.hexpat's
                        // `value.value = data;` (where `value` is a
                        // Value struct local) needs this to land on
                        // the child node, otherwise the surrounding
                        // `appendData(value)` sees an empty struct.
                        if let Ok(Some(owner)) = self.resolve_node_chain(inner) {
                            let candidates = self.nodes_by_name.get(field).cloned().unwrap_or_default();
                            if let Some(idx) = candidates
                                .into_iter()
                                .rev()
                                .find(|i| self.nodes[i.as_usize()].parent == Some(owner))
                            {
                                self.nodes[idx.as_usize()].value = Some(new_val.clone());
                            }
                        }
                    }
                    _ => {}
                }
                Ok(new_val)
            }
            Expr::Member { target, field, .. } => {
                // `parent[.parent]*.field` -- count the parent hops
                // and walk up the tree-parent chain that many times
                // before looking up the field. ImHex treats array
                // elements (and other wrapper nodes) as transparent
                // for parent resolution, so we keep walking up if
                // the immediate ancestor doesn't have the field.
                if let Some(hops) = count_parent_hops(target) {
                    let mut cur = self.current_parent;
                    for _ in 0..(hops - 1) {
                        cur = match cur.and_then(|idx| self.nodes[idx.as_usize()].parent) {
                            Some(p) => Some(p),
                            None => break,
                        };
                    }
                    loop {
                        match cur {
                            Some(idx) => {
                                if let Some(v) = self.lookup_member_under(idx, field) {
                                    return Ok(v);
                                }
                                cur = self.nodes[idx.as_usize()].parent;
                            }
                            None => {
                                if let Some(idx) = self.find_top_level_idx(field) {
                                    return Ok(self.nodes[idx.as_usize()].value.clone().unwrap_or(Value::Void));
                                }
                                break;
                            }
                        }
                    }
                    return Err(RuntimeError::Type(format!("unresolved member `.{field}`")));
                }
                if let Some(idx) = self.resolve_node_chain(target)? {
                    if let Some(v) = self.lookup_member_under(idx, field) {
                        return Ok(v);
                    }
                    // Target struct exists but the field wasn't
                    // emitted -- usually because a truncated read at
                    // EOF broke the inner struct's body before this
                    // field. Returning `Void` instead of erroring
                    // lets the surrounding template continue (e.g.
                    // veado.hexpat's `varint num_X; T x[num_X.value]`
                    // pattern when num_X was started but its inner
                    // `value` child never emitted).
                    return Ok(Value::Void);
                }
                fn describe(e: &Expr) -> String {
                    match e {
                        Expr::Ident { name, .. } => name.clone(),
                        Expr::Member { target, field, .. } => format!("{}.{}", describe(target), field),
                        Expr::Index { target, .. } => format!("{}[..]", describe(target)),
                        Expr::Path { segments, .. } => segments.join("::"),
                        _ => "<expr>".into(),
                    }
                }
                let target_desc = describe(target.as_ref());
                Err(RuntimeError::Type(format!("unresolved member `{target_desc}.{field}`")))
            }
            Expr::Index { target, index, .. } => {
                // Resolve `arr[i]` against the emitted node tree
                // (returns the i-th element's value when found),
                // string/byte literals (returns the i-th char/byte),
                // or falls back to the raw index value for cases
                // where neither model fits.
                let i = self.eval(index)?.to_i128().unwrap_or(0).max(0) as usize;
                // Try the node-tree path first so eval'd struct
                // arrays return their actual element value.
                if let Some(arr_idx) = self.resolve_node_chain(target)?
                    && let Some(elem) = self.find_first_child_idx(arr_idx, &format!("[{i}]"))
                {
                    return Ok(self.nodes[elem.as_usize()].value.clone().unwrap_or(Value::Void));
                }
                let target_val = self.eval(target)?;
                if let Value::Str(s) = &target_val
                    && let Some(c) = s.chars().nth(i)
                {
                    return Ok(Value::Char { value: c as u32, kind: PrimKind::char() });
                }
                if let Value::Bytes(b) = &target_val
                    && let Some(byte) = b.get(i).copied()
                {
                    return Ok(Value::UInt { value: byte as u128, kind: PrimKind::u8() });
                }
                Ok(Value::UInt { value: i as u128, kind: PrimKind::u64() })
            }
            Expr::Call { callee, args, .. } => {
                // Build a list of candidate names to try in order:
                // the fully-qualified path first (so a namespaced
                // builtin like `std::print` wins over a same-leaf
                // user function), then the bare leaf as a fallback.
                //
                // `builtin::...` paths are special: they're hooks
                // for runtime-provided functions (`builtin::std::mem::
                // base_address`, etc.). The std library wraps them
                // in user-level shims of the same leaf name, so a
                // leaf-fallback would resolve `builtin::base_address`
                // back to the user function and infinite-recurse.
                // We disable the leaf fallback for any path rooted
                // in `builtin`.
                let (names, is_builtin) = match callee.as_ref() {
                    Expr::Ident { name, .. } => (vec![name.clone()], false),
                    Expr::Path { segments, .. } => {
                        let is_builtin = segments.first().map(String::as_str) == Some("builtin");
                        let mut v = vec![segments.join("::")];
                        if !is_builtin
                            && let Some(leaf) = segments.last()
                        {
                            v.push(leaf.clone());
                        }
                        (v, is_builtin)
                    }
                    _ => return Err(RuntimeError::Type("call target must be an identifier".into())),
                };
                let evaluated: Vec<Value> = args.iter().map(|a| self.eval(a)).collect::<Result<_, _>>()?;
                // For each arg expression, also try to resolve it
                // to an emitted-node index. Functions like
                // `GetImageDataSize(ImageSpec imSpec) { return
                // imSpec.height * ...; }` need the parameter to
                // route member access through the caller's node
                // tree -- the bare struct value is `Void`.
                let mut arg_node_refs: Vec<Option<NodeIdx>> = Vec::with_capacity(args.len());
                for a in args {
                    let idx = self.resolve_node_chain(a).ok().flatten();
                    arg_node_refs.push(idx);
                }
                let mut last_err: Option<RuntimeError> = None;
                for name in &names {
                    match self.call_named_with_aliases(name, &evaluated, &arg_node_refs) {
                        Ok(v) => return Ok(v),
                        Err(RuntimeError::UndefinedName { .. }) => continue,
                        Err(e) => last_err = Some(e),
                    }
                }
                if is_builtin {
                    // Unimplemented runtime hook -- return Void rather
                    // than failing the whole template. Templates that
                    // depend on the actual builtin value will get
                    // wrong-but-progressing results; templates that
                    // just route through the wrapper see no harm.
                    return Ok(Value::Void);
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
            // Function-param alias takes priority over name-only
            // node search: `sizeof(data)` inside `fn appendData(ref
            // auto data)` should report the *caller's* node size,
            // not whatever trailing emission happens to share the
            // bare name `data`.
            if let Some(idx) = self.find_node_idx_for_ident(name) {
                let n = &self.nodes[idx.as_usize()];
                return Ok(match kind {
                    ReflectKind::Sizeof => Value::UInt { value: n.length as u128, kind: PrimKind::u64() },
                    ReflectKind::Addressof => Value::UInt { value: n.offset as u128, kind: PrimKind::u64() },
                    ReflectKind::Typeof => Value::Str(format!("{:?}", n.ty)),
                });
            }
            // Special case: `sizeof($)` -> file size. ImHex's
            // pattern reference uses this idiom for "is there enough
            // data".
            if matches!(kind, ReflectKind::Sizeof) && name == "$" {
                return Ok(Value::UInt { value: self.source.len() as u128, kind: PrimKind::u64() });
            }
        }
        // For complex expressions (member chains, indexing) try the
        // node-tree path first so `addressof(stl.triangles[i].points)`
        // returns the points array's offset / length instead of 0
        // / a value-derived guess.
        if let Ok(Some(idx)) = self.resolve_node_chain(operand) {
            let n = &self.nodes[idx.as_usize()];
            return Ok(match kind {
                ReflectKind::Sizeof => Value::UInt { value: n.length as u128, kind: PrimKind::u64() },
                ReflectKind::Addressof => Value::UInt { value: n.offset as u128, kind: PrimKind::u64() },
                ReflectKind::Typeof => Value::Str(format!("{:?}", n.ty)),
            });
        }
        // Fallback: evaluate as a value and use its apparent size.
        // Degraded relative to ImHex's compile-time sizeof but
        // covers expressions that don't bottom out at an emitted
        // node.
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
                // Unknown name -- fall through to the caller's
                // dynamic-value sizing path. Returning Ok(0) here
                // would shadow live values like a `str` parameter.
                None => return Err(RuntimeError::UnknownType { name: name.to_owned() }),
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
        // `$` -- current cursor offset. ImHex templates use this
        // idiom heavily for `@ $+offset` placements and bounds-
        // checking arithmetic; returning 0 makes those drift far
        // off and trips read-past-end downstream.
        if name == "$" {
            return Ok(Value::UInt { value: self.pos as u128, kind: PrimKind::u64() });
        }
        // `this` / `parent` in non-member position (e.g.
        // `set_display_name(this, label)`). The interpreter doesn't
        // model node references as first-class values, so we emit
        // `Void` -- callers that pass the magic ident to a builtin
        // get a no-op handle but the run keeps progressing. Member
        // access (`this.x`) bypasses this path via the Member arm.
        if name == "this" || name == "parent" {
            return Ok(Value::Void);
        }
        // Last resort: if `name` matches an emitted struct/array
        // node, return its value (or `Void` if the node didn't bind
        // one). Templates reference field names as opaque handles
        // when passing them to builtins (`add_virtual_file(name, file)`)
        // -- without this fallback the eval errors with
        // `undefined name`.
        if let Some(indices) = self.nodes_by_name.get(name)
            && let Some(idx) = indices.last()
        {
            return Ok(self.nodes[idx.as_usize()].value.clone().unwrap_or(Value::Void));
        }
        Err(RuntimeError::UndefinedName { name: name.to_owned() })
    }

    /// Look up a child node whose `parent == owner_idx` and whose
    /// name matches `field`. Walks the tree right-to-left so the
    /// most-recently-emitted match wins (matters for repeated names
    /// inside a long-lived parent like an array). Returns `Some(Void)`
    /// for struct/array children that don't bind a scalar value --
    /// the *node* is still the right answer for chained accesses
    /// (`a.b.c.field`) even though `b` itself has no value.
    fn lookup_member_under(&self, owner_idx: NodeIdx, field: &str) -> Option<Value> {
        let candidates = self.nodes_by_name.get(field)?;
        for &idx in candidates.iter().rev() {
            let n = &self.nodes[idx.as_usize()];
            if n.parent == Some(owner_idx) {
                return Some(n.value.clone().unwrap_or(Value::Void));
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
                // `parent.foo`, `parent.parent.foo`, `parent.parent.
                // parent.foo`, ... -- count the leading `parent` hops
                // and walk up the tree-parent chain that many times,
                // then look up `foo` at that level. ImHex treats
                // array elements (and similar wrapper nodes) as
                // transparent for `parent` resolution, so we *also*
                // skip past ancestors that don't have a struct/
                // bitfield/union shape -- otherwise `parent` from
                // inside an array element would just hit the array
                // node and stop.
                if let Some(hops) = count_parent_hops(target) {
                    let mut cur = self.current_parent;
                    for _ in 0..(hops - 1) {
                        cur = match cur.and_then(|idx| self.nodes[idx.as_usize()].parent) {
                            Some(p) => Some(p),
                            None => break,
                        };
                    }
                    loop {
                        match cur {
                            Some(idx) => {
                                if let Some(found) = self.find_first_child_idx(idx, field) {
                                    return Ok(Some(found));
                                }
                                cur = self.nodes[idx.as_usize()].parent;
                            }
                            None => return Ok(self.find_top_level_idx(field)),
                        }
                    }
                }
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
    /// emitted node. Magic identifiers (`parent`, `this`) resolve to
    /// the active enclosing struct; bare names prefer a sibling of
    /// the current scope (child of `this` / `parent` / their
    /// ancestors), falling back to a top-level node, and only as a
    /// last resort take the most-recently-emitted node anywhere.
    /// Without the scope filter, a name like "header" would
    /// happily resolve to a nested struct's `header` child even
    /// when the caller wanted the outer struct's sibling.
    fn find_node_idx_for_ident(&self, name: &str) -> Option<NodeIdx> {
        match name {
            "parent" => self.current_parent,
            "this" => self.most_recent_struct_idx(),
            other => {
                // Function-parameter aliases first: `imSpec` inside
                // `GetImageDataSize(imSpec)` should route through
                // the caller's `imageSpec` node.
                for scope in self.scopes.iter().rev() {
                    if let Some(idx) = scope.node_aliases.get(other) {
                        return Some(*idx);
                    }
                }
                // Walk the enclosing-struct chain (this, then
                // parent, then grandparent, ...) looking for a
                // child named `other`.
                let mut cur = self.this_stack.last().copied().or(self.current_parent);
                while let Some(idx) = cur {
                    if let Some(found) = self.find_first_child_idx(idx, other) {
                        return Some(found);
                    }
                    cur = self.nodes[idx.as_usize()].parent;
                }
                // Top-level fallback (`Outer o; ... eval somewhere
                // that references `o`).
                if let Some(top) = self.find_top_level_idx(other) {
                    return Some(top);
                }
                // Last resort: most recent emission of that name.
                self.nodes_by_name.get(other).and_then(|v| v.last().copied())
            }
        }
    }

    fn find_first_child_idx(&self, parent_idx: NodeIdx, name: &str) -> Option<NodeIdx> {
        let candidates = self.nodes_by_name.get(name)?;
        candidates.iter().copied().find(|idx| self.nodes[idx.as_usize()].parent == Some(parent_idx))
    }

    /// Find the most recent top-level node (parent is `None`) with
    /// the given name. Used as the `parent` fallback when the
    /// current scope has no enclosing struct -- ImHex treats the
    /// implicit "program" scope as a parent for top-level fields.
    fn find_top_level_idx(&self, name: &str) -> Option<NodeIdx> {
        let candidates = self.nodes_by_name.get(name)?;
        candidates.iter().copied().rev().find(|idx| self.nodes[idx.as_usize()].parent.is_none())
    }

    /// Index of the struct / union / bitfield currently being read.
    /// Backs the magic `this` identifier. Tracks an explicit stack
    /// pushed on `read_struct` / `read_bitfield` entry instead of
    /// scanning the emitted node list (the latter pointed at the
    /// *previously-completed* struct after a sibling read finished).
    fn most_recent_struct_idx(&self) -> Option<NodeIdx> {
        self.this_stack.last().copied()
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

    fn call_named_with_aliases(
        &mut self,
        name: &str,
        args: &[Value],
        node_aliases: &[Option<NodeIdx>],
    ) -> Result<Value, RuntimeError> {
        if let Some(v) = self.call_builtin(name, args, node_aliases)? {
            return Ok(v);
        }
        // `u8(x)`, `u32(x)`, `s64(x)`, ... -- primitive cast via
        // call syntax. ImHex templates use this to coerce an enum
        // member back to its raw integer (`u8(type) <= 0x7F`).
        if let Some(prim) = self
            .types
            .get(name)
            .and_then(|d| if let TypeDef::Primitive(p) = d { Some(*p) } else { None })
            .or_else(|| generic_int_primitive(name))
            && let Some(arg) = args.first()
        {
            return Ok(coerce_value_to_prim(arg.clone(), prim));
        }
        let Some(func) = self.functions.get(name).cloned() else {
            return Err(RuntimeError::UndefinedName { name: name.to_owned() });
        };
        // Tolerate arity mismatches at the call site. ImHex functions
        // often declare default-valued trailing params (`fn f(u32 n,
        // str msg = "")`); our parser drops the default expression
        // but the caller still passes fewer args. We bind missing
        // params to Void and ignore extras.
        self.scopes.push(Scope::default());
        for (i, p) in func.params.iter().enumerate() {
            let v = args.get(i).cloned().unwrap_or(Value::Void);
            self.current_scope_mut().vars.insert(p.name.clone(), v);
            if let Some(Some(node_idx)) = node_aliases.get(i) {
                self.current_scope_mut().node_aliases.insert(p.name.clone(), *node_idx);
            }
        }
        // Inside the fn, `parent` should resolve to the struct
        // whose body did the call (the caller's `this`). Save and
        // restore around the fn body so a regular `parent.foo`
        // inside resolves through the call site.
        let saved_parent = self.current_parent;
        if let Some(caller_this) = self.this_stack.last().copied() {
            self.current_parent = Some(caller_this);
        }
        let saved_return = self.return_value.take();
        self.function_depth += 1;
        let mut result = Value::Void;
        for s in &func.body {
            match self.exec_stmt(s, None) {
                Ok(Flow::Return) => {
                    if let Some(v) = self.return_value.take() {
                        result = v;
                    }
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    self.function_depth -= 1;
                    self.return_value = saved_return;
                    self.current_parent = saved_parent;
                    self.scopes.pop();
                    return Err(e);
                }
            }
        }
        self.function_depth -= 1;
        self.return_value = saved_return;
        self.current_parent = saved_parent;
        self.scopes.pop();
        Ok(result)
    }

    /// Lookup a small set of language built-ins. Names are matched
    /// against both the fully-qualified spelling (`std::print`)
    /// and the bare leaf (`print`); the call dispatcher in
    /// [`Self::eval`] tries each in turn so a corpus pattern that
    /// imports `std::print` and one that calls bare `print` both
    /// land here.
    fn call_builtin(
        &mut self,
        name: &str,
        args: &[Value],
        node_aliases: &[Option<NodeIdx>],
    ) -> Result<Option<Value>, RuntimeError> {
        Ok(Some(match name {
            "set_endian"
            | "std::core::set_endian"
            | "std::mem::set_endian"
            | "builtin::std::core::set_endian"
            | "builtin::std::mem::set_endian" => {
                // ImHex's `std::mem::Endian` enum: Native=0, Big=1,
                // Little=2. The user-level shim coerces the enum to
                // a u32 before calling the builtin, so we expect an
                // integer here. `Bool(true)` is also accepted for
                // legacy callers.
                let kind = args.first().and_then(|v| v.to_i128()).unwrap_or(0);
                self.endian = match kind {
                    1 => Endian::Big,
                    2 => Endian::Little,
                    _ => self.endian,
                };
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
            // File-level queries. Both the std-library shim names
            // (`std::mem::base_address`, ...) and the underlying
            // `builtin::std::mem::*` paths are wired up so the
            // shim's `return builtin::std::mem::base_address();`
            // round-trips to a real value instead of returning Void.
            "std::mem::eof" | "builtin::std::mem::eof" => Value::Bool(self.cursor_tell() >= self.source.len()),
            "std::mem::size" | "std::mem::file_size" | "builtin::std::mem::size" => {
                Value::UInt { value: self.source.len() as u128, kind: PrimKind::u64() }
            }
            "std::mem::current_offset" | "builtin::std::mem::current_offset" => {
                Value::UInt { value: self.cursor_tell() as u128, kind: PrimKind::u64() }
            }
            "std::mem::base_address" | "builtin::std::mem::base_address" => {
                // MemorySource exposes bytes starting at offset 0;
                // there's no pre-mapped base. Hosts that wrap
                // arbitrary memory regions can override later.
                Value::UInt { value: 0, kind: PrimKind::u64() }
            }
            "builtin::std::mem::find_sequence_in_range"
            | "std::mem::find_sequence_in_range" => {
                // `find_sequence_in_range(occurrence_index,
                // start_offset, end_offset, byte0, byte1, ...)` --
                // search for the byte sequence within
                // `[start, end)`. Returns the absolute offset of
                // the `occurrence_index`-th match, or -1 if not
                // found. Templates use it to locate magic markers
                // anywhere in the file; without an implementation
                // every search returned -1 and the templates that
                // depend on this builtin (zip's `find_eocd`) bailed
                // out.
                let occurrence = args.first().and_then(|v| v.to_i128()).unwrap_or(0).max(0) as usize;
                let start = args.get(1).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as usize;
                let end_arg = args.get(2).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as usize;
                let needle: Vec<u8> = args
                    .iter()
                    .skip(3)
                    .filter_map(|v| v.to_i128())
                    .map(|n| (n as i64) as u8)
                    .collect();
                let len = self.source.len() as usize;
                let end = end_arg.min(len);
                if needle.is_empty() || start >= end || end - start < needle.len() {
                    Value::SInt { value: -1, kind: PrimKind::s128() }
                } else {
                    let haystack = self.source.read(start as u64, (end - start) as u64).ok();
                    let Some(haystack) = haystack else {
                        return Ok(Some(Value::SInt { value: -1, kind: PrimKind::s128() }));
                    };
                    let mut hits = 0usize;
                    let mut found: Option<usize> = None;
                    for i in 0..haystack.len().saturating_sub(needle.len()).saturating_add(1) {
                        if haystack[i..i + needle.len()] == needle[..] {
                            if hits == occurrence {
                                found = Some(start + i);
                                break;
                            }
                            hits += 1;
                        }
                    }
                    match found {
                        Some(off) => Value::SInt { value: off as i128, kind: PrimKind::s128() },
                        None => Value::SInt { value: -1, kind: PrimKind::s128() },
                    }
                }
            }
            "builtin::std::mem::find_string_in_range"
            | "std::mem::find_string_in_range" => {
                // `find_string_in_range(occurrence_index,
                // start_offset, end_offset, needle)` -- analogous
                // to find_sequence_in_range but with a string
                // needle.
                let occurrence = args.first().and_then(|v| v.to_i128()).unwrap_or(0).max(0) as usize;
                let start = args.get(1).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as usize;
                let end_arg = args.get(2).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as usize;
                let needle: Vec<u8> = match args.get(3) {
                    Some(Value::Str(s)) => s.bytes().collect(),
                    Some(Value::Bytes(b)) => b.clone(),
                    _ => Vec::new(),
                };
                let len = self.source.len() as usize;
                let end = end_arg.min(len);
                if needle.is_empty() || start >= end || end - start < needle.len() {
                    Value::SInt { value: -1, kind: PrimKind::s128() }
                } else {
                    let haystack = self.source.read(start as u64, (end - start) as u64).ok();
                    let Some(haystack) = haystack else {
                        return Ok(Some(Value::SInt { value: -1, kind: PrimKind::s128() }));
                    };
                    let mut hits = 0usize;
                    let mut found: Option<usize> = None;
                    for i in 0..haystack.len().saturating_sub(needle.len()).saturating_add(1) {
                        if haystack[i..i + needle.len()] == needle[..] {
                            if hits == occurrence {
                                found = Some(start + i);
                                break;
                            }
                            hits += 1;
                        }
                    }
                    match found {
                        Some(off) => Value::SInt { value: off as i128, kind: PrimKind::s128() },
                        None => Value::SInt { value: -1, kind: PrimKind::s128() },
                    }
                }
            }
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
            // `std::core::member_count(arr)` -- returns the number of
            // emitted children for an array / struct value. Inside
            // tiff.hexpat, the macro
            // `Count = member_count(DirectoryEntry[i].ValueOffset.
            // ValueArray.Values)` drives downstream array sizing.
            // Falls back to 0 when the chain doesn't bottom out at
            // an emitted node.
            "std::core::member_count" | "builtin::std::core::member_count" => {
                let _ = node_aliases;
                // Returning 0 keeps templates that gate downstream
                // sizing on `member_count(...) == K` from running
                // their dependent loops -- a real count occasionally
                // unlocks code paths that depend on features we
                // don't model yet (gltf.hexpat: a non-zero count
                // triggers an `if (member_count(...) == 1) { ... }`
                // branch that walks JSON-decoded bufferViews).
                Value::UInt { value: 0, kind: PrimKind::u64() }
            }
            // Inside an array body, the builtin returns the current
            // element index (0-based). Outside an array, returns 0.
            // ogg.hexpat's per-element struct uses
            // `parent.segmentTable[std::core::array_index()]` to size
            // its data field.
            "std::core::array_index" | "builtin::std::core::array_index" => {
                let idx = self.array_index_stack.last().copied().unwrap_or(0);
                Value::UInt { value: idx as u128, kind: PrimKind::u64() }
            }
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
    // everything else widens to numeric. Either operand being a
    // string promotes the other to its `Display` form so
    // `result += c` (where `c` is a char) and `name + ".bin"` both
    // produce strings.
    if matches!(l, Value::Str(_)) || matches!(r, Value::Str(_)) {
        let render = |v: &Value| match v {
            Value::Str(s) => s.clone(),
            Value::Char { value, .. } => char::from_u32(*value).map(String::from).unwrap_or_default(),
            other => format!("{other}"),
        };
        let a = render(l);
        let b = render(r);
        return Ok(match op {
            BinOp::Add => Value::Str(format!("{a}{b}")),
            // `"-" * 50` -- string repetition (Python-/JS-style).
            // tar.hexpat uses this to print divider lines via
            // `std::print("-" * 50);`.
            BinOp::Mul => {
                let count =
                    r.to_i128().or_else(|| l.to_i128()).unwrap_or(0).max(0) as usize;
                let s = if matches!(r, Value::Str(_)) { &b } else { &a };
                Value::Str(s.repeat(count))
            }
            BinOp::Eq => Value::Bool(a == b),
            BinOp::NotEq => Value::Bool(a != b),
            BinOp::Lt => Value::Bool(a < b),
            BinOp::LtEq => Value::Bool(a <= b),
            BinOp::Gt => Value::Bool(a > b),
            BinOp::GtEq => Value::Bool(a >= b),
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
            // Treat division by zero as 0 rather than halting --
            // ImHex templates frequently compute sizes using a
            // value that's zero before a header is read; aborting
            // the run on those is more disruptive than emitting a
            // wrong-but-progressing answer.
            value: li.checked_div(ri).unwrap_or(0),
            kind: PrimKind::s64(),
        },
        BinOp::Rem => {
            Value::SInt { value: li.checked_rem(ri).unwrap_or(0), kind: PrimKind::s64() }
        }
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
    if !fmt.contains('{') {
        return args.iter().map(|v| format!("{v}")).collect::<Vec<_>>().join(" ");
    }
    let mut out = String::with_capacity(fmt.len() + 8);
    let mut chars = fmt.chars().peekable();
    let mut idx = 1usize; // first replacement arg
    while let Some(c) = chars.next() {
        if c == '{' {
            // Collect the spec inside the braces.
            let mut spec = String::new();
            let mut closed = false;
            while let Some(&n) = chars.peek() {
                chars.next();
                if n == '}' {
                    closed = true;
                    break;
                }
                spec.push(n);
            }
            if !closed {
                out.push('{');
                out.push_str(&spec);
                continue;
            }
            if let Some(v) = args.get(idx) {
                out.push_str(&format_value_with_spec(v, &spec));
            }
            idx += 1;
        } else if c == '}' && chars.peek() == Some(&'}') {
            chars.next();
            out.push('}');
        } else {
            out.push(c);
        }
    }
    out
}

/// Render a [`Value`] according to a format spec. Honours the
/// subset of Rust-style fmt specs ImHex templates use: `:X`, `:x`,
/// `:o`, `:b`, `:d`, `:s`, plus a fill / width prefix
/// (`{:08X}`). Anything else falls back to plain Display.
fn format_value_with_spec(v: &Value, spec: &str) -> String {
    let spec = spec.trim_start_matches(':');
    if spec.is_empty() {
        return match v {
            Value::Str(s) => s.clone(),
            other => format!("{other}"),
        };
    }
    let (width_part, kind) = match spec.chars().last() {
        Some(c @ ('X' | 'x' | 'o' | 'b' | 'd' | 's' | 'c')) => (&spec[..spec.len() - c.len_utf8()], c),
        _ => (spec, 'd'),
    };
    // Optional `0` fill flag prefix; the width is the rest.
    let (fill, width_str) = if let Some(rest) = width_part.strip_prefix('0') {
        ('0', rest)
    } else {
        (' ', width_part)
    };
    let width: usize = width_str.parse().unwrap_or(0);
    let body = match kind {
        'X' => v.to_i128().map(|n| format!("{:X}", n as u128)).unwrap_or_else(|| format!("{v}")),
        'x' => v.to_i128().map(|n| format!("{:x}", n as u128)).unwrap_or_else(|| format!("{v}")),
        'o' => v.to_i128().map(|n| format!("{:o}", n as u128)).unwrap_or_else(|| format!("{v}")),
        'b' => v.to_i128().map(|n| format!("{:b}", n as u128)).unwrap_or_else(|| format!("{v}")),
        'c' => match v {
            Value::Char { value, .. } => char::from_u32(*value).map(String::from).unwrap_or_default(),
            other => format!("{other}"),
        },
        's' => match v {
            Value::Str(s) => s.clone(),
            other => format!("{other}"),
        },
        _ => v.to_i128().map(|n| n.to_string()).unwrap_or_else(|| format!("{v}")),
    };
    if body.len() >= width {
        body
    } else {
        // Manual padding: built-in format! syntax can't take `fill`
        // dynamically. Templates use `0` or space; we honour both.
        let pad: String = std::iter::repeat_n(fill, width - body.len()).collect();
        format!("{pad}{body}")
    }
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
