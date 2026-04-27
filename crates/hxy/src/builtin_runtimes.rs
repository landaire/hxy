//! Native (non-WASM) template-runtime implementations that ship as
//! part of the app binary. The WASM sandbox is useful for user-
//! supplied plugins we can't audit; our own runtimes are trusted Rust
//! code, so there's no reason to pay the wasmtime overhead or the
//! manual rebuild+embed dance. The `plugins/bt-runtime` crate
//! continues to exist as a reference implementation against the WIT
//! world (and as a dogfood target for future WASM plugin authors).

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;

use hxy_core::ByteOffset;
use hxy_core::ByteRange;
use hxy_core::HexSource;
use hxy_plugin_host::ParsedTemplate;
use hxy_plugin_host::TemplateRuntime;
use hxy_plugin_host::template as wit;
use hxy_vfs::HandlerError;

/// Builtin list. Order matters: first match wins in the dispatch
/// table, and the app inserts user-installed runtimes ahead of these
/// so users can override by dropping a component into the plugin
/// directory.
pub fn builtins() -> Vec<Arc<dyn TemplateRuntime>> {
    vec![Arc::new(Bt010Runtime::new()), Arc::new(ImHexRuntime::new())]
}

/// 010 Editor Binary Template runtime -- the lexer+parser+interpreter
/// from `hxy-010-lang`, wrapped as a [`TemplateRuntime`].
struct Bt010Runtime {
    extensions: Vec<String>,
}

impl Bt010Runtime {
    fn new() -> Self {
        Self { extensions: vec!["bt".to_owned()] }
    }
}

impl TemplateRuntime for Bt010Runtime {
    fn name(&self) -> &str {
        "010-bt"
    }

    fn extensions(&self) -> &[String] {
        &self.extensions
    }

    fn parse(
        &self,
        source: Arc<dyn HexSource>,
        template_source: &str,
    ) -> Result<Arc<dyn ParsedTemplate>, HandlerError> {
        let tokens =
            hxy_010_lang::tokenize(template_source).map_err(|e| HandlerError::Malformed(format!("lex: {e}")))?;
        let program = hxy_010_lang::parse(tokens).map_err(|e| HandlerError::Malformed(format!("parse: {e}")))?;
        Ok(Arc::new(Bt010Parsed { program, source }))
    }
}

struct Bt010Parsed {
    program: hxy_010_lang::ast::Program,
    source: Arc<dyn HexSource>,
}

impl ParsedTemplate for Bt010Parsed {
    fn execute(&self, _args: &[wit::Arg]) -> Result<wit::ResultTree, HandlerError> {
        let shim = HexSourceShim(self.source.clone());
        let result = hxy_010_lang::Interpreter::new(shim).run(&self.program);
        Ok(to_result_tree(result))
    }

    fn expand_array(&self, _array_id: u64, _start: u64, _end: u64) -> Result<Vec<wit::Node>, HandlerError> {
        Err(HandlerError::Unsupported(
            "the built-in 010-bt runtime materialises arrays eagerly; no deferred expansion".into(),
        ))
    }
}

/// Adapter from [`hxy_core::HexSource`] (the app's shape) to
/// [`hxy_010_lang::HexSource`] (the interpreter's shape).
struct HexSourceShim(Arc<dyn HexSource>);

impl hxy_010_lang::HexSource for HexSourceShim {
    fn len(&self) -> u64 {
        self.0.len().get()
    }

    fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, hxy_010_lang::SourceError> {
        let start = ByteOffset::new(offset);
        let end = ByteOffset::new(offset.saturating_add(length));
        let range =
            ByteRange::new(start, end).map_err(|e| hxy_010_lang::SourceError::Host(format!("invalid range: {e}")))?;
        self.0.read(range).map_err(|e| hxy_010_lang::SourceError::Host(format!("{e}")))
    }
}

fn to_result_tree(r: hxy_010_lang::RunResult) -> wit::ResultTree {
    let nodes = r.nodes.iter().map(convert_node).collect();
    let diagnostics = r.diagnostics.iter().map(convert_diagnostic).collect();
    // The 010 language has no syntax for declaring a byte palette --
    // leave the override empty and the app falls back to the user's
    // chosen highlight scheme.
    wit::ResultTree { nodes, diagnostics, byte_palette: None }
}

