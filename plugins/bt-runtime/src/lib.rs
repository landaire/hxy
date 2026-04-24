//! Template-runtime plugin that executes 010 Editor `.bt` files.
//!
//! Wraps [`hxy_010_lang`]'s interpreter as a WIT `template-runtime`
//! world: `parsed_template` holds the parsed AST between invocations;
//! `execute` runs the interpreter against the host's byte source via
//! an adapter that shims `hxy_010_lang::HexSource` onto the WIT
//! `source.read` / `source.len` imports.

use std::cell::RefCell;

use hxy_010_lang::ast::Program;
use hxy_010_lang::{HexSource, Interpreter, NodeOut, RunResult, SourceError, parse, tokenize};
use hxy_plugin_api::template;
use hxy_plugin_api::template::source;
use hxy_plugin_api::template::{Guest, GuestParsedTemplate};

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

    fn execute(&self, _args: Vec<template::Arg>) -> template::ResultTree {
        self.ensure_parsed();
        let program = self.program.borrow();
        if let Some(err) = self.parse_error.borrow().as_ref() {
            return template::ResultTree {
                nodes: vec![],
                diagnostics: vec![template::Diagnostic {
                    message: err.clone(),
                    severity: template::Severity::Error,
                    file_offset: None,
                    template_line: None,
                }],
                byte_palette: None,
            };
        }
        let Some(program) = program.as_ref() else {
            return template::ResultTree { nodes: vec![], diagnostics: vec![], byte_palette: None };
        };

        let interpreter = Interpreter::new(HostSource);
        let result = interpreter.run(program);
        convert_result(result)
    }

    fn expand_array(
        &self,
        _array_id: u64,
        _start: u64,
        _end: u64,
    ) -> Result<Vec<template::Node>, template::Diagnostic> {
        Err(template::Diagnostic {
            message: "deferred arrays are not produced by the 010 runtime".to_owned(),
            severity: template::Severity::Error,
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

fn convert_result(result: RunResult) -> template::ResultTree {
    let nodes = result.nodes.iter().map(convert_node).collect();
    let diagnostics = result.diagnostics.iter().map(convert_diagnostic).collect();
    template::ResultTree { nodes, diagnostics, byte_palette: None }
}

fn convert_node(node: &NodeOut) -> template::Node {
    let display = node.attrs.iter().find_map(|(k, v)| {
        if k == "format" {
            Some(match v.as_str() {
                "hex" => template::DisplayHint::Hex,
                "decimal" => template::DisplayHint::Decimal,
                "binary" => template::DisplayHint::Binary,
                "ascii" => template::DisplayHint::Ascii,
                _ => template::DisplayHint::Decimal,
            })
        } else {
            None
        }
    });
    template::Node {
        name: node.name.clone(),
        type_name: convert_node_type(&node.ty),
        span: template::Span { offset: node.offset, length: node.length },
        value: node.value.as_ref().map(convert_value),
        parent: node.parent,
        array: None as Option<template::DeferredArray>,
        display,
    }
}

fn convert_scalar(k: hxy_010_lang::ScalarKind) -> template::ScalarKind {
    match k {
        hxy_010_lang::ScalarKind::U8 => template::ScalarKind::U8K,
        hxy_010_lang::ScalarKind::U16 => template::ScalarKind::U16K,
        hxy_010_lang::ScalarKind::U32 => template::ScalarKind::U32K,
        hxy_010_lang::ScalarKind::U64 => template::ScalarKind::U64K,
        hxy_010_lang::ScalarKind::S8 => template::ScalarKind::S8K,
        hxy_010_lang::ScalarKind::S16 => template::ScalarKind::S16K,
        hxy_010_lang::ScalarKind::S32 => template::ScalarKind::S32K,
        hxy_010_lang::ScalarKind::S64 => template::ScalarKind::S64K,
        hxy_010_lang::ScalarKind::F32 => template::ScalarKind::F32K,
        hxy_010_lang::ScalarKind::F64 => template::ScalarKind::F64K,
        hxy_010_lang::ScalarKind::Bool => template::ScalarKind::BoolK,
        hxy_010_lang::ScalarKind::Bytes => template::ScalarKind::BytesK,
        hxy_010_lang::ScalarKind::Str => template::ScalarKind::StringK,
    }
}

fn convert_node_type(t: &hxy_010_lang::NodeType) -> template::NodeType {
    match t {
        hxy_010_lang::NodeType::Scalar(k) => template::NodeType::Scalar(convert_scalar(*k)),
        hxy_010_lang::NodeType::ScalarArray(k, n) => {
            template::NodeType::ScalarArray((convert_scalar(*k), *n))
        }
        hxy_010_lang::NodeType::StructType(s) => template::NodeType::StructType(s.clone()),
        hxy_010_lang::NodeType::StructArray(s, n) => {
            template::NodeType::StructArray((s.clone(), *n))
        }
        hxy_010_lang::NodeType::EnumType(s) => template::NodeType::EnumType(s.clone()),
        hxy_010_lang::NodeType::EnumArray(s, n) => {
            template::NodeType::EnumArray((s.clone(), *n))
        }
        hxy_010_lang::NodeType::Unknown(s) => template::NodeType::Unknown(s.clone()),
    }
}

fn convert_value(v: &hxy_010_lang::Value) -> template::Value {
    match v {
        hxy_010_lang::Value::Void => template::Value::StringVal(String::new()),
        hxy_010_lang::Value::UInt { value, kind } => match kind.width {
            1 => template::Value::U8Val(*value as u8),
            2 => template::Value::U16Val(*value as u16),
            4 => template::Value::U32Val(*value as u32),
            _ => template::Value::U64Val(*value as u64),
        },
        hxy_010_lang::Value::SInt { value, kind } => match kind.width {
            1 => template::Value::S8Val(*value as i8),
            2 => template::Value::S16Val(*value as i16),
            4 => template::Value::S32Val(*value as i32),
            _ => template::Value::S64Val(*value as i64),
        },
        hxy_010_lang::Value::Float { value, kind } => {
            if kind.width == 4 {
                template::Value::F32Val(*value as f32)
            } else {
                template::Value::F64Val(*value)
            }
        }
        hxy_010_lang::Value::Char { value, .. } => template::Value::U8Val(*value as u8),
        hxy_010_lang::Value::Str(s) => template::Value::StringVal(s.clone()),
        hxy_010_lang::Value::Bool(b) => template::Value::U8Val(if *b { 1 } else { 0 }),
    }
}

fn convert_diagnostic(d: &hxy_010_lang::Diagnostic) -> template::Diagnostic {
    template::Diagnostic {
        message: d.message.clone(),
        severity: match d.severity {
            hxy_010_lang::Severity::Error => template::Severity::Error,
            hxy_010_lang::Severity::Warning => template::Severity::Warning,
            hxy_010_lang::Severity::Info => template::Severity::Info,
        },
        file_offset: d.file_offset,
        template_line: d.template_line,
    }
}

hxy_plugin_api::template::export_template_runtime!(Runtime with_types_in hxy_plugin_api::template);
