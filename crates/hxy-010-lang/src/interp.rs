//! Tree-walking interpreter for 010 Binary Template programs.
//!
//! Entry point: [`Interpreter::run`]. The interpreter walks the AST
//! sequentially, reading bytes from the supplied [`HexSource`] as it
//! encounters field declarations. Output is a flat pre-order list of
//! [`NodeOut`] records that mirrors the WIT `node` layout — so the
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
use crate::value::PrimClass;
use crate::value::PrimKind;
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

/// One emitted tree node. Mirrors the WIT `node` record.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeOut {
    pub name: String,
    pub type_name: String,
    pub offset: u64,
    pub length: u64,
    pub value: Option<Value>,
    pub parent: Option<u32>,
    /// `(key, value)` pairs pulled from `<attr=value>` lists. Stored
    /// opaquely so the renderer decides what to do with `format=hex`,
    /// `style=sHeading1`, etc.
    pub attrs: Vec<(String, String)>,
}

/// Output of running a template. Non-empty even when fatal errors
/// occur — the interpreter emits as much as it can before bailing.
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
}

/// Safety valve against templates with unbounded or near-unbounded
/// loops. 10M statements lets real templates iterate through big
/// archives while still catching `while(true)` holes in well under a
/// second.
pub const DEFAULT_STEP_LIMIT: u64 = 10_000_000;

