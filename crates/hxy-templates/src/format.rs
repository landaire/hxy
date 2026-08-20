//! Template value formatting shared by the frontends: scalar value
//! rendering for the panel's Value column, byte/string previews,
//! on-demand decoding of primitive-array elements, and clipboard
//! copy formatting for template nodes.

use hxy_core::copy::CopyKind;
use hxy_core::format::IntValueType;
use hxy_core::format::NumericBase;
use hxy_core::format::NumericFormat;
use hxy_core::format::TemplateValueFormats;
use hxy_plugin_host::template::Node;

/// Character budget for rendering a string value's preview before it
/// collapses to `"head..." (N bytes)`. Keeps a multi-megabyte
/// `string` field from blowing up a single row.
const STRING_VALUE_PREVIEW_CHARS: usize = 64;

/// Byte budget for rendering a byte value's hex-escaped preview before
/// it collapses to `'\xAB\xCD...' (N bytes)`. Smaller than the string
/// budget because each byte expands to four characters (`\xHH`).
const BYTES_VALUE_PREVIEW_BYTES: usize = 16;

/// Render a string value with surrounding double quotes and Rust-style
/// debug escaping. Empty strings come out as `""`, which is enough to
/// give the surrounding label a non-zero galley (an empty galley would
/// trip egui's `show_unaligned` overlay).
pub fn quote_string_preview(s: &str) -> String {
    let mut chars = s.chars();
    let preview: String = chars.by_ref().take(STRING_VALUE_PREVIEW_CHARS).collect();
    if chars.next().is_none() { format!("{preview:?}") } else { format!("{preview:?}... ({} bytes)", s.len()) }
}

/// Render a byte slice as `'\xAB\xCD...'` so the user can tell it apart
/// from a string at a glance. Long buffers truncate to
/// [`BYTES_VALUE_PREVIEW_BYTES`] with a `... (N bytes)` tail.
///
/// When the buffer is valid UTF-8 *and* mostly looks like a real
/// string (no embedded NULs, mostly printable / common-whitespace
/// codepoints), the preview routes through [`quote_string_preview`]
/// instead -- a byte buffer that's actually text reads much better
/// as `"PNG\r\n..."` than as `\x50\x4E\x47\x0D\x0A\x...`. Stops short
/// of running on long buffers (validity check on the first ~256
/// bytes) so the heuristic stays cheap.
pub fn quote_bytes_preview(b: &[u8]) -> String {
    use std::fmt::Write as _;

    if let Some(text) = bytes_as_string_preview(b) {
        return quote_string_preview(&text);
    }
    let head_len = BYTES_VALUE_PREVIEW_BYTES.min(b.len());
    let mut out = String::with_capacity(head_len * 4 + 16);
    out.push('\'');
    for byte in &b[..head_len] {
        let _ = write!(out, "\\x{byte:02X}");
    }
    out.push('\'');
    if b.len() > head_len {
        let _ = write!(out, "... ({} bytes)", b.len());
    }
    out
}

/// Decode `b` as UTF-8 and return the head when it's "string-like":
/// non-empty, decodes successfully (limited to a head sample so
/// multi-MB buffers don't get fully validated for nothing), and the
/// majority of decoded chars are printable / common whitespace.
/// The caller renders the result as a quoted string preview.
fn bytes_as_string_preview(b: &[u8]) -> Option<String> {
    /// Cap on how many bytes we try to UTF-8-decode for the
    /// preview. Keeps the heuristic O(1) regardless of how big
    /// the underlying buffer is. Picked > the visible preview
    /// budget so the preview itself is never short on chars.
    const SAMPLE_BYTES: usize = 256;

    if b.is_empty() {
        return None;
    }
    let sample = &b[..b.len().min(SAMPLE_BYTES)];
    // Tolerate a multi-byte char straddling the sample boundary:
    // accept the longest valid UTF-8 prefix.
    let valid_len = match std::str::from_utf8(sample) {
        Ok(_) => sample.len(),
        Err(e) => e.valid_up_to(),
    };
    if valid_len == 0 {
        return None;
    }
    let head = std::str::from_utf8(&sample[..valid_len]).ok()?;
    if !looks_like_text(head) {
        return None;
    }
    // Decode as much of the *full* buffer as is valid UTF-8 so a
    // long string preview shows the full byte count, not a 256-byte
    // truncation.
    let full_valid = match std::str::from_utf8(b) {
        Ok(_) => b.len(),
        Err(e) => e.valid_up_to(),
    };
    Some(std::str::from_utf8(&b[..full_valid]).ok()?.to_owned())
}

