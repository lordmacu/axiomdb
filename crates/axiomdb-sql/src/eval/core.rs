use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use axiomdb_core::error::DbError;
use axiomdb_types::{
    coerce::{coerce, CoercionMode},
    DataType, Value,
};

use crate::{
    expr::{BinaryOp, Expr},
    result::QueryResult,
    session::{session_collation_from_name, SessionCollation},
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

fn explicit_expr_collation(expr: &Expr) -> Option<SessionCollation> {
    match expr {
        Expr::Collate { collation, .. } => session_collation_from_name(collation).ok(),
        _ => None,
    }
}

fn explicit_collation_from_exprs(exprs: &[&Expr]) -> Option<SessionCollation> {
    exprs.iter().find_map(|expr| explicit_expr_collation(expr))
}

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

        Expr::Collate { expr, .. } => eval(expr, row),

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

        // Phase 20.4, Step 7: ANY/ALL as the RHS of a comparison.
        // When the RHS is AnyOf or AllOf, we handle it specially here
        // instead of evaluating it as a standalone expression.
        Expr::BinaryOp { op, left, right } if matches!(right.as_ref(), Expr::AnyOf { .. }) => {
            let (any_expr, array) = match right.as_ref() {
                Expr::AnyOf { expr, array } => (expr, array),
                _ => unreachable!(),
            };
            let _guard = explicit_collation_from_exprs(&[left, any_expr]).map(CollationGuard::new);
            let lhs_val = eval(left, row)?;
            let arr_val = eval(array, row)?;
            super::functions::any_all::eval_any_of(&lhs_val, &arr_val, *op)
        }
        Expr::BinaryOp { op, left, right } if matches!(right.as_ref(), Expr::AllOf { .. }) => {
            let (all_expr, array) = match right.as_ref() {
                Expr::AllOf { expr, array } => (expr, array),
                _ => unreachable!(),
            };
            let _guard = explicit_collation_from_exprs(&[left, all_expr]).map(CollationGuard::new);
            let lhs_val = eval(left, row)?;
            let arr_val = eval(array, row)?;
            super::functions::any_all::eval_all_of(&lhs_val, &arr_val, *op)
        }

        Expr::BinaryOp { op, left, right } => {
            let _guard = explicit_collation_from_exprs(&[left, right]).map(CollationGuard::new);
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
            let _guard = explicit_collation_from_exprs(&[expr, low, high]).map(CollationGuard::new);
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
            // Phase 20.4, Step 7: Handle `expr LIKE ANY(array)` / `expr LIKE ALL(array)`.
            // When the pattern is an AnyOf/AllOf node, we expand it element-wise.
            // The AnyOf/AllOf was created with a NULL placeholder for the LHS expr.
            // We substitute it with the actual LHS from this Like.
            if let Expr::AnyOf {
                expr: like_expr,
                array,
            } = pattern.as_ref()
            {
                // Fix up the NULL placeholder with the actual LHS
                let resolved_like_expr = if matches!(like_expr.as_ref(), Expr::Literal(Value::Null))
                {
                    (*expr).clone()
                } else {
                    like_expr.clone()
                };
                let _guard = explicit_collation_from_exprs(&[expr, &resolved_like_expr])
                    .map(CollationGuard::new);
                let v = eval(expr, row)?;
                let arr_val = eval(array, row)?;
                let escape_ch = if let Some(esc_expr) = escape {
                    match eval(esc_expr, row)? {
                        Value::Text(esc) => esc.chars().next(),
                        Value::Null => return Ok(Value::Null),
                        _ => None,
                    }
                } else {
                    None
                };

                // Evaluate LIKE ANY over the array
                let result =
                    crate::eval::functions::any_all::eval_like_any(&v, &arr_val, escape_ch)?;
                // Preserve NULL (3VL) - only apply negation to TRUE/FALSE
                return match result {
                    Value::Null => Ok(Value::Null),
                    Value::Bool(true) => Ok(Value::Bool(!*negated)),
                    Value::Bool(false) => Ok(Value::Bool(*negated)),
                    _ => Ok(result), // Should not happen for LIKE
                };
            }
            if let Expr::AllOf {
                expr: like_expr,
                array,
            } = pattern.as_ref()
            {
                // Fix up the NULL placeholder with the actual LHS
                let resolved_like_expr = if matches!(like_expr.as_ref(), Expr::Literal(Value::Null))
                {
                    (*expr).clone()
                } else {
                    like_expr.clone()
                };
                let _guard = explicit_collation_from_exprs(&[expr, &resolved_like_expr])
                    .map(CollationGuard::new);
                let v = eval(expr, row)?;
                let arr_val = eval(array, row)?;
                let escape_ch = if let Some(esc_expr) = escape {
                    match eval(esc_expr, row)? {
                        Value::Text(esc) => esc.chars().next(),
                        Value::Null => return Ok(Value::Null),
                        _ => None,
                    }
                } else {
                    None
                };

                // Evaluate LIKE ALL over the array
                let result =
                    crate::eval::functions::any_all::eval_like_all(&v, &arr_val, escape_ch)?;
                // Preserve NULL (3VL) - only apply negation to TRUE/FALSE
                return match result {
                    Value::Null => Ok(Value::Null),
                    Value::Bool(true) => Ok(Value::Bool(!*negated)),
                    Value::Bool(false) => Ok(Value::Bool(*negated)),
                    _ => Ok(result), // Should not happen for LIKE
                };
            }

            let _guard =
                explicit_collation_from_exprs(&[expr, pattern, escape.as_deref().unwrap_or(expr)])
                    .map(CollationGuard::new);
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
            let _guard = explicit_expr_collation(expr)
                .or_else(|| list.iter().find_map(explicit_expr_collation))
                .map(CollationGuard::new);
            let v = eval(expr, row)?;
            let result = eval_in(v, list, row)?;
            Ok(if *negated { apply_not(result) } else { result })
        }

        Expr::Function { name, args } => eval_function(name, args, row),

        // ── CAST ──────────────────────────────────────────────────────────────
        Expr::Cast { expr, target } => {
            let v = eval(expr, row)?;
            coerce(v, target.clone(), CoercionMode::Strict)
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

        // ArrayAgg is only valid as an aggregate — never reached by scalar eval.
        Expr::ArrayAgg { .. } => Err(DbError::InvalidValue {
            reason: "array_agg can only be used as an aggregate function".into(),
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
        Expr::Window { .. } => Err(DbError::InvalidValue {
            reason: "window function cannot be evaluated as a scalar expression".into(),
        }),

        // Phase 20.4 — ARRAY[expr, ...] constructor.
        Expr::ArrayConstructor { elements } => eval_array_constructor(elements, row),

        // Phase 20.4, Step 5 — array subscript `arr[index]` or slice `arr[lo:hi]`.
        Expr::Subscript {
            array,
            index,
            slice,
        } => {
            let arr_val = eval(array, row)?;
            if let Some(hi) = slice {
                // Slice: arr[lo:hi]
                let lo_val = eval(index, row)?;
                let hi_val = eval(hi, row)?;
                super::array_ops::array_slice(&arr_val, &lo_val, &hi_val)
            } else {
                // Regular subscript: arr[index]
                let idx_val = eval(index, row)?;
                super::array_ops::array_subscript(&arr_val, &idx_val)
            }
        }

        // Phase 20.4, Step 7 — ANY/ALL as standalone expressions (not in BinaryOp context).
        // For standalone `ANY(array)` (implicit =), we use Eq semantics.
        Expr::AnyOf { expr, array } => {
            use super::functions::any_all::eval_any_of;
            let elem_val = eval(expr, row)?;
            let arr_val = eval(array, row)?;
            eval_any_of(&elem_val, &arr_val, BinaryOp::Eq)
        }
        Expr::AllOf { expr, array } => {
            use super::functions::any_all::eval_all_of;
            let elem_val = eval(expr, row)?;
            let arr_val = eval(array, row)?;
            eval_all_of(&elem_val, &arr_val, BinaryOp::Eq)
        }
        Expr::Row(elems) => {
            let vals: Result<Vec<_>, _> = elems.iter().map(|e| eval(e, row)).collect();
            Ok(Value::Composite(vals?))
        }
        Expr::FieldAccess { col_idx, field_idx } => {
            let composite = row
                .get(*col_idx)
                .cloned()
                .ok_or(DbError::ColumnIndexOutOfBounds {
                    idx: *col_idx,
                    len: row.len(),
                })?;
            match composite {
                Value::Composite(fields) => {
                    fields
                        .into_iter()
                        .nth(*field_idx)
                        .ok_or(DbError::ColumnIndexOutOfBounds {
                            idx: *field_idx,
                            len: 0,
                        })
                }
                Value::Null => Ok(Value::Null),
                _ => Err(DbError::TypeMismatch {
                    expected: "composite".into(),
                    got: format!("{composite:?}"),
                }),
            }
        }

        // Phase 20.20 — XML constructor special forms.
        Expr::XmlElement { tag, attrs, content } => {
            #[cfg(not(feature = "xml"))]
            { let _ = (tag, attrs, content); return Err(DbError::NotImplemented { feature: "XML functions (compile with xml feature to enable)".into() }); }
            #[cfg(feature = "xml")]
            super::functions::xml::eval_xmlelement(tag, attrs, content, row, &mut NoSubquery)
        }
        Expr::XmlForest { items } => {
            #[cfg(not(feature = "xml"))]
            { let _ = items; return Err(DbError::NotImplemented { feature: "XML functions (compile with xml feature to enable)".into() }); }
            #[cfg(feature = "xml")]
            super::functions::xml::eval_xmlforest(items, row, &mut NoSubquery)
        }
        Expr::XmlRoot { doc, version, standalone } => {
            #[cfg(not(feature = "xml"))]
            { let _ = (doc, version, standalone); return Err(DbError::NotImplemented { feature: "XML functions (compile with xml feature to enable)".into() }); }
            #[cfg(feature = "xml")]
            super::functions::xml::eval_xmlroot(doc, version, *standalone, row, &mut NoSubquery)
        }
        Expr::XmlConcat { args } => {
            #[cfg(not(feature = "xml"))]
            { let _ = args; return Err(DbError::NotImplemented { feature: "XML functions (compile with xml feature to enable)".into() }); }
            #[cfg(feature = "xml")]
            super::functions::xml::eval_xmlconcat(args, row, &mut NoSubquery)
        }
        Expr::XmlQuery { xpath, doc } => {
            #[cfg(not(feature = "xml"))]
            { let _ = (xpath, doc); return Err(DbError::NotImplemented { feature: "XML functions (compile with xml feature to enable)".into() }); }
            #[cfg(feature = "xml")]
            super::functions::xml::eval_xmlquery(xpath, doc, row, &mut NoSubquery)
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
            Value::Timestamp(t) | Value::TimestampTz(t) => t.hash(state),
            Value::Uuid(u) => u.hash(state),
            Value::Array(elems) => {
                for elem in elems {
                    HashableValue(elem.clone()).hash(state);
                }
            }
            Value::Range(rv) => {
                rv.to_display_string().hash(state);
            }
            Value::Money(m, s, c) => {
                m.hash(state);
                s.hash(state);
                c.hash(state);
            }
            Value::Composite(fields) => {
                for f in fields {
                    HashableValue(f.clone()).hash(state);
                }
            }
            Value::Ltree(s) | Value::Xml(s) => s.hash(state),
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

        // Phase 20.4 — ANY/ALL array quantifiers: handled in BinaryOp.
        Expr::AnyOf { .. } | Expr::AllOf { .. } => {
            // AnyOf/AllOf should be handled by BinaryOp during eval_binary;
            // if reached here directly, it's a bug.
            Err(DbError::Internal {
                message: "AnyOf/AllOf reached eval_with directly — should be handled in BinaryOp"
                    .into(),
            })
        }

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

        Expr::Collate { expr, .. } => eval_with(expr, row, sq),

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

        // Phase 20.4, Step 7: ANY/ALL as the RHS of a comparison.
        // When the RHS is AnyOf or AllOf, we handle it specially here
        // instead of evaluating it as a standalone expression.
        Expr::BinaryOp { op, left, right } if matches!(right.as_ref(), Expr::AnyOf { .. }) => {
            let (any_expr, array) = match right.as_ref() {
                Expr::AnyOf { expr, array } => (expr, array),
                _ => unreachable!(),
            };
            let _guard = explicit_collation_from_exprs(&[left, any_expr]).map(CollationGuard::new);
            let lhs_val = eval_with(left, row, sq)?;
            let arr_val = eval_with(array, row, sq)?;
            super::functions::any_all::eval_any_of(&lhs_val, &arr_val, *op)
        }
        Expr::BinaryOp { op, left, right } if matches!(right.as_ref(), Expr::AllOf { .. }) => {
            let (all_expr, array) = match right.as_ref() {
                Expr::AllOf { expr, array } => (expr, array),
                _ => unreachable!(),
            };
            let _guard = explicit_collation_from_exprs(&[left, all_expr]).map(CollationGuard::new);
            let lhs_val = eval_with(left, row, sq)?;
            let arr_val = eval_with(array, row, sq)?;
            super::functions::any_all::eval_all_of(&lhs_val, &arr_val, *op)
        }

        Expr::BinaryOp { op, left, right } => {
            let _guard = explicit_collation_from_exprs(&[left, right]).map(CollationGuard::new);
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
            let _guard = explicit_collation_from_exprs(&[expr, low, high]).map(CollationGuard::new);
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
            // Phase 20.4, Step 7: Handle `expr LIKE ANY(array)` / `expr LIKE ALL(array)`.
            // When the pattern is an AnyOf/AllOf node, we expand it element-wise.
            // The AnyOf/AllOf was created with a NULL placeholder for the LHS expr.
            // We substitute it with the actual LHS from this Like.
            if let Expr::AnyOf {
                expr: like_expr,
                array,
            } = pattern.as_ref()
            {
                // Fix up the NULL placeholder with the actual LHS
                let resolved_like_expr = if matches!(like_expr.as_ref(), Expr::Literal(Value::Null))
                {
                    (*expr).clone()
                } else {
                    like_expr.clone()
                };
                let _guard = explicit_collation_from_exprs(&[expr, &resolved_like_expr])
                    .map(CollationGuard::new);
                let v = eval_with(expr, row, sq)?;
                let arr_val = eval_with(array, row, sq)?;
                let escape_ch = if let Some(esc_expr) = escape {
                    match eval_with(esc_expr, row, sq)? {
                        Value::Text(esc) => esc.chars().next(),
                        Value::Null => return Ok(Value::Null),
                        _ => None,
                    }
                } else {
                    None
                };

                // Evaluate LIKE ANY over the array
                let result =
                    crate::eval::functions::any_all::eval_like_any(&v, &arr_val, escape_ch)?;
                // Preserve NULL (3VL) - only apply negation to TRUE/FALSE
                return match result {
                    Value::Null => Ok(Value::Null),
                    Value::Bool(true) => Ok(Value::Bool(!*negated)),
                    Value::Bool(false) => Ok(Value::Bool(*negated)),
                    _ => Ok(result), // Should not happen for LIKE
                };
            }
            if let Expr::AllOf {
                expr: like_expr,
                array,
            } = pattern.as_ref()
            {
                // Fix up the NULL placeholder with the actual LHS
                let resolved_like_expr = if matches!(like_expr.as_ref(), Expr::Literal(Value::Null))
                {
                    (*expr).clone()
                } else {
                    like_expr.clone()
                };
                let _guard = explicit_collation_from_exprs(&[expr, &resolved_like_expr])
                    .map(CollationGuard::new);
                let v = eval_with(expr, row, sq)?;
                let arr_val = eval_with(array, row, sq)?;
                let escape_ch = if let Some(esc_expr) = escape {
                    match eval_with(esc_expr, row, sq)? {
                        Value::Text(esc) => esc.chars().next(),
                        Value::Null => return Ok(Value::Null),
                        _ => None,
                    }
                } else {
                    None
                };

                // Evaluate LIKE ALL over the array
                let result =
                    crate::eval::functions::any_all::eval_like_all(&v, &arr_val, escape_ch)?;
                // Preserve NULL (3VL) - only apply negation to TRUE/FALSE
                return match result {
                    Value::Null => Ok(Value::Null),
                    Value::Bool(true) => Ok(Value::Bool(!*negated)),
                    Value::Bool(false) => Ok(Value::Bool(*negated)),
                    _ => Ok(result), // Should not happen for LIKE
                };
            }

            let _guard =
                explicit_collation_from_exprs(&[expr, pattern, escape.as_deref().unwrap_or(expr)])
                    .map(CollationGuard::new);
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
            let _guard = explicit_expr_collation(expr)
                .or_else(|| list.iter().find_map(explicit_expr_collation))
                .map(CollationGuard::new);
            let v = eval_with(expr, row, sq)?;
            let result = eval_in_with(v, list, row, sq)?;
            Ok(if *negated { apply_not(result) } else { result })
        }

        Expr::Function { name, args } => {
            if let Some(value) = sq.eval_function(name, args, row)? {
                Ok(value)
            } else {
                eval_function(name, args, row)
            }
        }

        Expr::Cast { expr, target } => {
            let v = eval_with(expr, row, sq)?;
            coerce(v, target.clone(), CoercionMode::Strict)
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

        // ArrayAgg is only valid as an aggregate — never reached by scalar eval_with.
        Expr::ArrayAgg { .. } => Err(DbError::InvalidValue {
            reason: "array_agg can only be used as an aggregate function".into(),
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
        Expr::Window { .. } => Err(DbError::InvalidValue {
            reason: "window function cannot be evaluated as a scalar expression".into(),
        }),

        // Phase 20.4 — ARRAY[expr, ...] constructor (no subquery needed).
        Expr::ArrayConstructor { elements } => eval_array_constructor(elements, row),

        // Phase 20.4, Step 5 — array subscript `arr[index]` or slice `arr[lo:hi]`.
        Expr::Subscript {
            array,
            index,
            slice,
        } => {
            let arr_val = eval_with(array, row, sq)?;
            if let Some(hi) = slice {
                // Slice: arr[lo:hi]
                let lo_val = eval_with(index, row, sq)?;
                let hi_val = eval_with(hi, row, sq)?;
                super::array_ops::array_slice(&arr_val, &lo_val, &hi_val)
            } else {
                // Regular subscript: arr[index]
                let idx_val = eval_with(index, row, sq)?;
                super::array_ops::array_subscript(&arr_val, &idx_val)
            }
        }
        Expr::Row(elems) => {
            let vals: Result<Vec<_>, _> = elems.iter().map(|e| eval_with(e, row, sq)).collect();
            Ok(Value::Composite(vals?))
        }
        Expr::FieldAccess { col_idx, field_idx } => {
            let composite = row
                .get(*col_idx)
                .cloned()
                .ok_or(DbError::ColumnIndexOutOfBounds {
                    idx: *col_idx,
                    len: row.len(),
                })?;
            match composite {
                Value::Composite(fields) => {
                    fields
                        .into_iter()
                        .nth(*field_idx)
                        .ok_or(DbError::ColumnIndexOutOfBounds {
                            idx: *field_idx,
                            len: 0,
                        })
                }
                Value::Null => Ok(Value::Null),
                _ => Err(DbError::TypeMismatch {
                    expected: "composite".into(),
                    got: format!("{composite:?}"),
                }),
            }
        }

        // Phase 20.20 — XML constructor special forms.
        Expr::XmlElement { tag, attrs, content } => {
            #[cfg(not(feature = "xml"))]
            { let _ = (tag, attrs, content); return Err(DbError::NotImplemented { feature: "XML functions (compile with xml feature to enable)".into() }); }
            #[cfg(feature = "xml")]
            super::functions::xml::eval_xmlelement(tag, attrs, content, row, sq)
        }
        Expr::XmlForest { items } => {
            #[cfg(not(feature = "xml"))]
            { let _ = items; return Err(DbError::NotImplemented { feature: "XML functions (compile with xml feature to enable)".into() }); }
            #[cfg(feature = "xml")]
            super::functions::xml::eval_xmlforest(items, row, sq)
        }
        Expr::XmlRoot { doc, version, standalone } => {
            #[cfg(not(feature = "xml"))]
            { let _ = (doc, version, standalone); return Err(DbError::NotImplemented { feature: "XML functions (compile with xml feature to enable)".into() }); }
            #[cfg(feature = "xml")]
            super::functions::xml::eval_xmlroot(doc, version, *standalone, row, sq)
        }
        Expr::XmlConcat { args } => {
            #[cfg(not(feature = "xml"))]
            { let _ = args; return Err(DbError::NotImplemented { feature: "XML functions (compile with xml feature to enable)".into() }); }
            #[cfg(feature = "xml")]
            super::functions::xml::eval_xmlconcat(args, row, sq)
        }
        Expr::XmlQuery { xpath, doc } => {
            #[cfg(not(feature = "xml"))]
            { let _ = (xpath, doc); return Err(DbError::NotImplemented { feature: "XML functions (compile with xml feature to enable)".into() }); }
            #[cfg(feature = "xml")]
            super::functions::xml::eval_xmlquery(xpath, doc, row, sq)
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

// ── Phase 20.4 — ARRAY constructor ────────────────────────────────────────────

/// Type tag extracted from a Value for the purpose of array type inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArrayElemType {
    Null,
    Bool,
    Int,
    BigInt,
    Real,
    Decimal,
    Text,
    Json,
    Jsonb,
    Bytes,
    Date,
    Timestamp,
    Uuid,
    Array,
    Range,
    Money,
    Composite,
    Ltree,
    Xml,
}

impl ArrayElemType {
    fn from_value(v: &Value) -> Self {
        match v {
            Value::Null => Self::Null,
            Value::Bool(_) => Self::Bool,
            Value::Int(_) => Self::Int,
            Value::BigInt(_) => Self::BigInt,
            Value::Real(_) => Self::Real,
            Value::Decimal(..) => Self::Decimal,
            Value::Text(_) => Self::Text,
            Value::Json(_) => Self::Json,
            Value::Jsonb(_) => Self::Jsonb,
            Value::Bytes(_) => Self::Bytes,
            Value::Date(_) => Self::Date,
            Value::Timestamp(_) | Value::TimestampTz(_) => Self::Timestamp,
            Value::Uuid(_) => Self::Uuid,
            Value::Array(_) => Self::Array,
            Value::Range(_) => Self::Range,
            Value::Money(..) => Self::Money,
            Value::Composite(_) => Self::Composite,
            Value::Ltree(_) => Self::Ltree,
            Value::Xml(_) => Self::Xml,
        }
    }

    fn to_data_type(self) -> Option<DataType> {
        match self {
            Self::Null => None,
            Self::Bool => Some(DataType::Bool),
            Self::Int => Some(DataType::Int),
            Self::BigInt => Some(DataType::BigInt),
            Self::Real => Some(DataType::Real),
            Self::Decimal => Some(DataType::Decimal),
            Self::Text => Some(DataType::Text),
            Self::Json => Some(DataType::Json),
            Self::Jsonb => Some(DataType::Jsonb),
            Self::Bytes => Some(DataType::Bytes),
            Self::Date => Some(DataType::Date),
            Self::Timestamp => Some(DataType::Timestamp),
            Self::Uuid => Some(DataType::Uuid),
            Self::Array => None, // Nested arrays handled separately
            Self::Range => None, // Range types not supported as array elements
            Self::Money => Some(DataType::Money),
            Self::Composite => None,
            Self::Ltree => Some(DataType::Ltree),
            Self::Xml => Some(DataType::Xml),
        }
    }
}

/// Result of attempting to find a common type for array elements.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CommonType {
    /// All elements are NULL or we found a concrete type.
    Concrete(DataType),
    /// int + real → real widening.
    IntReal,
}

impl CommonType {
    fn merge(&self, other: &Value) -> Result<Self, DbError> {
        use ArrayElemType as T;
        let other_t = T::from_value(other);

        // NULL doesn't affect type inference.
        if matches!(other_t, T::Null) {
            return Ok(self.clone());
        }

        // Handle Array type: extract inner type and merge with self's inner type.
        // This is needed for nested arrays like ARRAY[ARRAY[1,2], ARRAY[3,4]].
        if matches!(other_t, T::Array) {
            let Value::Array(elems) = other else {
                unreachable!("other_t is Array but other is not Value::Array");
            };
            if elems.is_empty() {
                // Empty inner array - can't infer type, but merge succeeds if self is Array
                return Ok(self.clone());
            }
            // Get inner element type from first element
            let inner_elem = &elems[0];
            let inner_t = T::from_value(inner_elem);

            match self {
                // If self is Array type, merge inner types
                Self::Concrete(DataType::Array(inner_box)) => {
                    let inner_dt = &**inner_box;
                    // inner_t is the ArrayElemType of the other array's element
                    // We need to compare/merge inner_dt with inner_t's DataType
                    if let Some(inner_elem_dt) = inner_t.to_data_type() {
                        if inner_dt == &inner_elem_dt {
                            // Same inner type - compatible
                            Ok(self.clone())
                        } else {
                            // Different inner types - try numeric widening
                            match (inner_dt.clone(), inner_t) {
                                (DataType::BigInt, T::Int) => {
                                    Ok(Self::Concrete(DataType::Array(Box::new(DataType::BigInt))))
                                }
                                (DataType::Int, T::BigInt) => {
                                    Ok(Self::Concrete(DataType::Array(Box::new(DataType::BigInt))))
                                }
                                (DataType::Int, T::Real) | (DataType::BigInt, T::Real) => {
                                    Ok(Self::Concrete(DataType::Array(Box::new(DataType::Real))))
                                }
                                (DataType::Real, T::Int)
                                | (DataType::Real, T::BigInt)
                                | (DataType::Real, T::Real) => {
                                    Ok(Self::Concrete(DataType::Array(Box::new(DataType::Real))))
                                }
                                (DataType::BigInt, T::BigInt) => Ok(self.clone()),
                                _ => Err(DbError::TypeMismatch {
                                    expected: inner_dt.name(),
                                    got: inner_elem_dt.name(),
                                }),
                            }
                        }
                    } else {
                        // inner_t doesn't have a simple DataType (e.g., it's also Array)
                        Err(DbError::TypeMismatch {
                            expected: inner_dt.name(),
                            got: "complex type".into(),
                        })
                    }
                }
                _ => Err(DbError::TypeMismatch {
                    expected: "Array".into(),
                    got: "non-Array".into(),
                }),
            }
        } else {
            // Not Array type - proceed with normal merge
            self.merge_non_array(other, other_t)
        }
    }

    fn merge_non_array(&self, _other: &Value, other_t: ArrayElemType) -> Result<Self, DbError> {
        use ArrayElemType as T;

        // Handle Array type explicitly - this can't be compared via to_data_type()
        if matches!(other_t, T::Array) {
            return Err(DbError::TypeMismatch {
                expected: "Array element type".into(),
                got: "Array".into(),
            });
        }

        // Same type — identity. Check first before other arms.
        if let Some(other_dt) = other_t.to_data_type() {
            if let Self::Concrete(dt) = self {
                if dt == &other_dt {
                    return Ok(self.clone());
                }
            }
        }

        match (self, other_t) {
            // Null merges with anything.
            (Self::Concrete(_), T::Null) | (Self::IntReal, T::Null) => Ok(self.clone()),

            // Real + anything numeric → Real (already widened)
            (Self::Concrete(DataType::Real), T::Int)
            | (Self::Concrete(DataType::Real), T::BigInt)
            | (Self::Concrete(DataType::Real), T::Real)
            | (Self::IntReal, T::Real)
            | (Self::IntReal, T::Int)
            | (Self::IntReal, T::BigInt) => Ok(Self::IntReal),

            // Int + Real → IntReal (widen to real)
            (Self::Concrete(DataType::Int), T::Real) => Ok(Self::IntReal),

            // BigInt + Real → IntReal (widen to real)
            (Self::Concrete(DataType::BigInt), T::Real) => Ok(Self::IntReal),

            // Int + Int → Int
            (Self::Concrete(DataType::Int), T::Int) => Ok(Self::Concrete(DataType::Int)),

            // Int + BigInt → BigInt (int widens to bigint for arithmetic)
            (Self::Concrete(DataType::Int), T::BigInt) => Ok(Self::Concrete(DataType::BigInt)),

            // BigInt + Int → BigInt
            (Self::Concrete(DataType::BigInt), T::Int) => Ok(Self::Concrete(DataType::BigInt)),

            // BigInt + BigInt → BigInt
            (Self::Concrete(DataType::BigInt), T::BigInt) => Ok(Self::Concrete(DataType::BigInt)),

            // Decimal + numeric → Decimal
            (Self::Concrete(DataType::Decimal), T::Int)
            | (Self::Concrete(DataType::Decimal), T::BigInt)
            | (Self::Concrete(DataType::Decimal), T::Decimal)
            | (Self::Concrete(DataType::Decimal), T::Real) => Err(DbError::TypeMismatch {
                expected: "DECIMAL".into(),
                got: other_t.to_data_type().unwrap_or(DataType::Text).name(),
            }),

            // Text + Text → Text
            (Self::Concrete(DataType::Text), T::Text) => Ok(self.clone()),

            // Text + Json → Text
            (Self::Concrete(DataType::Text), T::Json) => Ok(self.clone()),

            // Json + Json → Json
            (Self::Concrete(DataType::Json), T::Json) => Ok(self.clone()),

            // Bytes + Bytes → Bytes
            (Self::Concrete(DataType::Bytes), T::Bytes) => Ok(self.clone()),

            // Date + Date → Date
            (Self::Concrete(DataType::Date), T::Date) => Ok(self.clone()),

            // Timestamp + Timestamp → Timestamp
            (Self::Concrete(DataType::Timestamp), T::Timestamp) => Ok(self.clone()),

            // Uuid + Uuid → Uuid
            (Self::Concrete(DataType::Uuid), T::Uuid) => Ok(self.clone()),

            // Bool + Bool → Bool
            (Self::Concrete(DataType::Bool), T::Bool) => Ok(self.clone()),

            // Mismatched types
            (Self::Concrete(dt), other_t) => Err(DbError::TypeMismatch {
                expected: dt.name(),
                got: other_t.to_data_type().unwrap_or(DataType::Text).name(),
            }),

            (Self::IntReal, t) => Err(DbError::TypeMismatch {
                expected: "REAL".into(),
                got: t.to_data_type().unwrap_or(DataType::Text).name(),
            }),
        }
    }
}

/// Extracts the inner element type from a Value::Array.
fn get_array_ndim(value: &Value) -> Option<usize> {
    match value {
        Value::Array(elems) => {
            if elems.is_empty() {
                Some(1) // Empty array has ndim=1
            } else if let Value::Array(_) = &elems[0] {
                // Nested array — get inner ndim and add 1
                get_array_ndim(&elems[0]).map(|n| n + 1)
            } else {
                Some(1) // Leaf level
            }
        }
        _ => None,
    }
}

/// Validates that all nested arrays have consistent dimensions.
fn validate_nested_dims(values: &[Value]) -> Result<(), DbError> {
    if values.is_empty() {
        return Ok(());
    }

    let first_ndim = match get_array_ndim(&values[0]) {
        Some(n) => n,
        None => return Ok(()), // Non-array element, no nesting to validate
    };

    for (_i, v) in values.iter().enumerate().skip(1) {
        let ndim = get_array_ndim(v);
        match ndim {
            None => {
                return Err(DbError::TypeMismatch {
                    expected: format!("array with {} dimensions", first_ndim),
                    got: "non-array value".into(),
                });
            }
            Some(n) if n != first_ndim => {
                return Err(DbError::TypeMismatch {
                    expected: format!("array with {} dimensions", first_ndim),
                    got: format!("array with {} dimensions", n),
                });
            }
            Some(_) => {}
        }
    }

    // Validate inner arrays recursively
    for v in values {
        if let Value::Array(inner) = v {
            validate_nested_dims(inner)?;
        }
    }

    Ok(())
}

/// Infers the element type of an array value.
/// Returns `None` for empty arrays (can't infer type).
fn infer_array_element_type(arr: &Value) -> Option<DataType> {
    let Value::Array(elems) = arr else {
        return None;
    };
    if elems.is_empty() {
        return None;
    }
    let t = ArrayElemType::from_value(&elems[0]);
    if let Some(dt) = t.to_data_type() {
        Some(dt)
    } else if matches!(t, ArrayElemType::Array) {
        // Nested array - recursively infer inner type
        infer_array_element_type(&elems[0]).map(|inner| DataType::Array(Box::new(inner)))
    } else {
        None
    }
}

/// Evaluates `ARRAY[expr, ...]` — PostgreSQL-compatible array constructor.
///
/// - Empty `ARRAY[]` returns an empty array with a default element type.
/// - Nested arrays (`ARRAY[ARRAY[...], ...]`) are validated for dimension consistency.
/// - Type inference: all-int → int[], int+real → real[], etc.
fn eval_array_constructor(elements: &[Expr], row: &[Value]) -> Result<Value, DbError> {
    use CommonType as C;

    // Empty array — return an empty array with a default type.
    // The type is needed for containment checks (empty array contains nothing → TRUE).
    if elements.is_empty() {
        return Ok(Value::Array(vec![]));
    }

    // Phase 1: evaluate all elements.
    let mut values = Vec::with_capacity(elements.len());
    for elem in elements {
        let v = eval(elem, row)?;
        values.push(v);
    }

    // Phase 2: validate nested array dimensions.
    validate_nested_dims(&values)?;

    // Phase 3: infer common type.
    let mut common: Option<CommonType> = None;
    for v in &values {
        match common {
            None => {
                if matches!(ArrayElemType::from_value(v), ArrayElemType::Null) {
                    // First non-null element determines type.
                    continue;
                }
                let t = ArrayElemType::from_value(v);
                if let Some(dt) = t.to_data_type() {
                    common = Some(C::Concrete(dt));
                } else if matches!(t, ArrayElemType::Array) {
                    // Nested array — element type is determined by inner elements.
                    // Infer the inner element type recursively.
                    if let Some(inner_type) = infer_array_element_type(v) {
                        common = Some(C::Concrete(DataType::Array(Box::new(inner_type))));
                    } else {
                        // Empty nested array - use a default type
                        common = Some(C::Concrete(DataType::Int));
                    }
                }
            }
            Some(ref c) => {
                common = Some(c.merge(v)?);
            }
        }
    }

    // If all elements were NULL, use a default type (Int).
    // This allows ARRAY[NULL] to be used in containment checks where
    // NULL elements should produce NULL results.
    if common.is_none() {
        common = Some(C::Concrete(DataType::Int));
    }

    // Phase 4: coerce all elements to the common type.
    let final_type = match common.unwrap() {
        C::Concrete(dt) => dt,
        C::IntReal => DataType::Real,
    };

    let mut coerced = Vec::with_capacity(values.len());
    for v in values {
        let coerced_v = coerce(v, final_type.clone(), CoercionMode::Strict)?;
        coerced.push(coerced_v);
    }

    Ok(Value::Array(coerced))
}
