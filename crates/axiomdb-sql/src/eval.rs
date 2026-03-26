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

use std::cell::Cell;

use axiomdb_core::error::DbError;
use axiomdb_types::{
    coerce::{coerce, coerce_for_op, CoercionMode},
    Value,
};

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

use crate::{
    ast::SelectStmt,
    expr::{BinaryOp, Expr, UnaryOp},
    result::QueryResult,
    session::SessionCollation,
    text_semantics::{compare_text, like_match_collated},
};

// ── Session-collation thread-local ────────────────────────────────────────────

/// Active text-comparison collation for the current `eval` call stack.
///
/// Set to [`SessionCollation::Binary`] by default; overridden to `Es` for the
/// duration of a ctx-path execution via [`CollationGuard`].
///
/// Thread-local guarantees correct isolation between concurrent sessions
/// even though `eval` / `eval_with` are non-async functions called from a
/// Tokio spawn_blocking context.
thread_local! {
    static EVAL_COLLATION: Cell<SessionCollation> = Cell::new(SessionCollation::Binary);
}

/// Returns the active session collation for the current thread.
///
/// Called by `compare_values` and the LIKE handler to dispatch between
/// binary and folded text semantics.
pub(crate) fn current_eval_collation() -> SessionCollation {
    EVAL_COLLATION.with(|c| c.get())
}

/// RAII guard that sets the thread-local session collation on construction
/// and restores the previous value on drop.
///
/// ```rust,ignore
/// let _guard = CollationGuard::new(ctx.effective_collation());
/// // All eval() / eval_with() calls here use the session collation.
/// ```
pub struct CollationGuard {
    prev: SessionCollation,
}

impl CollationGuard {
    /// Sets the active collation for the current thread.
    pub fn new(coll: SessionCollation) -> Self {
        let prev = EVAL_COLLATION.with(|c| c.get());
        EVAL_COLLATION.with(|c| c.set(coll));
        Self { prev }
    }
}

impl Drop for CollationGuard {
    fn drop(&mut self) {
        EVAL_COLLATION.with(|c| c.set(self.prev));
    }
}

// ── SubqueryRunner ────────────────────────────────────────────────────────────

/// Provides subquery execution to [`eval_with`].
///
/// Implement this trait for a type that can execute a [`SelectStmt`] and return
/// its result. Use [`NoSubquery`] where subqueries are impossible; the compiler
/// monomorphizes `eval_with::<NoSubquery>` and eliminates all subquery branches
/// at zero runtime cost.
pub trait SubqueryRunner {
    fn run(&mut self, stmt: &SelectStmt) -> Result<QueryResult, DbError>;
}

/// Zero-cost runner for expression contexts that cannot contain subqueries.
///
/// `eval(expr, row)` uses this, so subquery `Expr` variants encountered by the
/// pure evaluator return `NotImplemented` instead of panicking.
pub struct NoSubquery;

impl SubqueryRunner for NoSubquery {
    fn run(&mut self, _: &SelectStmt) -> Result<QueryResult, DbError> {
        Err(DbError::NotImplemented {
            feature: "subquery in expression context — use eval_with instead of eval".into(),
        })
    }
}

/// Wrapper so a `FnMut` closure can be used as a [`SubqueryRunner`].
pub struct ClosureRunner<F>(pub F)
where
    F: FnMut(&SelectStmt) -> Result<QueryResult, DbError>;

impl<F> SubqueryRunner for ClosureRunner<F>
where
    F: FnMut(&SelectStmt) -> Result<QueryResult, DbError>,
{
    fn run(&mut self, stmt: &SelectStmt) -> Result<QueryResult, DbError> {
        (self.0)(stmt)
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Evaluates `expr` against `row` and returns the resulting [`Value`].
///
/// `row[col_idx]` must be pre-populated by the executor for each tuple.
/// Column references must have been resolved to indices by the semantic
/// analyzer (Phase 4.18) before calling this function.
///
/// ## Errors
/// - [`DbError::DivisionByZero`] — integer or decimal division / modulo by zero.
/// - [`DbError::Overflow`] — integer arithmetic overflow.
/// - [`DbError::TypeMismatch`] — incompatible operand types.
/// - [`DbError::ColumnIndexOutOfBounds`] — `col_idx >= row.len()`.
/// - [`DbError::NotImplemented`] — function call (Phase 4.19).
pub fn eval(expr: &Expr, row: &[Value]) -> Result<Value, DbError> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),

        Expr::Column { col_idx, name: _ } => {
            row.get(*col_idx)
                .cloned()
                .ok_or(DbError::ColumnIndexOutOfBounds {
                    idx: *col_idx,
                    len: row.len(),
                })
        }

        Expr::UnaryOp { op, operand } => {
            let v = eval(operand, row)?;
            eval_unary(*op, v)
        }

        // AND and OR short-circuit BEFORE evaluating the right operand.
        Expr::BinaryOp {
            op: BinaryOp::And,
            left,
            right,
        } => eval_and(left, right, row),

        Expr::BinaryOp {
            op: BinaryOp::Or,
            left,
            right,
        } => eval_or(left, right, row),

        Expr::BinaryOp { op, left, right } => {
            let l = eval(left, row)?;
            let r = eval(right, row)?;
            eval_binary(*op, l, r)
        }

        Expr::IsNull { expr, negated } => {
            let v = eval(expr, row)?;
            let is_null = matches!(v, Value::Null);
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }

        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = eval(expr, row)?;
            let lo = eval(low, row)?;
            let hi = eval(high, row)?;
            // BETWEEN low AND high  ≡  v >= low AND v <= high
            let ge = eval_binary(BinaryOp::GtEq, v.clone(), lo)?;
            let le = eval_binary(BinaryOp::LtEq, v, hi)?;
            let result = apply_and_values(ge, le);
            Ok(if *negated { apply_not(result) } else { result })
        }

        Expr::Like {
            expr,
            pattern,
            negated,
        } => {
            let v = eval(expr, row)?;
            let p = eval(pattern, row)?;
            match (v, p) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(text), Value::Text(pat)) => {
                    let matched = like_match_collated(current_eval_collation(), &text, &pat);
                    Ok(Value::Bool(if *negated { !matched } else { matched }))
                }
                (v, p) => Err(DbError::TypeMismatch {
                    expected: "Text LIKE Text".into(),
                    got: format!("{} LIKE {}", v.variant_name(), p.variant_name()),
                }),
            }
        }

        Expr::In {
            expr,
            list,
            negated,
        } => {
            let v = eval(expr, row)?;
            let result = eval_in(v, list, row)?;
            Ok(if *negated { apply_not(result) } else { result })
        }

        Expr::Function { name, args } => eval_function(name, args, row),

        // ── CAST ──────────────────────────────────────────────────────────────
        Expr::Cast { expr, target } => {
            let v = eval(expr, row)?;
            coerce(v, *target, CoercionMode::Strict)
        }

        // ── CASE WHEN ─────────────────────────────────────────────────────────
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            match operand {
                // ── Searched CASE: conditions are boolean expressions ──────────
                None => {
                    for (when_expr, then_expr) in when_thens {
                        let condition = eval(when_expr, row)?;
                        if is_truthy(&condition) {
                            return eval(then_expr, row);
                        }
                    }
                }

                // ── Simple CASE: compare base value against WHEN values ────────
                Some(base_expr) => {
                    let base_val = eval(base_expr, row)?;
                    for (val_expr, then_expr) in when_thens {
                        let val = eval(val_expr, row)?;
                        // Use eval() for NULL-safe equality and type coercion.
                        // NULL base or NULL val → UNKNOWN → is_truthy = false → no match.
                        let eq = eval(
                            &Expr::BinaryOp {
                                op: crate::expr::BinaryOp::Eq,
                                left: Box::new(Expr::Literal(base_val.clone())),
                                right: Box::new(Expr::Literal(val)),
                            },
                            &[],
                        )?;
                        if is_truthy(&eq) {
                            return eval(then_expr, row);
                        }
                    }
                }
            }

            // No WHEN branch matched — return ELSE or NULL.
            match else_result {
                Some(else_expr) => eval(else_expr, row),
                None => Ok(Value::Null),
            }
        }

        // Subquery variants — delegate to SubqueryRunner (NoSubquery returns NotImplemented).
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => {
            eval_with(expr, row, &mut NoSubquery)
        }

        // OuterColumn must be substituted by the executor before eval() is called.
        Expr::OuterColumn { name, col_idx } => Err(DbError::Internal {
            message: format!(
                "unsubstituted OuterColumn '{name}' (col_idx={col_idx}) — \
                 substitute_outer must be called before executing the inner query"
            ),
        }),

        // Param must be substituted before eval — programming error if reached.
        Expr::Param { idx } => Err(DbError::Internal {
            message: format!(
                "unsubstituted Param ?{idx} — substitute_params_in_ast must be \
                 called before executing a prepared statement"
            ),
        }),

        // GroupConcat is only valid as an aggregate — never reached by scalar eval.
        Expr::GroupConcat { .. } => Err(DbError::InvalidValue {
            reason: "GROUP_CONCAT can only be used as an aggregate function".into(),
        }),
    }
}

/// Evaluates `expr` against `row` using the given session collation for text
/// comparisons.
///
/// This is the primary entry point for ctx-based execution paths. All text
/// comparisons (`=`, `!=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IN`, `LIKE`) use
/// `collation` instead of binary byte order.
///
/// The collation is propagated via a thread-local [`CollationGuard`] so that
/// the entire recursive expression tree — including nested `eval` calls — sees
/// the same semantics.
pub fn eval_in_session(
    expr: &Expr,
    row: &[Value],
    collation: SessionCollation,
) -> Result<Value, DbError> {
    let _guard = CollationGuard::new(collation);
    eval(expr, row)
}