/// "String-like" heuristic for the head sample: at least 50% of
/// chars are printable / common whitespace, and there are no
/// interior NUL bytes (which signal a binary buffer that just
/// happens to contain ASCII letters near the start). NULs at the
/// very end are tolerated -- C-style null-terminated strings are
/// common in templates.
fn looks_like_text(head: &str) -> bool {
    let trimmed = head.trim_end_matches('\0');
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains('\0') {
        return false;
    }
    let total = trimmed.chars().count();
    let printable = trimmed.chars().filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r')).count();
    printable * 2 >= total
}

/// Returns `Some(text)` for a scalar value to render in the Value
/// column, or `None` for composite rows (struct headers, bitfield
/// parents) that have no value of their own. Callers must skip the
/// Label widget on `None` -- adding `Label::new("")` produces a
/// zero-width galley whose `line_height` is font-dependent and
/// often sub-pixel, which trips egui's `show_unaligned` debug
/// overlay on the cell's enclosing `Ui`.
///
/// Integer values respect the template's `display` hint when
/// present (`[[hex]]` / `[[decimal]]`); for fields without an
/// explicit hint, `fmt` decides between hex and decimal based
/// on the value's magnitude. Hex output for signed types uses
/// the bit pattern (`-1_i32` -> `0xFFFFFFFF`) so it lines up
/// with what the user would read off the hex view.
pub fn format_value(node: &Node, fmts: &TemplateValueFormats, inverse: bool) -> Option<String> {
    use hxy_plugin_host::template::Value;
    let v = node.value.as_ref()?;
    let display = node.display;
    let int = |ty: IntValueType| fmts.slot(ty);
    Some(match v {
        Value::U8Val(x) => format_unsigned_int(display, u64::from(*x), 1, int(IntValueType::U8), inverse),
        Value::U16Val(x) => format_unsigned_int(display, u64::from(*x), 2, int(IntValueType::U16), inverse),
        Value::U32Val(x) => format_unsigned_int(display, u64::from(*x), 4, int(IntValueType::U32), inverse),
        Value::U64Val(x) => format_unsigned_int(display, *x, 8, int(IntValueType::U64), inverse),
        Value::S8Val(x) => format_signed_int(display, i64::from(*x), 1, int(IntValueType::S8), inverse),
        Value::S16Val(x) => format_signed_int(display, i64::from(*x), 2, int(IntValueType::S16), inverse),
        Value::S32Val(x) => format_signed_int(display, i64::from(*x), 4, int(IntValueType::S32), inverse),
        Value::S64Val(x) => format_signed_int(display, *x, 8, int(IntValueType::S64), inverse),
        Value::F32Val(x) => format!("{x}"),
        Value::F64Val(x) => format!("{x}"),
        Value::BoolVal(b) => format!("{b}"),
        Value::BytesVal(b) => quote_bytes_preview(b),
        Value::StringVal(s) => quote_string_preview(s),
        Value::EnumVal((name, raw)) => format!("{name} ({raw})"),
    })
}

