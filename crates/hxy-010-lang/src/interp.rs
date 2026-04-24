//! Tree-walking interpreter for 010 Binary Template programs.
//!
//! Entry point: [`Interpreter::run`]. The interpreter walks the AST
//! sequentially, reading bytes from the supplied [`HexSource`] as it
//! encounters field declarations. Output is a flat pre-order list of
//! [`NodeOut`] records that mirrors the WIT `node` layout -- so the
//! plugin wrapper (phase 2j) is a straight translation, no further
//! restructuring.

use std::collections::HashMap;

use thiserror::Error;

use crate::ast::Attrs;
use crate::ast::BinOp;
use crate::ast::EnumDecl;
use crate::ast::Expr;
use crate::ast::FunctionDef;
use crate::ast::Program;
use crate::ast::Stmt;
use crate::ast::StructDecl;
use crate::ast::TopItem;
use crate::ast::TypeRef;
use crate::ast::UnaryOp;
use crate::source::Cursor;
use crate::source::HexSource;
use crate::source::SourceError;
use crate::value::Endian;
use crate::value::NodeType;
use crate::value::PrimClass;
use crate::value::PrimKind;
use crate::value::ScalarKind;
use crate::value::Value;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("undefined name `{name}`")]
    UndefinedName { name: String },

    #[error("unknown type `{name}`")]
    UnknownType { name: String },

    #[error("type error: {0}")]
    Type(String),

    #[error("source read failed: {0}")]
    Source(#[from] SourceError),

    #[error("builtin `{name}` called incorrectly: {reason}")]
    BadBuiltinCall { name: String, reason: &'static str },

    #[error("user function `{name}`: {reason}")]
    BadCall { name: String, reason: String },

    /// Member access on a composite expression whose path couldn't be
    /// resolved in `field_storage`. Carries the built lookup path and
    /// the enclosing prefix so the diagnostic points at what the
    /// template wrote and where the interpreter was looking.
    #[error("unresolved member `.{field}` (looked up `{path}` under prefix `{prefix}`)")]
    UnresolvedMember { field: String, path: String, prefix: String },

    /// Array index on a value the interpreter can't route -- either
    /// the base isn't a known array, or its indexed children never
    /// got stored.
    #[error("unresolved array index on `{target}`")]
    UnresolvedIndex { target: String },

    /// Bitfield declared on a non-integer type.
    #[error("bitfield type `{ty}` must be an integer primitive")]
    BadBitfieldType { ty: String },
}

/// Non-fatal issue emitted during execution. Lines up with the WIT
/// `diagnostic` record on the way out.
#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub message: String,
    pub severity: Severity,
    pub file_offset: Option<u64>,
    pub template_line: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// Index into [`RunResult::nodes`]. Separate from `u64` byte offsets,
/// `u32` bit widths, and other unrelated scalars floating through the
/// interpreter so a stray cast between them won't compile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct NodeIdx(u32);

impl NodeIdx {
    pub fn new(idx: u32) -> Self {
        Self(idx)
    }

    pub fn as_u32(self) -> u32 {
        self.0
    }

    pub fn as_usize(self) -> usize {
        self.0 as usize
    }
}

/// One emitted tree node. Mirrors the WIT `node` record.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeOut {
    pub name: String,
    pub ty: NodeType,
    pub offset: u64,
    pub length: u64,
    pub value: Option<Value>,
    pub parent: Option<NodeIdx>,
    /// `(key, value)` pairs pulled from `<attr=value>` lists. Stored
    /// opaquely so the renderer decides what to do with `format=hex`,
    /// `style=sHeading1`, etc.
    pub attrs: Vec<(String, String)>,
}

/// Output of running a template. Non-empty even when fatal errors
/// occur -- the interpreter emits as much as it can before bailing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunResult {
    pub nodes: Vec<NodeOut>,
    pub diagnostics: Vec<Diagnostic>,
    pub return_value: Option<Value>,
}

pub struct Interpreter<S: HexSource> {
    cursor: Cursor<S>,
    endian: Endian,
    /// Type registry: name -> concrete definition.
    types: HashMap<String, TypeDef>,
    /// Function registry.
    functions: HashMap<String, FunctionDef>,
    /// Scope chain for locals / fields. Last element is innermost.
    scopes: Vec<Scope>,
    /// Emitted tree so far.
    nodes: Vec<NodeOut>,
    /// Non-fatal diagnostics emitted so far.
    diagnostics: Vec<Diagnostic>,
    /// Statements executed so far. Compared against [`Self::step_limit`]
    /// at every `exec_stmt` call to kill runaway templates.
    steps: u64,
    /// Maximum statement executions before the interpreter aborts
    /// with an "exceeded step limit" diagnostic. Defaults to
    /// [`DEFAULT_STEP_LIMIT`].
    step_limit: u64,
    /// Persistent field storage keyed by dotted path (e.g.
    /// `DirectoryEntries[2].Key`). Populated as primitive and enum
    /// fields are read; outlives the read's own scope so cross-struct
    /// references like `chunk[0].ihdr.color_type` resolve after the
    /// struct body has popped.
    field_storage: HashMap<String, Value>,
    /// Current path prefix for [`field_storage`] writes. Segments
    /// accrete as the interpreter descends into struct bodies and
    /// array elements -- e.g. while reading `DirectoryEntries[2].Key`
    /// the path is `["DirectoryEntries[2]", "Key"]`.
    path: Vec<String>,
    /// Occurrence count of each (prefix, name) pair across the
    /// entire run -- used to index `PNG_CHUNK chunk;` declarations
    /// inside a loop as `chunk[0]`, `chunk[1]`, .... The counter lives
    /// on the interpreter (not on a scope) because the loop body's
    /// block scope is pushed and popped every iteration, but the
    /// conceptual array all the iterations build together belongs to
    /// the enclosing template, not the inner block.
    field_counts: HashMap<FieldSlot, u32>,
    /// Active bitfield accumulator. A bitfield declaration populates
    /// this on first encounter, reading the underlying integer from
    /// the source once and then peeling successive fields off it.
    /// Cleared when a non-bitfield field is read or the struct body
    /// ends.
    bitfield_slot: Option<BitfieldSlot>,
    /// Bitfield packing direction. `BitfieldRightToLeft()` in the
    /// template source flips this; default is left-to-right (high
    /// bits consumed first), matching 010's default.
    bitfield_right_to_left: bool,
}

/// Key for [`Interpreter::field_counts`]. Newtype wrapper over the
/// `(prefix, name)` tuple so different (same-string) counter spaces
/// can't be mixed up.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FieldSlot {
    prefix: String,
    name: String,
}

#[derive(Clone, Debug)]
struct BitfieldSlot {
    prim: PrimKind,
    /// Raw value read from the source at `offset`, already decoded
    /// for the current endian.
    raw: u64,
    /// File offset of the underlying storage word.
    offset: u64,
    /// Bits consumed by prior bitfields in this slot. New fields
    /// extract from position `consumed..consumed+width`.
    consumed: u32,
}

/// Safety valve against templates with unbounded or near-unbounded
/// loops. 10M statements lets real templates iterate through big
/// archives while still catching `while(true)` holes in well under a
/// second.
pub const DEFAULT_STEP_LIMIT: u64 = 10_000_000;

#[derive(Clone, Debug)]
enum TypeDef {
    Primitive(PrimKind),
    /// Aliased to another type name -- resolved on lookup.
    Alias(String),
    Enum(EnumDecl),
    Struct(StructDecl),
}

#[derive(Default)]
struct Scope {
    vars: HashMap<String, Value>,
}

/// Inner control-flow signal raised by `return` / `break` / `continue`
/// so outer loops and function calls unwind correctly.
enum Flow {
    Next,
    Break,
    Continue,
    Return(Option<Value>),
}

impl<S: HexSource> Interpreter<S> {
    pub fn new(source: S) -> Self {
        let mut me = Self {
            cursor: Cursor::new(source),
            endian: Endian::default(),
            types: HashMap::new(),
            functions: HashMap::new(),
            scopes: vec![Scope::default()],
            nodes: Vec::new(),
            diagnostics: Vec::new(),
            steps: 0,
            step_limit: DEFAULT_STEP_LIMIT,
            field_storage: HashMap::new(),
            path: Vec::new(),
            field_counts: HashMap::new(),
            bitfield_slot: None,
            bitfield_right_to_left: false,
        };
        me.register_primitives();
        me.register_constants();
        me
    }

