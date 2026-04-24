//! 010 Editor Binary Template language implementation — lexer, parser,
//! and tree-walking interpreter. Built incrementally: the lexer lands
//! first, with snapshot tests exercising real templates.

#![forbid(unsafe_code)]

pub mod ast;

pub(crate) mod interp;
pub(crate) mod lexer;
pub(crate) mod parser;
pub(crate) mod source;
pub(crate) mod token;
pub(crate) mod value;

#[cfg(feature = "arbitrary")]
pub mod fuzz;

pub use interp::Diagnostic;
pub use interp::Interpreter;
pub use interp::NodeIdx;
pub use interp::NodeOut;
pub use interp::RunResult;
pub use interp::RuntimeError;
pub use interp::Severity;
pub use lexer::LexError;
pub use lexer::tokenize;
pub use parser::ParseError;
pub use parser::parse;
pub use source::HexSource;
pub use source::MemorySource;
pub use source::SourceError;
pub use value::NodeType;
pub use value::ScalarKind;
pub use value::Value;