/// Pick a base for an integer value. The template's display hint
/// wins when it's `Hex` or `Decimal`; non-numeric hints
/// (`Binary`, `Ascii`, `Timestamp`, `Color`) leave us alone --
/// they don't fit into the user's binary hex/decimal toggle and
/// are passed through to whichever existing formatter handles
/// them. Otherwise the user's [`NumericFormat`] decides based on
/// `magnitude`.
fn pick_value_base(
    display: Option<hxy_plugin_host::template::DisplayHint>,
    magnitude: u64,
    fmt: NumericFormat,
) -> Option<NumericBase> {
    use hxy_plugin_host::template::DisplayHint;
    match display {
        Some(DisplayHint::Hex) => Some(NumericBase::Hex),
        Some(DisplayHint::Decimal) => Some(NumericBase::Decimal),
        Some(_) => None,
        None => Some(fmt.pick(magnitude)),
    }
}

fn format_unsigned_int(
    display: Option<hxy_plugin_host::template::DisplayHint>,
    value: u64,
    byte_width: usize,
    fmt: NumericFormat,
    inverse: bool,
) -> String {
    match flip_if(pick_value_base(display, value, fmt), inverse) {
        Some(NumericBase::Hex) => {
            let digits = byte_width * 2;
            format!("0x{value:0digits$X}")
        }
        Some(NumericBase::Decimal) | None => format!("{value}"),
    }
}

fn format_signed_int(
    display: Option<hxy_plugin_host::template::DisplayHint>,
    value: i64,
    byte_width: usize,
    fmt: NumericFormat,
    inverse: bool,
) -> String {
    let magnitude = value.unsigned_abs();
    match flip_if(pick_value_base(display, magnitude, fmt), inverse) {
        Some(NumericBase::Hex) => {
            // Bit-pattern hex matches what the user reads off
            // the hex view: -1_i32 displays as 0xFFFFFFFF, not
            // -0x1.
            let digits = byte_width * 2;
            let bits = match byte_width {
                1 => u64::from(value as i8 as u8),
                2 => u64::from(value as i16 as u16),
                4 => u64::from(value as i32 as u32),
                _ => value as u64,
            };
            format!("0x{bits:0digits$X}")
        }
        Some(NumericBase::Decimal) | None => format!("{value}"),
    }
}

/// Toggle a picked base when the user is holding the inverse-
/// format modifier. `None` (template hint we don't handle, like
/// `Binary` / `Ascii`) passes through unchanged so non-numeric
/// hints keep their special formatting even with the modifier
/// down.
fn flip_if(base: Option<NumericBase>, flip: bool) -> Option<NumericBase> {
    match base {
        Some(b) if flip => Some(b.toggle()),
        other => other,
    }
}

pub fn scalar_kind_width(kind: hxy_plugin_host::template::ScalarKind) -> Option<u64> {
    use hxy_plugin_host::template::ScalarKind as K;
    Some(match kind {
        K::U8K | K::S8K | K::BoolK => 1,
        K::U16K | K::S16K => 2,
        K::U32K | K::S32K | K::F32K => 4,
        K::U64K | K::S64K | K::F64K => 8,
        K::U128K | K::S128K => 16,
        K::BytesK | K::StringK => return None,
    })
}

pub fn scalar_kind_name(kind: hxy_plugin_host::template::ScalarKind) -> &'static str {
    use hxy_plugin_host::template::ScalarKind as K;
    match kind {
        K::U8K => "uchar",
        K::S8K => "char",
        K::U16K => "uint16",
        K::S16K => "int16",
        K::U32K => "uint32",
        K::S32K => "int32",
        K::U64K => "uint64",
        K::S64K => "int64",
        K::U128K => "uint128",
        K::S128K => "int128",
        K::F32K => "float",
        K::F64K => "double",
        K::BoolK => "bool",
        K::BytesK => "bytes",
        K::StringK => "string",
    }
}