/// Evaluates `expr` against `row` using `sq` for subqueries, with the given
/// session collation for text comparisons.
pub fn eval_with_in_session<R: SubqueryRunner>(
    expr: &Expr,
    row: &[Value],
    sq: &mut R,
    collation: SessionCollation,
) -> Result<Value, DbError> {
    let _guard = CollationGuard::new(collation);
    eval_with(expr, row, sq)
}

// ── eval_with — subquery-aware evaluator ──────────────────────────────────────

/// Evaluates `expr` against `row` using `sq` to execute any subquery nodes.
///
/// This is the primary evaluator for expressions that may contain subqueries.
/// All compound nodes (`AND`, `OR`, `CASE`, etc.) recurse through `eval_with`
/// so that subqueries nested at any depth are correctly dispatched to `sq`.
///
/// ## SubqueryRunner
///
/// The `sq` parameter is called for each subquery node. The executor builds
/// a [`ClosureRunner`] that captures `storage`, `txn`, and `SessionContext`,
/// performing outer-row substitution before executing the inner query.
///
/// Use `eval(expr, row)` (which calls `eval_with(expr, row, &mut NoSubquery)`)
/// for expression contexts that are provably subquery-free.
pub fn eval_with<R: SubqueryRunner>(
    expr: &Expr,
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),

        Expr::Column { col_idx, name: _ } => {
            row.get(*col_idx)
                .cloned()
                .ok_or(DbError::ColumnIndexOutOfBounds {
                    idx: *col_idx,
                    len: row.len(),
                })
        }

        Expr::OuterColumn { col_idx, name } => Err(DbError::Internal {
            message: format!(
                "unsubstituted OuterColumn '{name}' (col_idx={col_idx}) — \
                 substitute_outer must be called before executing the inner query"
            ),
        }),

        Expr::Param { idx } => Err(DbError::Internal {
            message: format!(
                "unsubstituted Param ?{idx} — substitute_params_in_ast must be \
                 called before executing a prepared statement"
            ),
        }),

        Expr::UnaryOp { op, operand } => {
            let v = eval_with(operand, row, sq)?;
            eval_unary(*op, v)
        }

        Expr::BinaryOp {
            op: BinaryOp::And,
            left,
            right,
        } => eval_and_with(left, right, row, sq),

        Expr::BinaryOp {
            op: BinaryOp::Or,
            left,
            right,
        } => eval_or_with(left, right, row, sq),

        Expr::BinaryOp { op, left, right } => {
            let l = eval_with(left, row, sq)?;
            let r = eval_with(right, row, sq)?;
            eval_binary(*op, l, r)
        }

        Expr::IsNull { expr, negated } => {
            let v = eval_with(expr, row, sq)?;
            let is_null = matches!(v, Value::Null);
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }

        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = eval_with(expr, row, sq)?;
            let lo = eval_with(low, row, sq)?;
            let hi = eval_with(high, row, sq)?;
            let ge = eval_binary(BinaryOp::GtEq, v.clone(), lo)?;
            let le = eval_binary(BinaryOp::LtEq, v, hi)?;
            let result = apply_and_values(ge, le);
            Ok(if *negated { apply_not(result) } else { result })
        }

        Expr::Like {
            expr,
            pattern,
            negated,
        } => {
            let v = eval_with(expr, row, sq)?;
            let p = eval_with(pattern, row, sq)?;
            match (v, p) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(text), Value::Text(pat)) => {
                    let matched = like_match_collated(current_eval_collation(), &text, &pat);
                    Ok(Value::Bool(if *negated { !matched } else { matched }))
                }
                (v, p) => Err(DbError::TypeMismatch {
                    expected: "Text LIKE Text".into(),
                    got: format!("{} LIKE {}", v.variant_name(), p.variant_name()),
                }),
            }
        }

        Expr::In {
            expr,
            list,
            negated,
        } => {
            let v = eval_with(expr, row, sq)?;
            let result = eval_in_with(v, list, row, sq)?;
            Ok(if *negated { apply_not(result) } else { result })
        }

        Expr::Function { name, args } => eval_function(name, args, row),

        Expr::Cast { expr, target } => {
            let v = eval_with(expr, row, sq)?;
            coerce(v, *target, CoercionMode::Strict)
        }

        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            match operand {
                None => {
                    for (when_expr, then_expr) in when_thens {
                        let condition = eval_with(when_expr, row, sq)?;
                        if is_truthy(&condition) {
                            return eval_with(then_expr, row, sq);
                        }
                    }
                }
                Some(base_expr) => {
                    let base_val = eval_with(base_expr, row, sq)?;
                    for (val_expr, then_expr) in when_thens {
                        let val = eval_with(val_expr, row, sq)?;
                        let eq = eval_binary(BinaryOp::Eq, base_val.clone(), val)?;
                        if is_truthy(&eq) {
                            return eval_with(then_expr, row, sq);
                        }
                    }
                }
            }
            match else_result {
                Some(else_expr) => eval_with(else_expr, row, sq),
                None => Ok(Value::Null),
            }
        }

        // ── Subquery variants ──────────────────────────────────────────────────
        Expr::Subquery(stmt) => {
            let result = sq.run(stmt)?;
            match result {
                QueryResult::Rows { rows, .. } => match rows.len() {
                    0 => Ok(Value::Null),
                    1 => rows
                        .into_iter()
                        .next()
                        .and_then(|r| r.into_iter().next())
                        .ok_or_else(|| DbError::Internal {
                            message: "scalar subquery returned an empty row".into(),
                        }),
                    n => Err(DbError::CardinalityViolation { count: n }),
                },
                _ => Err(DbError::Internal {
                    message: "scalar subquery did not return a Rows result".into(),
                }),
            }
        }

        Expr::InSubquery {
            expr,
            query,
            negated,
        } => {
            let left = eval_with(expr, row, sq)?;
            if matches!(left, Value::Null) {
                return Ok(Value::Null);
            }
            let result = sq.run(query)?;
            let subquery_rows = match result {
                QueryResult::Rows { rows, .. } => rows,
                _ => vec![],
            };
            let mut found = false;
            let mut has_null = false;
            for sq_row in &subquery_rows {
                let v = sq_row.first().cloned().unwrap_or(Value::Null);
                match v {
                    Value::Null => has_null = true,
                    ref iv if *iv == left => {
                        found = true;
                        break;
                    }
                    _ => {}
                }
            }
            let raw = if found {
                Value::Bool(true)
            } else if has_null {
                Value::Null
            } else {
                Value::Bool(false)
            };
            Ok(if *negated {
                match raw {
                    Value::Bool(b) => Value::Bool(!b),
                    other => other, // NULL stays NULL
                }
            } else {
                raw
            })
        }

        Expr::Exists { query, negated } => {
            let result = sq.run(query)?;
            let has_rows = matches!(&result, QueryResult::Rows { rows, .. } if !rows.is_empty());
            Ok(Value::Bool(if *negated { !has_rows } else { has_rows }))
        }

        // GroupConcat is only valid as an aggregate — never reached by scalar eval_with.
        Expr::GroupConcat { .. } => Err(DbError::InvalidValue {
            reason: "GROUP_CONCAT can only be used as an aggregate function".into(),
        }),
    }
}

// ── Short-circuit AND/OR with subquery support ────────────────────────────────

fn eval_and_with<R: SubqueryRunner>(
    left: &Expr,
    right: &Expr,
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    let l = eval_with(left, row, sq)?;
    match l {
        Value::Bool(false) => Ok(Value::Bool(false)),
        Value::Bool(true) => eval_with(right, row, sq),
        Value::Null => {
            let r = eval_with(right, row, sq)?;
            Ok(match r {
                Value::Bool(false) => Value::Bool(false),
                _ => Value::Null,
            })
        }
        other => Err(DbError::TypeMismatch {
            expected: "Bool".into(),
            got: other.variant_name().into(),
        }),
    }
}

fn eval_or_with<R: SubqueryRunner>(
    left: &Expr,
    right: &Expr,
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    let l = eval_with(left, row, sq)?;
    match l {
        Value::Bool(true) => Ok(Value::Bool(true)),
        Value::Bool(false) => eval_with(right, row, sq),
        Value::Null => {
            let r = eval_with(right, row, sq)?;
            Ok(match r {
                Value::Bool(true) => Value::Bool(true),
                _ => Value::Null,
            })
        }
        other => Err(DbError::TypeMismatch {
            expected: "Bool".into(),
            got: other.variant_name().into(),
        }),
    }
}

fn eval_in_with<R: SubqueryRunner>(
    v: Value,
    list: &[Expr],
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    let mut has_null_in_list = false;
    for item_expr in list {
        let item = eval_with(item_expr, row, sq)?;
        match item {
            Value::Null => has_null_in_list = true,
            ref iv => match compare_values(&v, iv) {
                Ok(std::cmp::Ordering::Equal) => return Ok(Value::Bool(true)),
                Ok(_) => {}
                Err(_) => {}
            },
        }
    }
    if has_null_in_list {
        Ok(Value::Null)
    } else {
        Ok(Value::Bool(false))
    }
}

// ── Scalar function evaluator ─────────────────────────────────────────────────

