//! Rust-native workflow scripts (NWS).
//!
//! The TypeScript compiler front-end and the Node sandbox are replaced by this module: a
//! lexer, a parser, a resolver that lowers the AST to a typed plan, and a tokio
//! interpreter that calls the engine directly. See `specs/nc-code-workflow-rust.md` for
//! the language and the architecture decision.

pub mod ast;
pub mod diagnostic;
pub mod facade;
pub mod graph;
#[cfg(test)]
mod graph_tests;
pub mod interpret;
pub mod ir;
pub mod lexer;
pub mod lower;
pub mod lower_expr;
pub mod parser;
pub mod plan;
pub mod value;

pub use ast::{Program, Span};
pub use diagnostic::Diagnostic;
pub use interpret::{run_script, ScriptOptions};
pub use ir::{Expr, ExprNode, Instr, Site, SiteKind, Slot};
pub use lexer::{lex, LexError, Token, TokenKind};
pub use parser::{parse, ParseError};
pub use plan::{Plan, SiteSummary};
