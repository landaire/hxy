//! Template-runtime plugin that executes 010 Editor `.bt` files.
//!
//! Wraps [`hxy_010_lang`]'s interpreter as a WIT `template-runtime`
//! world: `parsed_template` holds the parsed AST between invocations;
//! `execute` runs the interpreter against the host's byte source via
//! an adapter that shims `hxy_010_lang::HexSource` onto the WIT
//! `source.read` / `source.len` imports.

use std::cell::RefCell;

use hxy_010_lang::ast::Program;
use hxy_010_lang::{HexSource, Interpreter, NodeOut, RunResult, SourceError, Value, parse, tokenize};
use hxy_plugin_api::template::{
    Arg, DeferredArray, Diagnostic as WitDiagnostic, DisplayHint, Guest, GuestParsedTemplate, Node, ResultTree,
    Severity as WitSeverity, Span as WitSpan, Value as WitValue, source,
};

struct Runtime;

impl Guest for Runtime {
    type ParsedTemplate = ParsedTemplate;

    fn name() -> String {
        "010-bt".to_owned()
    }

    fn extensions() -> Vec<String> {
        vec!["bt".to_owned()]
    }
}

/// A pre-parsed 010 template held across `execute` and `expand_array`
/// calls. `RefCell` because the exported trait takes `&self` but we
/// want to update the parsed AST lazily once without re-parsing on
/// every execute.
pub struct ParsedTemplate {
    source_text: String,
    program: RefCell<Option<Program>>,
    /// Parse error (if any) captured on first access; surfaced as a
    /// single fatal diagnostic from `execute`.
    parse_error: RefCell<Option<String>>,
}

impl GuestParsedTemplate for ParsedTemplate {
    fn new(source: String) -> Self {
        Self { source_text: source, program: RefCell::new(None), parse_error: RefCell::new(None) }
    }

    fn execute(&self, _args: Vec<Arg>) -> ResultTree {
        self.ensure_parsed();
        let program = self.program.borrow();
        if let Some(err) = self.parse_error.borrow().as_ref() {
            return ResultTree {
                nodes: vec![],
                diagnostics: vec![WitDiagnostic {
                    message: err.clone(),
                    severity: WitSeverity::Error,
                    file_offset: None,
                    template_line: None,
                }],
            };
        }
        let Some(program) = program.as_ref() else {
            return ResultTree { nodes: vec![], diagnostics: vec![] };
        };

        let interpreter = Interpreter::new(HostSource);
        let result = interpreter.run(program);
        convert_result(result)
    }

    fn expand_array(&self, _array_id: u64, _start: u64, _end: u64) -> Result<Vec<Node>, WitDiagnostic> {
        // Deferred arrays aren't emitted by the current interpreter —
        // every array is fully materialised. The plugin still has to
        // satisfy the WIT contract, so we return "not supported".
        Err(WitDiagnostic {
            message: "deferred arrays are not produced by the 010 runtime".to_owned(),
            severity: WitSeverity::Error,
            file_offset: None,
            template_line: None,
        })
    }
}

impl ParsedTemplate {
    fn ensure_parsed(&self) {
        if self.program.borrow().is_some() || self.parse_error.borrow().is_some() {
            return;
        }
        let tokens = match tokenize(&self.source_text) {
            Ok(t) => t,
            Err(e) => {
                *self.parse_error.borrow_mut() = Some(format!("lex error: {e}"));
                return;
            }
        };
        match parse(tokens) {
            Ok(program) => *self.program.borrow_mut() = Some(program),
            Err(e) => *self.parse_error.borrow_mut() = Some(format!("parse error: {e}")),
        }
    }
}

/// Bridge between the host-imported `source` interface and the
/// [`HexSource`] trait the interpreter consumes.
struct HostSource;

impl HexSource for HostSource {
    fn len(&self) -> u64 {
        source::len()
    }

    fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, SourceError> {
        source::read(offset, length).map_err(SourceError::Host)
    }
}

fn convert_result(result: RunResult) -> ResultTree {
    let nodes = result.nodes.iter().map(convert_node).collect();
    let diagnostics = result.diagnostics.iter().map(convert_diagnostic).collect();
    ResultTree { nodes, diagnostics }
}

fn convert_node(node: &NodeOut) -> Node {
    // Display hint heuristic: if an attr says `format=hex`, render hex.
    let display = node.attrs.iter().find_map(|(k, v)| {
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
        name: node.name.clone(),
        type_name: node.type_name.clone(),
        span: WitSpan { offset: node.offset, length: node.length },
        value: node.value.as_ref().map(convert_value),
        parent: node.parent,
        array: None as Option<DeferredArray>,
        display,
    }
}

fn convert_value(v: &Value) -> WitValue {
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
        Value::Bool(b) => WitValue::U8Val(if *b { 1 } else { 0 }),
    }
}

fn convert_diagnostic(d: &hxy_010_lang::Diagnostic) -> WitDiagnostic {
    WitDiagnostic {
        message: d.message.clone(),
        severity: match d.severity {
            hxy_010_lang::Severity::Error => WitSeverity::Error,
            hxy_010_lang::Severity::Warning => WitSeverity::Warning,
            hxy_010_lang::Severity::Info => WitSeverity::Info,
        },
        file_offset: d.file_offset,
        template_line: d.template_line,
    }
}

hxy_plugin_api::template::export_template_runtime!(Runtime with_types_in hxy_plugin_api::template);
