use axiomdb_core::error::DbError;
use axiomdb_types::{
    coerce::{coerce, CoercionMode},
    Value,
};

use crate::{
    expr::{BinaryOp, Expr},
    result::QueryResult,
    session::SessionCollation,
    text_semantics::like_match_collated,
};

use super::{
    context::{current_eval_collation, CollationGuard, NoSubquery, SubqueryRunner},
    functions::eval_function,
    ops::{
        apply_and_values, apply_not, compare_values, eval_and, eval_binary, eval_in, eval_or,
        eval_unary, is_truthy,
    },
};

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
                                op: BinaryOp::Eq,
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
/// a [`crate::eval::ClosureRunner`] that captures `storage`, `txn`, and
/// `SessionContext`, performing outer-row substitution before executing the
/// inner query.
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