/// Evaluates a scalar function call against `row`.
///
/// Covers system functions (4.13), null-handling, numeric (4.19), string (4.19),
/// date/time (4.19 partial), and type inspection functions.
fn eval_function(name: &str, args: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    match name.to_ascii_lowercase().as_str() {
        // ── System functions (4.13) ─────────────────────────────────────────
        "version" | "axiomdb_version" => Ok(Value::Text("AxiomDB 0.1.0".into())),
        "current_user" | "user" | "session_user" | "system_user" => {
            Ok(Value::Text("axiomdb".into()))
        }
        "current_database" | "database" => Ok(Value::Text("main".into())),
        "current_schema" | "schema" => Ok(Value::Text("public".into())),
        "connection_id" => Ok(Value::BigInt(1)),
        "row_count" => Ok(Value::BigInt(0)),

        // ── LAST_INSERT_ID / lastval (4.14) ──────────────────────────────────
        "last_insert_id" | "lastval" => {
            let id = crate::executor::last_insert_id_value();
            Ok(Value::BigInt(id as i64))
        }

        // ── Null handling ────────────────────────────────────────────────────
        "coalesce" | "ifnull" | "nvl" => {
            for arg in args {
                let v = eval(arg, row)?;
                if !matches!(v, Value::Null) {
                    return Ok(v);
                }
            }
            Ok(Value::Null)
        }
        "nullif" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 arguments".into(),
                    got: format!("{}", args.len()),
                });
            }
            let a = eval(&args[0], row)?;
            let b = eval(&args[1], row)?;
            let eq = eval(
                &Expr::BinaryOp {
                    op: BinaryOp::Eq,
                    left: Box::new(Expr::Literal(a.clone())),
                    right: Box::new(Expr::Literal(b)),
                },
                &[],
            )?;
            Ok(if is_truthy(&eq) { Value::Null } else { a })
        }
        "isnull" => {
            if args.is_empty() {
                return Ok(Value::Bool(true));
            }
            let v = eval(&args[0], row)?;
            Ok(Value::Bool(matches!(v, Value::Null)))
        }
        "if" | "iff" => {
            if args.len() != 3 {
                return Err(DbError::TypeMismatch {
                    expected: "3 arguments for IF(cond, true_val, false_val)".into(),
                    got: format!("{}", args.len()),
                });
            }
            let cond = eval(&args[0], row)?;
            if is_truthy(&cond) {
                eval(&args[1], row)
            } else {
                eval(&args[2], row)
            }
        }

        // ── Numeric functions (4.19) ─────────────────────────────────────────
        "abs" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 argument".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Int(n.abs())),
                Value::BigInt(n) => Ok(Value::BigInt(n.abs())),
                Value::Real(f) => Ok(Value::Real(f.abs())),
                Value::Decimal(m, s) => Ok(Value::Decimal(m.abs(), s)),
                other => Err(DbError::TypeMismatch {
                    expected: "numeric".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "ceil" | "ceiling" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Real(n as f64)),
                Value::BigInt(n) => Ok(Value::Real(n as f64)),
                Value::Real(f) => Ok(Value::Real(f.ceil())),
                other => Err(DbError::TypeMismatch {
                    expected: "numeric".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "floor" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Real(n as f64)),
                Value::BigInt(n) => Ok(Value::Real(n as f64)),
                Value::Real(f) => Ok(Value::Real(f.floor())),
                other => Err(DbError::TypeMismatch {
                    expected: "numeric".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "round" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1+ args".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            let decimals = if args.len() > 1 {
                match eval(&args[1], row)? {
                    Value::Int(d) => d.max(0) as u32,
                    Value::BigInt(d) => d.max(0) as u32,
                    _ => 0,
                }
            } else {
                0
            };
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Int(n)),
                Value::BigInt(n) => Ok(Value::BigInt(n)),
                Value::Real(f) => {
                    let factor = 10f64.powi(decimals as i32);
                    Ok(Value::Real((f * factor).round() / factor))
                }
                other => Err(DbError::TypeMismatch {
                    expected: "numeric".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "pow" | "power" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let base = eval(&args[0], row)?;
            let exp = eval(&args[1], row)?;
            fn to_f64(v: Value) -> Option<f64> {
                match v {
                    Value::Null => None,
                    Value::Int(n) => Some(n as f64),
                    Value::BigInt(n) => Some(n as f64),
                    Value::Real(f) => Some(f),
                    _ => None,
                }
            }
            match (to_f64(base), to_f64(exp)) {
                (None, _) | (_, None) => Ok(Value::Null),
                (Some(b), Some(e)) => Ok(Value::Real(b.powf(e))),
            }
        }
        "sqrt" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Real((n as f64).sqrt())),
                Value::BigInt(n) => Ok(Value::Real((n as f64).sqrt())),
                Value::Real(f) => Ok(Value::Real(f.sqrt())),
                other => Err(DbError::TypeMismatch {
                    expected: "numeric".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "mod" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let a = eval(&args[0], row)?;
            let b = eval(&args[1], row)?;
            eval(
                &Expr::BinaryOp {
                    op: BinaryOp::Mod,
                    left: Box::new(Expr::Literal(a)),
                    right: Box::new(Expr::Literal(b)),
                },
                &[],
            )
        }
        "sign" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Int(n.signum())),
                Value::BigInt(n) => Ok(Value::BigInt(n.signum())),
                Value::Real(f) => Ok(Value::Real(f.signum())),
                other => Err(DbError::TypeMismatch {
                    expected: "numeric".into(),
                    got: other.variant_name().into(),
                }),
            }
        }

        // ── String functions (4.19) ──────────────────────────────────────────
        "length" | "char_length" | "character_length" | "len" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Int(s.chars().count() as i32)),
                Value::Bytes(b) => Ok(Value::Int(b.len() as i32)),
                other => Err(DbError::TypeMismatch {
                    expected: "Text or Bytes".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        // OCTET_LENGTH / BYTE_LENGTH — returns byte count (not character count for TEXT).
        // Handles BLOB, TEXT, and UUID (always 16 bytes). Returns NULL for NULL input.
        // Extended in Phase 4.19b to cover UUID.
        "octet_length" | "byte_length" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Int(s.len() as i32)),
                Value::Bytes(b) => Ok(Value::Int(b.len() as i32)),
                Value::Uuid(_) => Ok(Value::Int(16)),
                other => Err(DbError::TypeMismatch {
                    expected: "Text or Bytes".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "upper" | "ucase" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.to_uppercase())),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "lower" | "lcase" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.to_lowercase())),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "trim" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.trim().to_string())),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "ltrim" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.trim_start().to_string())),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "rtrim" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.trim_end().to_string())),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "substr" | "substring" | "mid" => {
            // SUBSTR(str, start[, length]) — 1-based indexing (SQL standard)
            if args.is_empty() {
                return Err(DbError::TypeMismatch {
                    expected: "2-3 args".into(),
                    got: "0".into(),
                });
            }
            let s = match eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Text".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let start = if args.len() > 1 {
                match eval(&args[1], row)? {
                    Value::Int(n) => n as usize,
                    Value::BigInt(n) => n as usize,
                    Value::Null => return Ok(Value::Null),
                    other => {
                        return Err(DbError::TypeMismatch {
                            expected: "Int".into(),
                            got: other.variant_name().into(),
                        })
                    }
                }
            } else {
                1
            };
            let chars: Vec<char> = s.chars().collect();
            let start_idx = if start == 0 {
                0
            } else {
                (start - 1).min(chars.len())
            };
            let result = if args.len() > 2 {
                match eval(&args[2], row)? {
                    Value::Int(n) => chars[start_idx..]
                        .iter()
                        .take(n.max(0) as usize)
                        .collect::<String>(),
                    Value::BigInt(n) => chars[start_idx..]
                        .iter()
                        .take(n.max(0) as usize)
                        .collect::<String>(),
                    Value::Null => return Ok(Value::Null),
                    other => {
                        return Err(DbError::TypeMismatch {
                            expected: "Int".into(),
                            got: other.variant_name().into(),
                        })
                    }
                }
            } else {
                chars[start_idx..].iter().collect::<String>()
            };
            Ok(Value::Text(result))
        }
        "concat" => {
            let mut result = String::new();
            for arg in args {
                match eval(arg, row)? {
                    Value::Null => {} // SQL CONCAT skips NULLs (MySQL behavior)
                    Value::Text(s) => result.push_str(&s),
                    Value::Int(n) => result.push_str(&n.to_string()),
                    Value::BigInt(n) => result.push_str(&n.to_string()),
                    Value::Real(f) => result.push_str(&f.to_string()),
                    other => result.push_str(&other.to_string()),
                }
            }
            Ok(Value::Text(result))
        }
        "concat_ws" => {
            // CONCAT_WS(separator, val1, val2, ...)
            if args.is_empty() {
                return Ok(Value::Text(String::new()));
            }
            let sep = match eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => other.to_string(),
            };
            let mut parts: Vec<String> = Vec::new();
            for a in &args[1..] {
                match eval(a, row)? {
                    Value::Null => {} // skip NULLs
                    v => parts.push(v.to_string()),
                }
            }
            Ok(Value::Text(parts.join(&sep)))
        }
        "repeat" | "replicate" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s = match eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => other.to_string(),
            };
            let n = match eval(&args[1], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Int(n) => n.max(0) as usize,
                Value::BigInt(n) => n.max(0) as usize,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Int".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            Ok(Value::Text(s.repeat(n)))
        }
        "replace" => {
            if args.len() != 3 {
                return Err(DbError::TypeMismatch {
                    expected: "3 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s = match eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Text".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let from = match eval(&args[1], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => other.to_string(),
            };
            let to = match eval(&args[2], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => other.to_string(),
            };
            Ok(Value::Text(s.replace(&from, &to)))
        }
        "reverse" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Text(s.chars().rev().collect())),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "left" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s = match eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Text".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let n = match eval(&args[1], row)? {
                Value::Int(n) => n.max(0) as usize,
                Value::BigInt(n) => n.max(0) as usize,
                _ => 0,
            };
            Ok(Value::Text(s.chars().take(n).collect()))
        }
        "right" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s = match eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => {
                    return Err(DbError::TypeMismatch {
                        expected: "Text".into(),
                        got: other.variant_name().into(),
                    })
                }
            };
            let n = match eval(&args[1], row)? {
                Value::Int(n) => n.max(0) as usize,
                Value::BigInt(n) => n.max(0) as usize,
                _ => 0,
            };
            let chars: Vec<char> = s.chars().collect();
            let start = chars.len().saturating_sub(n);
            Ok(Value::Text(chars[start..].iter().collect()))
        }
        "lpad" => {
            if args.len() < 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2-3 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s = match eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => other.to_string(),
            };
            let len = match eval(&args[1], row)? {
                Value::Int(n) => n.max(0) as usize,
                Value::BigInt(n) => n.max(0) as usize,
                _ => 0,
            };
            let pad = if args.len() > 2 {
                match eval(&args[2], row)? {
                    Value::Text(s) => s,
                    _ => " ".into(),
                }
            } else {
                " ".into()
            };
            let chars: Vec<char> = s.chars().collect();
            if chars.len() >= len {
                return Ok(Value::Text(chars[..len].iter().collect()));
            }
            let needed = len - chars.len();
            let pad_chars: Vec<char> = pad.chars().cycle().take(needed).collect();
            Ok(Value::Text(pad_chars.iter().chain(chars.iter()).collect()))
        }
        "rpad" => {
            if args.len() < 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2-3 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s = match eval(&args[0], row)? {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                other => other.to_string(),
            };
            let len = match eval(&args[1], row)? {
                Value::Int(n) => n.max(0) as usize,
                Value::BigInt(n) => n.max(0) as usize,
                _ => 0,
            };
            let pad = if args.len() > 2 {
                match eval(&args[2], row)? {
                    Value::Text(s) => s,
                    _ => " ".into(),
                }
            } else {
                " ".into()
            };
            let chars: Vec<char> = s.chars().collect();
            if chars.len() >= len {
                return Ok(Value::Text(chars[..len].iter().collect()));
            }
            let needed = len - chars.len();
            let pad_chars: Vec<char> = pad.chars().cycle().take(needed).collect();
            Ok(Value::Text(chars.iter().chain(pad_chars.iter()).collect()))
        }
        "locate" | "position" => {
            // LOCATE(needle, haystack) / POSITION(needle IN haystack) — same runtime form
            if args.len() < 2 {
                return Ok(Value::Int(0));
            }
            let needle = match eval(&args[0], row)? {
                Value::Text(s) => s,
                _ => return Ok(Value::Int(0)),
            };
            let haystack = match eval(&args[1], row)? {
                Value::Text(s) => s,
                _ => return Ok(Value::Int(0)),
            };
            Ok(match haystack.find(&needle[..]) {
                None => Value::Int(0),
                Some(byte_pos) => Value::Int(haystack[..byte_pos].chars().count() as i32 + 1),
            })
        }
        "instr" => {
            // INSTR(haystack, needle) — argument order reversed vs LOCATE
            if args.len() < 2 {
                return Ok(Value::Int(0));
            }
            let haystack = match eval(&args[0], row)? {
                Value::Text(s) => s,
                _ => return Ok(Value::Int(0)),
            };
            let needle = match eval(&args[1], row)? {
                Value::Text(s) => s,
                _ => return Ok(Value::Int(0)),
            };
            Ok(match haystack.find(&needle[..]) {
                None => Value::Int(0),
                Some(byte_pos) => Value::Int(haystack[..byte_pos].chars().count() as i32 + 1),
            })
        }
        "ascii" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Int(s.chars().next().map(|c| c as i32).unwrap_or(0))),
                other => Err(DbError::TypeMismatch {
                    expected: "Text".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "char" | "chr" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Int(n) => Ok(Value::Text(
                    char::from_u32(n as u32)
                        .map(|c| c.to_string())
                        .unwrap_or_default(),
                )),
                Value::BigInt(n) => Ok(Value::Text(
                    char::from_u32(n as u32)
                        .map(|c| c.to_string())
                        .unwrap_or_default(),
                )),
                other => Err(DbError::TypeMismatch {
                    expected: "Int".into(),
                    got: other.variant_name().into(),
                }),
            }
        }
        "space" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Int(n) => Ok(Value::Text(" ".repeat(n.max(0) as usize))),
                Value::BigInt(n) => Ok(Value::Text(" ".repeat(n.max(0) as usize))),
                _ => Ok(Value::Text(String::new())),
            }
        }
        "strcmp" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let a = match eval(&args[0], row)? {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            let b = match eval(&args[1], row)? {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            Ok(Value::Int(match a.cmp(&b) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }))
        }

        // ── Date/Time functions (4.19) ───────────────────────────────────────
        "now" | "current_timestamp" | "getdate" | "sysdate" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let micros = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as i64;
            Ok(Value::Timestamp(micros))
        }
        "current_date" | "curdate" | "today" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let days = (secs / 86400) as i32;
            Ok(Value::Date(days))
        }
        "unix_timestamp" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            Ok(Value::BigInt(secs as i64))
        }
        "year" | "month" | "day" | "hour" | "minute" | "second" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            let ndt = match v {
                Value::Null => return Ok(Value::Null),
                Value::Timestamp(micros) => micros_to_ndt(micros),
                Value::Date(days) => days_to_ndate(days).and_time(NaiveTime::MIN),
                _ => return Ok(Value::Null),
            };
            let result = match name.to_ascii_lowercase().as_str() {
                "year" => ndt.year(),
                "month" => ndt.month() as i32,
                "day" => ndt.day() as i32,
                "hour" => ndt.hour() as i32,
                "minute" => ndt.minute() as i32,
                "second" => ndt.second() as i32,
                _ => unreachable!(),
            };
            Ok(Value::Int(result))
        }
        "datediff" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let a = eval(&args[0], row)?;
            let b = eval(&args[1], row)?;
            let days_a = match a {
                Value::Date(d) => d as i64,
                Value::Timestamp(t) => t / 86_400_000_000,
                _ => return Ok(Value::Null),
            };
            let days_b = match b {
                Value::Date(d) => d as i64,
                Value::Timestamp(t) => t / 86_400_000_000,
                _ => return Ok(Value::Null),
            };
            Ok(Value::Int((days_a - days_b) as i32))
        }

        // ── Type inspection / conversion ─────────────────────────────────────
        "typeof" | "pg_typeof" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            Ok(Value::Text(v.variant_name().into()))
        }
        "to_char" | "str" | "tostring" => {
            let v = eval(
                args.first().ok_or_else(|| DbError::TypeMismatch {
                    expected: "1 arg".into(),
                    got: "0".into(),
                })?,
                row,
            )?;
            match v {
                Value::Null => Ok(Value::Null),
                other => Ok(Value::Text(other.to_string())),
            }
        }

        // ── UUID functions (4.19c) ───────────────────────────────────────────

        // gen_random_uuid() / uuid_generate_v4() — UUID v4 (random)
        "gen_random_uuid" | "uuid_generate_v4" | "random_uuid" | "newid" => {
            use rand::RngCore;
            let mut bytes = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut bytes);
            // Set version = 4 (bits 12-15 of octet 6)
            bytes[6] = (bytes[6] & 0x0F) | 0x40;
            // Set variant = RFC 4122 (bits 6-7 of octet 8)
            bytes[8] = (bytes[8] & 0x3F) | 0x80;
            Ok(Value::Uuid(bytes))
        }

        // uuid_generate_v7() — UUID v7 (time-ordered, monotonic)
        // Format: [48-bit unix_ms][4-bit ver=7][12-bit rand][2-bit var][62-bit rand]
        // Better B-Tree index locality than v4 because keys are time-ordered.
        "uuid_generate_v7" | "uuid7" => {
            use rand::RngCore;
            use std::time::{SystemTime, UNIX_EPOCH};
            let ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let mut bytes = [0u8; 16];
            rand::thread_rng().fill_bytes(&mut bytes);
            // Embed 48-bit timestamp in the first 6 bytes
            bytes[0] = ((ms >> 40) & 0xFF) as u8;
            bytes[1] = ((ms >> 32) & 0xFF) as u8;
            bytes[2] = ((ms >> 24) & 0xFF) as u8;
            bytes[3] = ((ms >> 16) & 0xFF) as u8;
            bytes[4] = ((ms >> 8) & 0xFF) as u8;
            bytes[5] = (ms & 0xFF) as u8;
            // Set version = 7
            bytes[6] = (bytes[6] & 0x0F) | 0x70;
            // Set variant = RFC 4122
            bytes[8] = (bytes[8] & 0x3F) | 0x80;
            Ok(Value::Uuid(bytes))
        }

        // is_valid_uuid(text) → BOOL — returns true if text is a valid UUID string
        "is_valid_uuid" | "is_uuid" => {
            let arg = args.first().ok_or_else(|| DbError::TypeMismatch {
                expected: "1 arg".into(),
                got: "0".into(),
            })?;
            match eval(arg, row)? {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Bool(parse_uuid_str(&s).is_some())),
                Value::Uuid(_) => Ok(Value::Bool(true)),
                _ => Ok(Value::Bool(false)),
            }
        }

        // ── BLOB / binary functions (4.19b) ──────────────────────────────────

        // FROM_BASE64(text) → BLOB
        // Decodes a standard base64-encoded string to raw bytes.
        // Returns NULL if the input is NULL or contains invalid base64.
        // MySQL-compatible: FROM_BASE64('aGVsbG8=') → x'68656c6c6f'
        "from_base64" => {
            let arg = args.first().ok_or_else(|| DbError::TypeMismatch {
                expected: "1 arg".into(),
                got: "0".into(),
            })?;
            match eval(arg, row)? {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => match b64_decode(s.trim()) {
                    Some(bytes) => Ok(Value::Bytes(bytes)),
                    None => Ok(Value::Null), // invalid base64 → NULL (MySQL behavior)
                },
                _ => Ok(Value::Null),
            }
        }

        // TO_BASE64(blob) → TEXT
        // Encodes raw bytes to a standard base64 string.
        // Also accepts TEXT (encodes the UTF-8 bytes) and UUID (encodes 16 bytes).
        // MySQL-compatible: TO_BASE64('hello') → 'aGVsbG8='
        "to_base64" => {
            let arg = args.first().ok_or_else(|| DbError::TypeMismatch {
                expected: "1 arg".into(),
                got: "0".into(),
            })?;
            match eval(arg, row)? {
                Value::Null => Ok(Value::Null),
                Value::Bytes(b) => Ok(Value::Text(b64_encode(&b))),
                Value::Text(s) => Ok(Value::Text(b64_encode(s.as_bytes()))),
                Value::Uuid(b) => Ok(Value::Text(b64_encode(&b))),
                _ => Ok(Value::Null),
            }
        }

        // ENCODE(blob, format) → TEXT
        // Encodes binary data to a text representation.
        // format: 'base64' or 'hex'
        // PostgreSQL-compatible: ENCODE(E'\\x68656c6c6f', 'hex') → '68656c6c6f'
        "encode" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let data = eval(&args[0], row)?;
            let fmt = eval(&args[1], row)?;
            let bytes = match data {
                Value::Null => return Ok(Value::Null),
                Value::Bytes(b) => b,
                Value::Text(s) => s.into_bytes(),
                Value::Uuid(b) => b.to_vec(),
                _ => return Ok(Value::Null),
            };
            let fmt_str = match fmt {
                Value::Text(s) => s.to_ascii_lowercase(),
                _ => {
                    return Err(DbError::TypeMismatch {
                        expected: "format string 'base64' or 'hex'".into(),
                        got: "non-text".into(),
                    })
                }
            };
            match fmt_str.as_str() {
                "base64" => Ok(Value::Text(b64_encode(&bytes))),
                "hex" => Ok(Value::Text(hex_encode(&bytes))),
                other => Err(DbError::NotImplemented {
                    feature: format!("ENCODE format '{other}' — supported: 'base64', 'hex'"),
                }),
            }
        }

        // DECODE(text, format) → BLOB
        // Decodes a text representation to binary data.
        // format: 'base64' or 'hex'
        // Returns NULL on invalid input (MySQL behavior for base64; error for invalid hex).
        "decode" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let data = eval(&args[0], row)?;
            let fmt = eval(&args[1], row)?;
            let text = match data {
                Value::Null => return Ok(Value::Null),
                Value::Text(s) => s,
                _ => {
                    return Err(DbError::TypeMismatch {
                        expected: "text".into(),
                        got: "non-text".into(),
                    })
                }
            };
            let fmt_str = match fmt {
                Value::Text(s) => s.to_ascii_lowercase(),
                _ => {
                    return Err(DbError::TypeMismatch {
                        expected: "format string 'base64' or 'hex'".into(),
                        got: "non-text".into(),
                    })
                }
            };
            match fmt_str.as_str() {
                "base64" => match b64_decode(text.trim()) {
                    Some(b) => Ok(Value::Bytes(b)),
                    None => Ok(Value::Null),
                },
                "hex" => match hex_decode(&text) {
                    Some(b) => Ok(Value::Bytes(b)),
                    None => Err(DbError::InvalidValue {
                        reason: format!("invalid hex string: '{text}'"),
                    }),
                },
                other => Err(DbError::NotImplemented {
                    feature: format!("DECODE format '{other}' — supported: 'base64', 'hex'"),
                }),
            }
        }

        // ── DATE_FORMAT(ts, fmt) ──────────────────────────────────────────────
        //
        // DATE_FORMAT(ts, fmt_str) → TEXT
        // Formats a date/timestamp value using MySQL-style format specifiers.
        // Returns NULL if ts or fmt is NULL or fmt is empty.
        "date_format" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let ts_val = eval(&args[0], row)?;
            let fmt_val = eval(&args[1], row)?;
            if matches!(ts_val, Value::Null) || matches!(fmt_val, Value::Null) {
                return Ok(Value::Null);
            }
            let fmt_str = match fmt_val {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            if fmt_str.is_empty() {
                return Ok(Value::Null);
            }
            let ndt = match ts_val {
                Value::Timestamp(micros) => micros_to_ndt(micros),
                Value::Date(days) => days_to_ndate(days).and_time(NaiveTime::MIN),
                Value::Text(ref s) => {
                    if let Some((ndt, _)) = str_to_date_inner(s, "%Y-%m-%d %H:%i:%s") {
                        ndt
                    } else if let Some((ndt, _)) = str_to_date_inner(s, "%Y-%m-%d") {
                        ndt
                    } else {
                        return Ok(Value::Null);
                    }
                }
                _ => return Ok(Value::Null),
            };
            Ok(Value::Text(date_format_str(ndt, &fmt_str)))
        }

        // ── STR_TO_DATE(str, fmt) ─────────────────────────────────────────────
        //
        // STR_TO_DATE(str, fmt) → Date | Timestamp | NULL
        // Parses a string using MySQL-style format specifiers.
        // Returns NULL on parse failure (never raises an error — MySQL behavior).
        // Returns Timestamp if the format contains time components (%H/%i/%s),
        // otherwise returns Date.
        "str_to_date" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let s_val = eval(&args[0], row)?;
            let fmt_val = eval(&args[1], row)?;
            if matches!(s_val, Value::Null) || matches!(fmt_val, Value::Null) {
                return Ok(Value::Null);
            }
            let s = match s_val {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            let fmt_str = match fmt_val {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            match str_to_date_inner(&s, &fmt_str) {
                None => Ok(Value::Null),
                Some((ndt, has_time)) => {
                    // SAFETY: 1970-01-01 00:00:00 is always valid.
                    let epoch_ndt = NaiveDate::from_ymd_opt(1970, 1, 1)
                        .unwrap()
                        .and_hms_opt(0, 0, 0)
                        .unwrap();
                    if has_time {
                        let micros = (ndt - epoch_ndt).num_microseconds().unwrap_or(0);
                        Ok(Value::Timestamp(micros))
                    } else {
                        let epoch_date = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                        let days = (ndt.date() - epoch_date).num_days() as i32;
                        Ok(Value::Date(days))
                    }
                }
            }
        }

        // ── FIND_IN_SET(needle, csv_list) ─────────────────────────────────────
        //
        // FIND_IN_SET(needle, list) → INT
        // Returns the 1-indexed position of needle in the comma-separated list,
        // or 0 if not found. Comparison is case-insensitive (MySQL default).
        // Returns NULL if either argument is NULL.
        "find_in_set" => {
            if args.len() != 2 {
                return Err(DbError::TypeMismatch {
                    expected: "2 args".into(),
                    got: format!("{}", args.len()),
                });
            }
            let needle_val = eval(&args[0], row)?;
            let list_val = eval(&args[1], row)?;
            if matches!(needle_val, Value::Null) || matches!(list_val, Value::Null) {
                return Ok(Value::Null);
            }
            let needle = match needle_val {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            let list = match list_val {
                Value::Text(s) => s,
                _ => return Ok(Value::Null),
            };
            Ok(Value::Int(find_in_set_inner(&needle, &list)))
        }

        // ── Unimplemented ────────────────────────────────────────────────────
        _ => Err(DbError::NotImplemented {
            feature: format!("function '{name}' — add to Phase 4.19 eval.rs"),
        }),
    }
}