pub fn decode_scalar_bytes(
    kind: hxy_plugin_host::template::ScalarKind,
    bytes: &[u8],
    endian: &str,
    display: Option<hxy_plugin_host::template::DisplayHint>,
    fmts: &TemplateValueFormats,
    inverse: bool,
) -> Option<String> {
    use hxy_plugin_host::template::ScalarKind as K;
    let big = endian == "big";
    let read_u = |b: &[u8]| -> u64 {
        let mut buf = [0u8; 8];
        if big {
            buf[8 - b.len()..].copy_from_slice(b);
            u64::from_be_bytes(buf)
        } else {
            buf[..b.len()].copy_from_slice(b);
            u64::from_le_bytes(buf)
        }
    };
    let read_i = |b: &[u8]| -> i64 {
        let raw = read_u(b);
        let shift = 64 - (b.len() as u32) * 8;
        ((raw << shift) as i64) >> shift
    };
    Some(match kind {
        K::U8K => format_unsigned_int(display, u64::from(*bytes.first()?), 1, fmts.slot(IntValueType::U8), inverse),
        K::S8K => format_signed_int(display, i64::from(*bytes.first()? as i8), 1, fmts.slot(IntValueType::S8), inverse),
        K::U16K => format_unsigned_int(display, read_u(bytes), 2, fmts.slot(IntValueType::U16), inverse),
        K::U32K => format_unsigned_int(display, read_u(bytes), 4, fmts.slot(IntValueType::U32), inverse),
        K::U64K => format_unsigned_int(display, read_u(bytes), 8, fmts.slot(IntValueType::U64), inverse),
        K::S16K => format_signed_int(display, read_i(bytes), 2, fmts.slot(IntValueType::S16), inverse),
        K::S32K => format_signed_int(display, read_i(bytes), 4, fmts.slot(IntValueType::S32), inverse),
        K::S64K => format_signed_int(display, read_i(bytes), 8, fmts.slot(IntValueType::S64), inverse),
        K::U128K | K::S128K => {
            // 128-bit ints don't have a u128/i128 path through `read_u`
            // / `read_i`. Render as `0x` + raw bytes in source-endian
            // order so the inspector still shows the bits the user
            // wrote, just without a typed numeric form.
            let mut out = String::with_capacity(2 + bytes.len() * 2);
            out.push_str("0x");
            let iter: Box<dyn Iterator<Item = &u8>> =
                if big { Box::new(bytes.iter()) } else { Box::new(bytes.iter().rev()) };
            for b in iter {
                out.push_str(&format!("{b:02X}"));
            }
            out
        }
        K::F32K => {
            let arr: [u8; 4] = bytes.try_into().ok()?;
            let v = if big { f32::from_be_bytes(arr) } else { f32::from_le_bytes(arr) };
            format!("{v}")
        }
        K::F64K => {
            let arr: [u8; 8] = bytes.try_into().ok()?;
            let v = if big { f64::from_be_bytes(arr) } else { f64::from_le_bytes(arr) };
            format!("{v}")
        }
        K::BoolK => format!("{}", bytes.first()? != &0),
        K::BytesK | K::StringK => return None,
    })
}

/// Read `node`'s byte span from `source` and format it according to
/// `kind`. Returns `None` when the bytes can't be read (out of
/// bounds, I/O error) -- the caller silently drops the copy.
pub fn format_template_copy(
    source: &std::sync::Arc<dyn hxy_core::HexSource>,
    node: &Node,
    kind: CopyKind,
) -> Option<String> {
    if kind.is_value() {
        let raw = scalar_value_u64(node.value.as_ref()?)?;
        return hxy_core::copy::format_scalar(kind, raw);
    }
    let start = hxy_core::ByteOffset::new(node.span.offset);
    let end = hxy_core::ByteOffset::new(node.span.offset.saturating_add(node.span.length));
    let range = hxy_core::ByteRange::new(start, end).ok()?;
    let bytes = source.read(range).ok()?;
    let ty = hxy_plugin_host::node_type_label(&node.type_name);
    hxy_core::copy::format_bytes(kind, &bytes, &node.name, &ty)
}

