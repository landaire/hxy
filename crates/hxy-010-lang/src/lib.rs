//! 010 Editor Binary Template language implementation — lexer, parser,
//! and tree-walking interpreter. Built incrementally: the lexer lands
//! first, with snapshot tests exercising real templates.

#![forbid(unsafe_code)]

pub mod ast;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod source;
pub mod token;
pub mod value;

pub use interp::Diagnostic;
pub use interp::Interpreter;
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
pub use token::Keyword;
pub use token::Span;
pub use token::Token;
pub use token::TokenKind;
pub use value::Endian;
pub use value::PrimKind;
pub use value::Value;
