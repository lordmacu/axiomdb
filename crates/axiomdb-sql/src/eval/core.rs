use std::collections::HashSet;
use std::hash::{Hash, Hasher};

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
        eval_unary, eval_xor, is_truthy, like_match_with_escape,
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
        Expr::Default => Ok(Value::Null),

        Expr::Column { col_idx, name: _ } => {
            row.get(*col_idx)
                .cloned()
                .ok_or(DbError::ColumnIndexOutOfBounds {
                    idx: *col_idx,
                    len: row.len(),
                })
        }

        // Plain `eval` has no access to the "proposed" row — evaluating a
        // `VALUES(col)` reference outside the ODKU helper is a programming
        // error. See `eval_with_proposed` for the ODKU-aware variant.
        Expr::InsertValue { col_idx, name } => Err(DbError::Internal {
            message: format!(
                "unsubstituted VALUES('{name}') (col_idx={col_idx}) — \
                 eval_with_proposed must be called inside an ODKU assignment"
            ),
        }),
        Expr::ExcludedValue { col_idx, name } => Err(DbError::Internal {
            message: format!(
                "unsubstituted EXCLUDED.{name} (col_idx={col_idx}) — \
                 ON CONFLICT evaluation must substitute the proposed row"
            ),
        }),

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

        Expr::BinaryOp {
            op: BinaryOp::Xor,
            left,
            right,
        } => eval_xor(left, right, row),

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
            escape,
        } => {
            let v = eval(expr, row)?;
            let p = eval(pattern, row)?;
            match (v, p) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(text), Value::Text(pat)) => {
                    let matched = if let Some(esc_expr) = escape {
                        match eval(esc_expr, row)? {
                            Value::Text(esc) => {
                                let ch = esc.chars().next().unwrap_or('\\');
                                like_match_with_escape(&text, &pat, ch)
                            }
                            Value::Null => return Ok(Value::Null),
                            _ => like_match_collated(current_eval_collation(), &text, &pat),
                        }
                    } else {
                        like_match_collated(current_eval_collation(), &text, &pat)
                    };
                    Ok(Value::Bool(if *negated { !matched } else { matched }))
                }
                (v, p) => Err(DbError::TypeMismatch {
                    expected: "Text LIKE Text".into(),
                    got: format!("{} LIKE {}", v.variant_name(), p.variant_name()),
                }),
            }
        }

        Expr::IsBoolean {
            expr,
            value,
            negated,
        } => {
            let v = eval(expr, row)?;
            let result = match &v {
                Value::Null => false,
                Value::Bool(b) => *b == *value,
                Value::Int(n) => (*n != 0) == *value,
                Value::BigInt(n) => (*n != 0) == *value,
                Value::Real(f) => (*f != 0.0) == *value,
                _ => false,
            };
            Ok(Value::Bool(if *negated { !result } else { result }))
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

        // Phase 11.19a — SQL/JSON standard query function.
        Expr::SqlJsonQuery { .. } => {
            crate::eval::functions::eval_sql_json_query(expr, row, &mut super::NoSubquery)
        }

        // OuterColumn must be substituted by the executor before eval() is called.
        Expr::OuterColumn {
            name,
            col_idx,
            depth,
        } => Err(DbError::Internal {
            message: format!(
                "unsubstituted OuterColumn '{name}' (col_idx={col_idx}, depth={depth}) — \
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

        // GROUPING(expr, ...) — returns a bitmask.
        // The hidden `__grouping_mask__` column is appended as the last element
        // of the row by `execute_select_grouped_sets`. Bit i of the mask is set
        // when universe[i] is absent from the current grouping set.
        // We compute the result as: for each arg j, check if universe_indices[j]
        // is set in the mask; if so, set bit (n-1-j) in the result (MSB = leftmost).
        Expr::Grouping {
            universe_indices, ..
        } => {
            let mask = match row.last() {
                Some(Value::BigInt(m)) => *m as u64,
                // Outside a GROUPING SETS pass (no hidden mask) → return 0.
                _ => 0u64,
            };
            let result = match universe_indices {
                None => 0i32,
                Some(indices) => {
                    let n = indices.len();
                    let mut bits = 0i32;
                    for (j, &ui) in indices.iter().enumerate() {
                        if ui == usize::MAX {
                            // Arg not in universe → never rolled up → bit stays 0.
                            continue;
                        }
                        if ui < 64 && (mask >> ui) & 1 == 1 {
                            // Bit n-1-j (MSB = leftmost arg). Max 31 args → fits i32.
                            bits |= 1i32 << (n - 1 - j);
                        }
                    }
                    bits
                }
            };
            Ok(Value::Int(result))
        }
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

// ── HashableValue — wraps Value for use in HashSet (IN subquery optimization) ─

/// Thin newtype enabling `Hash` for `Value` so that `IN (SELECT …)` can build a
/// `HashSet` for O(1) membership tests instead of O(n) linear scans.
///
/// `f64` is hashed via its IEEE-754 bit pattern; NaN is forbidden in AxiomDB
/// values so collisions from NaN bit patterns are not a concern.
#[derive(Clone, Debug)]
pub(crate) struct HashableValue(pub(crate) Value);

impl PartialEq for HashableValue {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for HashableValue {}

impl Hash for HashableValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(&self.0).hash(state);
        match &self.0 {
            Value::Null => {}
            Value::Bool(b) => b.hash(state),
            Value::Int(i) => i.hash(state),
            Value::BigInt(i) => i.hash(state),
            Value::Real(f) => f.to_bits().hash(state),
            Value::Decimal(m, s) => {
                m.hash(state);
                s.hash(state);
            }
            Value::Text(s) | Value::Json(s) => s.hash(state),
            Value::Jsonb(b) => b.hash(state),
            Value::Bytes(b) => b.hash(state),
            Value::Date(d) => d.hash(state),
            Value::Timestamp(t) => t.hash(state),
            Value::Uuid(u) => u.hash(state),
        }
    }
}

/// Pre-materialized result of an uncorrelated `IN (SELECT …)` subquery.
///
/// Built once from the `QueryResult`, then probed per outer row in O(1).
pub(crate) struct InSubquerySet {
    values: HashSet<HashableValue>,
    has_null: bool,
}

impl InSubquerySet {
    pub(crate) fn from_query_result(result: QueryResult) -> Self {
        let rows = match result {
            QueryResult::Rows { rows, .. } => rows,
            _ => vec![],
        };
        let mut values = HashSet::with_capacity(rows.len());
        let mut has_null = false;
        for row in rows {
            let v = row.into_iter().next().unwrap_or(Value::Null);
            if matches!(v, Value::Null) {
                has_null = true;
            } else {
                values.insert(HashableValue(v));
            }
        }
        Self { values, has_null }
    }

    pub(crate) fn contains(&self, val: &Value) -> (bool, bool) {
        let found = self.values.contains(&HashableValue(val.clone()));
        (found, self.has_null)
    }
}

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

        // DEFAULT resolves to NULL at eval time.
        // The executor (insert/update) replaces Null with the column's declared
        // default when default-expression persistence is implemented (4.18e).
        Expr::Default => Ok(Value::Null),

        // Phase 11.19a — SQL/JSON standard query function.
        Expr::SqlJsonQuery { .. } => crate::eval::functions::eval_sql_json_query(expr, row, sq),

        Expr::InsertValue { col_idx, name } => Err(DbError::Internal {
            message: format!(
                "unsubstituted VALUES('{name}') (col_idx={col_idx}) — \
                 eval_with_proposed must be called inside an ODKU assignment"
            ),
        }),
        Expr::ExcludedValue { col_idx, name } => Err(DbError::Internal {
            message: format!(
                "unsubstituted EXCLUDED.{name} (col_idx={col_idx}) — \
                 ON CONFLICT evaluation must substitute the proposed row"
            ),
        }),

        Expr::OuterColumn {
            col_idx,
            name,
            depth,
        } => Err(DbError::Internal {
            message: format!(
                "unsubstituted OuterColumn '{name}' (col_idx={col_idx}, depth={depth}) — \
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

        Expr::BinaryOp {
            op: BinaryOp::Xor,
            left,
            right,
        } => eval_xor_with(left, right, row, sq),

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
            escape,
        } => {
            let v = eval_with(expr, row, sq)?;
            let p = eval_with(pattern, row, sq)?;
            match (v, p) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Text(text), Value::Text(pat)) => {
                    let matched = if let Some(esc_expr) = escape {
                        match eval_with(esc_expr, row, sq)? {
                            Value::Text(esc) => {
                                let ch = esc.chars().next().unwrap_or('\\');
                                like_match_with_escape(&text, &pat, ch)
                            }
                            Value::Null => return Ok(Value::Null),
                            _ => like_match_collated(current_eval_collation(), &text, &pat),
                        }
                    } else {
                        like_match_collated(current_eval_collation(), &text, &pat)
                    };
                    Ok(Value::Bool(if *negated { !matched } else { matched }))
                }
                (v, p) => Err(DbError::TypeMismatch {
                    expected: "Text LIKE Text".into(),
                    got: format!("{} LIKE {}", v.variant_name(), p.variant_name()),
                }),
            }
        }

        Expr::IsBoolean {
            expr,
            value,
            negated,
        } => {
            let v = eval_with(expr, row, sq)?;
            let result = match &v {
                Value::Null => false,
                Value::Bool(b) => *b == *value,
                Value::Int(n) => (*n != 0) == *value,
                Value::BigInt(n) => (*n != 0) == *value,
                Value::Real(f) => (*f != 0.0) == *value,
                _ => false,
            };
            Ok(Value::Bool(if *negated { !result } else { result }))
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
            let (found, has_null) = sq.run_in_check(query, &left)?;
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

        // GROUPING — same as scalar eval path; sq is unused here.
        Expr::Grouping {
            universe_indices, ..
        } => {
            let mask = match row.last() {
                Some(Value::BigInt(m)) => *m as u64,
                _ => 0u64,
            };
            let result = match universe_indices {
                None => 0i32,
                Some(indices) => {
                    let n = indices.len();
                    let mut bits = 0i32;
                    for (j, &ui) in indices.iter().enumerate() {
                        if ui == usize::MAX {
                            continue;
                        }
                        if ui < 64 && (mask >> ui) & 1 == 1 {
                            bits |= 1i32 << (n - 1 - j);
                        }
                    }
                    bits
                }
            };
            Ok(Value::Int(result))
        }
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

fn eval_xor_with<R: SubqueryRunner>(
    left: &Expr,
    right: &Expr,
    row: &[Value],
    sq: &mut R,
) -> Result<Value, DbError> {
    let l = eval_with(left, row, sq)?;
    let r = eval_with(right, row, sq)?;
    match (&l, &r) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(a ^ b)),
        _ => Err(DbError::TypeMismatch {
            expected: "Bool".into(),
            got: l.variant_name().into(),
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
