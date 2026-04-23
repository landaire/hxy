//! 010 Editor Binary Template language implementation — lexer, parser,
//! and tree-walking interpreter. Built incrementally: the lexer lands
//! first, with snapshot tests exercising real templates.

#![forbid(unsafe_code)]

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod token;

pub use lexer::LexError;
pub use lexer::tokenize;
pub use parser::ParseError;
pub use parser::parse;
pub use token::Keyword;
pub use token::Span;
pub use token::Token;
pub use token::TokenKind;
