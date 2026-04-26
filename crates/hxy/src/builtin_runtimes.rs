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
    vec![Arc::new(Bt010Runtime::new())]
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
