//! Bytecode IR for ImHex pattern programs.
//!
//! Templates compile once to a flat [`Program`] (op stream + intern
//! tables). Runtime work moves into a tight match-dispatched VM
//! over `Vec<Op>` instead of a recursive AST walk through `Stmt` /
//! `Expr` variants.
//!
//! ## Why
//!
//! The AST interpreter spends most of bencode's torrent walk in
//! `exec_stmt` -> `read_struct` -> `exec_field_decl` -> `exec_stmt`
//! recursion. The cost is structural -- every nested Bencode value
//! spawns a fresh frame that re-resolves names, re-clones builtin
//! lookups, and re-walks the same `Expr` shapes. A flat op stream
//! collapses that into one dispatch loop and lets us pre-intern
//! every name into a `u32`.
//!
//! ## Lives in this crate today, generic by design
//!
//! The op set is intentionally language-agnostic -- it speaks in
//! cursor reads, node emissions, scope frames, member access,
//! control flow, and named function calls, none of which is
//! ImHex-specific. `hxy-010-lang` could lower its own AST to the
//! same op stream once we've pressure-tested the shape here.
//!
//! When the op set stabilises against the ImHex corpus, the plan is
//! to lift this module (plus the VM and a shared `Value` /
//! `PrimKind`) into a new `hxy-bc` workspace crate, leaving each
//! lang crate to host only its parser + AST + compile pass + the
//! lang-specific runtime callbacks (no_unique_address, transforms,
//! templates for ImHex; switch fall-through, DeclModifier, typedef
//! for 010). Premature factoring now risks letting ImHex-only
//! semantics leak into the IR, so the shared crate waits for parity
//! first.
//!
//! ## Status
//!
//! Scaffolding only at this stage. The op set covers value-stack
//! pushes, name load/store, arithmetic, and a stub for primitive
//! reads. The compile pass and full VM land in follow-up commits;
//! the existing AST interpreter remains the path the public
//! [`crate::Interpreter`] runs through. Once the bytecode path
//! reaches corpus parity it will replace the AST walker.
//!
//! Names ([`IdentId`]) and string literals ([`StrId`]) are each
//! distinct intern spaces -- they share a `u32` representation but
//! must not be cross-indexed. Type IDs ([`TypeId`]) point into a
//! flat `Vec<ResolvedType>` populated during compile.

use crate::ast::BinOp;
use crate::ast::Program as AstProgram;
use crate::ast::ReflectKind;
use crate::ast::Stmt;
use crate::ast::TopItem;
use crate::ast::TypeRef;
use crate::ast::UnaryOp;
use crate::interp::Severity;

/// Index into the program's identifier intern table. Used for every
/// runtime name lookup (variables, fields, functions, types). String
/// equality at runtime becomes `u32 == u32`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct IdentId(pub u32);

/// Index into the program's string-literal intern table. Distinct
/// from [`IdentId`] so we don't accidentally compare a literal
/// against a name.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct StrId(pub u32);

/// Index into the program's resolved-type table.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TypeId(pub u32);

/// Op-stream offset.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Pc(pub u32);

/// Index into [`Program::struct_bodies`]. Each compiled struct body
/// gets one. Distinct from [`TypeId`] so a `BodyId` cannot be used
/// where a type-table lookup is expected.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BodyId(pub u32);

/// Index into [`Program::enum_decls`]. Each top-level enum decl
/// the compile pass registers gets one.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EnumId(pub u32);

/// Index into [`Program::attr_lists`]. A single attr list can be
/// shared between multiple ops (decorative attrs on a placement vs.
/// the read it modifies, for example).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AttrListId(pub u32);

/// Intern table for identifiers (or string literals -- two of these
/// live on a [`Program`], one per kind). Insertion is O(1) hashed;
/// lookup is `u32 -> &str`.
#[derive(Debug, Default)]
pub struct InternTable {
    storage: Vec<String>,
    index: rustc_hash::FxHashMap<String, u32>,
}

impl InternTable {
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.index.get(s) {
            return id;
        }
        let id = self.storage.len() as u32;
        self.storage.push(s.to_owned());
        self.index.insert(s.to_owned(), id);
        id
    }

    pub fn get(&self, id: u32) -> &str {
        &self.storage[id as usize]
    }

    pub fn len(&self) -> usize {
        self.storage.len()
    }

    pub fn is_empty(&self) -> bool {
        self.storage.is_empty()
    }
}

/// One bytecode instruction. Operand layout follows the convention
/// "values on the operand stack, ids inline in the op." The VM
/// dispatches in a single match.
///
/// Op variants intentionally avoid heap-allocated payloads
/// (`String`, `Vec`) so the op stream stays cache-friendly. Anything
/// that needs a string goes through [`IdentId`] or [`StrId`].
#[derive(Clone, Copy, Debug)]
pub enum Op {
    // ---- value stack ----
    PushInt(i128),
    PushFloat(f64),
    PushStr(StrId),
    PushBool(bool),
    PushChar(u32),
    PushVoid,
    Pop,
    Dup,

    // ---- names / cursor ----
    LoadIdent(IdentId),
    StoreIdent(IdentId),
    LoadCursor,
    StoreCursor,

    // ---- expression operators ----
    BinOp(BinOp),
    UnOp(UnaryOp),
    Ternary,
    Member(IdentId),
    Index,

    // ---- function dispatch ----
    Call { name: IdentId, argc: u8 },
    Reflect(ReflectKind),

    // ---- reads (host-effect ops) ----
    ReadPrim { ty: TypeId, name: IdentId },
    ReadStruct { ty: TypeId, name: IdentId },
    /// Read a registered top-level enum. The VM dispatches into
    /// `read_enum` after ensuring the enum decl is in
    /// `Interpreter::types` so any variant-value expressions can
    /// resolve. Backing-struct enums (rar's vint-style) work too,
    /// since `read_enum` already handles them.
    ReadEnum { id: EnumId, name: IdentId },
    /// Fixed-size primitive array `Type name[N]` where `N` is a
    /// literal integer at compile time. The VM dispatches into
    /// `read_array`, which folds char-typed reads down to a single
    /// `Str`-valued node and emits one parent + N children for any
    /// other element type. The dyn variant ([`Op::ReadArrayDyn`])
    /// covers non-literal sizes by popping the count off the
    /// value stack.
    ReadArrayFixed { ty: TypeId, name: IdentId, count: u64 },
    /// Dynamic-size primitive array. Pops one [`Value`] from the
    /// operand stack, coerces to a u64 element count, then runs
    /// the same `read_array` host helper as [`Op::ReadArrayFixed`].
    /// Used when the source size expression is anything other than
    /// an integer literal (an identifier reference, a `length-1`
    /// arithmetic, ...).
    ReadArrayDyn { ty: TypeId, name: IdentId },
    ReadCharArr { name: IdentId }, // count on stack
    ReadDynArr { ty: TypeId, name: IdentId, pred: Pc, end: Pc },

    /// Append a pre-baked attribute list (key/value pairs) to the
    /// VM's pending-attrs buffer so the next read op picks them up.
    /// The compile pass emits one of these for every `[[attr, ...]]`
    /// list on a field decl. Decorative attrs (`name`, `comment`,
    /// `format`, ...) flow straight through; behaviour-changing
    /// attrs (`transform`, `no_unique_address`, ...) still need
    /// dedicated lowering -- the compile pass refuses such field
    /// decls today.
    PushAttrs(AttrListId),