fn convert_node(n: &hxy_010_lang::NodeOut) -> wit::Node {
    // 010 spells the display hint with a `<format=hex>` attribute;
    // the canonical key the rest of the host reads is
    // [`hxy_plugin_host::FORMAT_ATTR`]. Promote both names to the
    // typed [`DisplayHint`] field so consumers can branch without
    // re-parsing strings.
    let display = n.attrs.iter().find_map(|(k, v)| {
        if k == "format" || k == hxy_plugin_host::FORMAT_ATTR {
            Some(match v.as_str() {
                "hex" => wit::DisplayHint::Hex,
                "decimal" => wit::DisplayHint::Decimal,
                "binary" => wit::DisplayHint::Binary,
                "ascii" => wit::DisplayHint::Ascii,
                _ => wit::DisplayHint::Decimal,
            })
        } else {
            None
        }
    });
    // Every attribute the interpreter recorded flows through so the
    // UI (and other hosts) can act on them -- notably the canonical
    // `hxy_endian` (see `hxy_plugin_host::ENDIAN_ATTR`) on primitive
    // arrays, which the hex-view tooltip uses to decode individual
    // elements on hover.
    let attributes: Vec<(String, String)> = n.attrs.clone();
    wit::Node {
        name: n.name.clone(),
        type_name: convert_node_type(&n.ty),
        span: wit::Span { offset: n.offset, length: n.length },
        value: n.value.as_ref().map(convert_value),
        parent: n.parent.map(|p| p.as_u32()),
        array: None,
        display,
        attributes,
    }
}

fn convert_scalar(k: hxy_010_lang::ScalarKind) -> wit::ScalarKind {
    match k {
        hxy_010_lang::ScalarKind::U8 => wit::ScalarKind::U8K,
        hxy_010_lang::ScalarKind::U16 => wit::ScalarKind::U16K,
        hxy_010_lang::ScalarKind::U32 => wit::ScalarKind::U32K,
        hxy_010_lang::ScalarKind::U64 => wit::ScalarKind::U64K,
        hxy_010_lang::ScalarKind::S8 => wit::ScalarKind::S8K,
        hxy_010_lang::ScalarKind::S16 => wit::ScalarKind::S16K,
        hxy_010_lang::ScalarKind::S32 => wit::ScalarKind::S32K,
        hxy_010_lang::ScalarKind::S64 => wit::ScalarKind::S64K,
        hxy_010_lang::ScalarKind::F32 => wit::ScalarKind::F32K,
        hxy_010_lang::ScalarKind::F64 => wit::ScalarKind::F64K,
        hxy_010_lang::ScalarKind::Bool => wit::ScalarKind::BoolK,
        hxy_010_lang::ScalarKind::Bytes => wit::ScalarKind::BytesK,
        hxy_010_lang::ScalarKind::Str => wit::ScalarKind::StringK,
    }
}

fn convert_node_type(t: &hxy_010_lang::NodeType) -> wit::NodeType {
    match t {
        hxy_010_lang::NodeType::Scalar(k) => wit::NodeType::Scalar(convert_scalar(*k)),
        hxy_010_lang::NodeType::ScalarArray(k, n) => wit::NodeType::ScalarArray((convert_scalar(*k), *n)),
        hxy_010_lang::NodeType::StructType(s) => wit::NodeType::StructType(s.clone()),
        hxy_010_lang::NodeType::StructArray(s, n) => wit::NodeType::StructArray((s.clone(), *n)),
        hxy_010_lang::NodeType::EnumType(s) => wit::NodeType::EnumType(s.clone()),
        hxy_010_lang::NodeType::EnumArray(s, n) => wit::NodeType::EnumArray((s.clone(), *n)),
        hxy_010_lang::NodeType::Unknown(s) => wit::NodeType::Unknown(s.clone()),
    }
}