/// Walk a struct (or array-of-structs) node and produce a C99
/// designated-initialiser block or a Rust struct literal that
/// mirrors its children's field layout and values. Runs recursively
/// so nested structs and arrays render inline.
pub fn format_template_struct(nodes: &[Node], root_idx: usize, kind: CopyKind) -> Option<String> {
    let root = nodes.get(root_idx)?;
    let mut out = String::new();
    let ident = hxy_core::copy::sanitize_ident(&root.name);
    let ty = hxy_plugin_host::node_type_label(&root.type_name);
    match kind {
        CopyKind::StructRust => {
            use std::fmt::Write;
            let _ = write!(out, "let {ident}: {ty} = ");
            write_struct_body(&mut out, nodes, root_idx, StructSyntax::Rust, 0)?;
            out.push(';');
        }
        CopyKind::StructC => {
            use std::fmt::Write;
            let _ = write!(out, "{ty} {ident} = ");
            write_struct_body(&mut out, nodes, root_idx, StructSyntax::C, 0)?;
            out.push(';');
        }
        _ => return None,
    }
    Some(out)
}

#[derive(Clone, Copy)]
enum StructSyntax {
    Rust,
    C,
}

/// Recursive body writer: emits `{ field: value, ... }` for a
/// struct, `[v0, v1, ...]` / `{ v0, v1, ... }` for an array, or a
/// literal for a scalar leaf. Returns `None` if the tree is
/// inconsistent (no children of a struct, e.g.).
fn write_struct_body(out: &mut String, nodes: &[Node], idx: usize, syntax: StructSyntax, depth: usize) -> Option<()> {
    use hxy_plugin_host::template::NodeType;
    use std::fmt::Write;

    let node = nodes.get(idx)?;
    let children: Vec<usize> =
        nodes.iter().enumerate().filter_map(|(i, n)| (n.parent == Some(idx as u32)).then_some(i)).collect();

    match &node.type_name {
        NodeType::StructType(name) | NodeType::StructArray((name, _)) => {
            // For arrays of structs, each child element IS a
            // struct node; we recurse into each so the output is
            // `[ Struct { .. }, Struct { .. } ]`.
            let is_array = matches!(node.type_name, NodeType::StructArray(_));
            if is_array {
                open_array(out, syntax);
                for (i, &cidx) in children.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_struct_body(out, nodes, cidx, syntax, depth + 1)?;
                }
                close_array(out, syntax);
            } else {
                match syntax {
                    StructSyntax::Rust => {
                        let _ = write!(out, "{name} {{");
                    }
                    StructSyntax::C => out.push('{'),
                }
                for &cidx in &children {
                    let child = &nodes[cidx];
                    out.push('\n');
                    for _ in 0..=depth {
                        out.push_str("    ");
                    }
                    match syntax {
                        StructSyntax::Rust => {
                            let _ = write!(out, "{}: ", hxy_core::copy::sanitize_ident(&child.name));
                        }
                        StructSyntax::C => {
                            let _ = write!(out, ".{} = ", hxy_core::copy::sanitize_ident(&child.name));
                        }
                    }
                    write_struct_body(out, nodes, cidx, syntax, depth + 1)?;
                    out.push(',');
                }
                out.push('\n');
                for _ in 0..depth {
                    out.push_str("    ");
                }
                out.push('}');
            }
        }
        NodeType::EnumType(_) | NodeType::EnumArray(_) => {
            // Enums and enum-arrays print their raw scalar value --
            // the named variant isn't tracked on the wire.
            write_scalar_or_array(out, node, &children, nodes, syntax, depth)?;
        }
        NodeType::Scalar(_) | NodeType::ScalarArray(_) | NodeType::Unknown(_) => {
            write_scalar_or_array(out, node, &children, nodes, syntax, depth)?;
        }
    }
    Some(())
}

fn write_scalar_or_array(
    out: &mut String,
    node: &Node,
    children: &[usize],
    nodes: &[Node],
    syntax: StructSyntax,
    depth: usize,
) -> Option<()> {
    use hxy_plugin_host::template::NodeType;
    let is_array = matches!(node.type_name, NodeType::ScalarArray(_) | NodeType::EnumArray(_));
    if is_array {
        open_array(out, syntax);
        // Scalar arrays may either have child element nodes (one
        // per entry) or a bare `value` of Bytes. Handle the nodes
        // case first; when empty, fall back to formatting the
        // raw value.
        if !children.is_empty() {
            for (i, &cidx) in children.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_struct_body(out, nodes, cidx, syntax, depth + 1)?;
            }
        } else if let Some(v) = node.value.as_ref() {
            out.push_str(&format_scalar_literal(v, syntax));
        }
        close_array(out, syntax);
    } else if let Some(v) = node.value.as_ref() {
        out.push_str(&format_scalar_literal(v, syntax));
    } else {
        out.push('0');
    }
    Some(())
}

