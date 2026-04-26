//! ImHex pattern language: lexer + parser + interpreter.
//!
//! This is a **clean-room** implementation built solely from the
//! public language reference and from reading user-authored
//! `.hexpat` files for surface-syntax cues. We do not look at the
//! upstream `pattern_language` runtime source, and we do not bundle
//! any of the GPL-licensed corpus from `WerWolv/ImHex-Patterns`.
//! Tests use synthetic patterns we author here.
//!
//! The pattern language is a near-cousin of 010 Editor's binary
//! template language: declarations read bytes, attributes annotate
//! them, and the result is a tree of fields the host renders. Where
//! the two diverge enough that a shared parser would be a tar pit
//! (declaration shape, attribute syntax, namespace / template
//! features, placement `@` operator) we keep separate ASTs and
//! converge at the host's [`hxy_plugin_host::template`] schema --
//! see the `Phase 0` schema-generalisation work for the shared
//! field set.

pub mod ast;
pub mod imports;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod source;
pub mod token;
pub mod value;

pub use imports::DirImportResolver;
pub use imports::ImportResolver;
pub use imports::NoImportResolver;
pub use imports::SharedResolver;
pub use imports::chained_resolver;
pub use interp::Diagnostic;
pub use interp::Interpreter;
pub use interp::NodeIdx;
pub use interp::NodeOut;
pub use interp::RunResult;
pub use interp::RuntimeError;
pub use interp::Severity;
pub use lexer::LexError;
pub use lexer::Pragmas;
pub use lexer::extract_pragmas;
pub use lexer::tokenize;
pub use parser::ParseError;
pub use parser::parse;
pub use source::HexSource;
pub use source::MemorySource;
pub use source::SourceError;
pub use value::NodeType;
pub use value::PrimKind;
pub use value::ScalarKind;
pub use value::Value;