// ── Date / time helpers (4.19d) ───────────────────────────────────────────────

/// Converts `Value::Timestamp(micros)` to a `NaiveDateTime` (UTC).
///
/// Uses pure NaiveDateTime arithmetic (no timezone conversion needed) so that
/// results are stable across all chrono 0.4.x versions.
fn micros_to_ndt(micros: i64) -> NaiveDateTime {
    // SAFETY: 1970-01-01 00:00:00 is always a valid NaiveDateTime.
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let secs = micros.div_euclid(1_000_000);
    let nanos = micros.rem_euclid(1_000_000) * 1_000;
    epoch
        .checked_add_signed(chrono::Duration::seconds(secs))
        .and_then(|dt| dt.checked_add_signed(chrono::Duration::nanoseconds(nanos)))
        .unwrap_or(epoch)
}

/// Converts `Value::Date(days)` (days since 1970-01-01) to `NaiveDate`.
fn days_to_ndate(days: i32) -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1)
        .unwrap()
        .checked_add_signed(chrono::Duration::days(days as i64))
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
}

/// Formats `ndt` according to MySQL-compatible format specifiers in `fmt`.
///
/// Unknown specifiers are passed through literally (`%X` → `"%X"`), matching
/// MySQL behavior. English-only month/weekday names (out-of-scope: locale).
fn date_format_str(ndt: NaiveDateTime, fmt: &str) -> String {
    const MONTH_NAMES: &[&str] = &[
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    const MONTH_ABBR: &[&str] = &[
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const WEEKDAY_NAMES: &[&str] = &[
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const WEEKDAY_ABBR: &[&str] = &["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

    let mut out = String::with_capacity(fmt.len() + 8);
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            None => out.push('%'),
            Some('Y') => out.push_str(&format!("{:04}", ndt.year())),
            Some('y') => out.push_str(&format!("{:02}", ndt.year().abs() % 100)),
            Some('m') => out.push_str(&format!("{:02}", ndt.month())),
            Some('c') => out.push_str(&format!("{}", ndt.month())),
            Some('M') => out.push_str(MONTH_NAMES[(ndt.month() - 1) as usize]),
            Some('b') => out.push_str(MONTH_ABBR[(ndt.month() - 1) as usize]),
            Some('d') => out.push_str(&format!("{:02}", ndt.day())),
            Some('e') => out.push_str(&format!("{}", ndt.day())),
            Some('H') => out.push_str(&format!("{:02}", ndt.hour())),
            Some('h') => {
                let h = ndt.hour() % 12;
                out.push_str(&format!("{:02}", if h == 0 { 12 } else { h }));
            }
            Some('i') => out.push_str(&format!("{:02}", ndt.minute())),
            Some('s') | Some('S') => out.push_str(&format!("{:02}", ndt.second())),
            Some('p') => out.push_str(if ndt.hour() < 12 { "AM" } else { "PM" }),
            Some('W') => {
                let wd = ndt.weekday().num_days_from_sunday() as usize;
                out.push_str(WEEKDAY_NAMES[wd]);
            }
            Some('a') => {
                let wd = ndt.weekday().num_days_from_sunday() as usize;
                out.push_str(WEEKDAY_ABBR[wd]);
            }
            Some('j') => out.push_str(&format!("{:03}", ndt.ordinal())),
            Some('w') => out.push_str(&format!("{}", ndt.weekday().num_days_from_sunday())),
            Some('T') => out.push_str(&format!(
                "{:02}:{:02}:{:02}",
                ndt.hour(),
                ndt.minute(),
                ndt.second()
            )),
            Some('r') => {
                let h = ndt.hour() % 12;
                let h = if h == 0 { 12 } else { h };
                let am_pm = if ndt.hour() < 12 { "AM" } else { "PM" };
                out.push_str(&format!(
                    "{:02}:{:02}:{:02} {am_pm}",
                    h,
                    ndt.minute(),
                    ndt.second()
                ));
            }
            Some('%') => out.push('%'),
            Some(x) => {
                out.push('%');
                out.push(x);
            }
        }
    }
    out
}

/// Parses string `s` according to MySQL-compatible format `fmt`.
///
/// Returns `Some((NaiveDateTime, has_time))` on success.
/// - `has_time = true` → format contained `%H`, `%h`, `%i`, or `%s`/`%S`
///   (caller should return `Value::Timestamp`)
/// - `has_time = false` → date-only format (caller should return `Value::Date`)
///
/// Returns `None` on any parse failure, matching MySQL's NULL-on-bad-input
/// behavior for STR_TO_DATE.
fn str_to_date_inner(s: &str, fmt: &str) -> Option<(NaiveDateTime, bool)> {
    let mut has_date = false;
    let mut has_time = false;
    let mut year: i32 = 1970;
    let mut month: u32 = 1;
    let mut day: u32 = 1;
    let mut hour: u32 = 0;
    let mut minute: u32 = 0;
    let mut second: u32 = 0;

    let mut rem = s;
    let mut fmt_iter = fmt.chars().peekable();

    while let Some(fc) = fmt_iter.next() {
        if fc != '%' {
            // Literal character must match the corresponding char in rem.
            let mut rem_chars = rem.chars();
            match rem_chars.next() {
                Some(sc) if sc == fc => rem = rem_chars.as_str(),
                _ => return None,
            }
            continue;
        }
        let spec = fmt_iter.next()?;
        match spec {
            'Y' => {
                let (val, rest) = take_digits(rem, 4)?;
                year = val as i32;
                rem = rest;
                has_date = true;
            }
            'y' => {
                let (val, rest) = take_digits(rem, 2)?;
                year = if val < 70 {
                    2000 + val as i32
                } else {
                    1900 + val as i32
                };
                rem = rest;
                has_date = true;
            }
            'm' | 'c' => {
                let (val, rest) = take_digits(rem, 2)?;
                month = val;
                rem = rest;
                has_date = true;
            }
            'd' | 'e' => {
                let (val, rest) = take_digits(rem, 2)?;
                day = val;
                rem = rest;
                has_date = true;
            }
            'H' | 'h' => {
                let (val, rest) = take_digits(rem, 2)?;
                hour = val;
                rem = rest;
                has_time = true;
            }
            'i' => {
                let (val, rest) = take_digits(rem, 2)?;
                minute = val;
                rem = rest;
                has_time = true;
            }
            's' | 'S' => {
                let (val, rest) = take_digits(rem, 2)?;
                second = val;
                rem = rest;
                has_time = true;
            }
            _ => {
                // Unknown specifier: skip one character in rem.
                let mut rem_chars = rem.chars();
                rem_chars.next();
                rem = rem_chars.as_str();
            }
        }
    }

    // Validate component ranges.
    if month == 0 || month > 12 {
        return None;
    }
    if day == 0 || day > 31 {
        return None;
    }
    if hour > 23 {
        return None;
    }
    if minute > 59 {
        return None;
    }
    if second > 59 {
        return None;
    }

    // chrono validates day-in-month (e.g. Feb 30 → None).
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let time = NaiveTime::from_hms_opt(hour, minute, second)?;
    let _ = has_date; // used above; suppress lint
    Some((NaiveDateTime::new(date, time), has_time))
}

/// Take up to `max` ASCII decimal digits from the start of `s`.
/// Returns `(value, remainder)` or `None` if no digit is found.
fn take_digits(s: &str, max: usize) -> Option<(u32, &str)> {
    let n = s
        .bytes()
        .take(max)
        .take_while(|b| b.is_ascii_digit())
        .count();
    if n == 0 {
        return None;
    }
    let val: u32 = s[..n].parse().ok()?;
    Some((val, &s[n..]))
}

/// Returns the 1-indexed position of `needle` in the comma-separated `list`,
/// or 0 if not found. Comparison is case-insensitive (ASCII).
fn find_in_set_inner(needle: &str, list: &str) -> i32 {
    if list.is_empty() {
        return 0;
    }
    for (i, item) in list.split(',').enumerate() {
        if item.eq_ignore_ascii_case(needle) {
            return (i + 1) as i32;
        }
    }
    0
}

// ── BLOB helpers ──────────────────────────────────────────────────────────────

/// Standard Base64 alphabet (RFC 4648).
const B64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `bytes` to standard base64 (with `=` padding).
fn b64_encode(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_CHARS[((n >> 18) & 0x3F) as usize]);
        out.push(B64_CHARS[((n >> 12) & 0x3F) as usize]);
        out.push(if chunk.len() > 1 {
            B64_CHARS[((n >> 6) & 0x3F) as usize]
        } else {
            b'='
        });
        out.push(if chunk.len() > 2 {
            B64_CHARS[(n & 0x3F) as usize]
        } else {
            b'='
        });
    }
    // SAFETY: output contains only ASCII base64 characters + '='.
    unsafe { String::from_utf8_unchecked(out) }
}