    /// In-struct computed local emission: pops one value off the
    /// stack and emits a zero-length child node carrying the bound
    /// value, plus binds the name in the surrounding struct's
    /// scope. The AST walker emits this exact shape so member
    /// access on the parent struct (`outer.computed`) finds the
    /// binding. At top level (no enclosing struct) the binding is
    /// the only side effect -- `Op::StoreIdent` (already in the
    /// scaffold) handles that case directly.
    EmitComputedLocal { name: IdentId, ty_label: IdentId },

    /// Generic fallback read: dispatches into AST `read_scalar`
    /// with whatever type the program holds at [`TypeId`]. Used
    /// when the compile pass can't match a faster specialised op
    /// (e.g. the field type is a templated `using` alias, a
    /// namespace-qualified path, or any decl shape we don't yet
    /// pre-register as `EnterStruct` / `ReadEnum`). Loses the
    /// flat-op-stream perf win for those fields -- they walk the
    /// same AST nodes the AST runner does -- but lets the
    /// surrounding template compile end-to-end.
    ReadAny { ty: TypeId, name: IdentId },
    /// Same fallback shape for arrays. Pops a count off the stack;
    /// the AST `read_array` host helper handles char-as-string,
    /// per-element struct reads, EOF clamping, etc.
    ReadAnyArrayDyn { ty: TypeId, name: IdentId },
    /// Fixed-count fallback array.
    ReadAnyArrayFixed { ty: TypeId, name: IdentId, count: u64 },

    // ---- cursor save/restore ----
    SaveCursor,
    RestoreCursor,
    SeekTo, // offset on stack

    /// Top-level placement (`Type x @ offset;`). Pops one [`Value`]
    /// from the operand stack, seeks the cursor there, AND attaches
    /// an `hxy_placement` attribute to the next read so the renderer
    /// can show "this field was placed at N". Mirrors the AST's
    /// `all_attrs.push(("hxy_placement", offset.to_string()))` from
    /// `exec_field_decl`. Inside-struct placement (which save+
    /// restores the cursor) is a separate op the compile pass does
    /// not yet emit.
    PlacementSeek,

    // ---- struct/scope frames ----
    /// Read a compiled struct body. The VM pushes a struct node,
    /// pushes a fresh scope and `this_stack` entry, executes the
    /// body referenced by [`BodyId`], then closes the struct node
    /// (computing its length from the cursor delta).
    EnterStruct { body: BodyId, name: IdentId, display_name: IdentId },
    ExitStruct,
    PushScope,
    PopScope,

    // ---- control flow ----
    Jump(Pc),
    JumpIfFalse(Pc),
    JumpIfTrue(Pc),
    Break,
    Continue,
    Return,

    // ---- error containment ----
    EnterTry(Pc),
    ExitTry,

    // ---- diagnostic ----
    Diag(Severity, StrId),
}

impl Op {
    /// Static name for the variant. Used by the VM's "not yet
    /// supported" diagnostic so we get a stable string without
    /// formatting the operand payload.
    pub fn variant_name(&self) -> &'static str {
        match self {
            Op::PushInt(_) => "PushInt",
            Op::PushFloat(_) => "PushFloat",
            Op::PushStr(_) => "PushStr",
            Op::PushBool(_) => "PushBool",
            Op::PushChar(_) => "PushChar",
            Op::PushVoid => "PushVoid",
            Op::Pop => "Pop",
            Op::Dup => "Dup",
            Op::LoadIdent(_) => "LoadIdent",
            Op::StoreIdent(_) => "StoreIdent",
            Op::LoadCursor => "LoadCursor",
            Op::StoreCursor => "StoreCursor",
            Op::BinOp(_) => "BinOp",
            Op::UnOp(_) => "UnOp",
            Op::Ternary => "Ternary",
            Op::Member(_) => "Member",
            Op::Index => "Index",
            Op::Call { .. } => "Call",
            Op::Reflect(_) => "Reflect",
            Op::ReadPrim { .. } => "ReadPrim",
            Op::ReadStruct { .. } => "ReadStruct",
            Op::ReadEnum { .. } => "ReadEnum",
            Op::ReadArrayFixed { .. } => "ReadArrayFixed",
            Op::ReadArrayDyn { .. } => "ReadArrayDyn",
            Op::PushAttrs(_) => "PushAttrs",
            Op::ReadAny { .. } => "ReadAny",
            Op::ReadAnyArrayDyn { .. } => "ReadAnyArrayDyn",
            Op::ReadAnyArrayFixed { .. } => "ReadAnyArrayFixed",
            Op::EmitComputedLocal { .. } => "EmitComputedLocal",
            Op::ReadCharArr { .. } => "ReadCharArr",
            Op::ReadDynArr { .. } => "ReadDynArr",
            Op::SaveCursor => "SaveCursor",
            Op::RestoreCursor => "RestoreCursor",
            Op::SeekTo => "SeekTo",
            Op::PlacementSeek => "PlacementSeek",
            Op::EnterStruct { .. } => "EnterStruct",
            Op::ExitStruct => "ExitStruct",
            Op::PushScope => "PushScope",
            Op::PopScope => "PopScope",
            Op::Jump(_) => "Jump",
            Op::JumpIfFalse(_) => "JumpIfFalse",
            Op::JumpIfTrue(_) => "JumpIfTrue",
            Op::Break => "Break",
            Op::Continue => "Continue",
            Op::Return => "Return",
            Op::EnterTry(_) => "EnterTry",
            Op::ExitTry => "ExitTry",
            Op::Diag(_, _) => "Diag",
        }
    }
}

/// A compiled template. Built once per source program; reused
/// across runs against different fixtures.
#[derive(Debug, Default)]
pub struct Program {
    /// Top-level (entry-point) op stream. Walked once per run.
    pub ops: Vec<Op>,
    pub idents: InternTable,
    pub strings: InternTable,
    /// Resolved-type table. Indexed by [`TypeId`]. Each entry is the
    /// AST [`TypeRef`] that the corresponding op needs at runtime.
    /// Kept as `TypeRef` (not a flattened `ResolvedType`) so the VM
    /// can hand it straight to the existing `read_scalar` /
    /// `lookup_type` helpers without a parallel resolution pass.
    pub types: Vec<TypeRef>,
    /// Compiled struct bodies, indexed by [`BodyId`]. `EnterStruct`
    /// dispatches into one of these. Held out-of-line (rather than
    /// inlined into [`Self::ops`]) so a struct can be referenced
    /// from multiple sites without duplicating the body, and so a
    /// recursive struct can refer to its own `BodyId` cleanly.
    pub struct_bodies: Vec<StructBody>,
    /// Top-level enum decls registered by the compile pass, indexed
    /// by [`EnumId`]. The VM hands the corresponding [`crate::ast::EnumDecl`]
    /// straight to `Interpreter::read_enum`, so behaviour matches
    /// the AST walker (variant-value expressions, struct-backed
    /// vint-style enums, etc.) for free.
    pub enum_decls: Vec<crate::ast::EnumDecl>,
    /// Pre-baked attribute lists, indexed by [`AttrListId`]. Each
    /// entry is the (key, value) pairs for one `[[attr, ...]]`
    /// list on a field decl. The compile pass dedups by structural
    /// equality so identical attr lists share a slot.
    pub attr_lists: Vec<Vec<(String, String)>>,
    /// `using A = B;` aliases the compile pass discovered. The
    /// VM registers each into `Interpreter::types` so AST helpers
    /// (`read_array` element loop, `read_enum` backing-type lookup)
    /// can chase the alias the same way the AST walker would.
    pub using_aliases: Vec<(String, TypeRef)>,
    /// Every decl reached during compile (main + imports +
    /// namespaces) that the *AST fallback* code paths
    /// (`Op::ReadAny`, etc.) might need at run time. Pre-registered
    /// into `Interpreter::types` / `functions` at VM entry so name
    /// lookups inside `read_scalar` / `read_array` / `eval` resolve
    /// the same way the AST walker would. The bytecode-fast-path
    /// tables (`struct_bodies`, `enum_decls`, `using_aliases`) are
    /// a subset of these -- the compile pass populates both for
    /// the names it could lower.
    pub ast_decls: AstDecls,
}

