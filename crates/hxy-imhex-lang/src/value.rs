//! Runtime values the interpreter passes around between expression
//! evaluation and field-decl reads.
//!
//! Internally we widen integers to `u128` / `i128` so ImHex's
//! 128-bit native types fit without a separate code path. The host
//! adapter narrows back to the WIT [`Value`] variants on the way out
//! of the runtime; values that overflow 64 bits get serialised as
//! `bytes-val` since WIT doesn't carry a native 128-bit numeric.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

/// Primitive class: integers vs floats vs character vs boolean. A
/// `PrimKind` is the smallest unit of decode dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimClass {
    Int,
    Float,
    Char,
    Bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrimKind {
    pub class: PrimClass,
    /// Width in bytes. 1 / 2 / 4 / 8 / 16 for ints, 4 / 8 for floats,
    /// 1 / 2 for char (char16), 1 for bool.
    pub width: u8,
    pub signed: bool,
}

impl PrimKind {
    pub const fn u8() -> Self {
        Self { class: PrimClass::Int, width: 1, signed: false }
    }
    pub const fn s8() -> Self {
        Self { class: PrimClass::Int, width: 1, signed: true }
    }
    pub const fn u16() -> Self {
        Self { class: PrimClass::Int, width: 2, signed: false }
    }
    pub const fn s16() -> Self {
        Self { class: PrimClass::Int, width: 2, signed: true }
    }
    pub const fn u32() -> Self {
        Self { class: PrimClass::Int, width: 4, signed: false }
    }
    pub const fn s32() -> Self {
        Self { class: PrimClass::Int, width: 4, signed: true }
    }
    pub const fn u64() -> Self {
        Self { class: PrimClass::Int, width: 8, signed: false }
    }
    pub const fn s64() -> Self {
        Self { class: PrimClass::Int, width: 8, signed: true }
    }
    pub const fn u128() -> Self {
        Self { class: PrimClass::Int, width: 16, signed: false }
    }
    pub const fn s128() -> Self {
        Self { class: PrimClass::Int, width: 16, signed: true }
    }
    pub const fn f32() -> Self {
        Self { class: PrimClass::Float, width: 4, signed: true }
    }
    pub const fn f64() -> Self {
        Self { class: PrimClass::Float, width: 8, signed: true }
    }
    pub const fn char() -> Self {
        Self { class: PrimClass::Char, width: 1, signed: false }
    }
    pub const fn char16() -> Self {
        Self { class: PrimClass::Char, width: 2, signed: false }
    }
    pub const fn bool() -> Self {
        Self { class: PrimClass::Bool, width: 1, signed: false }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// Unsigned integer, widened to `u128` so 128-bit types fit.
    UInt {
        value: u128,
        kind: PrimKind,
    },
    /// Signed integer, widened to `i128`.
    SInt {
        value: i128,
        kind: PrimKind,
    },
    Float {
        value: f64,
        kind: PrimKind,
    },
    Bool(bool),
    /// Single character codepoint.
    Char {
        value: u32,
        kind: PrimKind,
    },
    /// Decoded string (UTF-8 lossy from the source bytes). Used for
    /// `char` array reads and `str` typed fields.
    Str(String),
    /// Raw byte run. Used for arrays the renderer surfaces but the
    /// runtime doesn't decode further.
    Bytes(Vec<u8>),
    /// Successful no-result -- the language has no `void` keyword
    /// in expression position, but several control-flow paths
    /// (assignment, declaration without initialiser) still need a
    /// "nothing meaningful" sentinel.
    Void,
}

impl Value {
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::UInt { value, .. } => *value != 0,
            Value::SInt { value, .. } => *value != 0,
            Value::Float { value, .. } => *value != 0.0,
            Value::Bool(b) => *b,
            Value::Char { value, .. } => *value != 0,
            Value::Str(s) => !s.is_empty(),
            Value::Bytes(b) => !b.is_empty(),
            Value::Void => false,
        }
    }

    /// Best-effort numeric coercion to an `i128`. Returns `None` for
    /// strings, byte runs, and `Void`.
    pub fn to_i128(&self) -> Option<i128> {
        Some(match self {
            Value::UInt { value, .. } => *value as i128,
            Value::SInt { value, .. } => *value,
            Value::Float { value, .. } => *value as i128,
            Value::Bool(b) => i128::from(*b),
            Value::Char { value, .. } => i128::from(*value),
            _ => return None,
        })
    }

    /// Floating coercion: same idea as [`Self::to_i128`].
    pub fn to_f64(&self) -> Option<f64> {
        Some(match self {
            Value::UInt { value, .. } => *value as f64,
            Value::SInt { value, .. } => *value as f64,
            Value::Float { value, .. } => *value,
            Value::Bool(b) => f64::from(u8::from(*b)),
            Value::Char { value, .. } => f64::from(*value),
            _ => return None,
        })
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::UInt { value, .. } => write!(f, "{value}"),
            Value::SInt { value, .. } => write!(f, "{value}"),
            Value::Float { value, .. } => write!(f, "{value}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Char { value, .. } => match char::from_u32(*value) {
                Some(c) => write!(f, "'{c}'"),
                None => write!(f, "0x{value:X}"),
            },
            Value::Str(s) => write!(f, "{s:?}"),
            Value::Bytes(b) => write!(f, "[{} bytes]", b.len()),
            Value::Void => write!(f, "()"),
        }
    }
}