/// Decodes a base64 string. Returns `None` on invalid input.
///
/// Accepts standard base64 with or without `=` padding and ignores embedded
/// whitespace (newlines inserted by MySQL's TO_BASE64 for long strings).
fn b64_decode(input: &str) -> Option<Vec<u8>> {
    // Build reverse lookup table: ASCII → 6-bit value (0xFF = invalid).
    let mut rev = [0xFFu8; 256];
    for (i, &c) in B64_CHARS.iter().enumerate() {
        rev[c as usize] = i as u8;
    }

    let bytes: Vec<u8> = input
        .bytes()
        .filter(|&b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .collect();

    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut i = 0;
    while i + 3 < bytes.len() {
        let c0 = rev[bytes[i] as usize];
        let c1 = rev[bytes[i + 1] as usize];
        let c2 = if bytes[i + 2] == b'=' {
            0u8
        } else {
            rev[bytes[i + 2] as usize]
        };
        let c3 = if bytes[i + 3] == b'=' {
            0u8
        } else {
            rev[bytes[i + 3] as usize]
        };
        if c0 == 0xFF || c1 == 0xFF || c2 == 0xFF || c3 == 0xFF {
            return None;
        }
        let n = ((c0 as u32) << 18) | ((c1 as u32) << 12) | ((c2 as u32) << 6) | (c3 as u32);
        out.push(((n >> 16) & 0xFF) as u8);
        if bytes[i + 2] != b'=' {
            out.push(((n >> 8) & 0xFF) as u8);
        }
        if bytes[i + 3] != b'=' {
            out.push((n & 0xFF) as u8);
        }
        i += 4;
    }
    if i != bytes.len() {
        return None; // input length not a multiple of 4
    }
    Some(out)
}

/// Encodes `bytes` to lowercase hex string (e.g. `"deadbeef"`).
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decodes a lowercase or uppercase hex string. Returns `None` on invalid input.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    // Accept optional "0x" / "\\x" prefix (PostgreSQL bytea hex format).
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .or_else(|| s.strip_prefix("\\x"))
        .unwrap_or(s);
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = char::from(pair[0]).to_digit(16)? as u8;
            let lo = char::from(pair[1]).to_digit(16)? as u8;
            Some((hi << 4) | lo)
        })
        .collect()
}

