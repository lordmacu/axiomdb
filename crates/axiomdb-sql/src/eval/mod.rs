//! Expression evaluator — evaluates [`Expr`] trees against a row of [`Value`]s.
//!
//! ## NULL semantics (3-valued logic)
//!
//! SQL uses three truth values: TRUE, FALSE, and UNKNOWN. UNKNOWN is
//! represented here as [`Value::Null`]. The evaluator propagates NULL
//! according to the full SQL 3-valued logic specification:
//!
//! - Arithmetic with NULL → NULL
//! - Comparison with NULL → NULL (`NULL = NULL` is NULL, not TRUE)
//! - `IS NULL` is immune: always returns TRUE or FALSE
//! - `AND`: FALSE short-circuits (FALSE AND NULL = FALSE)
//! - `OR`: TRUE short-circuits (TRUE OR NULL = TRUE)
//! - `NOT NULL = NULL`
//! - `IN`: TRUE if match found; NULL if no match but NULL in list; FALSE otherwise
//!
//! Use [`is_truthy`] to convert a result to a Rust `bool` for row filtering.

pub mod batch;
mod context;
mod core;
mod functions;
mod ops;
pub mod simd;

#[cfg(test)]
mod tests;

pub(crate) use context::current_eval_collation;
pub use context::{ClosureRunner, CollationGuard, NoSubquery, SubqueryRunner};
pub use core::{eval, eval_in_session, eval_with, eval_with_in_session};
pub use ops::{is_truthy, like_match};