    /// Seed the root scope with 010's built-in constants -- colour
    /// names, `CHECKSUM_*` algorithm IDs, `cNone`, etc. The values
    /// aren't byte-for-byte identical to 010's (our palette is
    /// symbolic anyway), but every template that references them
    /// just feeds them to no-op builtins like `SetBackColor`, so the
    /// actual number doesn't matter as long as resolution succeeds.
    fn register_constants(&mut self) {
        let scope = self.scopes.first_mut().expect("root scope");
        let mut bind = |name: &str, value: u64| {
            scope.vars.insert(name.to_owned(), Value::UInt { value: value as u128, kind: PrimKind::u32() });
        };
        // Colour constants. Values are 010's convention but we don't
        // render them -- any non-negative integer is fine.
        bind("cNone", 0xFFFF_FFFF);
        bind("cBlack", 0x00_000000);
        bind("cRed", 0x00_0000FF);
        bind("cDkRed", 0x00_000080);
        bind("cLtRed", 0x00_8080FF);
        bind("cGreen", 0x00_00FF00);
        bind("cDkGreen", 0x00_008000);
        bind("cLtGreen", 0x00_80FF80);
        bind("cBlue", 0x00_FF0000);
        bind("cDkBlue", 0x00_800000);
        bind("cLtBlue", 0x00_FF8080);
        bind("cPurple", 0x00_FF00FF);
        bind("cDkPurple", 0x00_800080);
        bind("cLtPurple", 0x00_FF80FF);
        bind("cAqua", 0x00_FFFF00);
        bind("cDkAqua", 0x00_808000);
        bind("cLtAqua", 0x00_FFFF80);
        bind("cYellow", 0x00_00FFFF);
        bind("cDkYellow", 0x00_008080);
        bind("cLtYellow", 0x00_80FFFF);
        bind("cGray", 0x00_808080);
        bind("cDkGray", 0x00_404040);
        bind("cLtGray", 0x00_C0C0C0);
        bind("cSilver", 0x00_C0C0C0);
        bind("cWhite", 0x00_FFFFFF);
        // Checksum / hash algorithm IDs -- these drive
        // [`Interpreter::checksum_builtin`].
        bind("CHECKSUM_BYTE", 0);
        bind("CHECKSUM_SHORT_LE", 1);
        bind("CHECKSUM_SHORT_BE", 2);
        bind("CHECKSUM_INT_LE", 3);
        bind("CHECKSUM_INT_BE", 4);
        bind("CHECKSUM_CRC32", CHECKSUM_CRC32_ID);
        bind("CHECKSUM_CRC16", 6);
        bind("CHECKSUM_ADLER32", 7);
        // Boolean aliases some templates use.
        bind("TRUE", 1);
        bind("FALSE", 0);
    }

    /// Full dotted path under which the next field read should be
    /// stored: the accumulated parent path joined with `.` plus the
    /// field's own name. `segment` is appended with no dot when it
    /// starts with `[`, matching the way array indices chain onto the
    /// array's own name (e.g. `chunks[0]` not `chunks.[0]`).
    fn storage_key(&self, name: &str) -> String {
        join_path(&self.path_prefix(), name)
    }

    fn path_prefix(&self) -> String {
        self.path.join(".")
    }

    /// Record a primitive / enum value at the current path plus `name`.
    /// Silent if the key already exists -- the last write wins, which
    /// matches 010's "redeclaring the same name at the same scope
    /// overwrites" behaviour for loops like
    /// `while (!FEof()) { PNG_CHUNK chunk; }`.
    fn store_field(&mut self, name: &str, value: Value) {
        self.store_at_path(self.storage_key(name), value);
    }

    /// Insert `value` at `key` in [`field_storage`]. Lookups handle
    /// the `[0]` <-> bare-name alias by trying
    /// [`strip_zero_indices`]-normalised keys, so stores don't need
    /// to mirror -- one write, one key.
    fn store_at_path(&mut self, key: String, value: Value) {
        self.field_storage.insert(key, value);
    }

    /// Segment to push onto [`Self::path`] for the next struct-body
    /// descent. First occurrence of a name in the current scope uses
    /// the bare name so `chunk.type.cname`-style unindexed references
    /// resolve; subsequent occurrences use `name[i]` so array-indexed
    /// references like `chunk[CHUNK_CNT - 1]` resolve. The counter
    /// lives in the *scope* rather than globally because structs
    /// nested inside a loop body belong to fresh iterations of the
    /// outer loop.
    fn next_struct_segment(&mut self, name: &str) -> String {
        let slot = FieldSlot { prefix: self.path_prefix(), name: name.to_owned() };
        let count = self.field_counts.entry(slot).or_insert(0);
        let idx = *count;
        *count += 1;
        // First occurrence uses the bare name so unindexed references
        // (`type.cname`, the common case when a field is declared
        // once) resolve directly. Subsequent occurrences carry `[i]`
        // so array-indexed references (`chunk[CHUNK_CNT-1]`) keep
        // their identity. Lookups compensate by stripping `[0]` from
        // query paths before falling back.
        if idx == 0 { name.to_owned() } else { format!("{name}[{idx}]") }
    }

    /// Override the statement-execution budget. Useful for fuzzing
    /// (keep it small) or for users running a deliberately expensive
    /// template (raise the cap).
    pub fn with_step_limit(mut self, limit: u64) -> Self {
        self.step_limit = limit;
        self
    }

    pub fn run(mut self, program: &Program) -> RunResult {
        // First pass: register function definitions so callers can
        // appear textually before the callee.
        for item in &program.items {
            if let TopItem::Function(f) = item {
                self.functions.insert(f.name.clone(), f.clone());
            }
        }
        // Execute top-level statements in order. A `return` here is
        // the whole-template exit signal.
        let mut exit_value: Option<Value> = None;
        for item in &program.items {
            let TopItem::Stmt(s) = item else { continue };
            match self.exec_stmt(s, None) {
                Ok(Flow::Return(v)) => {
                    exit_value = v;
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    self.diagnostics.push(Diagnostic {
                        message: e.to_string(),
                        severity: Severity::Error,
                        file_offset: Some(self.cursor.tell()),
                        template_line: None,
                    });
                    break;
                }
            }
        }
        RunResult { nodes: self.nodes, diagnostics: self.diagnostics, return_value: exit_value }
    }

    fn register_primitives(&mut self) {
        use PrimKind as P;
        let table: &[(&str, PrimKind)] = &[
            ("char", P::char()),
            ("CHAR", P::char()),
            ("uchar", P::uchar()),
            ("UCHAR", P::uchar()),
            ("byte", P::i8()),
            ("ubyte", P::u8()),
            ("BYTE", P::u8()),
            ("UBYTE", P::u8()),
            ("short", P::i16()),
            ("ushort", P::u16()),
            ("int16", P::i16()),
            ("uint16", P::u16()),
            ("SHORT", P::i16()),
            ("USHORT", P::u16()),
            ("WORD", P::u16()),
            ("int", P::i32()),
            ("uint", P::u32()),
            ("int32", P::i32()),
            ("uint32", P::u32()),
            ("long", P::i32()),
            ("ulong", P::u32()),
            ("INT", P::i32()),
            ("UINT", P::u32()),
            ("DWORD", P::u32()),
            ("int64", P::i64()),
            ("uint64", P::u64()),
            ("QWORD", P::u64()),
            ("UQWORD", P::u64()),
            ("float", P::f32()),
            ("double", P::f64()),
            ("FLOAT", P::f32()),
            ("DOUBLE", P::f64()),
            // 010 built-in date / time types. Width matches the
            // on-disk encoding; the `<format=...>` attribute and a
            // future renderer-side converter can turn the raw value
            // into a human-readable date.
            ("DOSDATE", P::u16()),
            ("DOSTIME", P::u16()),
            ("time_t", P::i32()),
            ("time64_t", P::i64()),
            ("FILETIME", P::u64()),
            ("OLETIME", P::f64()),
            ("hfloat", P::u16()), // half-float -- read as raw u16 for now
            ("HFLOAT", P::u16()),
        ];
        for (name, kind) in table {
            self.types.insert((*name).to_owned(), TypeDef::Primitive(*kind));
        }
    }

    fn resolve_type(&self, ty: &TypeRef) -> Result<TypeDef, RuntimeError> {
        let mut name = ty.name.clone();
        for _ in 0..32 {
            match self.types.get(&name) {
                Some(TypeDef::Alias(target)) => name = target.clone(),
                Some(def) => return Ok(def.clone()),
                None => return Err(RuntimeError::UnknownType { name: ty.name.clone() }),
            }
        }
        Err(RuntimeError::UnknownType { name: ty.name.clone() })
    }