/// Coarse-grained "what kind of node is this" classification. Used
/// when the host adapter builds the WIT-shaped `node-type`. Mirrors
/// the 010 lang's [`hxy_010_lang::ScalarKind`] / `NodeType` shapes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScalarKind {
    U8,
    U16,
    U32,
    U64,
    U128,
    S8,
    S16,
    S32,
    S64,
    S128,
    F32,
    F64,
    Bool,
    Bytes,
    Str,
    /// Char rendered as 1- or 2-byte unsigned int -- the host
    /// surfaces it as `u8`/`u16` until char rendering lands.
    Char,
    Char16,
}

impl ScalarKind {
    /// Pick the right [`ScalarKind`] for a primitive. Char widens to
    /// `Char` / `Char16` so the host can pick a renderer; bool is
    /// always `Bool`.
    pub fn from_prim(p: PrimKind) -> Self {
        match (p.class, p.width, p.signed) {
            (PrimClass::Float, 4, _) => Self::F32,
            (PrimClass::Float, _, _) => Self::F64,
            (PrimClass::Bool, _, _) => Self::Bool,
            (PrimClass::Char, 1, _) => Self::Char,
            (PrimClass::Char, _, _) => Self::Char16,
            (PrimClass::Int, 1, false) => Self::U8,
            (PrimClass::Int, 1, true) => Self::S8,
            (PrimClass::Int, 2, false) => Self::U16,
            (PrimClass::Int, 2, true) => Self::S16,
            (PrimClass::Int, 4, false) => Self::U32,
            (PrimClass::Int, 4, true) => Self::S32,
            (PrimClass::Int, 8, false) => Self::U64,
            (PrimClass::Int, 8, true) => Self::S64,
            (PrimClass::Int, 16, false) => Self::U128,
            (PrimClass::Int, 16, true) => Self::S128,
            // Unknown widths fall back to bytes -- the renderer can
            // still show the underlying data without a typed view.
            _ => Self::Bytes,
        }
    }
}

/// Tag describing a [`NodeOut`]'s shape. Carried as metadata through
/// the run so the host adapter doesn't have to re-derive it from
/// [`ScalarKind`] + array length.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeType {
    Scalar(ScalarKind),
    ScalarArray(ScalarKind, u64),
    StructType(String),
    StructArray(String, u64),
    EnumType(String),
    EnumArray(String, u64),
    BitfieldType(String),
    /// Escape hatch.
    Unknown(String),
}
