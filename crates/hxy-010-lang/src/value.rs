//! Runtime values for the 010 template interpreter.
//!
//! 010 has a small value domain: integers (signed + unsigned, up to
//! 64-bit), floats, strings, and composites (structs, arrays). We
//! collapse all integer widths into `i128` / `u128` in memory and
//! remember the declared width so reads / writes round-trip correctly.

use std::fmt;

/// Endianness a read should use. Top-level state on the interpreter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Endian {
    #[default]
    Little,
    Big,
}

/// A primitive numeric kind, including its declared width and
/// signedness. Stored verbatim so the value retains its 010 type
/// identity after being boxed into a `Value`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrimKind {
    pub class: PrimClass,
    /// Width in bytes as 010 reads it (1, 2, 4, or 8).
    pub width: u8,
    pub signed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimClass {
    Int,
    Float,
    /// `char` / `uchar` — same byte width as Int but rendered as ASCII.
    Char,
}

impl PrimKind {
    pub fn u8() -> Self { Self { class: PrimClass::Int, width: 1, signed: false } }
    pub fn i8() -> Self { Self { class: PrimClass::Int, width: 1, signed: true } }
    pub fn u16() -> Self { Self { class: PrimClass::Int, width: 2, signed: false } }
    pub fn i16() -> Self { Self { class: PrimClass::Int, width: 2, signed: true } }
    pub fn u32() -> Self { Self { class: PrimClass::Int, width: 4, signed: false } }
    pub fn i32() -> Self { Self { class: PrimClass::Int, width: 4, signed: true } }
    pub fn u64() -> Self { Self { class: PrimClass::Int, width: 8, signed: false } }
    pub fn i64() -> Self { Self { class: PrimClass::Int, width: 8, signed: true } }
    pub fn f32() -> Self { Self { class: PrimClass::Float, width: 4, signed: true } }
    pub fn f64() -> Self { Self { class: PrimClass::Float, width: 8, signed: true } }
    pub fn char() -> Self { Self { class: PrimClass::Char, width: 1, signed: true } }
    pub fn uchar() -> Self { Self { class: PrimClass::Char, width: 1, signed: false } }
}

/// A runtime value produced by evaluating an expression or reading
/// bytes from the source. Composite values carry their children
/// inline because the interpreter already walks them sequentially.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// `void` / no value (statement expressions).
    Void,
    /// Unsigned integer up to 64 bits stored in 128 for arithmetic.
    UInt { value: u128, kind: PrimKind },
    /// Signed integer up to 64 bits stored in 128 for arithmetic.
    SInt { value: i128, kind: PrimKind },
    Float { value: f64, kind: PrimKind },
    /// A single character — 010 uses this for `char` and `uchar` field
    /// reads, and for character literals in expressions.
    Char { value: u32, kind: PrimKind },
    /// Null-terminated or length-prefixed string. The interpreter
    /// decides which based on how the value was produced.
    Str(String),
    /// Boxed `true` / `false`. Stored separately from integers so
    /// evaluator paths can branch on the concrete shape rather than
    /// re-deriving it from value bits.
    Bool(bool),
}

impl Value {
    /// Truthiness as 010 sees it: non-zero integers, non-empty strings,
    /// `true` booleans, and any non-NaN non-zero float.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Void => false,
            Value::UInt { value, .. } => *value != 0,
            Value::SInt { value, .. } => *value != 0,
            Value::Float { value, .. } => *value != 0.0 && !value.is_nan(),
            Value::Char { value, .. } => *value != 0,
            Value::Str(s) => !s.is_empty(),
            Value::Bool(b) => *b,
        }
    }

    /// Interpret as a signed integer, losing precision where needed.
    pub fn to_i128(&self) -> Option<i128> {
        match self {
            Value::UInt { value, .. } => Some(*value as i128),
            Value::SInt { value, .. } => Some(*value),
            Value::Char { value, .. } => Some(*value as i128),
            Value::Bool(b) => Some(if *b { 1 } else { 0 }),
            Value::Float { value, .. } => Some(*value as i128),
            _ => None,
        }
    }

    /// Interpret as a floating-point value.
    pub fn to_f64(&self) -> Option<f64> {
        match self {
            Value::UInt { value, .. } => Some(*value as f64),
            Value::SInt { value, .. } => Some(*value as f64),
            Value::Float { value, .. } => Some(*value),
            Value::Char { value, .. } => Some(*value as f64),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    /// True when this value reads as a floating-point number.
    pub fn is_float(&self) -> bool {
        matches!(self, Value::Float { .. })
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Void => write!(f, "void"),
            Value::UInt { value, .. } => write!(f, "{value}"),
            Value::SInt { value, .. } => write!(f, "{value}"),
            Value::Float { value, .. } => write!(f, "{value}"),
            Value::Char { value, .. } => match char::from_u32(*value) {
                Some(c) if !c.is_control() => write!(f, "'{c}'"),
                _ => write!(f, "'\\x{value:02X}'"),
            },
            Value::Str(s) => write!(f, "{s:?}"),
            Value::Bool(b) => write!(f, "{b}"),
        }
    }
}
