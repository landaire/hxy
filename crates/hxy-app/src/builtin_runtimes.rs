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
use hxy_plugin_host::Arg;
use hxy_plugin_host::Diagnostic;
use hxy_plugin_host::DisplayHint;
use hxy_plugin_host::Node;
use hxy_plugin_host::ParsedTemplate;
use hxy_plugin_host::ResultTree;
use hxy_plugin_host::Severity;
use hxy_plugin_host::Span;
use hxy_plugin_host::TemplateRuntime;
use hxy_plugin_host::Value as WitValue;
use hxy_vfs::HandlerError;

/// Builtin list. Order matters: first match wins in the dispatch
/// table, and the app inserts user-installed runtimes ahead of these
/// so users can override by dropping a component into the plugin
/// directory.
pub fn builtins() -> Vec<Arc<dyn TemplateRuntime>> {
    vec![Arc::new(Bt010Runtime::new())]
}

/// 010 Editor Binary Template runtime — the lexer+parser+interpreter
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
        let tokens = hxy_010_lang::tokenize(template_source)
            .map_err(|e| HandlerError::Malformed(format!("lex: {e}")))?;
        let program = hxy_010_lang::parse(tokens)
            .map_err(|e| HandlerError::Malformed(format!("parse: {e}")))?;
        Ok(Arc::new(Bt010Parsed { program, source }))
    }
}

struct Bt010Parsed {
    program: hxy_010_lang::ast::Program,
    source: Arc<dyn HexSource>,
}

impl ParsedTemplate for Bt010Parsed {
    fn execute(&self, _args: &[Arg]) -> Result<ResultTree, HandlerError> {
        let shim = HexSourceShim(self.source.clone());
        let result = hxy_010_lang::Interpreter::new(shim).run(&self.program);
        Ok(to_result_tree(result))
    }

    fn expand_array(&self, _array_id: u64, _start: u64, _end: u64) -> Result<Vec<Node>, HandlerError> {
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
        let range = ByteRange::new(start, end)
            .map_err(|e| hxy_010_lang::SourceError::Host(format!("invalid range: {e}")))?;
        self.0
            .read(range)
            .map_err(|e| hxy_010_lang::SourceError::Host(format!("{e}")))
    }
}

fn to_result_tree(r: hxy_010_lang::RunResult) -> ResultTree {
    let nodes = r.nodes.iter().map(convert_node).collect();
    let diagnostics = r.diagnostics.iter().map(convert_diagnostic).collect();
    ResultTree { nodes, diagnostics }
}

fn convert_node(n: &hxy_010_lang::NodeOut) -> Node {
    let display = n.attrs.iter().find_map(|(k, v)| {
        if k == "format" {
            Some(match v.as_str() {
                "hex" => DisplayHint::Hex,
                "decimal" => DisplayHint::Decimal,
                "binary" => DisplayHint::Binary,
                "ascii" => DisplayHint::Ascii,
                _ => DisplayHint::Decimal,
            })
        } else {
            None
        }
    });
    Node {
        name: n.name.clone(),
        type_name: n.type_name.clone(),
        span: Span { offset: n.offset, length: n.length },
        value: n.value.as_ref().map(convert_value),
        parent: n.parent,
        array: None,
        display,
    }
}

fn convert_value(v: &hxy_010_lang::Value) -> WitValue {
    use hxy_010_lang::Value;
    match v {
        Value::Void => WitValue::StringVal(String::new()),
        Value::UInt { value, kind } => match kind.width {
            1 => WitValue::U8Val(*value as u8),
            2 => WitValue::U16Val(*value as u16),
            4 => WitValue::U32Val(*value as u32),
            _ => WitValue::U64Val(*value as u64),
        },
        Value::SInt { value, kind } => match kind.width {
            1 => WitValue::S8Val(*value as i8),
            2 => WitValue::S16Val(*value as i16),
            4 => WitValue::S32Val(*value as i32),
            _ => WitValue::S64Val(*value as i64),
        },
        Value::Float { value, kind } => {
            if kind.width == 4 {
                WitValue::F32Val(*value as f32)
            } else {
                WitValue::F64Val(*value)
            }
        }
        Value::Char { value, .. } => WitValue::U8Val(*value as u8),
        Value::Str(s) => WitValue::StringVal(s.clone()),
        Value::Bool(b) => WitValue::U8Val(u8::from(*b)),
    }
}

fn convert_diagnostic(d: &hxy_010_lang::Diagnostic) -> Diagnostic {
    Diagnostic {
        message: d.message.clone(),
        severity: match d.severity {
            hxy_010_lang::Severity::Error => Severity::Error,
            hxy_010_lang::Severity::Warning => Severity::Warning,
            hxy_010_lang::Severity::Info => Severity::Info,
        },
        file_offset: d.file_offset,
        template_line: d.template_line,
    }
}