    fn exec_stmt(&mut self, stmt: &Stmt, parent: Option<NodeIdx>) -> Result<Flow, RuntimeError> {
        self.steps = self.steps.saturating_add(1);
        if self.steps > self.step_limit {
            return Err(RuntimeError::Type(format!("exceeded step limit ({}) -- template aborted", self.step_limit)));
        }
        match stmt {
            Stmt::Block { stmts, .. } => self.exec_block(stmts, parent),
            Stmt::Expr { expr, .. } => {
                self.eval(expr)?;
                Ok(Flow::Next)
            }
            Stmt::FieldDecl { .. } => {
                self.exec_field_decl(stmt, parent)?;
                Ok(Flow::Next)
            }
            Stmt::TypedefAlias { new_name, source, .. } => {
                self.types.insert(new_name.clone(), TypeDef::Alias(source.name.clone()));
                Ok(Flow::Next)
            }
            Stmt::TypedefEnum(e) => {
                self.types.insert(e.name.clone(), TypeDef::Enum(e.clone()));
                Ok(Flow::Next)
            }
            Stmt::TypedefStruct(s) => {
                self.types.insert(s.name.clone(), TypeDef::Struct(s.clone()));
                Ok(Flow::Next)
            }
            Stmt::If { cond, then_branch, else_branch, .. } => {
                if self.eval(cond)?.is_truthy() {
                    self.exec_stmt(then_branch, parent)
                } else if let Some(e) = else_branch {
                    self.exec_stmt(e, parent)
                } else {
                    Ok(Flow::Next)
                }
            }
            Stmt::While { cond, body, .. } => {
                while self.eval(cond)?.is_truthy() {
                    match self.exec_stmt(body, parent)? {
                        Flow::Break => break,
                        Flow::Continue | Flow::Next => {}
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                    }
                }
                Ok(Flow::Next)
            }
            Stmt::DoWhile { body, cond, .. } => {
                loop {
                    match self.exec_stmt(body, parent)? {
                        Flow::Break => break,
                        Flow::Continue | Flow::Next => {}
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                    }
                    if !self.eval(cond)?.is_truthy() {
                        break;
                    }
                }
                Ok(Flow::Next)
            }
            Stmt::For { init, cond, step, body, .. } => {
                if let Some(init) = init {
                    self.exec_stmt(init, parent)?;
                }
                loop {
                    if let Some(c) = cond
                        && !self.eval(c)?.is_truthy()
                    {
                        break;
                    }
                    match self.exec_stmt(body, parent)? {
                        Flow::Break => break,
                        Flow::Continue | Flow::Next => {}
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                    }
                    if let Some(s) = step {
                        self.eval(s)?;
                    }
                }
                Ok(Flow::Next)
            }
            Stmt::Return { value, .. } => {
                let v = match value {
                    Some(e) => Some(self.eval(e)?),
                    None => None,
                };
                Ok(Flow::Return(v))
            }
            Stmt::Break { .. } => Ok(Flow::Break),
            Stmt::Continue { .. } => Ok(Flow::Continue),
            Stmt::Switch { scrutinee, arms, .. } => self.exec_switch(scrutinee, arms, parent),
        }
    }

    /// Evaluate `scrutinee`, walk `arms` in order, and run the first
    /// matching arm plus every arm after it (fall-through) until a
    /// `break` or the switch ends. A `default` arm is picked when no
    /// pattern matches; fall-through from a preceding case into
    /// `default` is permitted.
    fn exec_switch(
        &mut self,
        scrutinee: &Expr,
        arms: &[crate::ast::SwitchArm],
        parent: Option<NodeIdx>,
    ) -> Result<Flow, RuntimeError> {
        let scrut = self.eval(scrutinee)?;
        let mut matched_idx: Option<usize> = None;
        for (i, arm) in arms.iter().enumerate() {
            let Some(pat) = &arm.pattern else { continue };
            let pv = self.eval(pat)?;
            if values_equal(&scrut, &pv) {
                matched_idx = Some(i);
                break;
            }
        }
        let start = matched_idx.unwrap_or_else(|| arms.iter().position(|a| a.pattern.is_none()).unwrap_or(arms.len()));
        for arm in arms.iter().skip(start) {
            for s in &arm.body {
                match self.exec_stmt(s, parent)? {
                    Flow::Break => return Ok(Flow::Next),
                    Flow::Continue => return Ok(Flow::Continue),
                    Flow::Return(v) => return Ok(Flow::Return(v)),
                    Flow::Next => {}
                }
            }
        }
        Ok(Flow::Next)
    }