fn open_array(out: &mut String, syntax: StructSyntax) {
    out.push_str(match syntax {
        StructSyntax::Rust => "[",
        StructSyntax::C => "{",
    });
}

fn close_array(out: &mut String, syntax: StructSyntax) {
    out.push_str(match syntax {
        StructSyntax::Rust => "]",
        StructSyntax::C => "}",
    });
}

/// Literal rendering for a single scalar value. Mirrors the
/// inspector's conventions: integers hex-prefixed for Rust/C
/// (`0x...`), floats with trailing type suffix for Rust, booleans
/// lowercased. Falls back to a lossless debug form for values the
/// scalar formatters can't represent directly (strings, bytes,
/// enums).
fn format_scalar_literal(v: &hxy_plugin_host::template::Value, syntax: StructSyntax) -> String {
    use hxy_plugin_host::template::Value;
    match v {
        Value::U8Val(x) => format!("0x{x:02X}"),
        Value::U16Val(x) => format!("0x{x:04X}"),
        Value::U32Val(x) => format!("0x{x:08X}"),
        Value::U64Val(x) => format!("0x{x:016X}"),
        Value::S8Val(x) => format!("{x}"),
        Value::S16Val(x) => format!("{x}"),
        Value::S32Val(x) => format!("{x}"),
        Value::S64Val(x) => format!("{x}"),
        Value::F32Val(x) => match syntax {
            StructSyntax::Rust => format!("{x}f32"),
            StructSyntax::C => format!("{x}f"),
        },
        Value::F64Val(x) => match syntax {
            StructSyntax::Rust => format!("{x}f64"),
            StructSyntax::C => format!("{x}"),
        },
        Value::StringVal(s) => format!("{s:?}"),
        Value::BytesVal(bs) => {
            let mut out = String::new();
            out.push_str(match syntax {
                StructSyntax::Rust => "[",
                StructSyntax::C => "{",
            });
            for (i, b) in bs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("0x{b:02X}"));
            }
            out.push_str(match syntax {
                StructSyntax::Rust => "]",
                StructSyntax::C => "}",
            });
            out
        }
        Value::EnumVal((name, value)) => {
            // Print the numeric value (both syntaxes accept integer
            // literals here), with the variant name as a trailing
            // comment so it's still visible in the output.
            format!("{value} /* {name} */")
        }
        Value::BoolVal(b) => match syntax {
            StructSyntax::Rust | StructSyntax::C => format!("{b}"),
        },
    }
}

