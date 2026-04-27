//! Tree-walking interpreter for 010 Binary Template programs.
//!
//! Entry point: [`Interpreter::run`]. The interpreter walks the AST
//! sequentially, reading bytes from the supplied [`HexSource`] as it
//! encounters field declarations. Output is a flat pre-order list of
//! [`NodeOut`] records that mirrors the WIT `node` layout -- so the
//! plugin wrapper (phase 2j) is a straight translation, no further
//! restructuring.

use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

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

#[derive(Clone, Debug, Error, PartialEq, Eq)]
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

    /// Wall-clock budget configured via [`Interpreter::with_timeout`]
    /// elapsed mid-execution. The interpreter checks this every ~1024
    /// statements, so the actual wall time may overshoot slightly.
    #[error("template execution exceeded timeout of {timeout_ms} ms")]
    TimedOut { timeout_ms: u64 },

    /// A `while` / `do-while` / `for` body ran for many iterations
    /// without the source cursor advancing -- almost always a bug in
    /// the template (or in our handling of one of its built-ins) where
    /// the loop's exit condition is gated on file position but nothing
    /// inside the body actually consumes bytes.
    #[error("loop made no source progress for {iterations} consecutive iterations")]
    LoopStalled { iterations: u32 },
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
    /// Structured cause of the terminal failure, if execution stopped
    /// because of a runtime error. The same error is also pushed into
    /// `diagnostics` as a human-readable message; this field exists
    /// so callers can match on the cause programmatically (e.g.
    /// "did this template time out?") without parsing strings.
    pub terminal_error: Option<RuntimeError>,
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
    /// Spans for primitive arrays, keyed by the same dotted path
    /// `field_storage` uses. Lets `arr[i]` indexing decode element
    /// `i` lazily from the source instead of pre-materialising N
    /// `Value`s into `field_storage` (which was catastrophically slow
    /// for multi-MB byte arrays).
    array_storage: HashMap<String, ArraySpan>,
    /// Per-typedef array suffix discovered at decl time
    /// (`typedef CHAR DIGEST[20];`). When a field decl uses one
    /// of these as its type, the field reads `[N]` items even
    /// without a `[..]` on the field itself. Without this, the
    /// alias resolved to its scalar source and the read consumed
    /// only one element (so `header.groupID` returned just the
    /// first character of `RIFF`).
    typedef_array_size: HashMap<String, Expr>,
    /// In-memory byte sizes for `local`/`const` array declarations,
    /// keyed by name. Lets `sizeof(localArr)` return the declared
    /// total size (count * element width) even though the array
    /// never lands in the emitted node tree.
    local_array_bytes: HashMap<String, u64>,
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
    /// `BitfieldDisablePadding()` mode. When set, each new bitfield
    /// slot reads only the bytes needed for the *next* field's width
    /// (rounded up), rather than the underlying type's full width
    /// -- so consecutive `int x:24` fields consume 3 bytes each
    /// instead of 4. WebP/PNG-style packed bitstreams need this.
    bitfield_padding_disabled: bool,
    /// Configured wall-clock budget. Materialised into [`Self::deadline`]
    /// at the start of [`Self::run`] so the timer doesn't include
    /// time the caller spent between construction and execution.
    timeout: Option<Duration>,
    /// Absolute instant after which [`Self::exec_stmt`] starts
    /// returning [`RuntimeError::TimedOut`]. Computed from
    /// [`Self::timeout`] when `run` begins.
    deadline: Option<Instant>,
}

