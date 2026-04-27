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
    ReadArray { ty: TypeId, name: IdentId }, // count on stack
    ReadCharArr { name: IdentId },           // count on stack
    ReadDynArr { ty: TypeId, name: IdentId, pred: Pc, end: Pc },

    // ---- cursor save/restore ----
    SaveCursor,
    RestoreCursor,
    SeekTo, // offset on stack

    // ---- struct/scope frames ----
    EnterStruct { ty: TypeId, name: IdentId },
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
            Op::ReadArray { .. } => "ReadArray",
            Op::ReadCharArr { .. } => "ReadCharArr",
            Op::ReadDynArr { .. } => "ReadDynArr",
            Op::SaveCursor => "SaveCursor",
            Op::RestoreCursor => "RestoreCursor",
            Op::SeekTo => "SeekTo",
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
    pub ops: Vec<Op>,
    pub idents: InternTable,
    pub strings: InternTable,
    /// Resolved-type table. Indexed by [`TypeId`]. Each entry is the
    /// AST [`TypeRef`] that the corresponding op needs at runtime.
    /// Kept as `TypeRef` (not a flattened `ResolvedType`) so the VM
    /// can hand it straight to the existing `read_scalar` /
    /// `lookup_type` helpers without a parallel resolution pass.
    pub types: Vec<TypeRef>,
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
}

/// Compile an AST [`AstProgram`] to a flat [`Program`]. Returns
/// [`CompileError`] for any AST shape that does not yet have an op
/// lowering -- the caller (parity tests, future opt-in runner) can
/// fall back to the AST interpreter on failure.
///
/// This is the *staged* lowering: today only top-level
/// `PrimitiveType name;` field decls compile. Each new op gets
/// added with a paired test in follow-up commits.
pub fn compile(ast: &AstProgram) -> Result<Program, CompileError> {
    let mut p = Program::new();
    for it in &ast.items {
        match it {
            TopItem::Function(_) => {
                return Err(CompileError::UnsupportedTopItem("fn decl"));
            }
            TopItem::Stmt(s) => compile_top_stmt(&mut p, s)?,
        }
    }
    Ok(p)
}

fn compile_top_stmt(p: &mut Program, stmt: &Stmt) -> Result<(), CompileError> {
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
            if *is_const {
                return Err(CompileError::UnsupportedFieldDecl { reason: "const" });
            }
            if *is_io_var {
                return Err(CompileError::UnsupportedFieldDecl { reason: "io var" });
            }
            if pointer_width.is_some() {
                return Err(CompileError::UnsupportedFieldDecl { reason: "pointer" });
            }
            if placement.is_some() {
                return Err(CompileError::UnsupportedFieldDecl { reason: "placement" });
            }
            if init.is_some() {
                return Err(CompileError::UnsupportedFieldDecl { reason: "init" });
            }
            if array.is_some() {
                return Err(CompileError::UnsupportedFieldDecl { reason: "array" });
            }
            if !attrs.0.is_empty() {
                return Err(CompileError::UnsupportedFieldDecl { reason: "attrs" });
            }
            if !is_known_primitive(ty) {
                return Err(CompileError::UnsupportedFieldDecl { reason: "non-primitive type" });
            }
            let name_id = p.intern_ident(name);
            let ty_id = p.push_type(ty.clone());
            p.ops.push(Op::ReadPrim { ty: ty_id, name: name_id });
            Ok(())
        }
        Stmt::StructDecl(_) => Err(CompileError::UnsupportedStmt("struct decl")),
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
        Stmt::BitfieldField { .. } => {
            Err(CompileError::UnsupportedStmt("bitfield field"))
        }
    }
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
    fn compile_refuses_struct_decl_for_now() {
        let src = "struct S { u8 a; }; S s;\n";
        let tokens = crate::tokenize(src).unwrap();
        let ast = crate::parse(tokens).unwrap();
        let err = compile(&ast).unwrap_err();
        assert!(matches!(err, CompileError::UnsupportedStmt("struct decl")), "{err:?}");
    }
}