/// Parses a UUID string in the canonical `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`
/// format and returns the 16 raw bytes, or `None` if the format is invalid.
fn parse_uuid_str(s: &str) -> Option<[u8; 16]> {
    // Accept both hyphenated (36 chars) and compact (32 chars) forms.
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = char::from(chunk[0]).to_digit(16)? as u8;
        let lo = char::from(chunk[1]).to_digit(16)? as u8;
        bytes[i] = (hi << 4) | lo;
    }
    Some(bytes)
}

/// Returns `true` only for `Value::Bool(true)`.
///
/// Used by the executor to filter rows from WHERE predicates:
/// - `NULL` (UNKNOWN) → `false` — row excluded
/// - `Value::Bool(false)` → `false` — row excluded
/// - `Value::Bool(true)` → `true` — row included
/// - Any other value → `false` — type error in predicate; row excluded
pub fn is_truthy(value: &Value) -> bool {
    matches!(value, Value::Bool(true))
}

// ── NULL helpers ──────────────────────────────────────────────────────────────

/// AND truth table applied to already-evaluated values (no row context needed).
/// Used by BETWEEN to combine two comparison results.
fn apply_and_values(l: Value, r: Value) -> Value {
    match (&l, &r) {
        (Value::Bool(false), _) | (_, Value::Bool(false)) => Value::Bool(false),
        (Value::Bool(true), Value::Bool(true)) => Value::Bool(true),
        _ => Value::Null, // NULL AND TRUE = NULL, NULL AND NULL = NULL
    }
}

