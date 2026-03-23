//! # nexusdb-sql — SQL expression tree, evaluator, parser, and executor
//!
//! - 4.17: [`Expr`], [`eval`], [`is_truthy`] — expression evaluator with full NULL semantics
//! - 4.1–4.4: SQL parser (coming)
//! - 4.18: Semantic analyzer (coming)
//! - 4.5: Executor (coming)

pub mod eval;
pub mod expr;

pub use eval::{eval, is_truthy};
pub use expr::{BinaryOp, Expr, UnaryOp};