fn convert_value(v: &hxy_010_lang::Value) -> wit::Value {
    match v {
        hxy_010_lang::Value::Void => wit::Value::StringVal(String::new()),
        hxy_010_lang::Value::UInt { value, kind } => match kind.width {
            1 => wit::Value::U8Val(*value as u8),
            2 => wit::Value::U16Val(*value as u16),
            4 => wit::Value::U32Val(*value as u32),
            _ => wit::Value::U64Val(*value as u64),
        },
        hxy_010_lang::Value::SInt { value, kind } => match kind.width {
            1 => wit::Value::S8Val(*value as i8),
            2 => wit::Value::S16Val(*value as i16),
            4 => wit::Value::S32Val(*value as i32),
            _ => wit::Value::S64Val(*value as i64),
        },
        hxy_010_lang::Value::Float { value, kind } => {
            if kind.width == 4 {
                wit::Value::F32Val(*value as f32)
            } else {
                wit::Value::F64Val(*value)
            }
        }
        hxy_010_lang::Value::Char { value, .. } => wit::Value::U8Val(*value as u8),
        hxy_010_lang::Value::Str(s) => wit::Value::StringVal(s.clone()),
        hxy_010_lang::Value::Bool(b) => wit::Value::BoolVal(*b),
    }
}

fn convert_diagnostic(d: &hxy_010_lang::Diagnostic) -> wit::Diagnostic {
    wit::Diagnostic {
        message: d.message.clone(),
        severity: match d.severity {
            hxy_010_lang::Severity::Error => wit::Severity::Error,
            hxy_010_lang::Severity::Warning => wit::Severity::Warning,
            hxy_010_lang::Severity::Info => wit::Severity::Info,
        },
        file_offset: d.file_offset,
        template_line: d.template_line,
    }
}

/// In-process ImHex pattern-language runtime. Routes `.hexpat` /
/// `.pat` files to the [`hxy_imhex_lang`] interpreter and lifts the
/// resulting node tree into the host's WIT-shaped schema.
struct ImHexRuntime {
    extensions: Vec<String>,
    /// Resolver for `import std.io;` etc. The corpus is fetched
    /// into `.imhex-patterns/` at runtime; this resolver looks
    /// templates up under `<base>/includes/` (where the upstream
    /// std library lives) and falls back to `<base>/` for cases
    /// where a template author drops a flat `mylib.pat` next to
    /// the main file.
    resolver: hxy_imhex_lang::SharedResolver,
}

impl ImHexRuntime {
    fn new() -> Self {
        Self { extensions: vec!["hexpat".to_owned(), "pat".to_owned()], resolver: build_default_resolver() }
    }
}

/// Default search path for ImHex `import` resolution. Walks each
/// candidate base and stops at the first hit. Order matters --
/// the per-application data dir takes precedence over a repo-
/// local checkout, so the built app uses freshly fetched corpus
/// content rather than whatever was in the source tree at build
/// time.
fn build_default_resolver() -> hxy_imhex_lang::SharedResolver {
    let mut bases: Vec<std::path::PathBuf> = Vec::new();
    if let Some(data_dir) = imhex_patterns_data_dir() {
        bases.push(data_dir.join("includes"));
        bases.push(data_dir);
    }
    if let Some(repo) = imhex_patterns_repo_local() {
        bases.push(repo.join("includes"));
        bases.push(repo);
    }
    if bases.is_empty() {
        return std::sync::Arc::new(hxy_imhex_lang::NoImportResolver);
    }
    hxy_imhex_lang::chained_resolver(bases)
}

/// `<APP_DATA>/<APP_NAME>/imhex-patterns/` -- where the application
/// clones the upstream pattern repo at runtime. Mirrors how
/// [`crate::user_templates_dir`] picks its base.
fn imhex_patterns_data_dir() -> Option<std::path::PathBuf> {
    let base = dirs::data_dir()?;
    Some(base.join(crate::APP_NAME).join("imhex-patterns"))
}

/// `<repo>/.imhex-patterns/` -- the gitignored fetch target the
/// `scripts/fetch_imhex_patterns.sh` script populates. Found by
/// walking up from the running binary's location.
fn imhex_patterns_repo_local() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut dir: &std::path::Path = exe.parent()?;
    for _ in 0..6 {
        let candidate = dir.join(".imhex-patterns");
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
    None
}