/// NOT applied to an already-evaluated value.
fn apply_not(v: Value) -> Value {
    match v {
        Value::Bool(b) => Value::Bool(!b),
        Value::Null => Value::Null,
        other => other, // unreachable in well-typed expressions
    }
}

// ── Short-circuit AND / OR ────────────────────────────────────────────────────

fn eval_and(left: &Expr, right: &Expr, row: &[Value]) -> Result<Value, DbError> {
    let l = eval(left, row)?;
    match l {
        // FALSE dominates: short-circuit — do NOT evaluate right.
        Value::Bool(false) => Ok(Value::Bool(false)),
        // TRUE: result is entirely determined by right.
        Value::Bool(true) => eval(right, row),
        // NULL (UNKNOWN): must evaluate right.
        Value::Null => {
            let r = eval(right, row)?;
            Ok(match r {
                // FALSE wins over NULL.
                Value::Bool(false) => Value::Bool(false),
                // TRUE or NULL → UNKNOWN.
                _ => Value::Null,
            })
        }
        // Non-boolean left operand.
        other => Err(DbError::TypeMismatch {
            expected: "Bool".into(),
            got: other.variant_name().into(),
        }),
    }
}

fn eval_or(left: &Expr, right: &Expr, row: &[Value]) -> Result<Value, DbError> {
    let l = eval(left, row)?;
    match l {
        // TRUE dominates: short-circuit — do NOT evaluate right.
        Value::Bool(true) => Ok(Value::Bool(true)),
        // FALSE: result is entirely determined by right.
        Value::Bool(false) => eval(right, row),
        // NULL (UNKNOWN): must evaluate right.
        Value::Null => {
            let r = eval(right, row)?;
            Ok(match r {
                // TRUE wins over NULL.
                Value::Bool(true) => Value::Bool(true),
                // FALSE or NULL → UNKNOWN.
                _ => Value::Null,
            })
        }
        other => Err(DbError::TypeMismatch {
            expected: "Bool".into(),
            got: other.variant_name().into(),
        }),
    }
}

// ── Unary evaluation ──────────────────────────────────────────────────────────

fn eval_unary(op: UnaryOp, v: Value) -> Result<Value, DbError> {
    // NULL propagates through all unary ops.
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    match op {
        UnaryOp::Neg => match v {
            Value::Int(n) => n.checked_neg().map(Value::Int).ok_or(DbError::Overflow),
            Value::BigInt(n) => n.checked_neg().map(Value::BigInt).ok_or(DbError::Overflow),
            Value::Real(f) => Ok(Value::Real(-f)),
            Value::Decimal(m, s) => m
                .checked_neg()
                .map(|neg| Value::Decimal(neg, s))
                .ok_or(DbError::Overflow),
            other => Err(DbError::TypeMismatch {
                expected: "numeric".into(),
                got: other.variant_name().into(),
            }),
        },
        UnaryOp::Not => match v {
            Value::Bool(b) => Ok(Value::Bool(!b)),
            other => Err(DbError::TypeMismatch {
                expected: "Bool".into(),
                got: other.variant_name().into(),
            }),
        },
    }
}

// ── Binary evaluation ─────────────────────────────────────────────────────────

/// Evaluates a binary op on already-evaluated operands (non-AND/OR).
/// NULL propagates: if either operand is NULL, the result is NULL.
fn eval_binary(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    // NULL propagation for all binary ops except IS NULL.
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            eval_arithmetic(op, l, r)
        }

        BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq => eval_comparison(op, l, r),

        BinaryOp::Concat => eval_concat(l, r),

        // AND and OR are handled in `eval` before calling here.
        BinaryOp::And | BinaryOp::Or => unreachable!("AND/OR handled in eval"),
    }
}

// ── Arithmetic ────────────────────────────────────────────────────────────────

fn eval_arithmetic(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    let (l, r) = coerce_for_op(l, r)?;
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => int_arith(op, a, b),
        (Value::BigInt(a), Value::BigInt(b)) => bigint_arith(op, a, b),
        (Value::Real(a), Value::Real(b)) => Ok(Value::Real(real_arith(op, a, b)?)),
        (Value::Decimal(m1, s1), Value::Decimal(m2, s2)) => decimal_arith(op, m1, s1, m2, s2),
        _ => unreachable!("coerce_for_op ensures matching types"),
    }
}

fn int_arith(op: BinaryOp, a: i32, b: i32) -> Result<Value, DbError> {
    let result = match op {
        BinaryOp::Add => a.checked_add(b).ok_or(DbError::Overflow)?,
        BinaryOp::Sub => a.checked_sub(b).ok_or(DbError::Overflow)?,
        BinaryOp::Mul => a.checked_mul(b).ok_or(DbError::Overflow)?,
        BinaryOp::Div => {
            if b == 0 {
                return Err(DbError::DivisionByZero);
            }
            a.checked_div(b).ok_or(DbError::Overflow)? // handles MIN/-1
        }
        BinaryOp::Mod => {
            if b == 0 {
                return Err(DbError::DivisionByZero);
            }
            a.checked_rem(b).ok_or(DbError::Overflow)?
        }
        _ => unreachable!(),
    };
    Ok(Value::Int(result))
}

fn bigint_arith(op: BinaryOp, a: i64, b: i64) -> Result<Value, DbError> {
    let result = match op {
        BinaryOp::Add => a.checked_add(b).ok_or(DbError::Overflow)?,
        BinaryOp::Sub => a.checked_sub(b).ok_or(DbError::Overflow)?,
        BinaryOp::Mul => a.checked_mul(b).ok_or(DbError::Overflow)?,
        BinaryOp::Div => {
            if b == 0 {
                return Err(DbError::DivisionByZero);
            }
            a.checked_div(b).ok_or(DbError::Overflow)?
        }
        BinaryOp::Mod => {
            if b == 0 {
                return Err(DbError::DivisionByZero);
            }
            a.checked_rem(b).ok_or(DbError::Overflow)?
        }
        _ => unreachable!(),
    };
    Ok(Value::BigInt(result))
}

fn real_arith(op: BinaryOp, a: f64, b: f64) -> Result<f64, DbError> {
    Ok(match op {
        BinaryOp::Add => a + b,
        BinaryOp::Sub => a - b,
        BinaryOp::Mul => a * b,
        // IEEE 754: division by zero gives ±Infinity, which is allowed for Real.
        BinaryOp::Div => a / b,
        BinaryOp::Mod => a % b,
        _ => unreachable!(),
    })
}

fn decimal_arith(op: BinaryOp, m1: i128, s1: u8, m2: i128, s2: u8) -> Result<Value, DbError> {
    match op {
        BinaryOp::Add | BinaryOp::Sub => {
            // Align scales: bring both to the higher scale.
            let (a, b, scale) = if s1 >= s2 {
                let factor = 10i128.pow((s1 - s2) as u32);
                (m1, m2.checked_mul(factor).ok_or(DbError::Overflow)?, s1)
            } else {
                let factor = 10i128.pow((s2 - s1) as u32);
                (m1.checked_mul(factor).ok_or(DbError::Overflow)?, m2, s2)
            };
            let result = if op == BinaryOp::Add {
                a.checked_add(b).ok_or(DbError::Overflow)?
            } else {
                a.checked_sub(b).ok_or(DbError::Overflow)?
            };
            Ok(Value::Decimal(result, scale))
        }
        BinaryOp::Mul => {
            let result = m1.checked_mul(m2).ok_or(DbError::Overflow)?;
            let scale = s1.saturating_add(s2);
            Ok(Value::Decimal(result, scale))
        }
        BinaryOp::Div => {
            if m2 == 0 {
                return Err(DbError::DivisionByZero);
            }
            // Integer division of mantissas — scale preserved as s1 (truncation).
            // Full precision division is Phase 4.18b.
            let result = m1.checked_div(m2).ok_or(DbError::Overflow)?;
            Ok(Value::Decimal(result, s1))
        }
        BinaryOp::Mod => {
            if m2 == 0 {
                return Err(DbError::DivisionByZero);
            }
            let result = m1.checked_rem(m2).ok_or(DbError::Overflow)?;
            Ok(Value::Decimal(result, s1))
        }
        _ => unreachable!(),
    }
}

// ── Comparison ────────────────────────────────────────────────────────────────