/// AST-shaped decl bundle pre-collected for the AST fallback paths
/// (`Op::ReadAny`, `Op::ReadAnyArray*`). Each entry carries the
/// names it should be registered under -- either just the bare
/// name (top-level) or the bare + namespace-qualified pair (the
/// AST walker registers both, see `Interpreter::register_type`).
#[derive(Debug, Default)]
pub struct AstDecls {
    pub structs: Vec<NamedDecl<crate::ast::StructDecl>>,
    pub enums: Vec<NamedDecl<crate::ast::EnumDecl>>,
    pub bitfields: Vec<NamedDecl<crate::ast::BitfieldDecl>>,
    pub functions: Vec<NamedDecl<crate::ast::FunctionDef>>,
    /// `(register_under, template_params, target)`. The compile
    /// pass tracks `template_params` separately so the VM can
    /// build a `TypeDef::Alias { params, target }` -- templated
    /// aliases need params for the AST's substitution path.
    pub aliases: Vec<NamedDecl<UsingAliasInfo>>,
}

/// One AST decl plus the names it should register under. The bare
/// source name is in `bare`; `qualified` carries every namespace-
/// prefixed spelling (`bencode::Foo`, `bencode::utils::Bar`, ...)
/// the AST walker would have registered.
#[derive(Debug)]
pub struct NamedDecl<T> {
    pub bare: String,
    pub qualified: Vec<String>,
    pub decl: T,
}

#[derive(Debug, Clone)]
pub struct UsingAliasInfo {
    pub template_params: Vec<String>,
    pub target: TypeRef,
}

/// One compiled struct body plus the metadata the VM needs to set
/// up the struct node before running it.
#[derive(Debug)]
pub struct StructBody {
    /// The op stream for the struct's field reads, in source order.
    pub ops: Vec<Op>,
    /// The AST display name (`Foo` from `struct Foo { ... }`). The
    /// VM stamps this onto the emitted [`crate::interp::NodeOut`]
    /// so [`crate::value::NodeType::StructType`] carries the
    /// human-readable name the renderer expects.
    pub display_name: IdentId,
    /// The original AST decl. Pre-registered into
    /// `Interpreter::types` at VM entry so AST-style helpers
    /// (`read_array` for a struct element type, etc.) can resolve
    /// the struct by name. The bytecode `EnterStruct` op continues
    /// to dispatch through `Self::ops` -- this clone exists only
    /// for the AST-side fallback paths.
    pub ast_decl: crate::ast::StructDecl,
}

impl Program {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve an identifier slot.
    pub fn intern_ident(&mut self, s: &str) -> IdentId {
        IdentId(self.idents.intern(s))
    }

    /// Reserve a string-literal slot.
    pub fn intern_str(&mut self, s: &str) -> StrId {
        StrId(self.strings.intern(s))
    }

    /// Append a type reference to the type table. No dedup yet --
    /// the corpus templates rarely repeat the same `TypeRef` shape
    /// verbatim (spans differ), so the table stays small.
    pub fn push_type(&mut self, ty: TypeRef) -> TypeId {
        let id = self.types.len() as u32;
        self.types.push(ty);
        TypeId(id)
    }

    /// Reserve and store an attribute list. Linear-scan dedup keeps
    /// the table small while avoiding a second hashmap.
    pub fn push_attr_list(&mut self, list: Vec<(String, String)>) -> AttrListId {
        if let Some(existing) = self.attr_lists.iter().position(|l| l == &list) {
            return AttrListId(existing as u32);
        }
        let id = self.attr_lists.len() as u32;
        self.attr_lists.push(list);
        AttrListId(id)
    }
}