/// Extract a u64 bit pattern from a scalar [`hxy_plugin_host::template::Value`],
/// preserving signed-bit representation so hex displays match what the
/// user sees on the wire. Returns `None` for non-scalar values (Str /
/// Bool / Bytes / Enum).
fn scalar_value_u64(v: &hxy_plugin_host::template::Value) -> Option<u64> {
    use hxy_plugin_host::template::Value;
    Some(match v {
        Value::U8Val(x) => u64::from(*x),
        Value::U16Val(x) => u64::from(*x),
        Value::U32Val(x) => u64::from(*x),
        Value::U64Val(x) => *x,
        Value::S8Val(x) => *x as u8 as u64,
        Value::S16Val(x) => *x as u16 as u64,
        Value::S32Val(x) => *x as u32 as u64,
        Value::S64Val(x) => *x as u64,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_preview_renders_text_as_string() {
        let bytes: &[u8] = b"PNG\r\n";
        let preview = quote_bytes_preview(bytes);
        assert!(preview.starts_with('"'), "expected quoted string preview, got {preview:?}");
        assert!(preview.contains("PNG"), "expected ascii content, got {preview:?}");
    }

    #[test]
    fn bytes_preview_falls_back_to_hex_for_binary() {
        let bytes: &[u8] = &[0x8E, 0xA3, 0xFF, 0xC0, 0x42];
        let preview = quote_bytes_preview(bytes);
        assert!(preview.starts_with('\''), "expected hex preview, got {preview:?}");
        assert!(preview.contains("\\x8E"), "expected hex-escaped first byte, got {preview:?}");
    }

    #[test]
    fn bytes_preview_rejects_buffer_with_interior_nul() {
        let bytes: &[u8] = b"hello\0world";
        let preview = quote_bytes_preview(bytes);
        assert!(preview.starts_with('\''), "expected hex preview, got {preview:?}");
    }

    #[test]
    fn bytes_preview_accepts_trailing_nul_terminator() {
        let bytes: &[u8] = b"hello\0";
        let preview = quote_bytes_preview(bytes);
        assert!(preview.starts_with('"'), "expected string preview, got {preview:?}");
        assert!(preview.contains("hello"), "expected text content, got {preview:?}");
    }

    fn fmt_always(b: NumericBase) -> NumericFormat {
        NumericFormat::Always(b)
    }

    fn fmt_threshold(t: u64) -> NumericFormat {
        NumericFormat::Threshold { small: NumericBase::Decimal, large: NumericBase::Hex, threshold: t }
    }

    #[test]
    fn unsigned_int_decimal_default() {
        let s = format_unsigned_int(None, 42, 1, fmt_always(NumericBase::Decimal), false);
        assert_eq!(s, "42");
    }

    #[test]
    fn unsigned_int_hex_padded_per_width() {
        // u8 -> 2 hex digits, u32 -> 8, u64 -> 16. Width comes from
        // the byte_width arg, not the magnitude.
        let hex = fmt_always(NumericBase::Hex);
        assert_eq!(format_unsigned_int(None, 0xAB, 1, hex, false), "0xAB");
        assert_eq!(format_unsigned_int(None, 0xAB, 4, hex, false), "0x000000AB");
        assert_eq!(format_unsigned_int(None, 0xDEAD_BEEF, 8, hex, false), "0x00000000DEADBEEF");
    }

    #[test]
    fn signed_int_hex_uses_bit_pattern() {
        // -1 as i32 -> 0xFFFFFFFF (matches what the user reads off
        // the hex view), not -0x1 (signed-magnitude).
        let hex = fmt_always(NumericBase::Hex);
        assert_eq!(format_signed_int(None, -1, 4, hex, false), "0xFFFFFFFF");
        assert_eq!(format_signed_int(None, -1, 1, hex, false), "0xFF");
    }

    #[test]
    fn template_display_hint_overrides_user_format() {
        // `[[hex]]` on the field should still produce hex even when
        // the user's setting is Always(Decimal) -- the template
        // author's intent wins.
        use hxy_plugin_host::template::DisplayHint;
        let dec = fmt_always(NumericBase::Decimal);
        let hex = fmt_always(NumericBase::Hex);
        assert_eq!(format_unsigned_int(Some(DisplayHint::Hex), 16, 4, dec, false), "0x00000010");
        // Conversely, `[[decimal]]` overrides Always(Hex).
        assert_eq!(format_unsigned_int(Some(DisplayHint::Decimal), 16, 4, hex, false), "16");
    }

    #[test]
    fn threshold_picks_base_per_value_magnitude() {
        // small under threshold -> decimal, big over -> hex.
        let fmt = fmt_threshold(256);
        assert_eq!(format_unsigned_int(None, 100, 4, fmt, false), "100");
        assert_eq!(format_unsigned_int(None, 256, 4, fmt, false), "0x00000100");
        assert_eq!(format_unsigned_int(None, 0xDEAD, 4, fmt, false), "0x0000DEAD");
    }

    #[test]
    fn inverse_modifier_flips_picked_base() {
        // When the user holds the inverse modifier, the picked
        // base toggles. The toggle applies even on top of a
        // template display-hint -- the user is asking to peek
        // at the alternate base, regardless of the author's
        // chosen default.
        let dec = fmt_always(NumericBase::Decimal);
        assert_eq!(format_unsigned_int(None, 16, 4, dec, false), "16");
        assert_eq!(format_unsigned_int(None, 16, 4, dec, true), "0x00000010");
        // Threshold form: inverse swaps each branch.
        let fmt = fmt_threshold(256);
        assert_eq!(format_unsigned_int(None, 100, 4, fmt, true), "0x00000064");
        assert_eq!(format_unsigned_int(None, 256, 4, fmt, true), "256");
    }

    #[test]
    fn template_value_formats_per_type_lookup() {
        let mut fmts = TemplateValueFormats::default();
        // Override just u32 to decimal; u8 keeps its hex default.
        *fmts.slot_mut(IntValueType::U32) = NumericFormat::Always(NumericBase::Decimal);
        assert_eq!(fmts.slot(IntValueType::U8), NumericFormat::Always(NumericBase::Hex));
        assert_eq!(fmts.slot(IntValueType::U32), NumericFormat::Always(NumericBase::Decimal));
    }

    use hxy_plugin_host::template::NodeType;
    use hxy_plugin_host::template::ScalarKind;
    use hxy_plugin_host::template::Span;
    use hxy_plugin_host::template::Value;

    fn tree_node(name: &str, type_name: NodeType, parent: Option<u32>, value: Option<Value>, span: (u64, u64)) -> Node {
        Node {
            name: name.to_owned(),
            type_name,
            span: Span { offset: span.0, length: span.1 },
            value,
            parent,
            array: None,
            display: None,
            attributes: Vec::new(),
        }
    }

    /// Byte-for-byte parity with the egui app's struct copy output
    /// (the fns moved here verbatim from `crates/hxy/src/app/mod.rs`).
    #[test]
    fn struct_copy_matches_egui_fixture() {
        let nodes = vec![
            tree_node("header", NodeType::StructType("Header".to_owned()), None, None, (0, 6)),
            tree_node("magic", NodeType::Scalar(ScalarKind::U32K), Some(0), Some(Value::U32Val(0xCAFE_BABE)), (0, 4)),
            tree_node("flags", NodeType::Scalar(ScalarKind::S16K), Some(0), Some(Value::S16Val(-2)), (4, 2)),
        ];
        let rust = format_template_struct(&nodes, 0, CopyKind::StructRust).unwrap();
        assert_eq!(rust, "let header: Header = Header {\n    magic: 0xCAFEBABE,\n    flags: -2,\n};");
        let c = format_template_struct(&nodes, 0, CopyKind::StructC).unwrap();
        assert_eq!(c, "Header header = {\n    .magic = 0xCAFEBABE,\n    .flags = -2,\n};");
        assert!(format_template_struct(&nodes, 0, CopyKind::BytesHexSpaced).is_none());
    }

    #[test]
    fn template_copy_value_uses_bit_pattern_and_bytes_read_the_span() {
        let source: std::sync::Arc<dyn hxy_core::HexSource> =
            std::sync::Arc::new(hxy_core::MemorySource::new(vec![0x00, 0x50, 0x4B, 0xFF]));
        let node = tree_node("sig", NodeType::Scalar(ScalarKind::S8K), None, Some(Value::S8Val(-1)), (1, 2));
        assert_eq!(format_template_copy(&source, &node, CopyKind::ValueHex).as_deref(), Some("0xFF"));
        assert_eq!(format_template_copy(&source, &node, CopyKind::BytesHexSpaced).as_deref(), Some("50 4B"));
        // Span past EOF: the read fails and the copy is dropped.
        let oob = tree_node("oob", NodeType::Scalar(ScalarKind::U8K), None, None, (3, 4));
        assert_eq!(format_template_copy(&source, &oob, CopyKind::BytesHexSpaced), None);
    }
}