fn eval_comparison(op: BinaryOp, l: Value, r: Value) -> Result<Value, DbError> {
    let ord = compare_values(&l, &r)?;
    Ok(Value::Bool(match op {
        BinaryOp::Eq => ord == std::cmp::Ordering::Equal,
        BinaryOp::NotEq => ord != std::cmp::Ordering::Equal,
        BinaryOp::Lt => ord == std::cmp::Ordering::Less,
        BinaryOp::LtEq => ord != std::cmp::Ordering::Greater,
        BinaryOp::Gt => ord == std::cmp::Ordering::Greater,
        BinaryOp::GtEq => ord != std::cmp::Ordering::Less,
        _ => unreachable!(),
    }))
}

/// Compares two non-NULL values of compatible types.
fn compare_values(l: &Value, r: &Value) -> Result<std::cmp::Ordering, DbError> {
    // Try numeric widening for mixed types first; fall through on incompatible types.
    let (l, r) = match coerce_for_op(l.clone(), r.clone()) {
        Ok(pair) => pair,
        Err(_) => (l.clone(), r.clone()),
    };

    match (&l, &r) {
        (Value::Bool(a), Value::Bool(b)) => Ok(a.cmp(b)),
        (Value::Int(a), Value::Int(b)) => Ok(a.cmp(b)),
        (Value::BigInt(a), Value::BigInt(b)) => Ok(a.cmp(b)),
        (Value::Real(a), Value::Real(b)) => a.partial_cmp(b).ok_or(DbError::TypeMismatch {
            expected: "comparable Real".into(),
            got: "NaN".into(),
        }),
        (Value::Decimal(m1, s1), Value::Decimal(m2, s2)) => {
            // Align scales for comparison.
            if s1 == s2 {
                Ok(m1.cmp(m2))
            } else if s1 > s2 {
                let factor = 10i128.pow((*s1 - *s2) as u32);
                Ok(m1.cmp(&m2.saturating_mul(factor)))
            } else {
                let factor = 10i128.pow((*s2 - *s1) as u32);
                Ok(m1.saturating_mul(factor).cmp(m2))
            }
        }
        (Value::Text(a), Value::Text(b)) => Ok(compare_text(current_eval_collation(), a, b)),
        (Value::Bytes(a), Value::Bytes(b)) => Ok(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Ok(a.cmp(b)),
        (Value::Timestamp(a), Value::Timestamp(b)) => Ok(a.cmp(b)),
        (Value::Uuid(a), Value::Uuid(b)) => Ok(a.cmp(b)),
        _ => Err(DbError::TypeMismatch {
            expected: "comparable types".into(),
            got: format!("{} and {}", l.variant_name(), r.variant_name()),
        }),
    }
}

// ── String concat ─────────────────────────────────────────────────────────────

fn eval_concat(l: Value, r: Value) -> Result<Value, DbError> {
    match (l, r) {
        (Value::Text(a), Value::Text(b)) => Ok(Value::Text(a + &b)),
        (l, r) => Err(DbError::TypeMismatch {
            expected: "Text || Text".into(),
            got: format!("{} || {}", l.variant_name(), r.variant_name()),
        }),
    }
}

// ── IN list ───────────────────────────────────────────────────────────────────

fn eval_in(v: Value, list: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    // NULL expr → UNKNOWN.
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }

    let mut has_null_in_list = false;

    for item_expr in list {
        let item = eval(item_expr, row)?;
        match item {
            Value::Null => {
                has_null_in_list = true;
            }
            ref iv => {
                // Check equality (NULL-safe at the item level).
                match compare_values(&v, iv) {
                    Ok(std::cmp::Ordering::Equal) => return Ok(Value::Bool(true)),
                    Ok(_) => {}  // not equal, continue
                    Err(_) => {} // incompatible types, treat as not equal
                }
            }
        }
    }

    // No match found.
    if has_null_in_list {
        Ok(Value::Null) // UNKNOWN — can't determine definitively
    } else {
        Ok(Value::Bool(false)) // definitively not in list
    }
}

// ── LIKE ──────────────────────────────────────────────────────────────────────

/// Iterative LIKE pattern matching on Unicode characters.
///
/// `%` matches any sequence of zero or more characters.
/// `_` matches exactly one character.
/// All other characters match literally (case-sensitive).
///
/// Algorithm: O(n·m) with backtracking, handles all patterns including
/// multiple `%` without exponential blowup.
pub fn like_match(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pat: Vec<char> = pattern.chars().collect();
    let (n, m) = (text.len(), pat.len());

    let mut ti: usize = 0;
    let mut pi: usize = 0;
    // Backtrack points: last '%' in pattern and the text position at that time.
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;

    while ti < n {
        if pi < m && (pat[pi] == '_' || pat[pi] == text[ti]) {
            // Literal or '_' match — advance both.
            ti += 1;
            pi += 1;
        } else if pi < m && pat[pi] == '%' {
            // '%' — record backtrack point, advance only pattern.
            // '%' matches zero characters to start.
            star_pi = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(spi) = star_pi {
            // Mismatch — backtrack: '%' matches one more text character.
            star_ti += 1;
            ti = star_ti;
            pi = spi + 1;
        } else {
            // No backtrack point — definitive mismatch.
            return false;
        }
    }

    // Consume any trailing '%' in the pattern (they match empty string).
    while pi < m && pat[pi] == '%' {
        pi += 1;
    }

    pi == m
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── like_match ────────────────────────────────────────────────────────────

    #[test]
    fn test_like_exact_match() {
        assert!(like_match("hello", "hello"));
        assert!(!like_match("hello", "world"));
    }

    #[test]
    fn test_like_percent_any() {
        assert!(like_match("hello", "%"));
        assert!(like_match("", "%"));
        assert!(like_match("hello", "%ello"));
        assert!(like_match("hello", "hell%"));
        assert!(like_match("hello", "%ell%"));
        assert!(like_match("hello", "%"));
        assert!(!like_match("hello", "world%"));
    }

    #[test]
    fn test_like_underscore() {
        // _ matches exactly one char
        assert!(like_match("hello", "h_llo")); // h + _ + llo
        assert!(like_match("hello", "_____")); // 5 underscores = 5 chars ✓
        assert!(like_match("hello", "h___o")); // h + 3 underscores + o = h+e+l+l+o ✓
        assert!(!like_match("hello", "h____o")); // 6 pattern positions vs 5 text chars
        assert!(!like_match("hello", "____")); // 4 underscores vs 5 chars
        assert!(!like_match("hi", "___")); // 3 underscores vs 2 chars
    }

    #[test]
    fn test_like_multiple_percent() {
        assert!(like_match("abcdef", "%b%d%"));
        assert!(like_match("abcdef", "a%f"));
        assert!(!like_match("abcdef", "%z%"));
    }

    #[test]
    fn test_like_empty_string() {
        assert!(like_match("", "%"));
        assert!(like_match("", "%%"));
        assert!(!like_match("", "_"));
        assert!(like_match("", ""));
        assert!(!like_match("a", ""));
    }

    #[test]
    fn test_like_unicode() {
        // '_' must match one Unicode char, not one byte
        assert!(like_match("こんにちは", "_んにちは"));
        assert!(like_match("こんにちは", "%にちは"));
        assert!(like_match("🦀rust", "%rust"));
    }

    // ── coerce_for_op integration ─────────────────────────────────────────────

    #[test]
    fn test_eval_int_plus_bigint() {
        // Int(1) + BigInt(999) — coerce_for_op widens Int to BigInt.
        let expr = Expr::BinaryOp {
            op: BinaryOp::Add,
            left: Box::new(Expr::Literal(Value::Int(1))),
            right: Box::new(Expr::Literal(Value::BigInt(999))),
        };
        assert_eq!(eval(&expr, &[]).unwrap(), Value::BigInt(1000));
    }

    #[test]
    fn test_eval_int_eq_real() {
        // Int(5) = Real(5.0) — coerce_for_op widens Int to Real for comparison.
        let expr = Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Literal(Value::Int(5))),
            right: Box::new(Expr::Literal(Value::Real(5.0))),
        };
        assert_eq!(eval(&expr, &[]).unwrap(), Value::Bool(true));
    }

    #[test]
    fn test_eval_int_lt_real() {
        let expr = Expr::BinaryOp {
            op: BinaryOp::Lt,
            left: Box::new(Expr::Literal(Value::Int(3))),
            right: Box::new(Expr::Literal(Value::Real(3.5))),
        };
        assert_eq!(eval(&expr, &[]).unwrap(), Value::Bool(true));
    }

    #[test]
    fn test_eval_int_plus_decimal() {
        // Int(2) + Decimal(314, 2) = Decimal(514, 2) = 5.14
        let expr = Expr::BinaryOp {
            op: BinaryOp::Add,
            left: Box::new(Expr::Literal(Value::Int(2))),
            right: Box::new(Expr::Literal(Value::Decimal(314, 2))),
        };
        assert_eq!(eval(&expr, &[]).unwrap(), Value::Decimal(514, 2));
    }

    #[test]
    fn test_eval_incompatible_types_error() {
        let expr = Expr::BinaryOp {
            op: BinaryOp::Add,
            left: Box::new(Expr::Literal(Value::Int(1))),
            right: Box::new(Expr::Literal(Value::Text("a".into()))),
        };
        assert!(eval(&expr, &[]).is_err());
    }

    // ── is_truthy ────────────────────────────────────────────────────────────

    #[test]
    fn test_is_truthy() {
        assert!(is_truthy(&Value::Bool(true)));
        assert!(!is_truthy(&Value::Bool(false)));
        assert!(!is_truthy(&Value::Null));
        assert!(!is_truthy(&Value::Int(1)));
        assert!(!is_truthy(&Value::Text("true".into())));
    }
}
