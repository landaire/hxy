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

/// Compile an AST [`AstProgram`] to a flat [`Program`]. Returns
/// [`CompileError`] for any AST shape that does not yet have an op
/// lowering -- the caller (parity tests, future opt-in runner) can
/// fall back to the AST interpreter on failure.
///
/// Two-pass design:
///
/// 1. Pre-register simple struct decls (no template params, no
///    parent inheritance, no attrs, no union, body is sequential
///    primitive [`Stmt::FieldDecl`]s) so a top-level field can
///    refer to its `BodyId` before the body has been compiled.
/// 2. Compile each registered body and the top-level statements.
///
/// Anything outside the supported subset becomes a
/// [`CompileError`].
pub fn compile(ast: &AstProgram) -> Result<Program, CompileError> {
    let mut p = Program::new();
    let mut ctx = CompileCtx::default();

    // Pass 1: collect simple top-level structs + enums and reserve
    // their BodyIds / EnumIds. Reserving up-front means a struct
    // or enum referenced before its declaration still resolves
    // without a second AST traversal.
    for it in &ast.items {
        match it {
            TopItem::Stmt(Stmt::StructDecl(decl)) if struct_is_simple(decl) => {
                let display_name = p.intern_ident(&decl.name);
                let body_id = BodyId(p.struct_bodies.len() as u32);
                p.struct_bodies.push(StructBody {
                    ops: Vec::new(),
                    display_name,
                    ast_decl: decl.clone(),
                });
                ctx.struct_bodies.insert(decl.name.clone(), body_id);
            }
            TopItem::Stmt(Stmt::EnumDecl(decl)) if enum_is_simple(decl) => {
                let id = EnumId(p.enum_decls.len() as u32);
                p.enum_decls.push(decl.clone());
                ctx.enums.insert(decl.name.clone(), id);
            }
            _ => {}
        }
    }

    // Pass 2: compile struct bodies + top-level statements.
    for it in &ast.items {
        match it {
            TopItem::Function(_) => {
                return Err(CompileError::UnsupportedTopItem("fn decl"));
            }
            TopItem::Stmt(Stmt::StructDecl(decl)) => {
                let Some(&body_id) = ctx.struct_bodies.get(&decl.name) else {
                    return Err(CompileError::UnsupportedStmt(
                        "struct decl uses an unsupported shape (template, parent, attrs, ...)",
                    ));
                };
                let mut body_ops = Vec::new();
                for s in &decl.body {
                    compile_struct_body_stmt(&mut p, &ctx, &mut body_ops, s)?;
                }
                p.struct_bodies[body_id.0 as usize].ops = body_ops;
            }
            TopItem::Stmt(Stmt::EnumDecl(decl)) => {
                if !ctx.enums.contains_key(&decl.name) {
                    return Err(CompileError::UnsupportedStmt(
                        "enum decl uses an unsupported shape (template, transform attr, ...)",
                    ));
                }
                // Already registered in pass 1; nothing to lower.
            }
            TopItem::Stmt(s) => compile_top_stmt(&mut p, &ctx, s)?,
        }
    }
    Ok(p)
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
/// Same idea for enum names -> [`EnumId`].
#[derive(Default)]
struct CompileCtx {
    struct_bodies: rustc_hash::FxHashMap<String, BodyId>,
    enums: rustc_hash::FxHashMap<String, EnumId>,
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
            field_decl_must_be_plain_array_and_placement_ok(
                *is_const,
                *is_io_var,
                pointer_width,
                init,
                attrs,
            )?;
            // In-struct placement saves+restores the surrounding
            // cursor; the VM does not yet model that. Only the
            // top-level form (which is a plain seek) is lowered.
            if placement.is_some() {
                return Err(CompileError::UnsupportedFieldDecl {
                    reason: "in-struct `@ offset` placement (save+restore) not yet lowered",
                });
            }
            push_attr_list_op(p, out, attrs);
            compile_field_decl(p, ctx, out, ty, name, array.as_ref())
        }
        _ => Err(CompileError::UnsupportedStmt("non-field-decl in struct body")),
    }
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
            field_decl_must_be_plain_array_and_placement_ok(
                *is_const,
                *is_io_var,
                pointer_width,
                init,
                attrs,
            )?;
            // Borrow the top-level op stream by moving it out, then
            // restoring -- avoids fighting the borrow checker over
            // an `&mut Vec<Op>` aliased with `&mut Program`.
            let mut top_ops = std::mem::take(&mut p.ops);
            let res = (|| -> Result<(), CompileError> {
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
/// validate them (top-level vs in-struct rules differ). Attrs are
/// validated separately via [`field_attrs_must_be_decorative`] so
/// the caller can intern the (key, value) pairs after gating.
fn field_decl_must_be_plain_array_and_placement_ok(
    is_const: bool,
    is_io_var: bool,
    pointer_width: &Option<TypeRef>,
    init: &Option<crate::ast::Expr>,
    attrs: &crate::ast::Attrs,
) -> Result<(), CompileError> {
    if is_const {
        return Err(CompileError::UnsupportedFieldDecl { reason: "const" });
    }
    if is_io_var {
        return Err(CompileError::UnsupportedFieldDecl { reason: "io var" });
    }
    if pointer_width.is_some() {
        return Err(CompileError::UnsupportedFieldDecl { reason: "pointer" });
    }
    if init.is_some() {
        return Err(CompileError::UnsupportedFieldDecl { reason: "init" });
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
    match array {
        None => {
            if is_known_primitive(ty) {
                let name_id = p.intern_ident(name);
                let ty_id = p.push_type(ty.clone());
                out.push(Op::ReadPrim { ty: ty_id, name: name_id });
                Ok(())
            } else if let Some(body_id) = ctx.struct_bodies.get(ty.leaf()) {
                let name_id = p.intern_ident(name);
                let display = p.struct_bodies[body_id.0 as usize].display_name;
                out.push(Op::EnterStruct { body: *body_id, name: name_id, display_name: display });
                Ok(())
            } else if let Some(enum_id) = ctx.enums.get(ty.leaf()) {
                let name_id = p.intern_ident(name);
                out.push(Op::ReadEnum { id: *enum_id, name: name_id });
                Ok(())
            } else {
                Err(CompileError::UnsupportedFieldDecl {
                    reason: "field type is unrecognised (not a primitive, registered simple struct, or simple enum)",
                })
            }
        }
        Some(crate::ast::ArraySize::Fixed(crate::ast::Expr::IntLit { value, .. })) => {
            if !array_element_type_is_lowerable(ty, ctx) {
                return Err(CompileError::UnsupportedFieldDecl {
                    reason: "fixed-size array element type is not a primitive, registered struct, or registered enum",
                });
            }
            let name_id = p.intern_ident(name);
            let ty_id = p.push_type(ty.clone());
            out.push(Op::ReadArrayFixed { ty: ty_id, name: name_id, count: *value as u64 });
            Ok(())
        }
        Some(crate::ast::ArraySize::Fixed(size_expr)) => {
            if !array_element_type_is_lowerable(ty, ctx) {
                return Err(CompileError::UnsupportedFieldDecl {
                    reason: "dyn-size array element type is not a primitive, registered struct, or registered enum",
                });
            }
            compile_expr(p, out, size_expr)?;
            let name_id = p.intern_ident(name);
            let ty_id = p.push_type(ty.clone());
            out.push(Op::ReadArrayDyn { ty: ty_id, name: name_id });
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
/// `Interpreter::types` at VM entry.
fn array_element_type_is_lowerable(ty: &TypeRef, ctx: &CompileCtx) -> bool {
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
    fn compile_refuses_struct_with_template_params() {
        let src = "struct S<T> { T value; }; u8 dummy;\n";
        let tokens = crate::tokenize(src).unwrap();
        let ast = crate::parse(tokens).unwrap();
        // Template structs are not pre-registered; the field decl
        // succeeds (it's `u8 dummy;`) but the struct decl pass2
        // refuses since its body shape isn't simple.
        let err = compile(&ast).unwrap_err();
        match err {
            CompileError::UnsupportedStmt(_) => {}
            other => panic!("expected UnsupportedStmt, got {other:?}"),
        }
    }
}