#[derive(Clone, Debug)]
enum TypeDef {
    Primitive(PrimKind),
    /// Aliased to another type name — resolved on lookup.
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
        };
        me.register_primitives();
        me
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

    // ---- primitives table ----

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
            ("hfloat", P::u16()), // half-float — read as raw u16 for now
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

    // ---- statements ----

    fn exec_stmt(&mut self, stmt: &Stmt, parent: Option<u32>) -> Result<Flow, RuntimeError> {
        self.steps = self.steps.saturating_add(1);
        if self.steps > self.step_limit {
            return Err(RuntimeError::Type(format!(
                "exceeded step limit ({}) — template aborted",
                self.step_limit
            )));
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
                        && !self.eval(c)?.is_truthy() {
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
        }
    }

    fn exec_block(&mut self, stmts: &[Stmt], parent: Option<u32>) -> Result<Flow, RuntimeError> {
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

    // ---- field / variable declarations ----

    fn exec_field_decl(&mut self, stmt: &Stmt, parent: Option<u32>) -> Result<(), RuntimeError> {
        let Stmt::FieldDecl { modifier, ty, name, array_size, init, attrs, .. } = stmt else {
            unreachable!();
        };

        // `local` and `const` are ephemeral variables; they can still
        // have initializers but don't read from the source.
        if matches!(modifier, crate::ast::DeclModifier::Local | crate::ast::DeclModifier::Const) {
            let value = match init {
                Some(expr) => self.eval(expr)?,
                None => Value::Void,
            };
            self.current_scope_mut().vars.insert(name.clone(), value);
            return Ok(());
        }

        // Normal field read — resolve the type, read bytes, emit nodes,
        // bind the value into the current scope.
        let count = match array_size {
            Some(expr) => {
                let v = self.eval(expr)?;
                v.to_i128().ok_or_else(|| RuntimeError::Type(format!("array size is not numeric: {v:?}")))?
                    as u64
            }
            None => 0,
        };

        let value = if array_size.is_some() {
            self.read_array(name, ty, count, parent, attrs)?
        } else {
            self.read_scalar(name, ty, parent, attrs)?
        };

        self.current_scope_mut().vars.insert(name.clone(), value);
        Ok(())
    }

    fn current_scope_mut(&mut self) -> &mut Scope {
        self.scopes.last_mut().expect("scope stack is never empty")
    }

    fn read_scalar(
        &mut self,
        name: &str,
        ty: &TypeRef,
        parent: Option<u32>,
        attrs: &Attrs,
    ) -> Result<Value, RuntimeError> {
        let def = self.resolve_type(ty)?;
        match def {
            TypeDef::Primitive(p) => {
                let offset = self.cursor.tell();
                let bytes = self.cursor.read_advance(p.width as u64)?;
                let value = decode_prim(&bytes, p, self.endian)?;
                let idx = self.nodes.len() as u32;
                self.nodes.push(NodeOut {
                    name: name.to_owned(),
                    type_name: ty.name.clone(),
                    offset,
                    length: p.width as u64,
                    value: Some(value.clone()),
                    parent,
                    attrs: attrs_to_pairs(attrs),
                });
                let _ = idx;
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
                // Find matching variant — but don't require one; 010
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
                    type_name: ty.name.clone(),
                    offset,
                    length: pk.width as u64,
                    value: Some(value.clone()),
                    parent,
                    attrs: attrs_to_pairs(attrs),
                });
                Ok(value)
            }
            TypeDef::Struct(s) => {
                let offset = self.cursor.tell();
                let idx = self.nodes.len() as u32;
                self.nodes.push(NodeOut {
                    name: name.to_owned(),
                    type_name: ty.name.clone(),
                    offset,
                    length: 0,
                    value: None,
                    parent,
                    attrs: attrs_to_pairs(attrs),
                });
                self.exec_block(&s.body, Some(idx))?;
                // Back-fill length as end - start.
                let end = self.cursor.tell();
                self.nodes[idx as usize].length = end - offset;
                Ok(Value::Void)
            }
            TypeDef::Alias(_) => unreachable!("resolve_type follows aliases"),
        }
    }

    fn eval_enum_variant(
        &mut self,
        v: &crate::ast::EnumVariant,
    ) -> Result<(String, u64), RuntimeError> {
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
        parent: Option<u32>,
        attrs: &Attrs,
    ) -> Result<Value, RuntimeError> {
        let def = self.resolve_type(ty)?;
        // `char[N]` and `uchar[N]` are special: 010 treats them as
        // strings. Emit a single node with a Str value.
        if let TypeDef::Primitive(p) = def.clone()
            && matches!(p.class, PrimClass::Char) {
                let offset = self.cursor.tell();
                let bytes = self.cursor.read_advance(count)?;
                let s = String::from_utf8_lossy(&bytes).into_owned();
                let value = Value::Str(s);
                self.nodes.push(NodeOut {
                    name: name.to_owned(),
                    type_name: format!("{}[{}]", ty.name, count),
                    offset,
                    length: count,
                    value: Some(value.clone()),
                    parent,
                    attrs: attrs_to_pairs(attrs),
                });
                return Ok(value);
            }
        let offset = self.cursor.tell();
        let idx = self.nodes.len() as u32;
        self.nodes.push(NodeOut {
            name: name.to_owned(),
            type_name: format!("{}[{}]", ty.name, count),
            offset,
            length: 0,
            value: None,
            parent,
            attrs: attrs_to_pairs(attrs),
        });
        for i in 0..count {
            let elem_name = format!("[{i}]");
            // Fake a TypeRef for each element; spans don't matter.
            let elem_ty = TypeRef { name: ty.name.clone(), span: ty.span };
            self.read_scalar(&elem_name, &elem_ty, Some(idx), &Attrs::default())?;
        }
        let end = self.cursor.tell();
        self.nodes[idx as usize].length = end - offset;
        Ok(Value::Void)
    }

    // ---- expressions ----

    fn eval(&mut self, expr: &Expr) -> Result<Value, RuntimeError> {
        match expr {
            Expr::IntLit { value, .. } => {
                Ok(Value::UInt { value: *value as u128, kind: PrimKind::u64() })
            }
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
                let v = self.eval(operand)?;
                eval_unary(*op, &v)
            }
            Expr::Call { callee, args, .. } => {
                let Expr::Ident { name, .. } = &**callee else {
                    return Err(RuntimeError::Type("call target must be an identifier".into()));
                };
                let evaluated: Vec<Value> =
                    args.iter().map(|a| self.eval(a)).collect::<Result<_, _>>()?;
                self.call_named(name, &evaluated)
            }
            Expr::Member { target, field, .. } => {
                // Lookup via `name.field` — we don't have a real struct
                // value type yet, so fall back to "name_field" variable
                // lookup. Good enough to get arithmetic running; we'll
                // replace with proper member walking when struct values
                // land.
                let Expr::Ident { name, .. } = &**target else {
                    return Err(RuntimeError::Type("member access on non-name not yet supported".into()));
                };
                let composite = format!("{name}.{field}");
                self.lookup_ident(&composite)
                    .or_else(|_| self.lookup_ident(field))
            }
            Expr::Index { .. } => {
                Err(RuntimeError::Type("array indexing not yet supported".into()))
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
        Err(RuntimeError::UndefinedName { name: name.to_owned() })
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

    // ---- calls ----

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
                // We don't format — just emit the format string as an
                // info diagnostic for visibility.
                let msg = args.first().map(value_to_display).unwrap_or_default();
                self.diagnostics.push(Diagnostic {
                    message: msg,
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
                // Raise by returning "exit value" via Ok(None)? No —
                // we want to halt. Model as an error so `run` bails.
                Err(RuntimeError::Type(format!("template exited with {code}")))
            }
            "exists" => {
                let has = !matches!(args.first(), Some(Value::Void) | None);
                Ok(Some(Value::Bool(has)))
            }
            "RequiresVersion" => Ok(Some(Value::Void)),
            "FindFirst" => Ok(Some(self.find_first(args)?)),
            _ => Ok(None),
        }
    }

    fn read_fn_uint(&self, args: &[Value], width: u8, signed: bool) -> Result<Value, RuntimeError> {
        let offset = match args.first() {
            Some(v) => v.to_i128().unwrap_or(0) as u64,
            None => self.cursor.tell(),
        };
        let bytes = self.cursor.read_at(offset, width as u64)?;
        decode_prim(
            &bytes,
            PrimKind { class: PrimClass::Int, width, signed },
            self.endian,
        )
    }

    /// `FindFirst(data, matchcase, wholeword, method, tolerance, dir,
    /// start, size, wildcardMatch) -> int64`
    ///
    /// Only the common path — integer needle, optional `start`/`size`
    /// — is implemented. Other args are accepted and ignored so
    /// templates that pass the full 010 argument vector work.
    /// Returns -1 when the needle isn't found.
    fn find_first(&self, args: &[Value]) -> Result<Value, RuntimeError> {
        let not_found = Value::SInt { value: -1, kind: PrimKind::i64() };
        let Some(needle_val) = args.first() else { return Ok(not_found) };
        let Some(needle_i) = needle_val.to_i128() else { return Ok(not_found) };
        let start = args.get(6).and_then(|v| v.to_i128()).unwrap_or(0).max(0) as u64;
        let size_arg = args.get(7).and_then(|v| v.to_i128()).unwrap_or(0);
        let source_len = self.cursor.len();
        let end = if size_arg <= 0 {
            source_len
        } else {
            (start + size_arg as u64).min(source_len)
        };
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

// ---- primitive decoding ----

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

// ---- operator evaluation ----

fn eval_binary(op: BinOp, l: &Value, r: &Value) -> Result<Value, RuntimeError> {
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
                && v.is_float() {
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
            // yet — return the computed value without storing.
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
        A::Assign => BinOp::Add, // unreachable — callers filter this
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
    attrs
        .0
        .iter()
        .map(|a| (a.key.clone(), attr_expr_to_string(&a.value)))
        .collect()
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