    fn exec_block(&mut self, stmts: &[Stmt], parent: Option<NodeIdx>) -> Result<Flow, RuntimeError> {
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

    fn exec_field_decl(&mut self, stmt: &Stmt, parent: Option<NodeIdx>) -> Result<(), RuntimeError> {
        let Stmt::FieldDecl { modifier, ty, name, array_size, args, bit_width, init, attrs, .. } = stmt else {
            unreachable!();
        };

        // `local` and `const` are ephemeral variables; they can still
        // have initializers but don't read from the source.
        if matches!(modifier, crate::ast::DeclModifier::Local | crate::ast::DeclModifier::Const) {
            let value = match init {
                Some(expr) => self.eval(expr)?,
                None => Value::Void,
            };
            self.current_scope_mut().vars.insert(name.clone(), value.clone());
            self.store_field(name, value);
            return Ok(());
        }

        // Bitfield read: peel bits off a shared underlying integer
        // instead of advancing the cursor once per field.
        if let Some(bw_expr) = bit_width {
            let bw = self.eval(bw_expr)?.to_i128().unwrap_or(0) as u32;
            self.read_bitfield(name, ty, bw, parent, attrs)?;
            return Ok(());
        }
        // Any non-bitfield read closes an open slot.
        self.bitfield_slot = None;

        // Normal field read -- resolve the type, read bytes, emit nodes,
        // bind the value into the current scope.
        let count = match array_size {
            Some(expr) => {
                let v = self.eval(expr)?;
                v.to_i128().ok_or_else(|| RuntimeError::Type(format!("array size is not numeric: {v:?}")))? as u64
            }
            None => 0,
        };

        // Evaluate struct args once; the parameterised-struct read
        // binds them to the declared parameter names inside the
        // struct's own scope.
        let evaluated_args: Vec<Value> = args.iter().map(|a| self.eval(a)).collect::<Result<_, _>>()?;

        let value = if array_size.is_some() {
            self.read_array(name, ty, count, parent, attrs, &evaluated_args)?
        } else {
            self.read_scalar(name, ty, parent, attrs, &evaluated_args)?
        };

        self.current_scope_mut().vars.insert(name.clone(), value);
        Ok(())
    }

    /// Extract `width` bits from the active bitfield slot, allocating
    /// a new slot (and reading the underlying integer from the source)
    /// when the type changes or the previous slot has no room left.
    fn read_bitfield(
        &mut self,
        name: &str,
        ty: &TypeRef,
        width: u32,
        parent: Option<NodeIdx>,
        attrs: &Attrs,
    ) -> Result<(), RuntimeError> {
        let def = self.resolve_type(ty)?;
        let prim = match def {
            TypeDef::Primitive(p) if matches!(p.class, PrimClass::Int | PrimClass::Char) => p,
            _ => {
                return Err(RuntimeError::BadBitfieldType { ty: ty.name.clone() });
            }
        };
        let total_bits = (prim.width as u32) * 8;
        let width = width.min(total_bits);

        let need_new_slot = match &self.bitfield_slot {
            Some(slot) => slot.prim.width != prim.width || slot.consumed + width > total_bits,
            None => true,
        };
        if need_new_slot {
            let offset = self.cursor.tell();
            let bytes = self.cursor.read_advance(prim.width as u64)?;
            let decoded = decode_prim(&bytes, prim, self.endian)?;
            let raw = decoded.to_i128().unwrap_or(0) as u64;
            self.bitfield_slot = Some(BitfieldSlot { prim, raw, offset, consumed: 0 });
        }
        // Extract `width` bits from the slot.
        let (field_value, node_offset, node_length) = {
            let slot = self.bitfield_slot.as_mut().unwrap();
            let position = slot.consumed;
            let mask: u64 = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
            let shift =
                if self.bitfield_right_to_left { position } else { total_bits.saturating_sub(position + width) };
            let extracted = (slot.raw >> shift) & mask;
            slot.consumed += width;
            (extracted, slot.offset, prim.width as u64)
        };
        // Emit a node for the whole underlying storage (so the span
        // covers the word) but carry only the extracted value.
        let value = if prim.signed {
            // Sign-extend from `width` bits.
            let shift = 64 - width;
            let signed = if width == 0 { 0 } else { ((field_value << shift) as i64) >> shift };
            Value::SInt { value: signed as i128, kind: prim }
        } else {
            Value::UInt { value: field_value as u128, kind: prim }
        };
        self.nodes.push(NodeOut {
            name: name.to_owned(),
            ty: NodeType::Scalar(ScalarKind::from_prim(prim)),
            offset: node_offset,
            length: node_length,
            value: Some(value.clone()),
            parent,
            attrs: attrs_to_pairs(attrs),
        });
        self.current_scope_mut().vars.insert(name.to_owned(), value.clone());
        self.store_field(name, value);
        Ok(())
    }

    fn current_scope_mut(&mut self) -> &mut Scope {
        self.scopes.last_mut().expect("scope stack is never empty")
    }

    fn read_scalar(
        &mut self,
        name: &str,
        ty: &TypeRef,
        parent: Option<NodeIdx>,
        attrs: &Attrs,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        let def = self.resolve_type(ty)?;
        match def {
            TypeDef::Primitive(p) => {
                let offset = self.cursor.tell();
                let bytes = self.cursor.read_advance(p.width as u64)?;
                let value = decode_prim(&bytes, p, self.endian)?;
                self.nodes.push(NodeOut {
                    name: name.to_owned(),
                    ty: NodeType::Scalar(ScalarKind::from_prim(p)),
                    offset,
                    length: p.width as u64,
                    value: Some(value.clone()),
                    parent,
                    attrs: attrs_to_pairs(attrs),
                });
                self.store_field(name, value.clone());
                Ok(value)
            }
            TypeDef::Enum(e) => {
                let backing_def = match &e.backing {
                    Some(b) => self.resolve_type(b)?,
                    None => TypeDef::Primitive(PrimKind::i32()),
                };
                let TypeDef::Primitive(pk) = backing_def else {
                    return Err(RuntimeError::Type("enum backing must be primitive".into()));
                };
                let offset = self.cursor.tell();
                let bytes = self.cursor.read_advance(pk.width as u64)?;
                let raw = decode_prim(&bytes, pk, self.endian)?;
                let raw_u = raw.to_i128().unwrap_or(0) as u64;
                // Find matching variant -- but don't require one; 010
                // templates can read arbitrary values into an enum
                // field and just render them numerically.
                let variant = e
                    .variants
                    .iter()
                    .find_map(|v| self.eval_enum_variant(v).ok().filter(|(_, val)| *val == raw_u))
                    .map(|(n, _)| n);
                let display = variant.unwrap_or_else(|| format!("{raw_u}"));
                let value = Value::Str(display);
                self.nodes.push(NodeOut {
                    name: name.to_owned(),
                    ty: NodeType::EnumType(ty.name.clone()),
                    offset,
                    length: pk.width as u64,
                    value: Some(value.clone()),
                    parent,
                    attrs: attrs_to_pairs(attrs),
                });
                // The tree node carries the display string for the
                // UI. For arithmetic / comparisons -- e.g.
                // `if (type.Format == 1)` -- callers want the raw
                // numeric, so that's what we return and store. The
                // Str display stays only on the node.
                let raw_value = Value::UInt { value: raw_u as u128, kind: PrimKind::u64() };
                self.store_field(name, raw_value.clone());
                Ok(raw_value)
            }
            TypeDef::Struct(s) => {
                let offset = self.cursor.tell();
                let idx = NodeIdx::new(self.nodes.len() as u32);
                let type_name = if s.is_union {
                    NodeType::Unknown(format!("union {}", ty.name))
                } else {
                    NodeType::StructType(ty.name.clone())
                };
                self.nodes.push(NodeOut {
                    name: name.to_owned(),
                    ty: type_name,
                    offset,
                    length: 0,
                    value: None,
                    parent,
                    attrs: attrs_to_pairs(attrs),
                });
                // Descend: push a path segment for field_storage keys
                // and a scope so parameterised-struct args don't leak
                // into the caller. Repeated declarations of the same
                // name in the same scope -- `PNG_CHUNK chunk;` in a
                // loop -- pick up an `[i]` suffix so
                // `chunk[CHUNK_CNT-1].type.cname` resolves.
                let segment = self.next_struct_segment(name);
                self.path.push(segment);
                self.scopes.push(Scope::default());
                // Bind parameterised-struct args to their declared
                // param names inside the struct's own scope. Extra /
                // missing args fall through silently; 010 itself is
                // forgiving here.
                for (param, value) in s.params.iter().zip(args.iter()) {
                    self.current_scope_mut().vars.insert(param.name.clone(), value.clone());
                }
                let result = self.exec_struct_body(&s, offset, idx);
                self.scopes.pop();
                self.path.pop();
                result?;
                Ok(Value::Void)
            }
            TypeDef::Alias(_) => unreachable!("resolve_type follows aliases"),
        }
    }

    /// Execute a struct body. Unions overlay: every field starts at
    /// the same offset, the struct's own length is the max child
    /// length, and the cursor is left advanced past the widest field.
    fn exec_struct_body(&mut self, s: &StructDecl, offset: u64, idx: NodeIdx) -> Result<(), RuntimeError> {
        if !s.is_union {
            for stmt in &s.body {
                self.exec_stmt(stmt, Some(idx))?;
            }
            let end = self.cursor.tell();
            self.nodes[idx.as_usize()].length = end - offset;
            return Ok(());
        }
        // Union: rewind between fields and track the farthest advance.
        let mut max_end = offset;
        for stmt in &s.body {
            self.cursor.seek(offset);
            self.exec_stmt(stmt, Some(idx))?;
            max_end = max_end.max(self.cursor.tell());
        }
        self.cursor.seek(max_end);
        self.nodes[idx.as_usize()].length = max_end - offset;
        Ok(())
    }

    fn eval_enum_variant(&mut self, v: &crate::ast::EnumVariant) -> Result<(String, u64), RuntimeError> {
        // Enum variant values are parse-time expressions; they can
        // reference previous variants, but that's a rare pattern we
        // can grow into later. For now support literal ints only.
        let val = match &v.value {
            Some(expr) => {
                let eval = self.eval(expr)?;
                eval.to_i128().unwrap_or(0) as u64
            }
            None => 0, // TODO: auto-increment
        };
        Ok((v.name.clone(), val))
    }

    fn read_array(
        &mut self,
        name: &str,
        ty: &TypeRef,
        count: u64,
        parent: Option<NodeIdx>,
        attrs: &Attrs,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        let def = self.resolve_type(ty)?;
        // Primitive arrays are emitted as a single contiguous node --
        // `char[N]` / `uchar[N]` become a Str value, other numeric
        // primitives carry the raw byte range. Rendering a 500-byte
        // `uchar data[...]` as one coloured region matches 010's
        // behaviour and keeps the hex view from fragmenting into
        // thousands of outlined cells per chunk.
        if let TypeDef::Primitive(p) = def.clone() {
            let offset = self.cursor.tell();
            let total_bytes = count.saturating_mul(p.width as u64);
            let bytes = self.cursor.read_advance(total_bytes)?;
            let value = if matches!(p.class, PrimClass::Char) {
                Value::Str(String::from_utf8_lossy(&bytes).into_owned())
            } else {
                // No Value variant for raw byte arrays yet -- emit
                // Void so the UI knows the node has a span but no
                // inline scalar value. The renderer already falls
                // back to a hex dump for Void-valued arrays.
                Value::Void
            };
            // One tree node for the whole array so the hex view
            // paints it as a single contiguous region, matching
            // 010's rendering.
            // Tag the endian so the UI tooltip can re-decode each
            // element on hover without re-running the interpreter.
            let mut rendered_attrs = attrs_to_pairs(attrs);
            rendered_attrs.push((
                "hxy_endian".to_owned(),
                match self.endian {
                    Endian::Little => "little".to_owned(),
                    Endian::Big => "big".to_owned(),
                },
            ));
            self.nodes.push(NodeOut {
                name: name.to_owned(),
                ty: NodeType::ScalarArray(ScalarKind::from_prim(p), count),
                offset,
                length: total_bytes,
                value: if matches!(value, Value::Void) { None } else { Some(value.clone()) },
                parent,
                attrs: rendered_attrs,
            });
            self.store_field(name, value.clone());
            // Storage only (no tree node) for each element so
            // `sig.btPngSignature[0]` resolves without fragmenting
            // the tree.
            for i in 0..count {
                let start = (i * p.width as u64) as usize;
                let end = start + p.width as usize;
                let Some(slice) = bytes.get(start..end) else { break };
                let Ok(elem) = decode_prim(slice, p, self.endian) else { continue };
                let elem_path = format!("{}[{}]", self.storage_key(name), i);
                self.store_at_path(elem_path, elem);
            }
            return Ok(value);
        }
        let array_ty = match &def {
            TypeDef::Primitive(p) => NodeType::ScalarArray(ScalarKind::from_prim(*p), count),
            TypeDef::Enum(_) => NodeType::EnumArray(ty.name.clone(), count),
            TypeDef::Struct(_) => NodeType::StructArray(ty.name.clone(), count),
            TypeDef::Alias(_) => NodeType::Unknown(format!("{}[{}]", ty.name, count)),
        };
        let offset = self.cursor.tell();
        let idx = NodeIdx::new(self.nodes.len() as u32);
        self.nodes.push(NodeOut {
            name: name.to_owned(),
            ty: array_ty,
            offset,
            length: 0,
            value: None,
            parent,
            attrs: attrs_to_pairs(attrs),
        });
        for i in 0..count {
            let elem_name = format!("[{i}]");
            let elem_ty = TypeRef { name: ty.name.clone(), span: ty.span };
            // Each struct / enum element pushes its own path segment
            // (`arr[3]`) so descendant fields land at
            // `arr[3].field` in storage.
            let indexed = format!("{name}[{i}]");
            self.path.push(indexed);
            let saved_scope = self.scopes.len();
            let r = match &def {
                TypeDef::Struct(_) => self.read_scalar_struct_elem(&elem_name, &elem_ty, Some(idx), args),
                TypeDef::Enum(_) => {
                    self.read_scalar(&elem_name, &elem_ty, Some(idx), &Attrs::default(), &[]).map(|_| ())
                }
                _ => Ok(()),
            };
            self.path.pop();
            while self.scopes.len() > saved_scope {
                self.scopes.pop();
            }
            r?;
        }
        let end = self.cursor.tell();
        self.nodes[idx.as_usize()].length = end - offset;
        Ok(Value::Void)
    }

    /// Read one struct element of an array. The outer [`read_array`]
    /// pushed `arr[i]` onto `self.path`; we run the body in a fresh
    /// scope so param bindings don't leak across elements.
    fn read_scalar_struct_elem(
        &mut self,
        name: &str,
        ty: &TypeRef,
        parent: Option<NodeIdx>,
        args: &[Value],
    ) -> Result<(), RuntimeError> {
        let def = self.resolve_type(ty)?;
        let TypeDef::Struct(s) = def else {
            unreachable!("struct-elem read called with non-struct");
        };
        let offset = self.cursor.tell();
        let idx = NodeIdx::new(self.nodes.len() as u32);
        let type_name = if s.is_union {
            NodeType::Unknown(format!("union {}", ty.name))
        } else {
            NodeType::StructType(ty.name.clone())
        };
        self.nodes.push(NodeOut {
            name: name.to_owned(),
            ty: type_name,
            offset,
            length: 0,
            value: None,
            parent,
            attrs: Vec::new(),
        });
        self.scopes.push(Scope::default());
        for (param, value) in s.params.iter().zip(args.iter()) {
            self.current_scope_mut().vars.insert(param.name.clone(), value.clone());
        }
        let r = self.exec_struct_body(&s, offset, idx);
        self.scopes.pop();
        r
    }

    fn eval(&mut self, expr: &Expr) -> Result<Value, RuntimeError> {
        match expr {
            Expr::IntLit { value, .. } => Ok(Value::UInt { value: *value as u128, kind: PrimKind::u64() }),
            Expr::FloatLit { value, .. } => Ok(Value::Float { value: *value, kind: PrimKind::f64() }),
            Expr::StringLit { value, .. } => Ok(Value::Str(value.clone())),
            Expr::CharLit { value, .. } => Ok(Value::Char { value: *value, kind: PrimKind::char() }),
            Expr::Ident { name, .. } => self.lookup_ident(name),
            Expr::Binary { op, lhs, rhs, .. } => {
                let l = self.eval(lhs)?;
                let r = self.eval(rhs)?;
                eval_binary(*op, &l, &r)
            }
            Expr::Unary { op, operand, .. } => {
                // Pre/post inc/dec mutate the operand in place when
                // it's a name -- otherwise fall back to the pure-value
                // eval for `-x`, `~x`, `!x`, etc.
                if matches!(op, UnaryOp::PreInc | UnaryOp::PostInc | UnaryOp::PreDec | UnaryOp::PostDec)
                    && let Expr::Ident { name, .. } = &**operand
                {
                    let current = self.lookup_ident(name)?;
                    let i = current.to_i128().ok_or_else(|| RuntimeError::Type(format!("not numeric: {current:?}")))?;
                    let delta = match op {
                        UnaryOp::PreInc | UnaryOp::PostInc => 1,
                        UnaryOp::PreDec | UnaryOp::PostDec => -1,
                        _ => unreachable!(),
                    };
                    let updated = Value::SInt { value: i + delta, kind: PrimKind::i64() };
                    self.store_ident(name, updated.clone())?;
                    self.store_field(name, updated.clone());
                    return Ok(match op {
                        UnaryOp::PreInc | UnaryOp::PreDec => updated,
                        _ => Value::SInt { value: i, kind: PrimKind::i64() },
                    });
                }
                let v = self.eval(operand)?;
                eval_unary(*op, &v)
            }
            Expr::Call { callee, args, .. } => {
                let Expr::Ident { name, .. } = &**callee else {
                    return Err(RuntimeError::Type("call target must be an identifier".into()));
                };
                // `sizeof(TypeName)` takes a type name, not a value --
                // evaluating the argument as an expression would try
                // to look up `TypeName` as a variable. Intercept
                // before the generic arg eval.
                if name == "sizeof"
                    && let [Expr::Ident { name: arg_name, .. }] = args.as_slice()
                {
                    let bytes = self.sizeof_type(arg_name)?;
                    return Ok(Value::UInt { value: bytes as u128, kind: PrimKind::u64() });
                }
                let evaluated: Vec<Value> = args.iter().map(|a| self.eval(a)).collect::<Result<_, _>>()?;
                self.call_named(name, &evaluated)
            }
            Expr::Member { .. } | Expr::Index { .. } => {
                // Try path-based lookup first. When the interpreter
                // is in the middle of reading a struct body, the
                // current path prefix (`chunk[0]`) should scope
                // sibling field lookups: `type.cname` inside that
                // body resolves to `chunk[0].type.cname` in storage.
                if let Some(path) = self.build_path(expr)? {
                    for candidate in lookup_candidates(&path, &self.path_prefix()) {
                        if let Some(v) = self.field_storage.get(&candidate).cloned() {
                            return Ok(v);
                        }
                    }
                }
                // Fallbacks kept for simpler idioms:
                match expr {
                    Expr::Member { target, field, .. } => {
                        if let Expr::Ident { name, .. } = &**target {
                            let composite = format!("{name}.{field}");
                            self.lookup_ident(&composite).or_else(|_| self.lookup_ident(field))
                        } else {
                            let path = self.build_path(expr)?.unwrap_or_default();
                            Err(RuntimeError::UnresolvedMember {
                                field: field.clone(),
                                path,
                                prefix: self.path_prefix(),
                            })
                        }
                    }
                    Expr::Index { target, .. } => {
                        if let Expr::Ident { name, .. } = &**target {
                            self.lookup_ident(name)
                        } else {
                            let target_path = self.build_path(target)?.unwrap_or_default();
                            Err(RuntimeError::UnresolvedIndex { target: target_path })
                        }
                    }
                    _ => unreachable!(),
                }
            }
            Expr::Assign { op, target, value, .. } => {
                let Expr::Ident { name, .. } = &**target else {
                    return Err(RuntimeError::Type("assignment target must be an identifier".into()));
                };
                let rhs = self.eval(value)?;
                let new_val = match op {
                    crate::ast::AssignOp::Assign => rhs,
                    other => {
                        let current = self.lookup_ident(name)?;
                        let bin_op = compound_to_bin(*other);
                        eval_binary(bin_op, &current, &rhs)?
                    }
                };
                self.store_ident(name, new_val.clone())?;
                Ok(new_val)
            }
            Expr::Ternary { cond, then_val, else_val, .. } => {
                if self.eval(cond)?.is_truthy() {
                    self.eval(then_val)
                } else {
                    self.eval(else_val)
                }
            }
        }
    }

    fn lookup_ident(&self, name: &str) -> Result<Value, RuntimeError> {
        for scope in self.scopes.iter().rev() {
            if let Some(v) = scope.vars.get(name) {
                return Ok(v.clone());
            }
        }
        // Fall back to persistent field storage: an enum variant
        // identifier (e.g. `Privileges` from an earlier `typedef enum`)
        // or a cross-scope struct field access.
        if let Some(v) = self.field_storage.get(name) {
            return Ok(v.clone());
        }
        // Enum-variant lookup: scan registered enums for a matching
        // variant and return its numeric value.
        for def in self.types.values() {
            if let TypeDef::Enum(e) = def
                && let Some(v) = e.variants.iter().find(|v| v.name == name)
            {
                let raw = match &v.value {
                    Some(Expr::IntLit { value, .. }) => *value as i128,
                    _ => 0,
                };
                return Ok(Value::UInt { value: raw as u128, kind: PrimKind::u64() });
            }
        }
        Err(RuntimeError::UndefinedName { name: name.to_owned() })
    }

    /// Compute the dotted path for a Member/Index chain rooted at an
    /// identifier. Returns `None` if any segment isn't path-evaluable
    /// (e.g. a function call as a subscript). Index values come from
    /// regular expression evaluation.
    fn build_path(&mut self, expr: &Expr) -> Result<Option<String>, RuntimeError> {
        match expr {
            Expr::Ident { name, .. } => Ok(Some(name.clone())),
            Expr::Member { target, field, .. } => {
                let Some(base) = self.build_path(target)? else { return Ok(None) };
                Ok(Some(format!("{base}.{field}")))
            }
            Expr::Index { target, index, .. } => {
                let Some(base) = self.build_path(target)? else { return Ok(None) };
                let iv = self.eval(index)?;
                let Some(i) = iv.to_i128() else { return Ok(None) };
                Ok(Some(format!("{base}[{i}]")))
            }
            _ => Ok(None),
        }
    }

    fn store_ident(&mut self, name: &str, value: Value) -> Result<(), RuntimeError> {
        for scope in self.scopes.iter_mut().rev() {
            if scope.vars.contains_key(name) {
                scope.vars.insert(name.to_owned(), value);
                return Ok(());
            }
        }
        // Auto-declare in the current scope (C-ish behaviour for
        // 010 globals that aren't explicitly `local`).
        self.current_scope_mut().vars.insert(name.to_owned(), value);
        Ok(())
    }

    fn call_named(&mut self, name: &str, args: &[Value]) -> Result<Value, RuntimeError> {
        if let Some(v) = self.call_builtin(name, args)? {
            return Ok(v);
        }
        let Some(func) = self.functions.get(name).cloned() else {
            return Err(RuntimeError::UndefinedName { name: name.to_owned() });
        };
        if func.params.len() != args.len() {
            return Err(RuntimeError::BadCall {
                name: name.to_owned(),
                reason: format!("expected {} args, got {}", func.params.len(), args.len()),
            });
        }
        self.scopes.push(Scope::default());
        for (p, v) in func.params.iter().zip(args.iter()) {
            self.current_scope_mut().vars.insert(p.name.clone(), v.clone());
        }
        let mut ret = Value::Void;
        for s in &func.body {
            match self.exec_stmt(s, None)? {
                Flow::Return(v) => {
                    ret = v.unwrap_or(Value::Void);
                    break;
                }
                Flow::Next | Flow::Continue => {}
                Flow::Break => break,
            }
        }
        self.scopes.pop();
        Ok(ret)
    }

    fn call_builtin(&mut self, name: &str, args: &[Value]) -> Result<Option<Value>, RuntimeError> {
        match name {
            "LittleEndian" => {
                self.endian = Endian::Little;
                Ok(Some(Value::Void))
            }
            "BigEndian" => {
                self.endian = Endian::Big;
                Ok(Some(Value::Void))
            }
            "FTell" => Ok(Some(Value::UInt { value: self.cursor.tell() as u128, kind: PrimKind::u64() })),
            "FSeek" => {
                let [pos] = args else {
                    return Err(RuntimeError::BadBuiltinCall { name: name.into(), reason: "expected 1 arg" });
                };
                let offset = pos.to_i128().unwrap_or(0) as u64;
                self.cursor.seek(offset);
                Ok(Some(Value::Void))
            }
            "FEof" => Ok(Some(Value::Bool(self.cursor.at_eof()))),
            "FileSize" => Ok(Some(Value::UInt { value: self.cursor.len() as u128, kind: PrimKind::u64() })),
            "ReadUInt" => Ok(Some(self.read_fn_uint(args, 4, false)?)),
            "ReadInt" => Ok(Some(self.read_fn_uint(args, 4, true)?)),
            "ReadUShort" => Ok(Some(self.read_fn_uint(args, 2, false)?)),
            "ReadShort" => Ok(Some(self.read_fn_uint(args, 2, true)?)),
            "ReadUQuad" | "ReadUInt64" => Ok(Some(self.read_fn_uint(args, 8, false)?)),
            "ReadQuad" | "ReadInt64" => Ok(Some(self.read_fn_uint(args, 8, true)?)),
            "ReadByte" | "ReadUByte" => Ok(Some(self.read_fn_uint(args, 1, false)?)),
            "Warning" => {
                let msg = args.first().map(value_to_display).unwrap_or_default();
                self.diagnostics.push(Diagnostic {
                    message: msg,
                    severity: Severity::Warning,
                    file_offset: Some(self.cursor.tell()),
                    template_line: None,
                });
                Ok(Some(Value::Void))
            }
            "Printf" => {
                let fmt = args.first().map(value_to_display).unwrap_or_default();
                let rest: Vec<&Value> = args.iter().skip(1).collect();
                let message = format_printf(&fmt, &rest);
                self.diagnostics.push(Diagnostic {
                    message,
                    severity: Severity::Info,
                    file_offset: Some(self.cursor.tell()),
                    template_line: None,
                });
                Ok(Some(Value::Void))
            }
            "Exit" => {
                let code = args.first().and_then(|v| v.to_i128()).unwrap_or(0);
                self.diagnostics.push(Diagnostic {
                    message: format!("Exit({code})"),
                    severity: Severity::Error,
                    file_offset: Some(self.cursor.tell()),
                    template_line: None,
                });
                // Raise by returning "exit value" via Ok(None)? No --
                // we want to halt. Model as an error so `run` bails.
                Err(RuntimeError::Type(format!("template exited with {code}")))
            }
            "exists" => {
                let has = !matches!(args.first(), Some(Value::Void) | None);
                Ok(Some(Value::Bool(has)))
            }
            "RequiresVersion" => Ok(Some(Value::Void)),
            "FindFirst" => Ok(Some(self.find_first(args)?)),
            // Layout / display settings are presentational -- the
            // lexer+parser already preserves the setting, but the
            // renderer takes cues from field attrs rather than global
            // flags. No-ops are fine for all of these.
            "BitfieldRightToLeft" => {
                self.bitfield_right_to_left = true;
                Ok(Some(Value::Void))
            }
            "BitfieldLeftToRight" => {
                self.bitfield_right_to_left = false;
                Ok(Some(Value::Void))
            }
            "DisplayFormatHex" | "DisplayFormatDecimal" | "DisplayFormatBinary" | "SetForeColor" | "SetBackColor" => {
                Ok(Some(Value::Void))
            }
            "Strlen" => {
                let s = args.first().map(value_to_display).unwrap_or_default();
                Ok(Some(Value::UInt { value: s.len() as u128, kind: PrimKind::u64() }))
            }
            "SPrintf" => {
                // 010's signature: `SPrintf(out, fmt, args...) -> length`.
                // We don't have lvalue semantics for `out`, so we at
                // least populate it in the current scope if it's a
                // bare identifier name known to `out`'s callers.
                let fmt = args.get(1).map(value_to_display).unwrap_or_default();
                let rest: Vec<&Value> = args.iter().skip(2).collect();
                let formatted = format_printf(&fmt, &rest);
                // Best effort: the caller passed the first arg by
                // value rather than by reference, so we can't write
                // back into it. Record the formatted string in the
                // current scope under whatever the first arg reported.
                // This is a lossy approximation of 010's out-param
                // semantics but lets `SPrintf(s, "%d", x); Printf(s);`
                // surface the same text in diagnostics.
                if let Some(Value::Str(name)) = args.first() {
                    self.store_ident(name, Value::Str(formatted.clone())).ok();
                }
                Ok(Some(Value::UInt { value: formatted.len() as u128, kind: PrimKind::u64() }))
            }
            "EnumToString" => {
                // Return the variant name for an enum-valued arg, or
                // the numeric string when no variant matches.
                let raw = args.first().and_then(|v| v.to_i128()).unwrap_or(0) as u64;
                let mut display: Option<String> = None;
                for def in self.types.values() {
                    if let TypeDef::Enum(e) = def
                        && let Some(v) = e
                            .variants
                            .iter()
                            .find(|v| matches!(&v.value, Some(Expr::IntLit { value, .. }) if *value == raw))
                    {
                        display = Some(v.name.clone());
                        break;
                    }
                }
                Ok(Some(Value::Str(display.unwrap_or_else(|| raw.to_string()))))
            }
            "Checksum" => Ok(Some(self.checksum_builtin(args)?)),
            _ => Ok(None),
        }
    }

    /// Static size of `name`, walking aliases and summing struct
    /// field widths. Returns 0 for unknown types (matches 010's
    /// forgiving semantics -- templates sometimes take `sizeof` of a
    /// conditionally-defined type).
    fn sizeof_type(&self, name: &str) -> Result<u64, RuntimeError> {
        let mut cur = name.to_owned();
        for _ in 0..32 {
            match self.types.get(&cur) {
                Some(TypeDef::Primitive(p)) => return Ok(p.width as u64),
                Some(TypeDef::Alias(target)) => cur = target.clone(),
                Some(TypeDef::Enum(e)) => {
                    return Ok(match &e.backing {
                        Some(t) => self.sizeof_type(&t.name)?,
                        None => 4,
                    });
                }
                Some(TypeDef::Struct(s)) => {
                    let body = s.body.clone();
                    let is_union = s.is_union;
                    let mut total: u64 = 0;
                    for stmt in &body {
                        if let Stmt::FieldDecl { ty, array_size, bit_width, modifier, .. } = stmt {
                            if matches!(modifier, crate::ast::DeclModifier::Local | crate::ast::DeclModifier::Const) {
                                continue;
                            }
                            let elem = self.sizeof_type(&ty.name).unwrap_or(0);
                            if bit_width.is_some() {
                                // Bitfields pack into their underlying
                                // storage. Charge one underlying word
                                // per run instead of one per field --
                                // tight, but good enough for the only
                                // caller (template dispatch).
                                total = total.max(elem);
                                continue;
                            }
                            let len = match array_size {
                                Some(_) => 0, // dynamic array; can't size statically
                                None => elem,
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

    /// `Checksum(algo, start, size, ...)` -- read `size` bytes from
    /// `start` and checksum them with the requested algorithm. Only
    /// `CHECKSUM_CRC32` (enum value 5 in 010) is implemented; other
    /// algorithms return 0 with an Info diagnostic so templates don't
    /// die on them.
    fn checksum_builtin(&mut self, args: &[Value]) -> Result<Value, RuntimeError> {
        let algo_raw = args.first().and_then(|v| v.to_i128()).unwrap_or(-1);
        // CHECKSUM_CRC32 is the value 010 hands to templates. Real
        // 010 uses the enum tag `CHECKSUM_CRC32` -- our `lookup_ident`
        // resolves that to a u64; templates that pass the constant
        // directly come through as the same numeric. The value is 5
        // in 010's public header; be liberal and also accept callers
        // that pass a string name so template authors don't have to
        // care.
        let algo_is_crc32 = algo_raw == CHECKSUM_CRC32_ID as i128
            || matches!(args.first(), Some(Value::Str(s)) if s.eq_ignore_ascii_case("crc32"));
        let start = args.get(1).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as u64;
        let size_arg = args.get(2).and_then(|v| v.to_i128()).unwrap_or(0);
        let source_len = self.cursor.len();
        let size =
            if size_arg <= 0 { source_len.saturating_sub(start) } else { (size_arg as u64).min(source_len - start) };
        if !algo_is_crc32 {
            self.diagnostics.push(Diagnostic {
                message: format!("Checksum algo {algo_raw} not implemented; returning 0"),
                severity: Severity::Info,
                file_offset: Some(self.cursor.tell()),
                template_line: None,
            });
            return Ok(Value::UInt { value: 0, kind: PrimKind::u64() });
        }
        let bytes = self.cursor.read_at(start, size)?;
        let crc = crc32_ieee(&bytes);
        Ok(Value::UInt { value: crc as u128, kind: PrimKind::u32() })
    }

    fn read_fn_uint(&self, args: &[Value], width: u8, signed: bool) -> Result<Value, RuntimeError> {
        let offset = match args.first() {
            Some(v) => v.to_i128().unwrap_or(0) as u64,
            None => self.cursor.tell(),
        };
        let bytes = self.cursor.read_at(offset, width as u64)?;
        decode_prim(&bytes, PrimKind { class: PrimClass::Int, width, signed }, self.endian)
    }

    /// `FindFirst(data, matchcase, wholeword, method, tolerance, dir,
    /// start, size, wildcardMatch) -> int64`
    ///
    /// Only the common path -- integer needle, optional `start`/`size`
    /// -- is implemented. Other args are accepted and ignored so
    /// templates that pass the full 010 argument vector work.
    /// Returns -1 when the needle isn't found.
    fn find_first(&self, args: &[Value]) -> Result<Value, RuntimeError> {
        let not_found = Value::SInt { value: -1, kind: PrimKind::i64() };
        let Some(needle_val) = args.first() else { return Ok(not_found) };
        let Some(needle_i) = needle_val.to_i128() else { return Ok(not_found) };
        let start = args.get(6).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as u64;
        let size_arg = args.get(7).and_then(|v| v.to_i128()).unwrap_or(0);
        let source_len = self.cursor.len();
        let end = if size_arg <= 0 { source_len } else { (start + size_arg as u64).min(source_len) };
        if start >= end {
            return Ok(not_found);
        }

        // Pick the shortest LE byte representation that fits the
        // needle. Matches how 010 encodes integer search values:
        // `uchar` -> 1 byte, `ushort` -> 2, `uint` -> 4, `uint64` -> 8.
        let pattern: Vec<u8> = if let Ok(v) = u8::try_from(needle_i) {
            vec![v]
        } else if let Ok(v) = u16::try_from(needle_i) {
            v.to_le_bytes().to_vec()
        } else if let Ok(v) = u32::try_from(needle_i) {
            v.to_le_bytes().to_vec()
        } else {
            (needle_i as i64).to_le_bytes().to_vec()
        };
        let bytes = self.cursor.read_at(start, end - start)?;
        let offset = bytes.windows(pattern.len()).position(|w| w == pattern);
        match offset {
            Some(pos) => Ok(Value::SInt { value: (start + pos as u64) as i128, kind: PrimKind::i64() }),
            None => Ok(not_found),
        }
    }
}

fn decode_prim(bytes: &[u8], kind: PrimKind, endian: Endian) -> Result<Value, RuntimeError> {
    if bytes.len() as u8 != kind.width {
        return Err(RuntimeError::Type(format!("short read: got {} bytes, need {}", bytes.len(), kind.width)));
    }
    let to_arr8 = |b: &[u8]| -> [u8; 8] {
        let mut a = [0u8; 8];
        if matches!(endian, Endian::Little) {
            for (i, v) in b.iter().enumerate() {
                a[i] = *v;
            }
        } else {
            for (i, v) in b.iter().enumerate() {
                a[8 - b.len() + i] = *v;
            }
        }
        a
    };
    Ok(match kind.class {
        PrimClass::Char => {
            let v = bytes[0] as u32;
            Value::Char { value: v, kind }
        }
        PrimClass::Int => {
            let a = to_arr8(bytes);
            let raw = match endian {
                Endian::Little => u64::from_le_bytes(a),
                Endian::Big => u64::from_be_bytes(a),
            };
            if kind.signed {
                // Sign-extend from `kind.width` bytes to 64 bits.
                let shift = 64 - (kind.width as u32) * 8;
                let signed = ((raw as i64) << shift) >> shift;
                Value::SInt { value: signed as i128, kind }
            } else {
                Value::UInt { value: raw as u128, kind }
            }
        }
        PrimClass::Float => {
            if kind.width == 4 {
                let arr: [u8; 4] = bytes.try_into().unwrap();
                let v = match endian {
                    Endian::Little => f32::from_le_bytes(arr),
                    Endian::Big => f32::from_be_bytes(arr),
                };
                Value::Float { value: v as f64, kind }
            } else {
                let arr: [u8; 8] = bytes.try_into().unwrap();
                let v = match endian {
                    Endian::Little => f64::from_le_bytes(arr),
                    Endian::Big => f64::from_be_bytes(arr),
                };
                Value::Float { value: v, kind }
            }
        }
    })
}

fn eval_binary(op: BinOp, l: &Value, r: &Value) -> Result<Value, RuntimeError> {
    // String equality: 010 templates rely on `type.cname == "IHDR"`
    // for chunk dispatch. Both sides must be strings; mismatched
    // types fall through to the numeric path and blow up, which
    // matches 010's "types must match" semantics.
    if let (Value::Str(a), Value::Str(b)) = (l, r) {
        return Ok(match op {
            BinOp::Eq => Value::Bool(a == b),
            BinOp::NotEq => Value::Bool(a != b),
            BinOp::Lt => Value::Bool(a < b),
            BinOp::Gt => Value::Bool(a > b),
            BinOp::LtEq => Value::Bool(a <= b),
            BinOp::GtEq => Value::Bool(a >= b),
            BinOp::Add => Value::Str(format!("{a}{b}")),
            _ => return Err(RuntimeError::Type(format!("string operand not supported for {op:?}"))),
        });
    }
    // Float path: if either side is a float, coerce both to f64.
    if l.is_float() || r.is_float() {
        let lf = l.to_f64().ok_or_else(|| RuntimeError::Type(format!("not numeric: {l:?}")))?;
        let rf = r.to_f64().ok_or_else(|| RuntimeError::Type(format!("not numeric: {r:?}")))?;
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
            _ => {
                return Err(RuntimeError::Type(format!("float operand not supported for {op:?}")));
            }
        });
    }
    let li = l.to_i128().ok_or_else(|| RuntimeError::Type(format!("not numeric: {l:?}")))?;
    let ri = r.to_i128().ok_or_else(|| RuntimeError::Type(format!("not numeric: {r:?}")))?;
    let out = match op {
        BinOp::Add => Value::SInt { value: li.wrapping_add(ri), kind: PrimKind::i64() },
        BinOp::Sub => Value::SInt { value: li.wrapping_sub(ri), kind: PrimKind::i64() },
        BinOp::Mul => Value::SInt { value: li.wrapping_mul(ri), kind: PrimKind::i64() },
        BinOp::Div => Value::SInt { value: li.checked_div(ri).unwrap_or(0), kind: PrimKind::i64() },
        BinOp::Rem => Value::SInt { value: li.checked_rem(ri).unwrap_or(0), kind: PrimKind::i64() },
        BinOp::BitAnd => Value::SInt { value: li & ri, kind: PrimKind::i64() },
        BinOp::BitOr => Value::SInt { value: li | ri, kind: PrimKind::i64() },
        BinOp::BitXor => Value::SInt { value: li ^ ri, kind: PrimKind::i64() },
        BinOp::Shl => Value::SInt { value: li.wrapping_shl(ri as u32 & 63), kind: PrimKind::i64() },
        BinOp::Shr => Value::SInt { value: li.wrapping_shr(ri as u32 & 63), kind: PrimKind::i64() },
        BinOp::Eq => Value::Bool(li == ri),
        BinOp::NotEq => Value::Bool(li != ri),
        BinOp::Lt => Value::Bool(li < ri),
        BinOp::Gt => Value::Bool(li > ri),
        BinOp::LtEq => Value::Bool(li <= ri),
        BinOp::GtEq => Value::Bool(li >= ri),
        BinOp::LogicalAnd => Value::Bool(li != 0 && ri != 0),
        BinOp::LogicalOr => Value::Bool(li != 0 || ri != 0),
    };
    Ok(out)
}

fn eval_unary(op: UnaryOp, v: &Value) -> Result<Value, RuntimeError> {
    match op {
        UnaryOp::Neg => {
            if let Some(f) = v.to_f64()
                && v.is_float()
            {
                return Ok(Value::Float { value: -f, kind: PrimKind::f64() });
            }
            let i = v.to_i128().ok_or_else(|| RuntimeError::Type(format!("not numeric: {v:?}")))?;
            Ok(Value::SInt { value: -i, kind: PrimKind::i64() })
        }
        UnaryOp::Pos => Ok(v.clone()),
        UnaryOp::Not => Ok(Value::Bool(!v.is_truthy())),
        UnaryOp::BitNot => {
            let i = v.to_i128().ok_or_else(|| RuntimeError::Type(format!("not numeric: {v:?}")))?;
            Ok(Value::SInt { value: !i, kind: PrimKind::i64() })
        }
        UnaryOp::PreInc | UnaryOp::PostInc | UnaryOp::PreDec | UnaryOp::PostDec => {
            // We don't track lvalues well enough to update in place
            // yet -- return the computed value without storing.
            let i = v.to_i128().ok_or_else(|| RuntimeError::Type(format!("not numeric: {v:?}")))?;
            let delta = match op {
                UnaryOp::PreInc | UnaryOp::PostInc => 1,
                UnaryOp::PreDec | UnaryOp::PostDec => -1,
                _ => unreachable!(),
            };
            Ok(Value::SInt { value: i + delta, kind: PrimKind::i64() })
        }
    }
}

fn compound_to_bin(op: crate::ast::AssignOp) -> BinOp {
    use crate::ast::AssignOp as A;
    match op {
        A::Assign => BinOp::Add, // unreachable -- callers filter this
        A::AddAssign => BinOp::Add,
        A::SubAssign => BinOp::Sub,
        A::MulAssign => BinOp::Mul,
        A::DivAssign => BinOp::Div,
        A::RemAssign => BinOp::Rem,
        A::AndAssign => BinOp::BitAnd,
        A::OrAssign => BinOp::BitOr,
        A::XorAssign => BinOp::BitXor,
        A::ShlAssign => BinOp::Shl,
        A::ShrAssign => BinOp::Shr,
    }
}

fn attrs_to_pairs(attrs: &Attrs) -> Vec<(String, String)> {
    attrs.0.iter().map(|a| (a.key.clone(), attr_expr_to_string(&a.value))).collect()
}

fn attr_expr_to_string(e: &Expr) -> String {
    match e {
        Expr::IntLit { value, .. } => value.to_string(),
        Expr::FloatLit { value, .. } => value.to_string(),
        Expr::StringLit { value, .. } => value.clone(),
        Expr::CharLit { value, .. } => format!("{value}"),
        Expr::Ident { name, .. } => name.clone(),
        _ => format!("{e:?}"),
    }
}

fn value_to_display(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => format!("{other}"),
    }
}

/// Name of CHECKSUM_CRC32 in 010's public `CHECKSUM_*` enum. Templates
/// that pass the named constant get it resolved through the enum
/// lookup in [`Interpreter::lookup_ident`]; this fallback matches the
/// numeric value for callers that pass it raw. 010's actual constant
/// value is 5 (`CHECKSUM_SUM64=1, CRC16=2, CRC32=5, ...`); we use
/// that same number so round-tripping through the source produces a
/// consistent match.
const CHECKSUM_CRC32_ID: u64 = 5;

/// Ordered list of `field_storage` keys to try for a
/// `Member`/`Index` lookup. Starts with the most specific match
/// (path exactly as the user typed it), falls back to scoping under
/// the current struct-body prefix, then to forms where `[0]`
/// subscripts are stripped -- since single-occurrence fields are
/// stored without a `[0]` suffix.
fn lookup_candidates(path: &str, prefix: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(4);
    let mut push = |s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    push(path.to_owned());
    push(strip_zero_indices(path));
    let scoped = join_path(prefix, path);
    push(scoped.clone());
    push(strip_zero_indices(&scoped));
    out
}

/// Strip `[0]` subscripts --
/// used at lookup time so a query like `sig.btPngSignature[0]`
/// matches the natural storage key for the first (and only)
/// occurrence of a field, which we store without the `[0]` suffix.
fn strip_zero_indices(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'['
            && let Some(rel) = path[i..].find(']')
            && &path[i + 1..i + rel] == "0"
        {
            i += rel + 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Join a parent `prefix` with a field `segment`, suppressing the
/// `.` separator when the segment is an index bracket (`[3]`) so the
/// resulting key reads `arr[3]` rather than `arr.[3]`.
fn join_path(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        segment.to_owned()
    } else if segment.starts_with('[') {
        format!("{prefix}{segment}")
    } else {
        format!("{prefix}.{segment}")
    }
}

/// Loose equality for switch arm matching. Falls through the
/// arithmetic numeric comparison used by [`eval_binary`] and adds a
/// string path for character / string literals appearing in `case`
/// labels.
fn values_equal(a: &Value, b: &Value) -> bool {
    if let (Value::Str(x), Value::Str(y)) = (a, b) {
        return x == y;
    }
    match (a.to_i128(), b.to_i128()) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Minimal printf-style formatter: handles the `%d / %i / %u / %x /
/// %X / %s / %c / %%` conversions 010 templates actually use. Unknown
/// specifiers pass through literally so diagnostics stay readable.
fn format_printf(fmt: &str, args: &[&Value]) -> String {
    let mut out = String::with_capacity(fmt.len());
    let mut chars = fmt.chars().peekable();
    let mut arg_idx = 0usize;
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // Consume flags / width / precision / length modifiers in a
        // best-effort fashion -- we don't honour padding but accept
        // the full spec so formats like `%08X` don't confuse us.
        let mut spec = String::from("%");
        while let Some(&peek) = chars.peek() {
            spec.push(peek);
            chars.next();
            if peek.is_ascii_alphabetic() || peek == '%' {
                break;
            }
        }
        let conv = spec.chars().last().unwrap_or('%');
        let mut take = || -> Option<&Value> {
            let v = args.get(arg_idx).copied();
            if v.is_some() {
                arg_idx += 1;
            }
            v
        };
        match conv {
            '%' => out.push('%'),
            's' => out.push_str(&take().map(value_to_display).unwrap_or_default()),
            'c' => {
                let v = take().and_then(|v| v.to_i128()).unwrap_or(0) as u32;
                if let Some(ch) = char::from_u32(v) {
                    out.push(ch);
                }
            }
            'd' | 'i' => {
                let v = take().and_then(|v| v.to_i128()).unwrap_or(0);
                out.push_str(&v.to_string());
            }
            'u' => {
                let v = take().and_then(|v| v.to_i128()).unwrap_or(0) as u128;
                out.push_str(&v.to_string());
            }
            'x' => {
                let v = take().and_then(|v| v.to_i128()).unwrap_or(0) as u128;
                out.push_str(&format!("{v:x}"));
            }
            'X' => {
                let v = take().and_then(|v| v.to_i128()).unwrap_or(0) as u128;
                out.push_str(&format!("{v:X}"));
            }
            'f' | 'F' | 'g' | 'G' => {
                let v = take().and_then(|v| v.to_f64()).unwrap_or(0.0);
                out.push_str(&format!("{v}"));
            }
            _ => out.push_str(&spec),
        }
    }
    out
}

/// Standard CRC-32/IEEE 802.3 -- PNG, zlib, et al. Polynomial
/// 0xEDB88320, initial 0xFFFFFFFF, final xor 0xFFFFFFFF.
fn crc32_ieee(bytes: &[u8]) -> u32 {
    const POLY: u32 = 0xEDB88320;
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ POLY } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod crc_test {
    use super::crc32_ieee;

    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32_ieee(b""), 0);
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF43926);
        assert_eq!(crc32_ieee(b"hello"), 0x3610A686);
    }
}