impl TemplateRuntime for ImHexRuntime {
    fn name(&self) -> &str {
        "imhex-pattern"
    }

    fn extensions(&self) -> &[String] {
        &self.extensions
    }

    fn parse(
        &self,
        source: Arc<dyn HexSource>,
        template_source: &str,
    ) -> Result<Arc<dyn ParsedTemplate>, HandlerError> {
        let tokens =
            hxy_imhex_lang::tokenize(template_source).map_err(|e| HandlerError::Malformed(format!("lex: {e}")))?;
        let program = hxy_imhex_lang::parse(tokens).map_err(|e| HandlerError::Malformed(format!("parse: {e}")))?;
        Ok(Arc::new(ImHexParsed { program, source, resolver: self.resolver.clone() }))
    }
}

struct ImHexParsed {
    program: hxy_imhex_lang::ast::Program,
    source: Arc<dyn HexSource>,
    resolver: hxy_imhex_lang::SharedResolver,
}

impl ParsedTemplate for ImHexParsed {
    fn execute(&self, _args: &[wit::Arg]) -> Result<wit::ResultTree, HandlerError> {
        let shim = ImHexSourceShim(self.source.clone());
        let result =
            hxy_imhex_lang::Interpreter::new(shim).with_import_resolver(self.resolver.clone()).run(&self.program);
        Ok(to_imhex_result_tree(result))
    }

    fn expand_array(&self, _array_id: u64, _start: u64, _end: u64) -> Result<Vec<wit::Node>, HandlerError> {
        Err(HandlerError::Unsupported(
            "the built-in imhex-pattern runtime materialises arrays eagerly; no deferred expansion".into(),
        ))
    }
}

struct ImHexSourceShim(Arc<dyn HexSource>);

impl hxy_imhex_lang::HexSource for ImHexSourceShim {
    fn len(&self) -> u64 {
        self.0.len().get()
    }

    fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, hxy_imhex_lang::SourceError> {
        let start = ByteOffset::new(offset);
        let end = ByteOffset::new(offset.saturating_add(length));
        let range =
            ByteRange::new(start, end).map_err(|e| hxy_imhex_lang::SourceError::Host(format!("invalid range: {e}")))?;
        self.0.read(range).map_err(|e| hxy_imhex_lang::SourceError::Host(format!("{e}")))
    }
}

fn to_imhex_result_tree(r: hxy_imhex_lang::RunResult) -> wit::ResultTree {
    let nodes = r.nodes.iter().map(convert_imhex_node).collect();
    let diagnostics = r.diagnostics.iter().map(convert_imhex_diagnostic).collect();
    wit::ResultTree { nodes, diagnostics, byte_palette: None }
}

fn convert_imhex_node(n: &hxy_imhex_lang::NodeOut) -> wit::Node {
    // Promote a `hxy_format` attribute (if present) to the typed
    // `DisplayHint` slot. Same convention as the 010 adapter -- both
    // runtimes target [`hxy_plugin_host::FORMAT_ATTR`].
    let display = n.attrs.iter().find_map(|(k, v)| {
        if k == hxy_plugin_host::FORMAT_ATTR {
            Some(match v.as_str() {
                "hex" => wit::DisplayHint::Hex,
                "decimal" => wit::DisplayHint::Decimal,
                "binary" => wit::DisplayHint::Binary,
                "ascii" => wit::DisplayHint::Ascii,
                _ => wit::DisplayHint::Decimal,
            })
        } else {
            None
        }
    });
    wit::Node {
        name: n.name.clone(),
        type_name: convert_imhex_node_type(&n.ty),
        span: wit::Span { offset: n.offset, length: n.length },
        value: n.value.as_ref().map(convert_imhex_value),
        parent: n.parent.map(|p| p.as_u32()),
        array: None,
        display,
        attributes: n.attrs.clone(),
    }
}

