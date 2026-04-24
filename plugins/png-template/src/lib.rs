//! PNG format parser as a per-template WASM plugin.
//!
//! This is the reference for the "WASM is the template" workflow:
//! the plugin hardcodes PNG's structure in Rust, compiles to a
//! component, and is dropped straight into the user's
//! `template-plugins/` directory. There's no text template, no
//! language runtime — the `.wasm` file is the entire template.
//!
//! Contrast with `plugins/bt-runtime`: that ships an interpreter for
//! 010 Editor's Binary Template DSL, so one runtime parses many
//! user-authored `.bt` files. Use that model when you want
//! end-users writing templates in a text DSL. Use this model when
//! you know the format and just want to ship a parser.

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

struct PngPlugin;

impl Guest for PngPlugin {
    type ParsedTemplate = PngParsed;

    fn name() -> String {
        "png".to_string()
    }

    fn extensions() -> Vec<String> {
        vec!["png".to_string()]
    }
}

/// Per-template plugins don't have a template source to parse — the
/// WASM binary is the template. `new` becomes a no-op constructor.
pub struct PngParsed;

impl GuestParsedTemplate for PngParsed {
    fn new(_source: String) -> Self {
        Self
    }

    fn execute(&self, _args: Vec<template::Arg>) -> template::ResultTree {
        let len = source::len();
        if len < 8 {
            return fail("file is shorter than the 8-byte PNG signature".into());
        }
        let sig = match source::read(0, 8) {
            Ok(b) => b,
            Err(e) => return fail(format!("read signature: {e}")),
        };
        if sig != [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
            return fail("signature bytes are not a PNG header".into());
        }

        let mut nodes: Vec<template::Node> = Vec::new();
        let mut diagnostics: Vec<template::Diagnostic> = Vec::new();

        nodes.push(template::Node {
            name: "PNG".to_string(),
            type_name: template::NodeType::StructType("file".to_string()),
            span: template::Span { offset: 0, length: len },
            value: None,
            parent: None,
            array: None,
            display: None,
            attributes: Vec::new(),
        });
        let root: u32 = 0;

        nodes.push(template::Node {
            name: "signature".to_string(),
            type_name: template::NodeType::ScalarArray((template::ScalarKind::U8K, 8)),
            span: template::Span { offset: 0, length: 8 },
            value: Some(template::Value::BytesVal(sig)),
            parent: Some(root),
            array: None,
            display: Some(template::DisplayHint::Hex),
            attributes: Vec::new(),
        });

        let mut off: u64 = 8;
        let mut saw_iend = false;
        while off + 12 <= len {
            let hdr = match source::read(off, 8) {
                Ok(b) => b,
                Err(e) => {
                    diagnostics.push(template::Diagnostic {
                        message: format!("read chunk header at {off:#x}: {e}"),
                        severity: template::Severity::Error,
                        file_offset: Some(off),
                        template_line: None,
                    });
                    break;
                }
            };
            let data_len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
            let ty_bytes = [hdr[4], hdr[5], hdr[6], hdr[7]];
            let ty = core::str::from_utf8(&ty_bytes).unwrap_or("????").to_string();
            let chunk_end = off + 12 + data_len;
            if chunk_end > len {
                diagnostics.push(template::Diagnostic {
                    message: format!(
                        "chunk `{ty}` claims length {data_len} but that runs past the end of the file"
                    ),
                    severity: template::Severity::Error,
                    file_offset: Some(off),
                    template_line: None,
                });
                break;
            }

            let chunk_idx = nodes.len() as u32;
            nodes.push(template::Node {
                name: format!("chunk {ty}"),
                type_name: template::NodeType::StructType("chunk".to_string()),
                span: template::Span { offset: off, length: 12 + data_len },
                value: None,
                parent: Some(root),
                array: None,
                display: None,
                attributes: Vec::new(),
            });

            nodes.push(template::Node {
                name: "length".to_string(),
                type_name: template::NodeType::Scalar(template::ScalarKind::U32K),
                span: template::Span { offset: off, length: 4 },
                value: Some(template::Value::U32Val(data_len as u32)),
                parent: Some(chunk_idx),
                array: None,
                display: Some(template::DisplayHint::Decimal),
                attributes: Vec::new(),
            });
            nodes.push(template::Node {
                name: "type".to_string(),
                type_name: template::NodeType::ScalarArray((template::ScalarKind::U8K, 4)),
                span: template::Span { offset: off + 4, length: 4 },
                value: Some(template::Value::StringVal(ty.clone())),
                parent: Some(chunk_idx),
                array: None,
                display: Some(template::DisplayHint::Ascii),
                attributes: Vec::new(),
            });

            if ty == "IHDR" && data_len == 13 {
                if let Ok(data) = source::read(off + 8, 13) {
                    let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                    let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
                    push_scalar(
                        &mut nodes,
                        chunk_idx,
                        "width",
                        template::ScalarKind::U32K,
                        off + 8,
                        4,
                        template::Value::U32Val(width),
                    );
                    push_scalar(
                        &mut nodes,
                        chunk_idx,
                        "height",
                        template::ScalarKind::U32K,
                        off + 12,
                        4,
                        template::Value::U32Val(height),
                    );
                    push_scalar(
                        &mut nodes,
                        chunk_idx,
                        "bit_depth",
                        template::ScalarKind::U8K,
                        off + 16,
                        1,
                        template::Value::U8Val(data[8]),
                    );
                    push_scalar(
                        &mut nodes,
                        chunk_idx,
                        "color_type",
                        template::ScalarKind::U8K,
                        off + 17,
                        1,
                        template::Value::U8Val(data[9]),
                    );
                    push_scalar(
                        &mut nodes,
                        chunk_idx,
                        "compression",
                        template::ScalarKind::U8K,
                        off + 18,
                        1,
                        template::Value::U8Val(data[10]),
                    );
                    push_scalar(
                        &mut nodes,
                        chunk_idx,
                        "filter",
                        template::ScalarKind::U8K,
                        off + 19,
                        1,
                        template::Value::U8Val(data[11]),
                    );
                    push_scalar(
                        &mut nodes,
                        chunk_idx,
                        "interlace",
                        template::ScalarKind::U8K,
                        off + 20,
                        1,
                        template::Value::U8Val(data[12]),
                    );
                }
            } else if data_len > 0 {
                nodes.push(template::Node {
                    name: "data".to_string(),
                    type_name: template::NodeType::ScalarArray((
                        template::ScalarKind::U8K,
                        data_len,
                    )),
                    span: template::Span { offset: off + 8, length: data_len },
                    value: None,
                    parent: Some(chunk_idx),
                    array: None,
                    display: None,
                    attributes: Vec::new(),
                });
            }

            if let Ok(crc_bytes) = source::read(off + 8 + data_len, 4) {
                let crc = u32::from_be_bytes([crc_bytes[0], crc_bytes[1], crc_bytes[2], crc_bytes[3]]);
                push_scalar(
                    &mut nodes,
                    chunk_idx,
                    "crc",
                    template::ScalarKind::U32K,
                    off + 8 + data_len,
                    4,
                    template::Value::U32Val(crc),
                );
            }

            if ty == "IEND" {
                saw_iend = true;
                off = chunk_end;
                break;
            }
            off = chunk_end;
        }

        if !saw_iend {
            diagnostics.push(template::Diagnostic {
                message: "no IEND chunk found — file may be truncated".into(),
                severity: template::Severity::Warning,
                file_offset: Some(off),
                template_line: None,
            });
        }

        template::ResultTree { nodes, diagnostics, byte_palette: None }
    }

    fn expand_array(
        &self,
        _array_id: u64,
        _start: u64,
        _end: u64,
    ) -> Result<Vec<template::Node>, template::Diagnostic> {
        Err(template::Diagnostic {
            message: "png template materialises arrays eagerly".to_string(),
            severity: template::Severity::Error,
            file_offset: None,
            template_line: None,
        })
    }
}

fn push_scalar(
    nodes: &mut Vec<template::Node>,
    parent: u32,
    name: &str,
    kind: template::ScalarKind,
    offset: u64,
    length: u64,
    value: template::Value,
) {
    nodes.push(template::Node {
        name: name.to_string(),
        type_name: template::NodeType::Scalar(kind),
        span: template::Span { offset, length },
        value: Some(value),
        parent: Some(parent),
        array: None,
        display: None,
        attributes: Vec::new(),
    });
}

fn fail(message: String) -> template::ResultTree {
    template::ResultTree {
        nodes: Vec::new(),
        diagnostics: vec![template::Diagnostic {
            message,
            severity: template::Severity::Error,
            file_offset: Some(0),
            template_line: None,
        }],
        byte_palette: None,
    }
}

hxy_plugin_api::template::export_template_runtime!(PngPlugin with_types_in hxy_plugin_api::template);
