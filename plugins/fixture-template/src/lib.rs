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

use hxy_plugin_api::template::Arg;
use hxy_plugin_api::template::DeferredArray;
use hxy_plugin_api::template::Diagnostic;
use hxy_plugin_api::template::Guest;
use hxy_plugin_api::template::GuestParsedTemplate;
use hxy_plugin_api::template::Node;
use hxy_plugin_api::template::ResultTree;
use hxy_plugin_api::template::Severity;
use hxy_plugin_api::template::Span;
use hxy_plugin_api::template::Value;
use hxy_plugin_api::template::source;

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
    // Ignored — we parse the *data* source, not the template source.
    _source: String,
}

impl GuestParsedTemplate for ParsedTemplate {
    fn new(source: String) -> Self {
        Self { _source: source }
    }

    fn execute(&self, _args: Vec<Arg>) -> ResultTree {
        let len = source::len();
        if len < 12 {
            return ResultTree {
                nodes: vec![],
                diagnostics: vec![Diagnostic {
                    message: format!("source too small: need 12 bytes, got {len}"),
                    severity: Severity::Error,
                    file_offset: Some(0),
                    template_line: None,
                }],
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
        // 0: root
        nodes.push(Node {
            name: "File".to_string(),
            type_name: "struct".to_string(),
            span: Span { offset: 0, length: len },
            value: None,
            parent: None,
            array: None,
            display: None,
        });
        // 1: magic
        nodes.push(Node {
            name: "magic".to_string(),
            type_name: "u32".to_string(),
            span: Span { offset: 0, length: 4 },
            value: Some(Value::U32Val(magic)),
            parent: Some(0),
            array: None,
            display: Some(hxy_plugin_api::template::DisplayHint::Hex),
        });
        // 2: count
        nodes.push(Node {
            name: "count".to_string(),
            type_name: "u64".to_string(),
            span: Span { offset: 4, length: 8 },
            value: Some(Value::U64Val(count)),
            parent: Some(0),
            array: None,
            display: Some(hxy_plugin_api::template::DisplayHint::Decimal),
        });
        // 3: deferred data array
        nodes.push(Node {
            name: "data".to_string(),
            type_name: "u32[]".to_string(),
            span: Span { offset: 12, length: count.saturating_mul(4) },
            value: None,
            parent: Some(0),
            array: Some(DeferredArray {
                id: 1,
                element_type: "u32".to_string(),
                count,
                stride: 4,
                first_offset: 12,
            }),
            display: None,
        });

        ResultTree { nodes, diagnostics: vec![] }
    }

    fn expand_array(&self, array_id: u64, start: u64, end: u64) -> Result<Vec<Node>, Diagnostic> {
        if array_id != 1 {
            return Err(Diagnostic {
                message: format!("unknown array id {array_id}"),
                severity: Severity::Error,
                file_offset: None,
                template_line: None,
            });
        }
        let mut out = Vec::with_capacity((end - start) as usize);
        for i in start..end {
            let offset = 12 + i * 4;
            let bytes = source::read(offset, 4).map_err(|e| Diagnostic {
                message: format!("read element {i}: {e}"),
                severity: Severity::Error,
                file_offset: Some(offset),
                template_line: None,
            })?;
            let v = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            out.push(Node {
                name: format!("[{i}]"),
                type_name: "u32".to_string(),
                span: Span { offset, length: 4 },
                value: Some(Value::U32Val(v)),
                parent: None,
                array: None,
                display: Some(hxy_plugin_api::template::DisplayHint::Hex),
            });
        }
        Ok(out)
    }
}

fn catastrophic(message: String) -> ResultTree {
    ResultTree {
        nodes: vec![],
        diagnostics: vec![Diagnostic { message, severity: Severity::Error, file_offset: None, template_line: None }],
    }
}

hxy_plugin_api::template::export_template_runtime!(Runtime with_types_in hxy_plugin_api::template);