fn convert_imhex_scalar_kind(k: &hxy_imhex_lang::ScalarKind) -> wit::ScalarKind {
    use hxy_imhex_lang::ScalarKind as K;
    match k {
        K::U8 => wit::ScalarKind::U8K,
        K::U16 => wit::ScalarKind::U16K,
        K::U32 => wit::ScalarKind::U32K,
        K::U64 => wit::ScalarKind::U64K,
        K::U128 => wit::ScalarKind::U128K,
        K::S8 => wit::ScalarKind::S8K,
        K::S16 => wit::ScalarKind::S16K,
        K::S32 => wit::ScalarKind::S32K,
        K::S64 => wit::ScalarKind::S64K,
        K::S128 => wit::ScalarKind::S128K,
        K::F32 => wit::ScalarKind::F32K,
        K::F64 => wit::ScalarKind::F64K,
        K::Bool => wit::ScalarKind::BoolK,
        K::Bytes => wit::ScalarKind::BytesK,
        K::Str => wit::ScalarKind::StringK,
        // No dedicated `char` kind in the WIT enum -- fall back on
        // the underlying byte width. The renderer surfaces ASCII for
        // single-byte chars regardless.
        K::Char => wit::ScalarKind::U8K,
        K::Char16 => wit::ScalarKind::U16K,
    }
}

fn convert_imhex_node_type(t: &hxy_imhex_lang::NodeType) -> wit::NodeType {
    use hxy_imhex_lang::NodeType as T;
    match t {
        T::Scalar(k) => wit::NodeType::Scalar(convert_imhex_scalar_kind(k)),
        T::ScalarArray(k, n) => wit::NodeType::ScalarArray((convert_imhex_scalar_kind(k), *n)),
        T::StructType(name) => wit::NodeType::StructType(name.clone()),
        T::StructArray(name, n) => wit::NodeType::StructArray((name.clone(), *n)),
        T::EnumType(name) => wit::NodeType::EnumType(name.clone()),
        T::EnumArray(name, n) => wit::NodeType::EnumArray((name.clone(), *n)),
        // Bitfields don't have a dedicated WIT variant -- surface as
        // a struct so the renderer treats them as expandable parents.
        T::BitfieldType(name) => wit::NodeType::StructType(name.clone()),
        T::Unknown(s) => wit::NodeType::Unknown(s.clone()),
    }
}

fn convert_imhex_value(v: &hxy_imhex_lang::Value) -> wit::Value {
    use hxy_imhex_lang::Value as V;
    match v {
        V::Void => wit::Value::StringVal(String::new()),
        // Narrow back to the smallest WIT variant that fits. ImHex's
        // 128-bit ints fall through to `bytes-val` so the bits stay
        // visible (WIT has no native 128-bit numeric).
        V::UInt { value, kind } => match kind.width {
            1 => wit::Value::U8Val(*value as u8),
            2 => wit::Value::U16Val(*value as u16),
            4 => wit::Value::U32Val(*value as u32),
            8 => wit::Value::U64Val(*value as u64),
            _ => wit::Value::BytesVal(value.to_le_bytes().to_vec()),
        },
        V::SInt { value, kind } => match kind.width {
            1 => wit::Value::S8Val(*value as i8),
            2 => wit::Value::S16Val(*value as i16),
            4 => wit::Value::S32Val(*value as i32),
            8 => wit::Value::S64Val(*value as i64),
            _ => wit::Value::BytesVal((*value as u128).to_le_bytes().to_vec()),
        },
        V::Float { value, kind } => {
            if kind.width == 4 {
                wit::Value::F32Val(*value as f32)
            } else {
                wit::Value::F64Val(*value)
            }
        }
        V::Bool(b) => wit::Value::BoolVal(*b),
        V::Char { value, .. } => wit::Value::U8Val(*value as u8),
        V::Str(s) => wit::Value::StringVal(s.clone()),
        V::Bytes(b) => wit::Value::BytesVal(b.clone()),
    }
}

fn convert_imhex_diagnostic(d: &hxy_imhex_lang::Diagnostic) -> wit::Diagnostic {
    wit::Diagnostic {
        message: d.message.clone(),
        severity: match d.severity {
            hxy_imhex_lang::Severity::Error => wit::Severity::Error,
            hxy_imhex_lang::Severity::Warning => wit::Severity::Warning,
            hxy_imhex_lang::Severity::Info => wit::Severity::Info,
        },
        file_offset: d.file_offset,
        template_line: d.template_line,
    }
}