/// Bookkeeping for a primitive array stored once at read time and
/// decoded element-by-element on access. Keeps `field_storage` from
/// growing one entry per byte for `uchar data[N]`-style declarations.
#[derive(Clone, Debug)]
struct ArraySpan {
    /// Source offset of element 0.
    source_offset: u64,
    /// Number of elements in the array.
    count: u64,
    /// Element primitive kind (carries width + signedness).
    prim: PrimKind,
    /// Endian to decode each element with -- captured at read time so
    /// later `BigEndian()` / `LittleEndian()` calls can't change the
    /// interpretation of an already-read array.
    endian: Endian,
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
    /// Aliases from a parameter name to the storage path of the
    /// argument it was bound to. Templates that pass a struct by
    /// reference (`string ReadFoo(Foo &x) { return x.field; }`) get
    /// the param `x` rewritten to the real path so member lookups
    /// reach the original record. Set on function-call entry; cleared
    /// when the scope pops.
    path_aliases: HashMap<String, String>,
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
            array_storage: HashMap::new(),
            typedef_array_size: HashMap::new(),
            local_array_bytes: HashMap::new(),
            path: Vec::new(),
            field_counts: HashMap::new(),
            bitfield_slot: None,
            bitfield_right_to_left: false,
            bitfield_padding_disabled: false,
            timeout: None,
            deadline: None,
        };
        me.register_primitives();
        me.register_constants();
        me
    }

    /// Seed the root scope with 010's built-in constants -- color
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
        // Color constants. Values are 010's convention but we don't
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
        bind("CHECKSUM_INT64_LE", 8);
        bind("CHECKSUM_INT64_BE", 9);
        bind("CHECKSUM_MD5", 10);
        bind("CHECKSUM_SHA1", 11);
        bind("CHECKSUM_SHA256", 12);
        bind("CHECKSUM_SHA512", 13);
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

    /// Read element `idx` from a registered primitive array span and
    /// decode it with the array's frozen endian. Out-of-bounds is a
    /// runtime error rather than a clamp -- 010 itself returns
    /// garbage in that case, but a clean error is more useful here.
    fn decode_array_element(&self, span: &ArraySpan, idx: u64) -> Result<Value, RuntimeError> {
        if idx >= span.count {
            return Err(RuntimeError::Type(format!("array index out of bounds: {idx} >= {}", span.count)));
        }
        let off = span.source_offset + idx * span.prim.width as u64;
        let bytes = self.cursor.read_at(off, span.prim.width as u64)?;
        decode_prim(&bytes, span.prim, span.endian)
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

    /// Wall-clock budget for [`Self::run`]. The deadline starts when
    /// `run` is invoked, not at construction time. Checked every ~1024
    /// statements so the overshoot is bounded but the per-step cost
    /// stays in the noise.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn run(mut self, program: &Program) -> RunResult {
        self.deadline = self.timeout.map(|d| Instant::now() + d);
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
        let mut terminal_error: Option<RuntimeError> = None;
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
                    terminal_error = Some(e);
                    break;
                }
            }
        }
        RunResult { nodes: self.nodes, diagnostics: self.diagnostics, return_value: exit_value, terminal_error }
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
            ("LONG", P::i32()),
            ("ULONG", P::u32()),
            ("INT", P::i32()),
            ("UINT", P::u32()),
            ("DWORD", P::u32()),
            ("int64", P::i64()),
            ("uint64", P::u64()),
            ("UINT64", P::u64()),
            ("INT64", P::i64()),
            ("ULONG64", P::u64()),
            ("LONG64", P::i64()),
            ("__int64", P::i64()),
            ("QWORD", P::u64()),
            ("UQWORD", P::u64()),
            // 010 aliases `QUAD` to a signed 64-bit integer
            // (per the docs at sweetscape.com -- it's an
            // older name kept for compatibility).
            ("QUAD", P::i64()),
            ("UQUAD", P::u64()),
            // Lowercase byte aliases that some templates use.
            ("int8", P::i8()),
            ("uint8", P::u8()),
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
            // 010's NUL-terminated string types. Modelling them as a
            // single byte read is a degraded behaviour -- a real
            // string read would consume bytes up to the NUL -- but
            // it lets templates that declare `string` typed fields
            // and function return types parse and dispatch without
            // erroring on `unknown type`.
            ("string", P::char()),
            ("STRING", P::char()),
            ("wstring", P::u16()),
            ("WSTRING", P::u16()),
            ("wchar_t", P::u16()),
            ("WCHAR_T", P::u16()),
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
        // Wall-clock budget. `Instant::now()` is several hundred ns on
        // Windows, so amortise the cost across ~1024 statements.
        if self.steps & 0x3FF == 0
            && let Some(deadline) = self.deadline
            && Instant::now() >= deadline
        {
            let timeout_ms = self.timeout.map(|d| d.as_millis() as u64).unwrap_or(0);
            return Err(RuntimeError::TimedOut { timeout_ms });
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
            Stmt::TypedefAlias { new_name, source, array_size, .. } => {
                self.types.insert(new_name.clone(), TypeDef::Alias(source.name.clone()));
                if let Some(size_expr) = array_size {
                    // Snapshot the size expr so a later
                    // `DIGEST x;` field decl can re-attach it.
                    self.typedef_array_size.insert(new_name.clone(), size_expr.clone());
                }
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
                let mut stuck = StuckCounter::new();
                while self.eval(cond)?.is_truthy() {
                    let before = self.cursor.tell();
                    match self.exec_stmt(body, parent)? {
                        Flow::Break => break,
                        Flow::Continue | Flow::Next => {}
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                    }
                    stuck.observe(self.cursor.tell() == before)?;
                }
                Ok(Flow::Next)
            }
            Stmt::DoWhile { body, cond, .. } => {
                let mut stuck = StuckCounter::new();
                loop {
                    let before = self.cursor.tell();
                    match self.exec_stmt(body, parent)? {
                        Flow::Break => break,
                        Flow::Continue | Flow::Next => {}
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                    }
                    stuck.observe(self.cursor.tell() == before)?;
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
                let mut stuck = StuckCounter::new();
                loop {
                    if let Some(c) = cond
                        && !self.eval(c)?.is_truthy()
                    {
                        break;
                    }
                    let before = self.cursor.tell();
                    match self.exec_stmt(body, parent)? {
                        Flow::Break => break,
                        Flow::Continue | Flow::Next => {}
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                    }
                    if let Some(s) = step {
                        self.eval(s)?;
                    }
                    stuck.observe(self.cursor.tell() == before)?;
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
        // Multi-decl groups -- a block of FieldDecls with no other
        // stmts -- come from `local int a, b;` and similar. They
        // shouldn't push a fresh scope: the locals need to land
        // in the surrounding struct/function scope so subsequent
        // statements can read and reassign them.
        let is_decl_group = !stmts.is_empty()
            && stmts.iter().all(|s| matches!(s, Stmt::FieldDecl { .. }));
        if !is_decl_group {
            self.scopes.push(Scope::default());
        }
        let mut flow = Flow::Next;
        for s in stmts {
            flow = self.exec_stmt(s, parent)?;
            if !matches!(flow, Flow::Next) {
                break;
            }
        }
        if !is_decl_group {
            self.scopes.pop();
        }
        Ok(flow)
    }

    fn exec_field_decl(&mut self, stmt: &Stmt, parent: Option<NodeIdx>) -> Result<(), RuntimeError> {
        let Stmt::FieldDecl { modifier, ty, name, array_size, args, bit_width, init, attrs, .. } = stmt else {
            unreachable!();
        };

        // `local` and `const` are ephemeral variables; they can still
        // have initializers but don't read from the source.
        if matches!(modifier, crate::ast::DeclModifier::Local | crate::ast::DeclModifier::Const) {
            // Track byte size for `sizeof(localArr)` lookups before
            // we lose the array_size info to the eval.
            if let Some(size_expr) = array_size
                .as_ref()
                .cloned()
                .or_else(|| self.typedef_array_size.get(&ty.name).cloned())
                && let Some(count) = self.eval(&size_expr)?.to_i128()
                && count >= 0
            {
                let elem = self.sizeof_type(&ty.name).unwrap_or(0);
                self.local_array_bytes.insert(name.clone(), count as u64 * elem);
            }
            let value = match init {
                Some(expr) => self.eval(expr)?,
                None => self.default_local_value(ty, array_size.as_ref())?,
            };
            self.current_scope_mut().vars.insert(name.clone(), value.clone());
            self.store_field(name, value);
            return Ok(());
        }

        // Bitfield read: peel bits off a shared underlying integer
        // instead of advancing the cursor once per field.
        if let Some(bw_expr) = bit_width {
            let v = self.eval(bw_expr)?;
            let bw =
                v.to_i128().ok_or_else(|| RuntimeError::Type(format!("bitfield width is not numeric: {v:?}")))? as u32;
            self.read_bitfield(name, ty, bw, parent, attrs)?;
            return Ok(());
        }
        // Any non-bitfield read closes an open slot.
        self.bitfield_slot = None;

        // Normal field read -- resolve the type, read bytes, emit nodes,
        // bind the value into the current scope.
        //
        // If the field's type is a typedef whose declaration carries
        // an array suffix (`typedef CHAR DIGEST[20];`), promote the
        // field to an array read of that size when the field itself
        // doesn't already supply one. Without this, `DIGEST x;`
        // collapsed to a single-char read and downstream `x !=
        // "RIFF"` failed with a Char-vs-Str compare.
        let array_size = match (array_size, self.typedef_array_size.get(&ty.name).cloned()) {
            (Some(e), _) => Some(e.clone()),
            (None, Some(typedef_size)) => Some(typedef_size),
            (None, None) => None,
        };
        let array_size = array_size.as_ref();
        let mut count = match array_size {
            Some(expr) => {
                let v = self.eval(expr)?;
                let n = v.to_i128().ok_or_else(|| RuntimeError::Type(format!("array size is not numeric: {v:?}")))?;
                // Negative sizes happen when a corrupt header field
                // is sign-extended through arithmetic
                // (`byte data[ChunkSize - read_size]` where ChunkSize
                // came in negative). Clamp to zero so the read
                // surfaces a clean EOF instead of asking for
                // ~2^64 bytes.
                if n < 0 { 0 } else { n as u64 }
            }
            None => 0,
        };
        // Empty `[]` on a single-byte type means "read until NUL or
        // EOF" (010's flex-array convention -- COFF.bt's
        // `BYTE Name[]` is the canonical case). Compute the run
        // length here so the array path advances the cursor instead
        // of looping at zero progress.
        let is_flex_byte_array = matches!(array_size, Some(Expr::IntLit { value: 0, .. }))
            && matches!(
                ty.name.as_str(),
                "char" | "CHAR" | "uchar" | "UCHAR" | "byte" | "BYTE" | "ubyte" | "UBYTE"
            );
        if is_flex_byte_array && count == 0 {
            let off = self.cursor.tell();
            let avail = self.cursor.len().saturating_sub(off);
            let probe = self.cursor.read_at(off, avail)?;
            count = probe
                .iter()
                .position(|&b| b == 0)
                .map(|i| (i + 1) as u64)
                .unwrap_or(avail);
        }

        // Evaluate struct args once; the parameterised-struct read
        // binds them to the declared parameter names inside the
        // struct's own scope. Capture each arg's storage path
        // alongside its value so ref-typed parameters resolve back
        // to the caller's record (`Section(SectionHeaders[i])` lets
        // the body access `SecHeader.Name` instead of seeing Void).
        let mut evaluated_args: Vec<Value> = Vec::with_capacity(args.len());
        let mut arg_paths: Vec<Option<String>> = Vec::with_capacity(args.len());
        for a in args.iter() {
            let path = self.build_path(a)?;
            arg_paths.push(path);
            evaluated_args.push(self.eval(a)?);
        }
        // Resolve each path against the current prefix; we want a
        // fully qualified storage key the function body can read.
        let prefix = self.path_prefix();
        let arg_paths: Vec<Option<String>> = arg_paths
            .into_iter()
            .map(|p| {
                let p = p?;
                lookup_candidates(&p, &prefix)
                    .into_iter()
                    .find(|c| {
                        self.field_storage.contains_key(c)
                            || self.array_storage.contains_key(c)
                            || self.field_storage.keys().any(|k| k.starts_with(&format!("{c}.")))
                            || self.array_storage.keys().any(|k| k.starts_with(&format!("{c}.")))
                    })
                    .or(Some(p))
            })
            .collect();

        let read_result = if array_size.is_some() {
            self.read_array(name, ty, count, parent, attrs, &evaluated_args, &arg_paths)
        } else {
            self.read_scalar(name, ty, parent, attrs, &evaluated_args, &arg_paths)
        };
        // Past-EOF reads at the end of a long while/do-while loop
        // (templates that exit on `!FEof()` after the body already
        // overshot by one field) get downgraded to a Warning so the
        // overall run still produces output. The cursor is advanced
        // to len so the surrounding loop sees FEof() == true on its
        // next iteration.
        let value = match read_result {
            Ok(v) => v,
            Err(RuntimeError::Source(SourceError::OutOfBounds { offset, end, len }))
                if offset >= len =>
            {
                self.diagnostics.push(Diagnostic {
                    message: format!(
                        "field `{name}` read [{offset}..{end}) past EOF (file is {len} bytes); skipped"
                    ),
                    severity: Severity::Warning,
                    file_offset: Some(offset),
                    template_line: None,
                });
                self.cursor.seek(len);
                Value::Void
            }
            Err(e) => return Err(e),
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
            // Enums backed by an integer primitive are valid bitfield
            // bases; 010 packs the raw integer into the slot the same
            // way as for the underlying type.
            TypeDef::Enum(e) => {
                let backing = match &e.backing {
                    Some(b) => self.resolve_type(b)?,
                    None => TypeDef::Primitive(PrimKind::i32()),
                };
                match backing {
                    TypeDef::Primitive(p) if matches!(p.class, PrimClass::Int | PrimClass::Char) => p,
                    _ => return Err(RuntimeError::BadBitfieldType { ty: ty.name.clone() }),
                }
            }
            _ => {
                return Err(RuntimeError::BadBitfieldType { ty: ty.name.clone() });
            }
        };
        let type_bits = (prim.width as u32) * 8;
        let width = width.min(type_bits);
        // Slot byte width: with padding disabled, the underlying
        // storage shrinks to just enough bytes for `width` bits so
        // consecutive fields pack tightly. With padding (the
        // default), one full type-width word backs each slot.
        let slot_bytes = if self.bitfield_padding_disabled {
            width.div_ceil(8) as u8
        } else {
            prim.width
        };
        let slot_total_bits = (slot_bytes as u32) * 8;

        let need_new_slot = match &self.bitfield_slot {
            Some(slot) => slot.prim.width != prim.width || slot.consumed + width > slot_total_bits,
            None => true,
        };
        if need_new_slot {
            let offset = self.cursor.tell();
            let bytes = self.cursor.read_advance(slot_bytes as u64)?;
            // Decode using a kind that matches the slot byte width
            // (padding-disabled may give us an odd byte count like 3).
            let slot_prim = PrimKind { class: prim.class, width: slot_bytes, signed: prim.signed };
            let decoded = decode_prim_for_bitfield(&bytes, slot_prim, self.endian);
            self.bitfield_slot = Some(BitfieldSlot { prim, raw: decoded, offset, consumed: 0 });
        }
        // Extract `width` bits from the slot.
        let (field_value, node_offset, node_length) = {
            let slot = self.bitfield_slot.as_mut().unwrap();
            let position = slot.consumed;
            let mask: u64 = if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
            let shift =
                if self.bitfield_right_to_left { position } else { slot_total_bits.saturating_sub(position + width) };
            let extracted = (slot.raw >> shift) & mask;
            slot.consumed += width;
            (extracted, slot.offset, slot_bytes as u64)
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
        // Tag the emitted node with the actual bit count so the UI
        // can render `B4` instead of the parent integer type. The
        // span still covers the whole underlying word (several
        // fields may share it), and the value carries only the
        // extracted bits.
        let mut pairs = attrs_to_pairs(attrs);
        // Name must match `hxy_plugin_host::BITFIELD_BITS_ATTR`;
        // this crate doesn't depend on hxy-plugin-host, so the key
        // is duplicated as a string literal. Update both sides if
        // the name ever changes.
        pairs.push(("hxy_bits".to_owned(), width.to_string()));
        self.nodes.push(NodeOut {
            name: name.to_owned(),
            ty: NodeType::Scalar(ScalarKind::from_prim(prim)),
            offset: node_offset,
            length: node_length,
            value: Some(value.clone()),
            parent,
            attrs: pairs,
        });
        self.current_scope_mut().vars.insert(name.to_owned(), value.clone());
        self.store_field(name, value);
        Ok(())
    }

    fn current_scope_mut(&mut self) -> &mut Scope {
        self.scopes.last_mut().expect("scope stack is never empty")
    }

    /// Default value for a `local`/`const` declaration that has no
    /// initializer. Char arrays get a zero-filled `Value::Str` so
    /// templates can index-write into them (the
    /// `local char buf[N]; ReadBytes(buf, ...)` idiom). Everything
    /// else stays `Void` until a write happens.
    fn default_local_value(
        &mut self,
        ty: &TypeRef,
        array_size: Option<&Expr>,
    ) -> Result<Value, RuntimeError> {
        // `local string s;` defaults to the empty string so the
        // typical idiom `s += "frag"` works without first writing
        // a literal -- otherwise the implicit Void operand trips
        // the integer path with "not numeric: Void".
        if matches!(ty.name.as_str(), "string" | "STRING" | "wstring" | "WSTRING") && array_size.is_none() {
            return Ok(Value::Str(String::new()));
        }
        let array_size = array_size
            .cloned()
            .or_else(|| self.typedef_array_size.get(&ty.name).cloned());
        let Some(size_expr) = array_size else {
            return Ok(Value::Void);
        };
        let is_char_like = matches!(
            ty.name.as_str(),
            "char" | "CHAR" | "uchar" | "UCHAR" | "byte" | "BYTE" | "ubyte" | "UBYTE"
        );
        if !is_char_like {
            return Ok(Value::Void);
        }
        let n = self
            .eval(&size_expr)?
            .to_i128()
            .ok_or_else(|| RuntimeError::Type("local array size is not numeric".into()))?
            .max(0) as usize;
        Ok(Value::Str("\0".repeat(n)))
    }

    fn read_scalar(
        &mut self,
        name: &str,
        ty: &TypeRef,
        parent: Option<NodeIdx>,
        attrs: &Attrs,
        args: &[Value],
        arg_paths: &[Option<String>],
    ) -> Result<Value, RuntimeError> {
        // 010's `string` / `wstring` types read a NUL-terminated
        // sequence (not a single character). Handle them here before
        // resolve_type kicks in: our type registry pretends `string`
        // is `char` for sizing fallbacks, but reading a `char` only
        // consumes one byte, which leaves the cursor mid-string and
        // throws downstream length math off by N.
        let ty_name = ty.name.as_str();
        if matches!(ty_name, "string" | "STRING" | "wstring" | "WSTRING") {
            let wide = matches!(ty_name, "wstring" | "WSTRING");
            let offset = self.cursor.tell();
            let stride: u64 = if wide { 2 } else { 1 };
            let max_len = self.cursor.len().saturating_sub(offset);
            let raw = self.cursor.read_at(offset, max_len)?;
            let term_pos = if wide {
                raw.chunks_exact(2)
                    .position(|w| w == [0, 0])
                    .map(|i| i * 2 + 2)
                    .unwrap_or(raw.len())
            } else {
                raw.iter().position(|&b| b == 0).map(|i| i + 1).unwrap_or(raw.len())
            };
            let consumed = term_pos as u64;
            // Advance cursor past the NUL.
            self.cursor.read_advance(consumed)?;
            let s = decode_string(&raw[..term_pos.saturating_sub(stride as usize)], wide, self.endian);
            let value = Value::Str(s);
            self.nodes.push(NodeOut {
                name: name.to_owned(),
                ty: NodeType::Scalar(ScalarKind::from_prim(PrimKind::char())),
                offset,
                length: consumed,
                value: Some(value.clone()),
                parent,
                attrs: attrs_to_pairs(attrs),
            });
            self.store_field(name, value.clone());
            return Ok(value);
        }
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
                // Enum backing is constrained to a `Primitive` integer
                // above, so `to_i128` always succeeds; the fallback is a
                // belt-and-braces floor.
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
                // `this` magic ident resolves to the current struct's
                // own storage path. Templates use it to pass `this`
                // into helper functions (`findTable(this, "cmap")` in
                // TTF.bt) without naming the outer record explicitly.
                let self_path = self.path_prefix();
                self.current_scope_mut().path_aliases.insert("this".to_owned(), self_path);
                // Bind parameterised-struct args to their declared
                // param names inside the struct's own scope. Extra /
                // missing args fall through silently; 010 itself is
                // forgiving here. Ref-typed (or struct-typed) params
                // also get a path alias so member lookups inside the
                // body resolve back to the caller's record.
                for (i, param) in s.params.iter().enumerate() {
                    if let Some(value) = args.get(i) {
                        self.current_scope_mut().vars.insert(param.name.clone(), value.clone());
                    }
                    let is_struct_ty = matches!(self.types.get(&param.ty.name), Some(TypeDef::Struct(_)));
                    if (param.is_ref || is_struct_ty)
                        && let Some(Some(path)) = arg_paths.get(i)
                    {
                        self.current_scope_mut()
                            .path_aliases
                            .insert(param.name.clone(), path.clone());
                    }
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
        arg_paths: &[Option<String>],
    ) -> Result<Value, RuntimeError> {
        let def = self.resolve_type(ty)?;
        // Primitive arrays are emitted as a single contiguous node --
        // `char[N]` / `uchar[N]` become a Str value, other numeric
        // primitives carry the raw byte range. Rendering a 500-byte
        // `uchar data[...]` as one colored region matches 010's
        // behaviour and keeps the hex view from fragmenting into
        // thousands of outlined cells per chunk.
        if let TypeDef::Primitive(p) = def.clone() {
            let offset = self.cursor.tell();
            let mut total_bytes = count.saturating_mul(p.width as u64);
            // Clamp the request at EOF -- corrupt or speculative
            // template reads (off-by-one loops, header fields
            // pointing past the end of a small fixture) shouldn't
            // hard-fail the whole template. Surface a Warning so the
            // overshoot is visible.
            let avail = self.cursor.len().saturating_sub(offset);
            if total_bytes > avail {
                self.diagnostics.push(Diagnostic {
                    message: format!(
                        "array `{name}` requested {total_bytes} bytes at offset {offset}, only {avail} available; clamped to fit"
                    ),
                    severity: Severity::Warning,
                    file_offset: Some(offset),
                    template_line: None,
                });
                total_bytes = avail;
            }
            let bytes = self.cursor.read_advance(total_bytes)?;
            let count = if p.width == 0 { 0 } else { total_bytes / p.width as u64 };
            let value = if matches!(p.class, PrimClass::Char)
                && let Ok(_) = str::from_utf8(&bytes)
            {
                Value::Str(String::from_utf8(bytes).expect("string should be UTF-8"))
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
            // Only store a string-array value into field_storage:
            // a Void store under the bare path (`h.sig`) would
            // shadow the per-element decode for queries like
            // `h.sig[0]` because `strip_zero_indices` matches the
            // bare path first, returning Void before the array
            // span ever gets consulted. Strings are fine to
            // expose under the bare path -- callers that ask for
            // `s.value[i]` indexed access on a char array don't
            // hit `array_storage`; they read the Str directly.
            if matches!(value, Value::Str(_)) {
                self.store_field(name, value.clone());
            }
            // Register an array span so `arr[i]` lookups can decode
            // element `i` from the source on demand. Storing one
            // entry per element used to be the dominant cost for
            // large `uchar data[N]` arrays -- O(N) `format!()` +
            // HashMap inserts per array, multiplied across every
            // record in a multi-megabyte file.
            let storage_key = self.storage_key(name);
            let span = ArraySpan { source_offset: offset, count, prim: p, endian: self.endian };
            // Mirror under the bare-name path so loop iterations
            // can be referenced as `arr.X` (latest wins) in addition
            // to the explicit `arr[N].X` indexed form.
            let bare = strip_indexed_segments(&storage_key);
            if bare != storage_key {
                self.array_storage.insert(bare, span.clone());
            }
            self.array_storage.insert(storage_key, span);
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
                TypeDef::Struct(_) => self.read_scalar_struct_elem(&elem_name, &elem_ty, Some(idx), args, arg_paths),
                TypeDef::Enum(_) => {
                    self.read_scalar(&elem_name, &elem_ty, Some(idx), &Attrs::default(), &[], &[]).map(|_| ())
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
        arg_paths: &[Option<String>],
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
        let self_path = self.path_prefix();
        self.current_scope_mut().path_aliases.insert("this".to_owned(), self_path);
        for (i, param) in s.params.iter().enumerate() {
            if let Some(value) = args.get(i) {
                self.current_scope_mut().vars.insert(param.name.clone(), value.clone());
            }
            let is_struct_ty = matches!(self.types.get(&param.ty.name), Some(TypeDef::Struct(_)));
            if (param.is_ref || is_struct_ty)
                && let Some(Some(path)) = arg_paths.get(i)
            {
                self.current_scope_mut()
                    .path_aliases
                    .insert(param.name.clone(), path.clone());
            }
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
                // Short-circuit `&&` / `||` so the right operand isn't
                // evaluated when the left already determines the
                // result. Templates rely on this for guards like
                // `exists(h.x) && h.x > 0` where the right side would
                // raise an UndefinedName when the field is absent.
                if matches!(op, BinOp::LogicalAnd | BinOp::LogicalOr) {
                    let l = self.eval(lhs)?;
                    let l_truthy = l.is_truthy();
                    if matches!(op, BinOp::LogicalAnd) && !l_truthy {
                        return Ok(Value::Bool(false));
                    }
                    if matches!(op, BinOp::LogicalOr) && l_truthy {
                        return Ok(Value::Bool(true));
                    }
                    let r = self.eval(rhs)?;
                    return Ok(Value::Bool(r.is_truthy()));
                }
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
                    // First try as a type name (`sizeof(MyStruct)`).
                    // If it's not a registered type, fall through and
                    // treat the arg as a variable / field reference --
                    // 010 lets `sizeof(foo)` return the byte size of a
                    // previously-read field.
                    if self.types.contains_key(arg_name) {
                        let bytes = self.sizeof_type(arg_name)?;
                        return Ok(Value::UInt { value: bytes as u128, kind: PrimKind::u64() });
                    }
                    if let Some(bytes) = self.field_byte_length(arg_name) {
                        return Ok(Value::UInt { value: bytes as u128, kind: PrimKind::u64() });
                    }
                }
                // `startof(field)` returns the byte offset where a
                // previously-read field landed in the source. Same
                // ident-vs-expr dance as `sizeof`.
                if name == "startof"
                    && let [Expr::Ident { name: arg_name, .. }] = args.as_slice()
                    && let Some(off) = self.field_byte_offset(arg_name)
                {
                    return Ok(Value::UInt { value: off as u128, kind: PrimKind::u64() });
                }
                // `parentof(X)` returns a path placeholder. The path
                // is what callers actually use (build_path picks it
                // up); the returned value is just a marker so the
                // call expression doesn't trip a type error.
                if name == "parentof" && args.len() == 1 {
                    return Ok(Value::Void);
                }
                // `exists(field)`: probe whether the named field was
                // ever stored. Has to run before generic arg eval so
                // a missing path doesn't trip an `UndefinedName` /
                // `UnresolvedMember` error before `exists` ever sees
                // it. We don't model 010's `function_exists` here --
                // templates that pass a function name fall through to
                // the builtin's value-based check.
                if name == "exists" && args.len() == 1 {
                    if let Some(path) = self.build_path(&args[0])? {
                        let prefix = self.path_prefix();
                        let found_field = lookup_candidates(&path, &prefix)
                            .into_iter()
                            .any(|c| self.field_storage.contains_key(&c));
                        let found_array = lookup_candidates(&path, &prefix)
                            .into_iter()
                            .any(|c| self.array_storage.contains_key(&c));
                        if found_field || found_array {
                            return Ok(Value::Bool(true));
                        }
                        // Path could be built but nothing's stored:
                        // unambiguous "not present".
                        return Ok(Value::Bool(false));
                    }
                    // Non-path argument: fall through to value check.
                }
                // `function_exists(name)`: 010 returns whether a
                // user-defined function with that name was declared.
                // Names of builtins also report as existing.
                if name == "function_exists" && args.len() == 1 {
                    let fn_name = match &args[0] {
                        Expr::Ident { name, .. } => name.clone(),
                        Expr::StringLit { value, .. } => value.clone(),
                        _ => return Ok(Value::Bool(false)),
                    };
                    let exists = self.functions.contains_key(&fn_name);
                    return Ok(Value::Bool(exists));
                }
                // `SScanf(src, fmt, out_lvalues...)`: 010 parses `src`
                // against the printf-style format and writes each
                // captured value into the corresponding out lvalue.
                // We intercept before generic eval so the destination
                // names survive (the generic path would only see their
                // current values). Returns the count of fields filled.
                if name == "SScanf" && args.len() >= 3 {
                    let src = self.eval(&args[0])?;
                    let fmt = self.eval(&args[1])?;
                    let src_str = value_to_display(&src);
                    let fmt_str = value_to_display(&fmt);
                    let mut filled = 0u128;
                    let mut s = src_str.as_str();
                    let mut out_iter = args[2..].iter();
                    let mut chars = fmt_str.chars().peekable();
                    while let Some(c) = chars.next() {
                        if c != '%' {
                            // Non-format chars must match literally;
                            // mismatch ends the scan early (sscanf
                            // semantics).
                            s = s.trim_start_matches([' ', '\t', '\n']);
                            if !s.starts_with(c) {
                                break;
                            }
                            s = &s[c.len_utf8()..];
                            continue;
                        }
                        // Skip width / length modifiers (digits, 'l',
                        // 'L', 'h'), pick up the conversion letter.
                        let mut conv = None;
                        for nc in chars.by_ref() {
                            if nc.is_ascii_digit() || nc == 'l' || nc == 'L' || nc == 'h' {
                                continue;
                            }
                            conv = Some(nc);
                            break;
                        }
                        let Some(conv) = conv else { break };
                        let Some(out_arg) = out_iter.next() else { break };
                        let Expr::Ident { name: out_name, .. } = out_arg else { break };
                        s = s.trim_start_matches([' ', '\t', '\n']);
                        match conv {
                            'd' | 'i' => {
                                let end = s
                                    .find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '+'))
                                    .unwrap_or(s.len());
                                let Ok(n) = s[..end].parse::<i64>() else { break };
                                self.store_ident(out_name, Value::SInt { value: n as i128, kind: PrimKind::i64() })?;
                                s = &s[end..];
                                filled += 1;
                            }
                            'u' => {
                                let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
                                let Ok(n) = s[..end].parse::<u64>() else { break };
                                self.store_ident(out_name, Value::UInt { value: n as u128, kind: PrimKind::u64() })?;
                                s = &s[end..];
                                filled += 1;
                            }
                            'x' | 'X' => {
                                let s2 = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
                                let end = s2
                                    .find(|c: char| !c.is_ascii_hexdigit())
                                    .unwrap_or(s2.len());
                                let Ok(n) = u64::from_str_radix(&s2[..end], 16) else { break };
                                self.store_ident(out_name, Value::UInt { value: n as u128, kind: PrimKind::u64() })?;
                                s = &s2[end..];
                                filled += 1;
                            }
                            's' => {
                                let end = s
                                    .find(|c: char| c == ' ' || c == '\t' || c == '\n')
                                    .unwrap_or(s.len());
                                self.store_ident(out_name, Value::Str(s[..end].to_owned()))?;
                                s = &s[end..];
                                filled += 1;
                            }
                            'f' | 'g' | 'e' => {
                                let end = s
                                    .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E'))
                                    .unwrap_or(s.len());
                                let Ok(n) = s[..end].parse::<f64>() else { break };
                                self.store_ident(out_name, Value::Float { value: n, kind: PrimKind::f64() })?;
                                s = &s[end..];
                                filled += 1;
                            }
                            _ => break,
                        }
                    }
                    return Ok(Value::SInt { value: filled as i128, kind: PrimKind::i32() });
                }
                // `SPrintf(dest, fmt, args...)`: 010 writes the
                // formatted string into the lvalue `dest`. We need
                // its name (not its current value) so the write goes
                // back into scope, hence this pre-eval intercept.
                if name == "SPrintf"
                    && let Some(Expr::Ident { name: dest, .. }) = args.first()
                    && args.len() >= 2
                {
                    let fmt = self.eval(&args[1]).map(|v| value_to_display(&v)).unwrap_or_default();
                    let mut rest: Vec<Value> = Vec::with_capacity(args.len().saturating_sub(2));
                    for a in &args[2..] {
                        rest.push(self.eval(a)?);
                    }
                    let rest_refs: Vec<&Value> = rest.iter().collect();
                    let formatted = format_printf(&fmt, &rest_refs);
                    let dest_name = dest.clone();
                    let len = formatted.len() as u128;
                    self.store_ident(&dest_name, Value::Str(formatted))?;
                    return Ok(Value::UInt { value: len, kind: PrimKind::u64() });
                }
                // `ReadBytes(dest, offset, count)` writes into the
                // destination buffer in the caller's scope. We need
                // the destination's *name* (not its current value),
                // so this has to run before the generic arg eval.
                if name == "ReadBytes"
                    && let Some(Expr::Ident { name: dest, .. }) = args.first()
                    && args.len() >= 3
                {
                    let offset = self.eval(&args[1])?.to_i128().unwrap_or(0).max(0) as u64;
                    let count = self.eval(&args[2])?.to_i128().unwrap_or(0).max(0) as u64;
                    let bytes = self.cursor.read_at(offset, count)?;
                    let dest_name = dest.clone();
                    let mut buf = match self.lookup_ident(&dest_name).unwrap_or(Value::Void) {
                        Value::Str(s) => s.into_bytes(),
                        _ => Vec::new(),
                    };
                    if buf.len() < bytes.len() {
                        buf.resize(bytes.len(), 0);
                    }
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    let new_val = Value::Str(String::from_utf8_lossy(&buf).into_owned());
                    self.store_ident(&dest_name, new_val)?;
                    return Ok(Value::Void);
                }
                // For user-defined functions whose params are passed
                // by reference, capture the *path* of the argument
                // before generic eval (which discards path info).
                // Used by `<read=fn>`-style helpers that walk struct
                // fields via a `&ref` parameter:
                //   `string ReadFoo(Foo &x) { return x.field; }`
                // We bind `x` to the original storage path so member
                // lookups inside the function body reach the real
                // record.
                let aliases = self.collect_call_aliases(name, args)?;
                let evaluated: Vec<Value> = args.iter().map(|a| self.eval(a)).collect::<Result<_, _>>()?;
                self.call_named_with_aliases(name, &evaluated, aliases)
            }
            Expr::Member { .. } | Expr::Index { .. } => {
                // Try path-based lookup first. When the interpreter
                // is in the middle of reading a struct body, the
                // current path prefix (`chunk[0]`) should scope
                // sibling field lookups: `type.cname` inside that
                // body resolves to `chunk[0].type.cname` in storage.
                if let Some(path) = self.build_path(expr)? {
                    // For Index expressions, decode the array element
                    // first. The zero-stripped fallback used for
                    // single-occurrence struct counters (`chunks[0]`
                    // stored as `chunks`) would otherwise return the
                    // whole `Str` payload of an array under the bare
                    // path -- so `magic.ver[0]` would give the
                    // 3-char string instead of the first char.
                    if matches!(expr, Expr::Index { .. })
                        && let Some((base, idx)) = split_trailing_index(&path)
                    {
                        for candidate in lookup_candidates(base, &self.path_prefix()) {
                            if let Some(span) = self.array_storage.get(&candidate).cloned() {
                                return self.decode_array_element(&span, idx);
                            }
                        }
                    }
                    let mut storage_void: Option<()> = None;
                    for candidate in lookup_candidates(&path, &self.path_prefix()) {
                        if let Some(v) = self.field_storage.get(&candidate).cloned() {
                            // Local variables seed field_storage with
                            // Void at decl time; subsequent `=`
                            // assignments only update the scope chain
                            // (so we don't have to track which scope
                            // owns the path). When path lookup hits
                            // Void, fall back to a scope-based lookup
                            // by leaf name so the live value wins
                            // over the stale Void placeholder.
                            if matches!(v, Value::Void) {
                                storage_void = Some(());
                                continue;
                            }
                            return Ok(v);
                        }
                    }
                    if storage_void.is_some()
                        && let Some(leaf) = path.rsplit('.').next()
                        && let Ok(v) = self.lookup_ident(leaf)
                        && !matches!(v, Value::Void)
                    {
                        return Ok(v);
                    }
                    // Member access on a primitive-array path that
                    // wasn't stored as a scalar (`SecHeader.Name`
                    // when Name is `BYTE Name[8]`). Decode the whole
                    // span as a UTF-8 string so equality and
                    // string-comparison expressions see meaningful
                    // bytes instead of falling through to the
                    // bare-name fallback.
                    if matches!(expr, Expr::Member { .. }) {
                        for candidate in lookup_candidates(&path, &self.path_prefix()) {
                            if let Some(span) = self.array_storage.get(&candidate).cloned() {
                                let total = span.count.saturating_mul(span.prim.width as u64);
                                let bytes = self.cursor.read_at(span.source_offset, total)?;
                                return Ok(Value::Str(String::from_utf8_lossy(&bytes).into_owned()));
                            }
                        }
                    }
                    // The path resolves to a struct -- no scalar
                    // value is stored at the top key, but child
                    // fields exist under it. Return Void so callers
                    // that pass the struct by reference (and look up
                    // members through the alias) see a placeholder
                    // instead of a hard `UndefinedName`.
                    let prefix = self.path_prefix();
                    let has_children = lookup_candidates(&path, &prefix).into_iter().any(|c| {
                        let probe = format!("{c}.");
                        self.field_storage.keys().any(|k| k.starts_with(&probe))
                            || self.array_storage.keys().any(|k| k.starts_with(&probe))
                    });
                    if has_children {
                        return Ok(Value::Void);
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
                    Expr::Index { target, index, .. } => {
                        // Slice into a string-valued ident: `name[i]`
                        // where `name` resolves to a `Value::Str`.
                        // 010's char-array semantics let templates
                        // index into a `char[]` field by ordinal
                        // (tar's `OctalStrToInt(char str[])` does
                        // `str[10-i] - 0x30` on the digits).
                        let target_value = if let Expr::Ident { name, .. } = &**target {
                            self.lookup_ident(name)?
                        } else {
                            let target_path = self.build_path(target)?.unwrap_or_default();
                            return Err(RuntimeError::UnresolvedIndex { target: target_path });
                        };
                        let idx_value = self.eval(index)?;
                        let idx = idx_value.to_i128().ok_or_else(|| {
                            RuntimeError::Type(format!(
                                "array index is not numeric: {idx_value:?}"
                            ))
                        })?;
                        if let Value::Str(s) = &target_value {
                            let bytes = s.as_bytes();
                            if idx < 0 || (idx as usize) >= bytes.len() {
                                return Err(RuntimeError::Type(format!(
                                    "string index out of range: {idx} >= {}",
                                    bytes.len()
                                )));
                            }
                            return Ok(Value::Char {
                                value: bytes[idx as usize] as u32,
                                kind: PrimKind::char(),
                            });
                        }
                        // Other Value variants don't have an
                        // index-into operation; fall back to the
                        // whole value (matches the previous
                        // behaviour).
                        Ok(target_value)
                    }
                    _ => unreachable!(),
                }
            }
            Expr::Assign { op, target, value, .. } => self.exec_assign(*op, target, value),
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
        // Path aliases (`this`, ref params bound at call entry) have
        // no scalar value of their own; the path itself is what
        // matters for member lookups. Return Void so callers that
        // pass them through expressions (e.g. `findTable(this, "...")`)
        // don't trip an UndefinedName here.
        if self.resolve_path_alias(name).is_some() {
            return Ok(Value::Void);
        }
        // Struct fields declared inside a void function (010's
        // MachO.bt-style \`parse_symbol_table\` declares
        // `Symbols symbols(...)` inside the helper) leave their
        // child entries in field_storage but no scalar at the top
        // key. Return Void so a later `Imports imports(symbols, ...)`
        // arg eval doesn't trip UndefinedName -- collect_call_aliases
        // then qualifies `symbols` to the storage path so member
        // lookups inside the call find the real fields.
        let prefix = self.path_prefix();
        let has_children = lookup_candidates(name, &prefix).into_iter().any(|c| {
            let probe = format!("{c}.");
            self.field_storage.keys().any(|k| k.starts_with(&probe))
                || self.array_storage.keys().any(|k| k.starts_with(&probe))
        });
        if has_children {
            return Ok(Value::Void);
        }
        // Fall back to persistent field storage. Walk the current
        // path prefix from the leaf upward so a bare reference
        // like `cbCFFolder` resolves to a sibling stored under the
        // enclosing struct (`cabFile.cffolder.cbCFFolder`) without
        // the template having to spell out the chain.
        for candidate in lookup_candidates(name, &self.path_prefix()) {
            if let Some(v) = self.field_storage.get(&candidate) {
                return Ok(v.clone());
            }
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
            Expr::Ident { name, .. } => {
                // Substitute the storage path when `name` is a ref
                // alias. Inside `string ReadFoo(Foo &x) { ... }`
                // a mention of `x.field` builds the path
                // `<actual>.field` instead of the literal `x.field`.
                if let Some(alias) = self.resolve_path_alias(name) {
                    return Ok(Some(alias));
                }
                Ok(Some(name.clone()))
            }
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
            // `parentof(X)` returns the storage path of X's parent
            // struct -- one segment shorter than X's own path.
            // bplist.bt walks back to the trailer offset with
            // `parentof(this).offsetTableOffset` and similar.
            Expr::Call { callee, args, .. }
                if matches!(&**callee, Expr::Ident { name, .. } if name == "parentof")
                    && args.len() == 1 =>
            {
                let Some(child) = self.build_path(&args[0])? else { return Ok(None) };
                Ok(Some(parent_path(&child)))
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

    fn exec_assign(
        &mut self,
        op: crate::ast::AssignOp,
        target: &Expr,
        value: &Expr,
    ) -> Result<Value, RuntimeError> {
        let rhs = self.eval(value)?;
        match target {
            Expr::Ident { name, .. } => {
                let new_val = match op {
                    crate::ast::AssignOp::Assign => rhs,
                    other => {
                        let current = self.lookup_ident(name)?;
                        let bin_op = compound_to_bin(other);
                        eval_binary(bin_op, &current, &rhs)?
                    }
                };
                self.store_ident(name, new_val.clone())?;
                Ok(new_val)
            }
            Expr::Index { target: arr, index, .. } => {
                let idx = self.eval(index)?.to_i128().ok_or_else(|| {
                    RuntimeError::Type("indexed assignment: index is not numeric".into())
                })?;
                if idx < 0 {
                    return Err(RuntimeError::Type(format!(
                        "indexed assignment: negative index {idx}"
                    )));
                }
                // Char-buffer mutation: `local char buf[N]; buf[k] = 0;`.
                // The local is a `Value::Str` whose bytes we patch in
                // place so later reads see the NUL terminator.
                if let Expr::Ident { name, .. } = &**arr
                    && let Ok(Value::Str(s)) = self.lookup_ident(name)
                {
                    let mut bytes = s.into_bytes();
                    let i = idx as usize;
                    if i >= bytes.len() {
                        bytes.resize(i + 1, 0);
                    }
                    let byte = match op {
                        crate::ast::AssignOp::Assign => rhs.to_i128().unwrap_or(0) as u8,
                        other => {
                            let cur = bytes[i] as i128;
                            let bin_op = compound_to_bin(other);
                            let folded = eval_binary(
                                bin_op,
                                &Value::SInt { value: cur, kind: PrimKind::i32() },
                                &rhs,
                            )?;
                            folded.to_i128().unwrap_or(0) as u8
                        }
                    };
                    bytes[i] = byte;
                    let new_str = String::from_utf8_lossy(&bytes).into_owned();
                    self.store_ident(name, Value::Str(new_str))?;
                    return Ok(Value::UInt { value: byte as u128, kind: PrimKind::u8() });
                }
                // General case (`local <enum> NAMES[N]; NAMES[k] = ...`):
                // route through field_storage under `name[k]`. The
                // existing index-read path already finds those keys.
                let Some(base_path) = self.build_path(arr)? else {
                    return Err(RuntimeError::Type("indexed assignment: unresolvable base".into()));
                };
                let key = format!("{base_path}[{idx}]");
                let new_val = match op {
                    crate::ast::AssignOp::Assign => rhs,
                    other => {
                        let cur = self.field_storage.get(&key).cloned().unwrap_or(Value::Void);
                        let bin_op = compound_to_bin(other);
                        eval_binary(bin_op, &cur, &rhs)?
                    }
                };
                self.field_storage.insert(key, new_val.clone());
                Ok(new_val)
            }
            Expr::Member { target: obj, field, .. } => {
                // `obj.field = v`: store at the resolved path so later
                // reads via the same chain find it. Also seed the
                // bare-name lookup so 010's path-insensitive idioms
                // (which read `field` directly after writing it under
                // `obj.field`) keep working.
                let Some(base_path) = self.build_path(obj)? else {
                    return Err(RuntimeError::Type("member assignment: unresolvable base".into()));
                };
                let key = format!("{base_path}.{field}");
                let new_val = match op {
                    crate::ast::AssignOp::Assign => rhs,
                    other => {
                        let cur = self
                            .field_storage
                            .get(&key)
                            .cloned()
                            .or_else(|| self.lookup_ident(field).ok())
                            .unwrap_or(Value::Void);
                        let bin_op = compound_to_bin(other);
                        eval_binary(bin_op, &cur, &rhs)?
                    }
                };
                self.field_storage.insert(key, new_val.clone());
                Ok(new_val)
            }
            other => Err(RuntimeError::Type(format!(
                "unsupported assignment target: {other:?}"
            ))),
        }
    }

    fn call_named_with_aliases(
        &mut self,
        name: &str,
        args: &[Value],
        aliases: Vec<(String, String)>,
    ) -> Result<Value, RuntimeError> {
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
        for (param_name, real_path) in aliases {
            self.current_scope_mut().path_aliases.insert(param_name, real_path);
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

    /// Build the parameter -> path alias list for a user function
    /// call. Each ref-typed (or struct-typed) parameter whose
    /// matching argument resolves to a storage path gets an alias
    /// entry. Returns an empty list for builtins or when the function
    /// isn't user-defined; the caller then takes the no-alias path.
    fn collect_call_aliases(
        &mut self,
        name: &str,
        args: &[Expr],
    ) -> Result<Vec<(String, String)>, RuntimeError> {
        let Some(func) = self.functions.get(name).cloned() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for (param, arg) in func.params.iter().zip(args.iter()) {
            // Only stash aliases for ref params and struct-typed
            // params; plain scalar args go through unmodified so a
            // mutated alias doesn't leak back into the caller.
            let is_struct_ty = matches!(self.types.get(&param.ty.name), Some(TypeDef::Struct(_)));
            if !param.is_ref && !is_struct_ty {
                continue;
            }
            if let Some(path) = self.build_path(arg)? {
                // Resolve the path against the current prefix so the
                // alias is fully qualified -- the function body runs
                // with no prefix of its own.
                let qualified = lookup_candidates(&path, &self.path_prefix())
                    .into_iter()
                    .find(|c| {
                        self.field_storage.contains_key(c)
                            || self.array_storage.contains_key(c)
                            || self.field_storage.keys().any(|k| k.starts_with(&format!("{c}.")))
                            || self.array_storage.keys().any(|k| k.starts_with(&format!("{c}.")))
                    })
                    .unwrap_or(path);
                out.push((param.name.clone(), qualified));
            }
        }
        Ok(out)
    }

    /// Look up the alias for `name` in the current scope (innermost
    /// first). Returns the underlying storage path when the name was
    /// bound as a ref param at call entry.
    fn resolve_path_alias(&self, name: &str) -> Option<String> {
        for scope in self.scopes.iter().rev() {
            if let Some(p) = scope.path_aliases.get(name) {
                return Some(p.clone());
            }
        }
        None
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
            // 010 has both `Printf` and `Warning`. Templates in the
            // wild occasionally typo `printf` (lowercase) and `Waring`
            // -- accept both as aliases so a typo doesn't stop the
            // template from running.
            "printf" => self.call_builtin("Printf", args),
            "Waring" => self.call_builtin("Warning", args),
            "FSkip" => {
                let n = args.first().and_then(|v| v.to_i128()).unwrap_or(0);
                let target = self.cursor.tell() as i128 + n;
                self.cursor.seek(target.max(0) as u64);
                Ok(Some(Value::Void))
            }
            "Assert" => {
                // `Assert(cond)` / `Assert(cond, msg)`. Surface a
                // diagnostic and halt the run on failure -- 010
                // itself raises a hard error here, so doing the same
                // catches buggy templates rather than producing a
                // partial tree the user has to second-guess.
                let cond_truthy = args.first().is_some_and(Value::is_truthy);
                if cond_truthy {
                    return Ok(Some(Value::Void));
                }
                let msg = args.get(1).map(value_to_display).unwrap_or_else(|| "assertion failed".to_owned());
                self.diagnostics.push(Diagnostic {
                    message: format!("Assert: {msg}"),
                    severity: Severity::Error,
                    file_offset: Some(self.cursor.tell()),
                    template_line: None,
                });
                Err(RuntimeError::Type(format!("Assert failed: {msg}")))
            }
            // Bitfield padding pragmas are presentational. The
            // interpreter packs bits the way 010 does by default;
            // padding is a renderer concern we don't model.
            "BitfieldDisablePadding" => {
                self.bitfield_padding_disabled = true;
                self.bitfield_slot = None;
                Ok(Some(Value::Void))
            }
            "BitfieldEnablePadding" => {
                self.bitfield_padding_disabled = false;
                self.bitfield_slot = None;
                Ok(Some(Value::Void))
            }
            "ReadBytes" => Ok(Some(self.read_bytes_builtin(args)?)),
            "ReadFloat" => Ok(Some(self.read_float_builtin(args, false)?)),
            "ReadDouble" => Ok(Some(self.read_float_builtin(args, true)?)),
            "ReadString" => Ok(Some(self.read_string_builtin(args, false)?)),
            "ReadStringLength" => Ok(Some(self.read_string_length_builtin(args, false)?)),
            "ReadWString" => Ok(Some(self.read_string_builtin(args, true)?)),
            "ReadWStringLength" => Ok(Some(self.read_string_length_builtin(args, true)?)),
            // String / number conversion helpers. We don't model
            // wide strings as a distinct type -- everything is utf-8
            // -- so the W* variants degrade to their narrow
            // counterparts.
            "Atoi" | "BinaryStrToInt" => {
                let s = args.first().map(value_to_display).unwrap_or_default();
                let radix = if matches!(name, "BinaryStrToInt") { 2 } else { 10 };
                let v = i64::from_str_radix(s.trim(), radix).unwrap_or(0);
                Ok(Some(Value::SInt { value: v as i128, kind: PrimKind::i64() }))
            }
            "Atof" => {
                let s = args.first().map(value_to_display).unwrap_or_default();
                let v = s.trim().parse::<f64>().unwrap_or(0.0);
                Ok(Some(Value::Float { value: v, kind: PrimKind::f64() }))
            }
            "IntToBinaryStr" => {
                let v = args.first().and_then(|v| v.to_i128()).unwrap_or(0) as u64;
                let bits = args.get(1).and_then(|v| v.to_i128()).unwrap_or(64).clamp(1, 64) as u32;
                let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
                let masked = v & mask;
                Ok(Some(Value::Str(format!("{masked:0bits$b}", bits = bits as usize))))
            }
            "Str" | "str" | "WStringToString" | "StringToWString" => {
                Ok(Some(Value::Str(args.first().map(value_to_display).unwrap_or_default())))
            }
            // `Strcmp(a, b)` -> negative / 0 / positive, matching
            // libc. `Stricmp` is the case-insensitive version. The
            // counted variants (`Strncmp`, `Strnicmp`) clamp to `n`
            // chars before comparing.
            "Strcmp" | "Stricmp" | "Strncmp" | "Strnicmp" | "WStrcmp" | "WStricmp" | "WStrncmp" | "WStrnicmp"
            | "Memcmp" => {
                Ok(Some(Value::SInt { value: self.strcmp_builtin(name, args) as i128, kind: PrimKind::i32() }))
            }
            "Strlen" | "WStrlen" => {
                let s = args.first().map(value_to_display).unwrap_or_default();
                Ok(Some(Value::UInt { value: s.chars().count() as u128, kind: PrimKind::u64() }))
            }
            "SubStr" => {
                let s = args.first().map(value_to_display).unwrap_or_default();
                let start = args.get(1).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as usize;
                let len = args.get(2).and_then(|v| v.to_i128()).unwrap_or(-1);
                let chars: Vec<char> = s.chars().collect();
                let start = start.min(chars.len());
                let end = if len < 0 { chars.len() } else { start.saturating_add(len as usize).min(chars.len()) };
                Ok(Some(Value::Str(chars[start..end].iter().collect())))
            }
            "StrDel" => {
                let s = args.first().map(value_to_display).unwrap_or_default();
                let start = args.get(1).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as usize;
                let len = args.get(2).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as usize;
                let chars: Vec<char> = s.chars().collect();
                let start = start.min(chars.len());
                let end = start.saturating_add(len).min(chars.len());
                let mut out: String = chars[..start].iter().collect();
                out.extend(chars[end..].iter());
                Ok(Some(Value::Str(out)))
            }
            "Strncpy" | "Strcpy" | "WStrcpy" | "WStrncpy" | "Memcpy" => {
                // 010's signature mutates the first arg, which we
                // don't have lvalue support for. Best effort: echo
                // the source string back so chained calls keep
                // running.
                Ok(Some(Value::Str(args.get(1).map(value_to_display).unwrap_or_default())))
            }
            "IsCharAlpha" => Ok(Some(Value::Bool(char_predicate(args, char::is_alphabetic)))),
            "IsCharAlphaNumeric" => Ok(Some(Value::Bool(char_predicate(args, char::is_alphanumeric)))),
            "IsCharNum" | "IsCharDigit" => Ok(Some(Value::Bool(char_predicate(args, |c| c.is_ascii_digit())))),
            "IsCharWhitespace" => Ok(Some(Value::Bool(char_predicate(args, char::is_whitespace)))),
            "ToUpper" => Ok(Some(Value::Str(args.first().map(value_to_display).unwrap_or_default().to_uppercase()))),
            "ToLower" => Ok(Some(Value::Str(args.first().map(value_to_display).unwrap_or_default().to_lowercase()))),
            // Math helpers. Only the simple ones templates actually
            // call -- full libm is out of scope.
            "Min" => Ok(Some(min_max_builtin(args, true))),
            "Max" => Ok(Some(min_max_builtin(args, false))),
            "Abs" => {
                let v = args.first().and_then(|v| v.to_i128()).unwrap_or(0);
                Ok(Some(Value::SInt { value: v.abs(), kind: PrimKind::i64() }))
            }
            "Pow" => {
                let base = args.first().and_then(|v| v.to_f64()).unwrap_or(0.0);
                let exp = args.get(1).and_then(|v| v.to_f64()).unwrap_or(0.0);
                Ok(Some(Value::Float { value: base.powf(exp), kind: PrimKind::f64() }))
            }
            "Sqrt" => {
                let v = args.first().and_then(|v| v.to_f64()).unwrap_or(0.0);
                Ok(Some(Value::Float { value: v.sqrt(), kind: PrimKind::f64() }))
            }
            "Ceil" => {
                let v = args.first().and_then(|v| v.to_f64()).unwrap_or(0.0);
                Ok(Some(Value::Float { value: v.ceil(), kind: PrimKind::f64() }))
            }
            "Floor" => {
                let v = args.first().and_then(|v| v.to_f64()).unwrap_or(0.0);
                Ok(Some(Value::Float { value: v.floor(), kind: PrimKind::f64() }))
            }
            "Round" => {
                let v = args.first().and_then(|v| v.to_f64()).unwrap_or(0.0);
                Ok(Some(Value::Float { value: v.round(), kind: PrimKind::f64() }))
            }
            // UI / editor state queries. Headless interpretation has
            // no editor state, so each of these returns a benign
            // sentinel: zeros for offsets / sizes, an empty string
            // for the file name. Templates that branch on these
            // typically have a fallback path.
            "GetCursorPos" | "GetSelStart" => Ok(Some(Value::UInt { value: 0, kind: PrimKind::u64() })),
            "GetSelSize" => Ok(Some(Value::UInt { value: 0, kind: PrimKind::u64() })),
            "GetFileName" | "GetFileNameW" | "GetFilePath" => Ok(Some(Value::Str(String::new()))),
            // UI side-effect builtins -- output panes, status bars,
            // input dialogs. Templates that call these expect the
            // user to react; in headless mode they're no-ops.
            "OutputPaneClear" | "StatusMessage" | "ClearOutput" | "FPrintf" | "RunTemplate" => Ok(Some(Value::Void)),
            "InputRadioButtonBox"
            | "InputDirectory"
            | "InputDouble"
            | "InputFloat"
            | "InputNumber"
            | "InputOpenFileName"
            | "InputSaveFileName"
            | "InputString"
            | "MessageBox" => {
                // Return a "user cancelled" sentinel for input dialogs:
                // empty string / zero. Templates that gate on input
                // tend to early-exit when they see the sentinel.
                Ok(Some(Value::Str(String::new())))
            }
            "TimeTToString" | "FileTimeToString" | "OleTimeToString" | "DOSTimeToString" | "DOSDateToString"
            | "GUIDToString" => {
                // Stringification of various date/time and GUID
                // primitives. Templates use these for display only;
                // a hex-style round-trip of the underlying integer
                // is good enough for headless runs.
                let v = args.first().and_then(|v| v.to_i128()).unwrap_or(0);
                Ok(Some(Value::Str(format!("0x{:X}", v as u128))))
            }
            "ChecksumAlgArrayStr" | "ChecksumAlgStr" => {
                // Return the algorithm tag literally so templates
                // that round-trip it through `Printf` still produce
                // recognizable diagnostics.
                Ok(Some(Value::Str(args.first().map(value_to_display).unwrap_or_default())))
            }
            "SScanf" | "SScan" => {
                // Best-effort SScanf: reports zero matches. Real
                // SScanf requires lvalue out-params we don't model.
                Ok(Some(Value::SInt { value: 0, kind: PrimKind::i32() }))
            }
            // Endian-state queries. Track the current setting so
            // `if (IsBigEndian()) ...` flows the right branch even
            // for templates that toggle between sections.
            "IsLittleEndian" => Ok(Some(Value::Bool(matches!(self.endian, Endian::Little)))),
            "IsBigEndian" => Ok(Some(Value::Bool(matches!(self.endian, Endian::Big)))),
            // Buffer mutation builtins. We're a read-only interpreter
            // -- there's no editable backing for `InsertBytes` etc.
            // -- so each one returns a benign zero so calls don't
            // trap. Templates that depend on the side-effect won't
            // produce correct trees, but they won't error out either.
            "InsertBytes" | "DeleteBytes" | "OverwriteBytes" | "WriteString" | "Memset" => Ok(Some(Value::Void)),
            // Bookmark / file-table introspection. Headless = none of
            // these have anything to report.
            "AddBookmark"
            | "GetBookmarkArraySize"
            | "GetBookmarkName"
            | "GetBookmarkPos"
            | "GetBookmarkType"
            | "GetNumBookmarks" => Ok(Some(Value::SInt { value: 0, kind: PrimKind::i64() })),
            "GetArg" | "GetNumArgs" | "GetFileNum" => Ok(Some(Value::SInt { value: 0, kind: PrimKind::i64() })),
            "FileExists" | "FindOpenFile" | "FileSelect" | "FileOpen" | "FileClose" => {
                Ok(Some(Value::SInt { value: 0, kind: PrimKind::i64() }))
            }
            "FileNameGetBase" | "FileNameSetExtension" => {
                Ok(Some(Value::Str(args.first().map(value_to_display).unwrap_or_default())))
            }
            // Style / color queries -- presentational, no headless
            // equivalent. Return 0 so arithmetic on the result keeps
            // working.
            "GetBackColor"
            | "GetForeColor"
            | "SetColor"
            | "SetStyle"
            | "DisasmSetMode"
            | "ThemeAutoScaleColors"
            | "OutputPaneSave" => Ok(Some(Value::SInt { value: 0, kind: PrimKind::i64() })),
            // String search / split. We model strings opaquely, so
            // these answer the most common questions lossily.
            "Strstr" | "WStrstr" => {
                let hay = args.first().map(value_to_display).unwrap_or_default();
                let needle = args.get(1).map(value_to_display).unwrap_or_default();
                let pos = hay.find(&needle).map(|p| p as i64).unwrap_or(-1);
                Ok(Some(Value::SInt { value: pos as i128, kind: PrimKind::i64() }))
            }
            "Strchr" | "WStrchr" => {
                let hay = args.first().map(value_to_display).unwrap_or_default();
                let ch = args.get(1).and_then(|v| v.to_i128()).unwrap_or(0) as u32;
                let pos = match char::from_u32(ch) {
                    Some(c) => hay.find(c).map(|p| p as i64).unwrap_or(-1),
                    None => -1,
                };
                Ok(Some(Value::SInt { value: pos as i128, kind: PrimKind::i64() }))
            }
            "Strcat" => {
                // 010's `Strcat` mutates the first arg; we can't, so
                // return the concatenation as a value. Templates that
                // immediately Print the result behave correctly.
                let a = args.first().map(value_to_display).unwrap_or_default();
                let b = args.get(1).map(value_to_display).unwrap_or_default();
                Ok(Some(Value::Str(format!("{a}{b}"))))
            }
            "WSubStr" => self.call_builtin("SubStr", args),
            "FindAll" => {
                // Returns the count of matches. Without a backing
                // buffer to scan, we report 0 -- callers that branch
                // on the count typically take the empty-result path.
                Ok(Some(Value::SInt { value: 0, kind: PrimKind::i64() }))
            }
            "function_exists" | "exists_function" => {
                let name = args.first().map(value_to_display).unwrap_or_default();
                Ok(Some(Value::Bool(self.functions.contains_key(&name))))
            }
            // Wide / line text helpers from the editor's text-mode
            // file API. Headless reads don't have a line index, so
            // each one is a benign zero.
            "TextGetNumLines" | "TextGetLineSize" | "TextAddressToLine" | "TextLineToAddress" | "ReadLine" => {
                Ok(Some(Value::SInt { value: 0, kind: PrimKind::i64() }))
            }
            "Time64TToString" => self.call_builtin("TimeTToString", args),
            "ToColonHexString" => {
                let v = args.first().and_then(|v| v.to_i128()).unwrap_or(0);
                Ok(Some(Value::Str(format!("{:x}", v as u128))))
            }
            "RegExMatch" => Ok(Some(Value::Bool(false))),
            "SwapBytes" => Ok(Some(args.first().cloned().unwrap_or(Value::Void))),
            _ => Ok(None),
        }
    }

    /// Look up the byte length of a previously-emitted node by name.
    /// Used by `sizeof(field)` -- distinct from `sizeof(TypeName)`,
    /// which routes through [`Self::sizeof_type`].
    fn field_byte_length(&self, name: &str) -> Option<u64> {
        for n in self.nodes.iter().rev() {
            if n.name == name {
                return Some(n.length);
            }
        }
        // Locals don't emit nodes, but we tracked their byte size at
        // declaration so `sizeof(localArr)` works without scanning.
        self.local_array_bytes.get(name).copied()
    }

    /// Look up the source byte offset of a previously-emitted node by
    /// name. Used by `startof(field)`.
    fn field_byte_offset(&self, name: &str) -> Option<u64> {
        for n in self.nodes.iter().rev() {
            if n.name == name {
                return Some(n.offset);
            }
        }
        None
    }

    /// `ReadBytes(out, offset, count)` -- 010 writes into `out`; we
    /// can't, so return a `Str` containing the raw bytes (lossy utf-8)
    /// for callers that just want the buffer for `Printf`-style
    /// inspection. Templates that read into a typed buffer for
    /// downstream use will see degraded behaviour but won't trap.
    fn read_bytes_builtin(&self, args: &[Value]) -> Result<Value, RuntimeError> {
        let offset = args.get(1).and_then(|v| v.to_i128()).unwrap_or_else(|| self.cursor.tell() as i128);
        let count = args.get(2).and_then(|v| v.to_i128()).unwrap_or(0);
        let offset = offset.max(0) as u64;
        let count = count.max(0) as u64;
        let bytes = self.cursor.read_at(offset, count)?;
        Ok(Value::Str(String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// `ReadFloat(offset?)` / `ReadDouble(offset?)`.
    fn read_float_builtin(&self, args: &[Value], double: bool) -> Result<Value, RuntimeError> {
        let offset = match args.first() {
            Some(v) => v.to_i128().unwrap_or(0) as u64,
            None => self.cursor.tell(),
        };
        let width: u8 = if double { 8 } else { 4 };
        let bytes = self.cursor.read_at(offset, width as u64)?;
        decode_prim(&bytes, PrimKind { class: PrimClass::Float, width, signed: true }, self.endian)
    }

    /// `ReadString(offset?, maxLen?)` / `ReadWString(...)`. Reads
    /// until NUL or `maxLen`, defaulting to a generous cap so
    /// pathological templates don't read forever.
    fn read_string_builtin(&self, args: &[Value], wide: bool) -> Result<Value, RuntimeError> {
        let offset = args.first().and_then(|v| v.to_i128()).unwrap_or_else(|| self.cursor.tell() as i128).max(0) as u64;
        let max_len = args.get(1).and_then(|v| v.to_i128()).unwrap_or(READ_STRING_DEFAULT_CAP).max(0) as u64;
        let stride: u64 = if wide { 2 } else { 1 };
        let max_bytes = max_len.saturating_mul(stride);
        let cap = max_bytes.min(self.cursor.len().saturating_sub(offset));
        let raw = self.cursor.read_at(offset, cap)?;
        let s = decode_string(&raw, wide, self.endian);
        Ok(Value::Str(s))
    }

    /// `ReadStringLength(offset, length)` -- read exactly `length`
    /// bytes (or chars, for wide).
    fn read_string_length_builtin(&self, args: &[Value], wide: bool) -> Result<Value, RuntimeError> {
        let offset = args.first().and_then(|v| v.to_i128()).unwrap_or(0).max(0) as u64;
        let length = args.get(1).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as u64;
        let stride: u64 = if wide { 2 } else { 1 };
        let bytes = self.cursor.read_at(offset, length.saturating_mul(stride))?;
        Ok(Value::Str(decode_string(&bytes, wide, self.endian)))
    }

    /// libc-style `strcmp` family. Negative if `a < b`, zero on
    /// equal, positive otherwise. Counted variants honour their
    /// length cap; case-insensitive variants ASCII-fold first.
    fn strcmp_builtin(&self, name: &str, args: &[Value]) -> i32 {
        let a = args.first().map(value_to_display).unwrap_or_default();
        let b = args.get(1).map(value_to_display).unwrap_or_default();
        let case_insensitive = matches!(name, "Stricmp" | "Strnicmp" | "WStricmp" | "WStrnicmp");
        let counted = matches!(name, "Strncmp" | "Strnicmp" | "WStrncmp" | "WStrnicmp" | "Memcmp");
        let (lhs, rhs) = if case_insensitive { (a.to_ascii_lowercase(), b.to_ascii_lowercase()) } else { (a, b) };
        let (lhs_b, rhs_b) = if counted {
            let n = args.get(2).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as usize;
            (&lhs.as_bytes()[..lhs.len().min(n)], &rhs.as_bytes()[..rhs.len().min(n)])
        } else {
            (lhs.as_bytes(), rhs.as_bytes())
        };
        match lhs_b.cmp(rhs_b) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
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
                                Some(Expr::IntLit { value, .. }) => elem.saturating_mul(*value),
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
        let algo_str = matches!(args.first(), Some(Value::Str(_)))
            .then(|| value_to_display(args.first().unwrap()).to_lowercase());
        let start = args.get(1).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as u64;
        let size_arg = args.get(2).and_then(|v| v.to_i128()).unwrap_or(0);
        let source_len = self.cursor.len();
        let size =
            if size_arg <= 0 { source_len.saturating_sub(start) } else { (size_arg as u64).min(source_len - start) };
        let bytes = self.cursor.read_at(start, size)?;
        // Algorithm IDs come from 010's `CHECKSUM_*` constants
        // (CRC32=5, CRC16=6, ADLER32=7). Pass-by-string also works
        // for templates that encode the algo as a literal name.
        let crc32 = algo_raw == CHECKSUM_CRC32_ID as i128
            || matches!(&algo_str, Some(s) if s == "crc32");
        let crc16 = algo_raw == 6 || matches!(&algo_str, Some(s) if s == "crc16");
        let adler32 = algo_raw == 7 || matches!(&algo_str, Some(s) if s == "adler32");
        if crc32 {
            return Ok(Value::UInt { value: crc32_ieee(&bytes) as u128, kind: PrimKind::u32() });
        }
        if adler32 {
            return Ok(Value::UInt { value: adler32_checksum(&bytes) as u128, kind: PrimKind::u32() });
        }
        if crc16 {
            return Ok(Value::UInt { value: crc16_ccitt(&bytes) as u128, kind: PrimKind::u16() });
        }
        self.diagnostics.push(Diagnostic {
            message: format!("Checksum algo {algo_raw} not implemented; returning 0"),
            severity: Severity::Info,
            file_offset: Some(self.cursor.tell()),
            template_line: None,
        });
        Ok(Value::UInt { value: 0, kind: PrimKind::u64() })
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

fn char_value_to_string(value: u32) -> String {
    if value < 0x80 {
        (value as u8 as char).to_string()
    } else {
        char::from_u32(value).map(|c| c.to_string()).unwrap_or_else(|| String::from('\u{FFFD}'))
    }
}

fn eval_binary(op: BinOp, l: &Value, r: &Value) -> Result<Value, RuntimeError> {
    // String equality: 010 templates rely on `type.cname == "IHDR"`
    // for chunk dispatch. Both sides must be strings; mismatched
    // types fall through to the numeric path and blow up, which
    // matches 010's "types must match" semantics.
    if let (Value::Str(a), Value::Str(b)) = (l, r) {
        // Trim at the first NUL before comparison so a fixed-width
        // `char buf[N]` with a shorter NUL-terminated payload still
        // compares equal to its literal counterpart -- 010 templates
        // routinely write `magic != \"Kaydara FBX Binary  \"` against
        // a `char[23]` field whose tail is `\\0\\x1a\\0`. Mirrors C
        // strcmp / 010's own char-array semantics.
        let trim = |s: &str| match s.find('\0') {
            Some(idx) => s[..idx].to_owned(),
            None => s.to_owned(),
        };
        let (ta, tb) = (trim(a), trim(b));
        return Ok(match op {
            BinOp::Eq => Value::Bool(ta == tb),
            BinOp::NotEq => Value::Bool(ta != tb),
            BinOp::Lt => Value::Bool(ta < tb),
            BinOp::Gt => Value::Bool(ta > tb),
            BinOp::LtEq => Value::Bool(ta <= tb),
            BinOp::GtEq => Value::Bool(ta >= tb),
            BinOp::Add => Value::Str(format!("{a}{b}")),
            _ => return Err(RuntimeError::Type(format!("string operand not supported for {op:?}"))),
        });
    }
    // String + char (or char + string) concatenates: 010 templates
    // build sentinel byte sequences with `"Rar!" + '\x1A' + '\x07'`.
    // Comparisons on a string vs. a single-char string also slip in
    // through the same conversion.
    if matches!(op, BinOp::Add | BinOp::Eq | BinOp::NotEq)
        && let Some((a, b)) = match (l, r) {
            (Value::Str(s), Value::Char { value, .. }) => {
                Some((s.clone(), char_value_to_string(*value)))
            }
            (Value::Char { value, .. }, Value::Str(s)) => {
                Some((char_value_to_string(*value), s.clone()))
            }
            _ => None,
        }
    {
        return Ok(match op {
            BinOp::Add => Value::Str(format!("{a}{b}")),
            BinOp::Eq => Value::Bool(a == b),
            BinOp::NotEq => Value::Bool(a != b),
            _ => unreachable!(),
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
    // Past-EOF reads return Void (we'd otherwise hard-fail the
    // run); treating Void as 0 in numeric ops lets the surrounding
    // `if (status & 0x80) ...` branches degrade gracefully so the
    // template can finish whatever loop hit the clamp.
    let li = l.to_i128().or_else(|| matches!(l, Value::Void).then_some(0))
        .ok_or_else(|| RuntimeError::Type(format!("not numeric: {l:?}")))?;
    let ri = r.to_i128().or_else(|| matches!(r, Value::Void).then_some(0))
        .ok_or_else(|| RuntimeError::Type(format!("not numeric: {r:?}")))?;
    // 010 templates routinely mix signed and unsigned integer reads
    // (e.g. `local uint32 magic = ReadInt();` paired with
    // `if (magic == MACHO_64)` against an unsigned enum constant).
    // For equality, compare the raw bit patterns at the narrower of
    // the two operands' widths so a sign-extended SInt and the same
    // bytes loaded as a UInt compare equal.
    if matches!(op, BinOp::Eq | BinOp::NotEq) && let (Some(a), Some(b)) = (int_bits(l), int_bits(r))
    {
        let eq = a == b;
        return Ok(Value::Bool(if matches!(op, BinOp::Eq) { eq } else { !eq }));
    }
    let out = match op {
        BinOp::Add => Value::SInt { value: li.wrapping_add(ri), kind: PrimKind::i64() },
        BinOp::Sub => Value::SInt { value: li.wrapping_sub(ri), kind: PrimKind::i64() },
        BinOp::Mul => Value::SInt { value: li.wrapping_mul(ri), kind: PrimKind::i64() },
        BinOp::Div => Value::SInt {
            value: li.checked_div(ri).ok_or_else(|| RuntimeError::Type("integer divide by zero".into()))?,
            kind: PrimKind::i64(),
        },
        BinOp::Rem => Value::SInt {
            value: li.checked_rem(ri).ok_or_else(|| RuntimeError::Type("integer remainder by zero".into()))?,
            kind: PrimKind::i64(),
        },
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

/// Extract the raw integer bits of `v`, masked down to its declared
/// width. Used by `Eq` / `NotEq` so a 32-bit signed value sign-extended
/// to i128 still compares equal to the same byte pattern read as
/// uint32. Returns `None` for non-integer kinds.
fn int_bits(v: &Value) -> Option<u128> {
    match v {
        Value::UInt { value, kind } => Some(mask_to_width(*value, kind.width)),
        Value::SInt { value, kind } => Some(mask_to_width(*value as u128, kind.width)),
        Value::Char { value, .. } => Some(*value as u128),
        Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        _ => None,
    }
}

fn mask_to_width(v: u128, width: u8) -> u128 {
    if width >= 16 {
        v
    } else {
        let bits = width as u32 * 8;
        let mask = (1u128 << bits) - 1;
        v & mask
    }
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

/// Cap for `ReadString` / `ReadWString` calls that don't pass an
/// explicit length. Templates that read NUL-terminated strings
/// usually live well under this; the cap exists so a malformed
/// source (no NUL anywhere) doesn't read the full file.
const READ_STRING_DEFAULT_CAP: i128 = 4096;

/// Apply `pred` to the first character of the first argument, if any.
/// Used by the `IsCharAlpha` / `IsCharDigit` / etc. builtins.
fn char_predicate(args: &[Value], pred: fn(char) -> bool) -> bool {
    let s = args.first().map(value_to_display).unwrap_or_default();
    s.chars().next().map(pred).unwrap_or(false)
}

/// Return whichever of the two operands is smaller / larger. Falls
/// back to the first arg when typed comparison isn't possible (mixed
/// or non-numeric values); 010 itself is forgiving here.
fn min_max_builtin(args: &[Value], pick_min: bool) -> Value {
    let Some(a) = args.first() else {
        return Value::SInt { value: 0, kind: PrimKind::i64() };
    };
    let Some(b) = args.get(1) else { return a.clone() };
    let cmp = match (a.to_f64(), b.to_f64()) {
        (Some(x), Some(y)) => x.partial_cmp(&y),
        _ => match (a.to_i128(), b.to_i128()) {
            (Some(x), Some(y)) => Some(x.cmp(&y)),
            _ => None,
        },
    };
    match (cmp, pick_min) {
        (Some(std::cmp::Ordering::Less), true) | (Some(std::cmp::Ordering::Greater), false) => a.clone(),
        (Some(_), _) => b.clone(),
        (None, _) => a.clone(),
    }
}

/// Decode a NUL-terminated byte slice as a (lossy) Rust `String`.
/// `wide` interprets the input as 16-bit code units in `endian` byte
/// order; otherwise each byte is a code point (utf-8 lossy).
fn decode_string(bytes: &[u8], wide: bool, endian: Endian) -> String {
    if !wide {
        let trimmed: &[u8] = match bytes.iter().position(|&b| b == 0) {
            Some(idx) => &bytes[..idx],
            None => bytes,
        };
        return String::from_utf8_lossy(trimmed).into_owned();
    }
    let mut units: Vec<u16> = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let pair = match endian {
            Endian::Little => u16::from_le_bytes([chunk[0], chunk[1]]),
            Endian::Big => u16::from_be_bytes([chunk[0], chunk[1]]),
        };
        if pair == 0 {
            break;
        }
        units.push(pair);
    }
    String::from_utf16_lossy(&units)
}

/// Ordered list of `field_storage` keys to try for a
/// `Member`/`Index` lookup. Starts with the most specific match
/// (path exactly as the user typed it), falls back to scoping under
/// the current struct-body prefix, then to forms where `[0]`
/// subscripts are stripped -- since single-occurrence fields are
/// stored without a `[0]` suffix.
fn lookup_candidates(path: &str, prefix: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(8);
    let mut push = |s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    push(path.to_owned());
    push(strip_zero_indices(path));
    // Try the path scoped to the current prefix, then walk up the
    // prefix segment-by-segment so a query like `bmiHeader.biBitCount`
    // can match a sibling stored at `images.bmiHeader.biBitCount`
    // even while the interpreter is busy reading `images.data`.
    let mut current = prefix.to_owned();
    loop {
        let scoped = join_path(&current, path);
        push(scoped.clone());
        push(strip_zero_indices(&scoped));
        if current.is_empty() {
            break;
        }
        // Drop the last `.segment` (or the whole string if no dot).
        match current.rfind('.') {
            Some(idx) => current.truncate(idx),
            None => current.clear(),
        }
    }
    out
}

/// Drop the `[N]` suffix from each segment that's still followed by
/// another segment (`.`). Templates that walk loop-built records via
/// the bare name (`patch.pOffset.bOffset` after a few `PATCHCHUNK
/// patch;` iterations) expect to see the latest record, matching
/// 010's "single slot, last write wins" model. We keep the trailing
/// bracket -- `arr[2]` at the leaf is an explicit element index, not
/// a struct counter -- so explicit `arr[N]` queries still hit the
/// per-element decode.
/// Drop the last `.segment` of a path. `parentof(this)` resolves to
/// the enclosing struct's storage key by trimming the leaf segment
/// from the current path. Returns an empty string for top-level
/// records that have no parent.
fn parent_path(path: &str) -> String {
    match path.rfind('.') {
        Some(idx) => path[..idx].to_owned(),
        None => String::new(),
    }
}

fn strip_indexed_segments(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'['
            && let Some(rel) = path[i..].find(']')
        {
            let inner = &path[i + 1..i + rel];
            let close = i + rel;
            let after_close = close + 1;
            // Strip only when the bracket is followed by another
            // path segment (`.`), and the contents look like an
            // unsigned integer.
            if inner.bytes().all(|b| b.is_ascii_digit())
                && after_close < bytes.len()
                && bytes[after_close] == b'.'
            {
                i = after_close;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
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
/// Split a path ending in `[N]` into its base and the parsed index.
/// Returns `None` if the path doesn't end with a closing bracket or
/// the bracket contents aren't a valid unsigned integer. Used by the
/// lazy primitive-array indexing path to find the parent span for a
/// query like `frData[5]` or `entries[3].deFileName[2]`.
fn split_trailing_index(path: &str) -> Option<(&str, u64)> {
    let path = path.strip_suffix(']')?;
    let bracket = path.rfind('[')?;
    let inner = &path[bracket + 1..];
    let idx = inner.parse::<u64>().ok()?;
    Some((&path[..bracket], idx))
}

/// Threshold for the per-loop forward-progress guard: bail after
/// this many consecutive iterations during which the source cursor
/// didn't move. High enough to let counter loops run unmolested,
/// low enough to surface a clearer diagnostic well before the
/// wall-clock timeout fires.
const LOOP_STALL_LIMIT: u32 = 1_000;

/// Tracks how many consecutive iterations of a loop have failed to
/// advance the source cursor. Lives on the Rust call stack of the
/// loop's `exec_stmt` arm, so each loop instance gets its own
/// counter without any state on the [`Interpreter`] itself.
struct StuckCounter {
    consecutive: u32,
}

impl StuckCounter {
    fn new() -> Self {
        Self { consecutive: 0 }
    }

    fn observe(&mut self, stalled: bool) -> Result<(), RuntimeError> {
        if stalled {
            self.consecutive += 1;
            if self.consecutive >= LOOP_STALL_LIMIT {
                return Err(RuntimeError::LoopStalled { iterations: self.consecutive });
            }
        } else {
            self.consecutive = 0;
        }
        Ok(())
    }
}

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
    // Compare integer-like operands by their masked bit pattern so a
    // sign-extended SInt matches the same byte pattern stored as a
    // UInt (`switch (idByte & ETMask)` with case ETInt = 0x10 should
    // match a uchar `idByte` of 0x10).
    if let (Some(x), Some(y)) = (int_bits(a), int_bits(b)) {
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
/// Decode the storage word backing a bitfield slot. Unlike
/// `decode_prim`, accepts non-power-of-two byte counts (1..=8) so
/// `BitfieldDisablePadding()` can pack a 24-bit field into 3 bytes.
fn decode_prim_for_bitfield(bytes: &[u8], prim: PrimKind, endian: Endian) -> u64 {
    let mut buf = [0u8; 8];
    let n = bytes.len().min(8);
    if matches!(endian, Endian::Little) {
        buf[..n].copy_from_slice(&bytes[..n]);
    } else {
        // Big-endian: place the bytes at the high end of the slot
        // so left-to-right packing reads from the most significant
        // bits first.
        let off = 8 - prim.width as usize;
        buf[off..off + n].copy_from_slice(&bytes[..n]);
    }
    if matches!(endian, Endian::Little) {
        u64::from_le_bytes(buf)
    } else {
        u64::from_be_bytes(buf)
    }
}

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

/// Adler-32 checksum (RFC 1950). Sum-mod-65521 of the bytes, with a
/// running running-sum-mod-65521 in the high half. Used by zlib and
/// hence by DEX file headers.
fn adler32_checksum(bytes: &[u8]) -> u32 {
    const BASE: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in bytes {
        a = (a + byte as u32) % BASE;
        b = (b + a) % BASE;
    }
    (b << 16) | a
}

/// CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF, no reflection,
/// no final XOR). The variant 010 references for `CHECKSUM_CRC16`.
fn crc16_ccitt(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in bytes {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
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