/// Reasons the compile pass could not lower an AST [`AstProgram`] to
/// a [`Program`]. Each variant names the specific shape we don't yet
/// handle so the bytecode-vs-AST parity test can report exactly what
/// is missing.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum CompileError {
    #[error("unsupported top-level item: {0}")]
    UnsupportedTopItem(&'static str),

    #[error("unsupported statement: {0}")]
    UnsupportedStmt(&'static str),

    #[error("unsupported field decl shape: {reason}")]
    UnsupportedFieldDecl { reason: &'static str },

    #[error("unsupported expression: {0}")]
    UnsupportedExpr(&'static str),
}

/// Compile an AST [`AstProgram`] to a flat [`Program`] with no
/// import resolution. Convenience wrapper around
/// [`compile_with_resolver`] for callers that don't have an
/// import resolver wired up; templates that `import std.*` will
/// fail to lower if they actually use any imported decl.
pub fn compile(ast: &AstProgram) -> Result<Program, CompileError> {
    let resolver = crate::imports::NoImportResolver;
    compile_with_resolver(ast, &resolver)
}

/// Compile an AST [`AstProgram`] to a flat [`Program`], resolving
/// `import a.b.c;` statements via `resolver`. Returns
/// [`CompileError`] for any AST shape that does not yet have an
/// op lowering.
///
/// Three-pass design:
///
/// 1. Recursively collect every struct + enum decl reachable
///    through the *main* AST + every imported AST + every
///    `namespace { ... }` body. Reserve a `BodyId` / `EnumId`
///    for each decl whose shape we know how to lower (no
///    templates, no parent inheritance, no `[[transform]]`).
/// 2. Compile each registered struct body. Imports may have
///    introduced struct types that the main program references,
///    so this pass walks the imported ASTs too.
/// 3. Compile the main program's top-level statements (the
///    actual byte-reading walk). Imported files contribute
///    decls only -- their top-level statements never execute.
pub fn compile_with_resolver(
    ast: &AstProgram,
    resolver: &dyn crate::imports::ImportResolver,
) -> Result<Program, CompileError> {
    let mut builder = Compiler {
        p: Program::new(),
        ctx: CompileCtx::default(),
        resolver,
        seen_imports: rustc_hash::FxHashSet::default(),
        namespace_stack: Vec::new(),
    };

    // Pass 1: collect decls from the main AST and every transitively
    // imported AST.
    builder.collect_decls(&ast.items)?;

    // Reset the seen-imports set so pass 2 walks the same imports
    // again to find their struct bodies. Imports are deduped by
    // path, so re-walking them is a no-op in `seen_imports` terms;
    // we just need every imported AST visited once per pass.
    builder.seen_imports.clear();
    builder.compile_struct_bodies(&ast.items)?;

    builder.seen_imports.clear();
    builder.compile_top_items(&ast.items)?;

    Ok(builder.p)
}

/// Compile-pass state. Owns the in-progress [`Program`] plus the
/// auxiliary maps + resolver.
struct Compiler<'r> {
    p: Program,
    ctx: CompileCtx,
    resolver: &'r dyn crate::imports::ImportResolver,
    /// Joined `a::b::c` paths for every import we've already
    /// expanded. Mirrors `Interpreter::imported`.
    seen_imports: rustc_hash::FxHashSet<String>,
    /// Active namespace prefix while collecting decls inside a
    /// `namespace foo::bar { ... }` body. Empty at the top level.
    /// Mirrors `Interpreter::namespace_stack`.
    namespace_stack: Vec<String>,
}

impl<'r> Compiler<'r> {
    fn qualified_name(&self, bare: &str) -> Vec<String> {
        if self.namespace_stack.is_empty() {
            return Vec::new();
        }
        vec![format!("{}::{}", self.namespace_stack.join("::"), bare)]
    }
}

impl<'r> Compiler<'r> {
    /// Pass 1: register every struct + enum + using decl reachable
    /// from `items`. Descends into namespaces and imported ASTs.
    /// Top-level fn decls are recorded into `ast_decls.functions`
    /// so the AST fallback path can call them, but no bytecode op
    /// is emitted for the body.
    fn collect_decls(&mut self, items: &[TopItem]) -> Result<(), CompileError> {
        for it in items {
            match it {
                TopItem::Function(f) => {
                    self.p.ast_decls.functions.push(NamedDecl {
                        bare: f.name.clone(),
                        qualified: self.qualified_name(&f.name),
                        decl: f.clone(),
                    });
                }
                TopItem::Stmt(s) => self.collect_decls_from_stmt(s)?,
            }
        }
        Ok(())
    }

    fn collect_decls_from_stmt(&mut self, s: &Stmt) -> Result<(), CompileError> {
        match s {
            Stmt::StructDecl(decl) => {
                self.p.ast_decls.structs.push(NamedDecl {
                    bare: decl.name.clone(),
                    qualified: self.qualified_name(&decl.name),
                    decl: decl.clone(),
                });
                if struct_is_simple(decl) && !self.ctx.struct_bodies.contains_key(&decl.name) {
                    let display_name = self.p.intern_ident(&decl.name);
                    let body_id = BodyId(self.p.struct_bodies.len() as u32);
                    self.p.struct_bodies.push(StructBody {
                        ops: Vec::new(),
                        display_name,
                        ast_decl: decl.clone(),
                    });
                    self.ctx.struct_bodies.insert(decl.name.clone(), body_id);
                }
                Ok(())
            }
            Stmt::EnumDecl(decl) => {
                self.p.ast_decls.enums.push(NamedDecl {
                    bare: decl.name.clone(),
                    qualified: self.qualified_name(&decl.name),
                    decl: decl.clone(),
                });
                if enum_is_simple(decl) && !self.ctx.enums.contains_key(&decl.name) {
                    let id = EnumId(self.p.enum_decls.len() as u32);
                    self.p.enum_decls.push(decl.clone());
                    self.ctx.enums.insert(decl.name.clone(), id);
                }
                Ok(())
            }
            Stmt::BitfieldDecl(decl) => {
                self.p.ast_decls.bitfields.push(NamedDecl {
                    bare: decl.name.clone(),
                    qualified: self.qualified_name(&decl.name),
                    decl: decl.clone(),
                });
                Ok(())
            }
            Stmt::UsingAlias { new_name, template_params, source, .. } => {
                self.p.ast_decls.aliases.push(NamedDecl {
                    bare: new_name.clone(),
                    qualified: self.qualified_name(new_name),
                    decl: UsingAliasInfo {
                        template_params: template_params.clone(),
                        target: source.clone(),
                    },
                });
                // Bytecode-fast-path alias chase only handles the
                // bare `using A = B;` form. Templated aliases
                // (`using Foo<T> = Bar<T>;`) still register into
                // the AST fallback list above so the AST runner
                // can substitute when read_scalar walks them.
                if template_params.is_empty()
                    && source.template_args.is_empty()
                    && !self.ctx.aliases.contains_key(new_name)
                {
                    self.ctx.aliases.insert(new_name.clone(), source.clone());
                    self.p.using_aliases.push((new_name.clone(), source.clone()));
                }
                Ok(())
            }
            Stmt::FnDecl(f) => {
                self.p.ast_decls.functions.push(NamedDecl {
                    bare: f.name.clone(),
                    qualified: self.qualified_name(&f.name),
                    decl: f.clone(),
                });
                Ok(())
            }
            Stmt::Namespace { path, body, .. } => {
                for seg in path {
                    self.namespace_stack.push(seg.clone());
                }
                for s in body {
                    self.collect_decls_from_stmt(s)?;
                }
                for _ in path {
                    self.namespace_stack.pop();
                }
                Ok(())
            }
            Stmt::Import { path, .. } => {
                self.expand_import_for_collect(path);
                Ok(())
            }
            // Other statement shapes (FieldDecl, If, While, ...)
            // do not introduce decls; ignore them in pass 1.
            _ => Ok(()),
        }
    }

    /// Best-effort import expansion: a missing or malformed import
    /// is dropped silently (the AST walker behaves the same way --
    /// it surfaces a diagnostic, not a hard error). The compile
    /// pass will still fail loudly if the missing decl is then
    /// referenced by a field decl.
    fn expand_import_for_collect(&mut self, path: &[String]) {
        if path.is_empty() {
            return;
        }
        let key = path.join("::");
        if !self.seen_imports.insert(key) {
            return;
        }
        let Some(source) = self.resolver.resolve(path) else {
            return;
        };
        let Ok(tokens) = crate::tokenize(&source) else { return };
        let Ok(program) = crate::parse(tokens) else { return };
        let _ = self.collect_decls(&program.items);
    }

    /// Pass 2: compile every registered struct body. Walks the
    /// main AST + imported ASTs since either may declare a
    /// struct that the main program references.
    fn compile_struct_bodies(&mut self, items: &[TopItem]) -> Result<(), CompileError> {
        for it in items {
            if let TopItem::Stmt(s) = it {
                self.compile_struct_bodies_from_stmt(s)?;
            }
        }
        Ok(())
    }

    fn compile_struct_bodies_from_stmt(&mut self, s: &Stmt) -> Result<(), CompileError> {
        match s {
            Stmt::StructDecl(decl) => {
                if let Some(&body_id) = self.ctx.struct_bodies.get(&decl.name) {
                    let mut body_ops = Vec::new();
                    let mut body_ok = true;
                    for s in &decl.body {
                        if let Err(_e) =
                            compile_struct_body_stmt(&mut self.p, &self.ctx, &mut body_ops, s)
                        {
                            // Body uses something the bytecode VM
                            // can't lower (a control-flow stmt, a
                            // computed local, an unsupported attr).
                            // Drop the struct from the fast path:
                            // the AST fallback (`Op::ReadAny`) at
                            // any caller will pick it up via the
                            // pre-registered ast_decl. Keeping the
                            // empty body would silently emit a no-
                            // op struct read.
                            body_ok = false;
                            break;
                        }
                    }
                    if body_ok {
                        self.p.struct_bodies[body_id.0 as usize].ops = body_ops;
                    } else {
                        // Mark the body as "unusable" by zeroing
                        // its name entry so subsequent field decls
                        // pick the fallback path.
                        self.ctx.struct_bodies.remove(&decl.name);
                    }
                }
                Ok(())
            }
            Stmt::Namespace { body, .. } => {
                for s in body {
                    self.compile_struct_bodies_from_stmt(s)?;
                }
                Ok(())
            }
            Stmt::Import { path, .. } => {
                self.expand_import_for_struct_bodies(path);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn expand_import_for_struct_bodies(&mut self, path: &[String]) {
        if path.is_empty() {
            return;
        }
        let key = path.join("::");
        if !self.seen_imports.insert(key) {
            return;
        }
        let Some(source) = self.resolver.resolve(path) else {
            return;
        };
        let Ok(tokens) = crate::tokenize(&source) else { return };
        let Ok(program) = crate::parse(tokens) else { return };
        let _ = self.compile_struct_bodies(&program.items);
    }

    /// Pass 3: compile the *main* program's top-level statements
    /// (the actual cursor walk). Imported files contribute decls
    /// only. Top-level fn decls were already collected in pass 1
    /// (into `ast_decls.functions`); their bodies have no
    /// bytecode lowering and they emit no top-level op.
    fn compile_top_items(&mut self, items: &[TopItem]) -> Result<(), CompileError> {
        for it in items {
            match it {
                TopItem::Function(_) => {} // already collected in pass 1
                TopItem::Stmt(s) => self.compile_top_stmt_dispatch(s)?,
            }
        }
        Ok(())
    }

    fn compile_top_stmt_dispatch(&mut self, s: &Stmt) -> Result<(), CompileError> {
        match s {
            Stmt::StructDecl(_)
            | Stmt::EnumDecl(_)
            | Stmt::BitfieldDecl(_)
            | Stmt::UsingAlias { .. }
            | Stmt::FnDecl(_) => Ok(()),
            Stmt::Namespace { body, .. } => {
                // Namespaces in the main program may carry top-
                // level statements (the bencode pattern wraps
                // most of its work in `namespace bencode { ... }`).
                // Walk into them; nested decls were already
                // collected in pass 1.
                for s in body {
                    self.compile_top_stmt_dispatch(s)?;
                }
                Ok(())
            }
            Stmt::Block { stmts, .. } => {
                // A bare `{ ... }` at top level just sequences
                // its inner statements. The AST walker handles
                // this in `exec_stmt`.
                for s in stmts {
                    self.compile_top_stmt_dispatch(s)?;
                }
                Ok(())
            }
            Stmt::Import { .. } => Ok(()),
            other => compile_top_stmt(&mut self.p, &self.ctx, other),
        }
    }
}

/// Enum is "simple" when the compile pass + VM can handle it
/// without extra plumbing. Reject template params and any attrs
/// (`[[transform(...)]]`-style attrs change runtime behaviour and
/// would need a dedicated lowering).
fn enum_is_simple(decl: &crate::ast::EnumDecl) -> bool {
    decl.template_params.is_empty() && decl.attrs.0.is_empty()
}

/// Side state the compile pass threads through its passes. Maps a
/// struct's source-level name to its reserved [`BodyId`] so field
/// decls can resolve struct types without a runtime name lookup.
/// Same idea for enum names -> [`EnumId`]; `using` aliases are
/// recorded as a single hop into another type-name (resolution
/// chases the chain in [`Self::resolve_alias`]).
#[derive(Default)]
struct CompileCtx {
    struct_bodies: rustc_hash::FxHashMap<String, BodyId>,
    enums: rustc_hash::FxHashMap<String, EnumId>,
    /// `using Foo = Bar;` -> `("Foo", TypeRef("Bar"))`. Aliases
    /// chase recursively in [`Self::resolve_alias`] so `using A =
    /// B; using B = u32; A x;` lowers to a u32 read.
    aliases: rustc_hash::FxHashMap<String, TypeRef>,
}

impl CompileCtx {
    /// Walk through any chain of `using` aliases pointing at this
    /// type. Returns the final un-aliased [`TypeRef`]. Cycles are
    /// broken by a depth cap (corpus aliases are at most a couple
    /// of hops; runaway cycles in user input shouldn't crash the
    /// compile pass).
    fn resolve_alias<'a>(&'a self, ty: &'a TypeRef) -> std::borrow::Cow<'a, TypeRef> {
        let mut current = std::borrow::Cow::Borrowed(ty);
        for _ in 0..16 {
            if current.path.len() != 1 || !current.template_args.is_empty() {
                return current;
            }
            match self.aliases.get(&current.path[0]) {
                Some(target) => current = std::borrow::Cow::Owned(target.clone()),
                None => return current,
            }
        }
        current
    }
}

/// True when a struct decl matches the narrow shape Phase B can
/// lower: no template params, no parent, no `[[attrs]]`, not a
/// union, and the body is purely sequential primitive field reads
/// (checked further inside [`compile_struct_body_stmt`]). Restrict
/// here so [`compile`] can pre-register only the structs it can
/// actually handle.
fn struct_is_simple(decl: &crate::ast::StructDecl) -> bool {
    decl.template_params.is_empty()
        && decl.parent.is_none()
        && decl.attrs.0.is_empty()
        && !decl.is_union
}

fn compile_struct_body_stmt(
    p: &mut Program,
    ctx: &CompileCtx,
    out: &mut Vec<Op>,
    stmt: &Stmt,
) -> Result<(), CompileError> {
    match stmt {
        Stmt::FieldDecl {
            is_const,
            ty,
            name,
            array,
            placement,
            init,
            attrs,
            is_io_var,
            pointer_width,
            ..
        } => {
            field_decl_must_be_plain_array_and_placement_ok(pointer_width, attrs)?;
            // In-struct placement saves+restores the surrounding
            // cursor; the VM does not yet model that. Only the
            // top-level form (which is a plain seek) is lowered.
            if placement.is_some() {
                return Err(CompileError::UnsupportedFieldDecl {
                    reason: "in-struct `@ offset` placement (save+restore) not yet lowered",
                });
            }
            // Computed local / const / io-var: bind a value but
            // don't read from the source. Inside a struct body we
            // additionally emit a zero-length child node so outer
            // member access (`outer.computed`) can find the
            // binding.
            if let Some(e) = compute_local_init(*is_const, *is_io_var, init.as_ref(), array.as_ref()) {
                compile_expr(p, out, e)?;
                let name_id = p.intern_ident(name);
                let ty_label = p.intern_ident(ty.leaf());
                out.push(Op::EmitComputedLocal { name: name_id, ty_label });
                return Ok(());
            }
            if compute_local_void(*is_io_var, init.as_ref(), array.as_ref()) {
                // `bool x in;` with no initializer -- bind Void.
                out.push(Op::PushVoid);
                let name_id = p.intern_ident(name);
                out.push(Op::StoreIdent(name_id));
                return Ok(());
            }
            push_attr_list_op(p, out, attrs);
            compile_field_decl(p, ctx, out, ty, name, array.as_ref())
        }
        _ => Err(CompileError::UnsupportedStmt("non-field-decl in struct body")),
    }
}

/// True when this field decl is a "computed local" -- something
/// the AST walker handles by evaluating an initializer (or a
/// value placeholder) and binding into scope without reading from
/// the source. Returns the expression to evaluate.
fn compute_local_init<'a>(
    is_const: bool,
    is_io_var: bool,
    init: Option<&'a crate::ast::Expr>,
    array: Option<&'a crate::ast::ArraySize>,
) -> Option<&'a crate::ast::Expr> {
    if array.is_some() {
        return None;
    }
    if (is_const && init.is_some())
        || (is_io_var && init.is_some())
        || (init.is_some() && !is_io_var)
    {
        return init;
    }
    None
}

/// `bool x in;` and similar: io-var with no initializer. Bind
/// `Void` in scope so subsequent expressions don't fail with
/// "undefined name". The AST walker does the same.
fn compute_local_void(
    is_io_var: bool,
    init: Option<&crate::ast::Expr>,
    array: Option<&crate::ast::ArraySize>,
) -> bool {
    is_io_var && init.is_none() && array.is_none()
}

/// If `attrs` contains decorative pairs, intern them and emit a
/// `PushAttrs` op so the next read picks them up. Empty attr lists
/// are skipped to keep the op stream tight.
fn push_attr_list_op(p: &mut Program, out: &mut Vec<Op>, attrs: &crate::ast::Attrs) {
    if attrs.0.is_empty() {
        return;
    }
    let pairs = ast_attrs_to_pairs(attrs);
    if pairs.is_empty() {
        return;
    }
    let id = p.push_attr_list(pairs);
    out.push(Op::PushAttrs(id));
}

fn compile_top_stmt(p: &mut Program, ctx: &CompileCtx, stmt: &Stmt) -> Result<(), CompileError> {
    match stmt {
        Stmt::FieldDecl {
            is_const,
            ty,
            name,
            array,
            placement,
            init,
            attrs,
            is_io_var,
            pointer_width,
            ..
        } => {
            field_decl_must_be_plain_array_and_placement_ok(pointer_width, attrs)?;
            // Borrow the top-level op stream by moving it out, then
            // restoring -- avoids fighting the borrow checker over
            // an `&mut Vec<Op>` aliased with `&mut Program`.
            let mut top_ops = std::mem::take(&mut p.ops);
            let res = (|| -> Result<(), CompileError> {
                // Computed local / const / io-var at top level:
                // bind in scope, no source read, no node emission.
                if let Some(e) =
                    compute_local_init(*is_const, *is_io_var, init.as_ref(), array.as_ref())
                {
                    compile_expr(p, &mut top_ops, e)?;
                    let name_id = p.intern_ident(name);
                    top_ops.push(Op::StoreIdent(name_id));
                    return Ok(());
                }
                if compute_local_void(*is_io_var, init.as_ref(), array.as_ref()) {
                    top_ops.push(Op::PushVoid);
                    let name_id = p.intern_ident(name);
                    top_ops.push(Op::StoreIdent(name_id));
                    return Ok(());
                }
                // Decorative attrs first so they sit in the pending
                // buffer; the AST walker prepends them to the
                // node's attr list before adding `hxy_placement`.
                push_attr_list_op(p, &mut top_ops, attrs);
                if let Some(offset_expr) = placement {
                    // Top-level placement is a plain seek (no
                    // save+restore); lower the offset, push the
                    // PlacementSeek op which seeks AND queues the
                    // `hxy_placement` attr for the next read.
                    compile_expr(p, &mut top_ops, offset_expr)?;
                    top_ops.push(Op::PlacementSeek);
                }
                compile_field_decl(p, ctx, &mut top_ops, ty, name, array.as_ref())
            })();
            p.ops = top_ops;
            res
        }
        Stmt::StructDecl(_) => unreachable!("handled by compile()"),
        Stmt::EnumDecl(_) => Err(CompileError::UnsupportedStmt("enum decl")),
        Stmt::BitfieldDecl(_) => Err(CompileError::UnsupportedStmt("bitfield decl")),
        Stmt::UsingAlias { .. } => Err(CompileError::UnsupportedStmt("using alias")),
        Stmt::FnDecl(_) => Err(CompileError::UnsupportedStmt("nested fn decl")),
        Stmt::Namespace { .. } => Err(CompileError::UnsupportedStmt("namespace")),
        Stmt::Import { .. } => Err(CompileError::UnsupportedStmt("import")),
        Stmt::Block { .. } => Err(CompileError::UnsupportedStmt("block")),
        Stmt::Expr { .. } => Err(CompileError::UnsupportedStmt("expr stmt")),
        Stmt::If { .. } => Err(CompileError::UnsupportedStmt("if")),
        Stmt::While { .. } => Err(CompileError::UnsupportedStmt("while")),
        Stmt::For { .. } => Err(CompileError::UnsupportedStmt("for")),
        Stmt::Match { .. } => Err(CompileError::UnsupportedStmt("match")),
        Stmt::TryBlock { .. } => Err(CompileError::UnsupportedStmt("try")),
        Stmt::Return { .. } => Err(CompileError::UnsupportedStmt("return")),
        Stmt::Break { .. } => Err(CompileError::UnsupportedStmt("break")),
        Stmt::Continue { .. } => Err(CompileError::UnsupportedStmt("continue")),
        Stmt::BitfieldField { .. } => Err(CompileError::UnsupportedStmt("bitfield field")),
    }
}

/// Reject every `FieldDecl` modifier outside the small subset the
/// VM currently implements. Placement (`@ offset`) and the array
/// shape are *not* checked here -- the per-context compile callers
/// validate them. `init` / `is_const` / `is_io_var` (computed
/// locals, constants, host-supplied vars) are NOT rejected here
/// either: the caller routes them into a separate lowering path
/// that emits expression ops + a store, mirroring what the AST
/// `exec_field_decl` does.
fn field_decl_must_be_plain_array_and_placement_ok(
    pointer_width: &Option<TypeRef>,
    attrs: &crate::ast::Attrs,
) -> Result<(), CompileError> {
    if pointer_width.is_some() {
        return Err(CompileError::UnsupportedFieldDecl { reason: "pointer" });
    }
    field_attrs_must_be_decorative(attrs)?;
    Ok(())
}

/// Allow attrs that the AST walker treats as pure pass-through to
/// the emitted node (`name`, `comment`, `format`, `sealed`,
/// `hidden`, `inline`, `color`, `single_color`, `bitfield_order`,
/// `right_to_left`, `static`). Reject `transform` and
/// `no_unique_address` -- those change cursor or value behaviour
/// and need dedicated lowering before the bytecode VM can match
/// the AST's results.
fn field_attrs_must_be_decorative(attrs: &crate::ast::Attrs) -> Result<(), CompileError> {
    for a in &attrs.0 {
        if !is_decorative_attr_name(&a.name) {
            return Err(CompileError::UnsupportedFieldDecl {
                reason: "field carries a behaviour-changing attr (transform / no_unique_address / unknown)",
            });
        }
        // Defensive: we only know how to format literal-shaped attr
        // arguments. A `[[name(std::format("Channel: {}", this))]]`
        // would need expression eval; reject so we don't silently
        // serialise the wrong string into the attr.
        for arg in &a.args {
            if !is_format_friendly_attr_arg(arg) {
                return Err(CompileError::UnsupportedFieldDecl {
                    reason: "decorative attr argument is non-literal (needs eval)",
                });
            }
        }
    }
    Ok(())
}

fn is_decorative_attr_name(name: &str) -> bool {
    matches!(
        name,
        "name"
            | "comment"
            | "format"
            | "format_read"
            | "format_write"
            | "format_entries"
            | "sealed"
            | "hidden"
            | "inline"
            | "color"
            | "single_color"
            | "bitfield_order"
            | "right_to_left"
            | "left_to_right"
            | "static"
            | "export"
            | "highlight_hidden"
    )
}

fn is_format_friendly_attr_arg(e: &crate::ast::Expr) -> bool {
    matches!(
        e,
        crate::ast::Expr::IntLit { .. }
            | crate::ast::Expr::StringLit { .. }
            | crate::ast::Expr::BoolLit { .. }
            | crate::ast::Expr::Ident { .. }
            | crate::ast::Expr::CharLit { .. }
    )
}

/// Mirror the AST's `attrs_to_pairs` helper so the bytecode path
/// emits identical (key, value) lists. Kept inline (rather than
/// reused from `interp`) because the AST helper is a private
/// `fn`; lifting it out is a follow-up.
fn ast_attrs_to_pairs(attrs: &crate::ast::Attrs) -> Vec<(String, String)> {
    attrs
        .0
        .iter()
        .map(|a| {
            let value = a.args.first().map(format_attr_arg).unwrap_or_default();
            (a.name.clone(), value)
        })
        .collect()
}

fn format_attr_arg(e: &crate::ast::Expr) -> String {
    match e {
        crate::ast::Expr::IntLit { value, .. } => value.to_string(),
        crate::ast::Expr::StringLit { value, .. } => value.clone(),
        crate::ast::Expr::BoolLit { value, .. } => value.to_string(),
        crate::ast::Expr::Ident { name, .. } => name.clone(),
        // Other shapes are blocked at the gate, but be defensive.
        other => format!("{:?}", other.span()),
    }
}

/// Lower a single `FieldDecl` (already gated by
/// [`field_decl_must_be_plain_array_and_placement_ok`]) into the
/// right op. Shared between top-level and struct-body callers
/// since the shape rules are identical.
fn compile_field_decl(
    p: &mut Program,
    ctx: &CompileCtx,
    out: &mut Vec<Op>,
    ty: &TypeRef,
    name: &str,
    array: Option<&crate::ast::ArraySize>,
) -> Result<(), CompileError> {
    let resolved = ctx.resolve_alias(ty);
    let ty = resolved.as_ref();
    match array {
        None => {
            if is_known_primitive(ty) {
                let name_id = p.intern_ident(name);
                let ty_id = p.push_type(ty.clone());
                out.push(Op::ReadPrim { ty: ty_id, name: name_id });
            } else if let Some(body_id) = ctx.struct_bodies.get(ty.leaf()) {
                let name_id = p.intern_ident(name);
                let display = p.struct_bodies[body_id.0 as usize].display_name;
                out.push(Op::EnterStruct { body: *body_id, name: name_id, display_name: display });
            } else if let Some(enum_id) = ctx.enums.get(ty.leaf()) {
                let name_id = p.intern_ident(name);
                out.push(Op::ReadEnum { id: *enum_id, name: name_id });
            } else {
                // Fallback: hand the type ref straight to AST
                // `read_scalar`. Loses the flat-op-stream perf
                // win but keeps the surrounding template
                // compilable. Any actual undefined-name failure
                // surfaces at run time as `RuntimeError::UnknownType`.
                let name_id = p.intern_ident(name);
                let ty_id = p.push_type(ty.clone());
                out.push(Op::ReadAny { ty: ty_id, name: name_id });
            }
            Ok(())
        }
        Some(crate::ast::ArraySize::Fixed(crate::ast::Expr::IntLit { value, .. })) => {
            let name_id = p.intern_ident(name);
            let ty_id = p.push_type(ty.clone());
            if array_element_type_is_lowerable(ty, ctx) {
                out.push(Op::ReadArrayFixed { ty: ty_id, name: name_id, count: *value as u64 });
            } else {
                out.push(Op::ReadAnyArrayFixed { ty: ty_id, name: name_id, count: *value as u64 });
            }
            Ok(())
        }
        Some(crate::ast::ArraySize::Fixed(size_expr)) => {
            compile_expr(p, out, size_expr)?;
            let name_id = p.intern_ident(name);
            let ty_id = p.push_type(ty.clone());
            if array_element_type_is_lowerable(ty, ctx) {
                out.push(Op::ReadArrayDyn { ty: ty_id, name: name_id });
            } else {
                out.push(Op::ReadAnyArrayDyn { ty: ty_id, name: name_id });
            }
            Ok(())
        }
        Some(crate::ast::ArraySize::Open) => Err(CompileError::UnsupportedFieldDecl {
            reason: "open `[]` arrays not yet lowered",
        }),
        Some(crate::ast::ArraySize::While(_)) => Err(CompileError::UnsupportedFieldDecl {
            reason: "`[while(...)]` arrays not yet lowered",
        }),
    }
}

/// Lower a tiny subset of [`crate::ast::Expr`] to ops that leave a
/// single [`crate::value::Value`] on the operand stack. Phase D.1
/// recognises only literal ints and bare identifiers -- enough to
/// size dynamic arrays whose width is a previously-bound field
/// (`u8 length; char value[length];`). Binary ops, member access,
/// and calls are explicit follow-up phases.
fn compile_expr(p: &mut Program, out: &mut Vec<Op>, expr: &crate::ast::Expr) -> Result<(), CompileError> {
    match expr {
        crate::ast::Expr::IntLit { value, .. } => {
            // The AST `eval` produces `Value::UInt { kind: u64 }`
            // regardless of magnitude; widen the literal to i128 so
            // future signed-arithmetic ops have headroom.
            out.push(Op::PushInt(*value as i128));
            Ok(())
        }
        crate::ast::Expr::Ident { name, .. } => {
            let id = p.intern_ident(name);
            out.push(Op::LoadIdent(id));
            Ok(())
        }
        crate::ast::Expr::Binary { op, lhs, rhs, .. } => {
            // Mirror `eval(Expr::Binary)`: lhs first, then rhs,
            // then the BinOp pops both. Short-circuit && and ||
            // need a Jump op (out of scope here); reject them
            // explicitly so the compile error is honest.
            if matches!(op, crate::ast::BinOp::LogicalAnd | crate::ast::BinOp::LogicalOr) {
                return Err(CompileError::UnsupportedExpr("logical && / || (need jumps)"));
            }
            compile_expr(p, out, lhs)?;
            compile_expr(p, out, rhs)?;
            out.push(Op::BinOp(*op));
            Ok(())
        }
        crate::ast::Expr::BoolLit { .. } => Err(CompileError::UnsupportedExpr("bool literal")),
        crate::ast::Expr::FloatLit { .. } => Err(CompileError::UnsupportedExpr("float literal")),
        crate::ast::Expr::StringLit { .. } => Err(CompileError::UnsupportedExpr("string literal")),
        crate::ast::Expr::CharLit { .. } => Err(CompileError::UnsupportedExpr("char literal")),
        crate::ast::Expr::NullLit { .. } => Err(CompileError::UnsupportedExpr("null literal")),
        crate::ast::Expr::Path { .. } => Err(CompileError::UnsupportedExpr("path")),
        crate::ast::Expr::Unary { .. } => Err(CompileError::UnsupportedExpr("unary op")),
        crate::ast::Expr::Call { .. } => Err(CompileError::UnsupportedExpr("call")),
        crate::ast::Expr::Index { .. } => Err(CompileError::UnsupportedExpr("index")),
        crate::ast::Expr::Member { .. } => Err(CompileError::UnsupportedExpr("member")),
        crate::ast::Expr::Assign { .. } => Err(CompileError::UnsupportedExpr("assign")),
        crate::ast::Expr::Ternary { .. } => Err(CompileError::UnsupportedExpr("ternary")),
        crate::ast::Expr::Reflect { .. } => Err(CompileError::UnsupportedExpr("reflect")),
        crate::ast::Expr::TypeRefExpr { .. } => Err(CompileError::UnsupportedExpr("type-ref expr")),
    }
}

/// True when `ty` is a type the bytecode VM can dispatch through
/// `read_array`'s per-element loop. Primitives go through the
/// AST's existing primitive read path; struct / enum names are
/// resolved to the AST decl that we pre-register into
/// `Interpreter::types` at VM entry. `using` aliases are chased
/// through to the underlying type before the check.
fn array_element_type_is_lowerable(ty: &TypeRef, ctx: &CompileCtx) -> bool {
    let resolved = ctx.resolve_alias(ty);
    let ty = resolved.as_ref();
    if is_known_primitive(ty) {
        return true;
    }
    if ty.template_args.is_empty() && ty.path.len() == 1 {
        if ctx.struct_bodies.contains_key(&ty.path[0]) {
            return true;
        }
        if ctx.enums.contains_key(&ty.path[0]) {
            return true;
        }
    }
    false
}

/// True when `ty` names a built-in primitive that the AST interpreter
/// registers in `register_primitives` -- i.e. something `read_scalar`
/// can decode without consulting struct / enum / bitfield decls. Also
/// accepts the `uN` / `sN` byte-aligned generic-int spellings.
fn is_known_primitive(ty: &TypeRef) -> bool {
    if !ty.template_args.is_empty() || ty.path.len() != 1 {
        return false;
    }
    let name = ty.leaf();
    matches!(
        name,
        "u8" | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "s8"
            | "s16"
            | "s32"
            | "s64"
            | "s128"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "float"
            | "double"
            | "f32"
            | "f64"
            | "char"
            | "char16"
            | "bool"
    ) || is_generic_int_spelling(name)
}

fn is_generic_int_spelling(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() < 2 {
        return false;
    }
    if bytes[0] != b'u' && bytes[0] != b's' {
        return false;
    }
    let Ok(bits) = name[1..].parse::<u32>() else {
        return false;
    };
    bits != 0 && bits <= 128 && bits.is_multiple_of(8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_table_returns_stable_ids_and_handles_collisions() {
        let mut t = InternTable::default();
        let a = t.intern("foo");
        let b = t.intern("bar");
        let a2 = t.intern("foo");
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_eq!(t.get(a), "foo");
        assert_eq!(t.get(b), "bar");
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn ident_and_str_id_spaces_are_independent() {
        // A `Program` keeps two intern tables. The same string lives
        // independently in each, with its own u32 id space; we don't
        // want a stray `StrId(7)` to silently match an `IdentId(7)`.
        let mut p = Program::new();
        let i = p.intern_ident("foo");
        let s = p.intern_str("foo");
        assert_eq!(i.0, 0);
        assert_eq!(s.0, 0);
        // The numeric id can collide; the type wrapper prevents
        // misuse at the compile-pass / VM boundary.
        assert_eq!(p.idents.get(i.0), "foo");
        assert_eq!(p.strings.get(s.0), "foo");
    }

    #[test]
    fn compile_lowers_top_level_primitive_field_to_read_prim() {
        let src = "u8 magic;\nu32 size;\n";
        let tokens = crate::tokenize(src).unwrap();
        let ast = crate::parse(tokens).unwrap();
        let bc = compile(&ast).expect("compile of top-level primitives must succeed");
        assert_eq!(bc.ops.len(), 2);
        match bc.ops[0] {
            Op::ReadPrim { ty, name } => {
                assert_eq!(bc.idents.get(name.0), "magic");
                assert_eq!(bc.types[ty.0 as usize].leaf(), "u8");
            }
            ref other => panic!("expected ReadPrim, got {other:?}"),
        }
        match bc.ops[1] {
            Op::ReadPrim { ty, name } => {
                assert_eq!(bc.idents.get(name.0), "size");
                assert_eq!(bc.types[ty.0 as usize].leaf(), "u32");
            }
            ref other => panic!("expected ReadPrim, got {other:?}"),
        }
    }

    #[test]
    fn compile_lowers_simple_struct_to_enter_struct() {
        let src = "struct S { u8 a; u32 b; }; S s;\n";
        let tokens = crate::tokenize(src).unwrap();
        let ast = crate::parse(tokens).unwrap();
        let bc = compile(&ast).expect("simple struct must lower");
        assert_eq!(bc.struct_bodies.len(), 1);
        assert_eq!(bc.struct_bodies[0].ops.len(), 2);
        assert_eq!(bc.ops.len(), 1);
        match bc.ops[0] {
            Op::EnterStruct { body, name, display_name } => {
                assert_eq!(body.0, 0);
                assert_eq!(bc.idents.get(name.0), "s");
                assert_eq!(bc.idents.get(display_name.0), "S");
            }
            ref other => panic!("expected EnterStruct, got {other:?}"),
        }
    }

    #[test]
    fn compile_template_struct_is_recorded_for_ast_fallback() {
        // Templated structs aren't pre-registered as a fast-path
        // BodyId (the bytecode VM can't substitute template
        // params yet), but they DO land in `ast_decls.structs`
        // so the AST fallback can dispatch them at run time.
        let src = "struct S<T> { T value; }; u8 dummy;\n";
        let tokens = crate::tokenize(src).unwrap();
        let ast = crate::parse(tokens).unwrap();
        let bc = compile(&ast).expect("compile must succeed via fallback path");
        assert!(
            bc.ast_decls.structs.iter().any(|d| d.bare == "S"),
            "templated struct missing from ast_decls.structs",
        );
        assert!(bc.struct_bodies.is_empty(), "templated struct should not get a fast-path BodyId");
    }
}
