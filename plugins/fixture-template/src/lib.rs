//! Test-only template runtime. Ignores the template source entirely
//! and parses the data source as:
//!
//! ```text
//! offset 0  : u32 magic
//! offset 4  : u64 count
//! offset 12 : u32[count] data     (returned as a deferred-array)
//! ```
//!
//! Exists so the host-side test suite can exercise the full pipeline
//! (execute, deferred-array expansion, diagnostics) without depending
//! on a real language implementation.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use hxy_plugin_api::template;
use hxy_plugin_api::template::source;
use hxy_plugin_api::template::{Guest, GuestParsedTemplate};

struct Runtime;

impl Guest for Runtime {
    type ParsedTemplate = ParsedTemplate;

    fn name() -> String {
        "fixture".to_string()
    }

    fn extensions() -> Vec<String> {
        vec!["fixture".to_string()]
    }
}

struct ParsedTemplate {
    _source: String,
}

impl GuestParsedTemplate for ParsedTemplate {
    fn new(source: String) -> Self {
        Self { _source: source }
    }

    fn execute(&self, _args: Vec<template::Arg>) -> template::ResultTree {
        let len = source::len();
        if len < 12 {
            return template::ResultTree {
                nodes: vec![],
                diagnostics: vec![template::Diagnostic {
                    message: format!("source too small: need 12 bytes, got {len}"),
                    severity: template::Severity::Error,
                    file_offset: Some(0),
                    template_line: None,
                }],
                byte_palette: None,
            };
        }
        let magic_bytes = match source::read(0, 4) {
            Ok(b) => b,
            Err(e) => return catastrophic(format!("read magic: {e}")),
        };
        let count_bytes = match source::read(4, 8) {
            Ok(b) => b,
            Err(e) => return catastrophic(format!("read count: {e}")),
        };
        let magic = u32::from_le_bytes([magic_bytes[0], magic_bytes[1], magic_bytes[2], magic_bytes[3]]);
        let count = u64::from_le_bytes([
            count_bytes[0], count_bytes[1], count_bytes[2], count_bytes[3],
            count_bytes[4], count_bytes[5], count_bytes[6], count_bytes[7],
        ]);

        let mut nodes = Vec::with_capacity(4);
        nodes.push(template::Node {
            name: "File".to_string(),
            type_name: template::NodeType::StructType("File".to_string()),
            span: template::Span { offset: 0, length: len },
            value: None,
            parent: None,
            array: None,
            display: None,
        });
        nodes.push(template::Node {
            name: "magic".to_string(),
            type_name: template::NodeType::Scalar(template::ScalarKind::U32K),
            span: template::Span { offset: 0, length: 4 },
            value: Some(template::Value::U32Val(magic)),
            parent: Some(0),
            array: None,
            display: Some(template::DisplayHint::Hex),
        });
        nodes.push(template::Node {
            name: "count".to_string(),
            type_name: template::NodeType::Scalar(template::ScalarKind::U64K),
            span: template::Span { offset: 4, length: 8 },
            value: Some(template::Value::U64Val(count)),
            parent: Some(0),
            array: None,
            display: Some(template::DisplayHint::Decimal),
        });
        nodes.push(template::Node {
            name: "data".to_string(),
            type_name: template::NodeType::ScalarArray((template::ScalarKind::U32K, count)),
            span: template::Span { offset: 12, length: count.saturating_mul(4) },
            value: None,
            parent: Some(0),
            array: Some(template::DeferredArray {
                id: 1,
                element_type: "u32".to_string(),
                count,
                stride: 4,
                first_offset: 12,
            }),
            display: None,
        });

        template::ResultTree { nodes, diagnostics: vec![], byte_palette: None }
    }

    fn expand_array(
        &self,
        array_id: u64,
        start: u64,
        end: u64,
    ) -> Result<Vec<template::Node>, template::Diagnostic> {
        if array_id != 1 {
            return Err(template::Diagnostic {
                message: format!("unknown array id {array_id}"),
                severity: template::Severity::Error,
                file_offset: None,
                template_line: None,
            });
        }
        let mut out = Vec::with_capacity((end - start) as usize);
        for i in start..end {
            let offset = 12 + i * 4;
            let bytes = source::read(offset, 4).map_err(|e| template::Diagnostic {
                message: format!("read element {i}: {e}"),
                severity: template::Severity::Error,
                file_offset: Some(offset),
                template_line: None,
            })?;
            let v = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            out.push(template::Node {
                name: format!("[{i}]"),
                type_name: template::NodeType::Scalar(template::ScalarKind::U32K),
                span: template::Span { offset, length: 4 },
                value: Some(template::Value::U32Val(v)),
                parent: None,
                array: None,
                display: Some(template::DisplayHint::Hex),
            });
        }
        Ok(out)
    }
}

fn catastrophic(message: String) -> template::ResultTree {
    template::ResultTree {
        nodes: vec![],
        diagnostics: vec![template::Diagnostic {
            message,
            severity: template::Severity::Error,
            file_offset: None,
            template_line: None,
        }],
        byte_palette: None,
    }
}

hxy_plugin_api::template::export_template_runtime!(Runtime with_types_in hxy_plugin_api::template);
