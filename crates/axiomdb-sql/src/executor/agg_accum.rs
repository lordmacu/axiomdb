// ── Accumulator ───────────────────────────────────────────────────────────────

/// Per-group state for a single aggregate expression.
#[derive(Debug)]
enum AggAccumulator {
    /// `COUNT(*)` — increments for every row.
    CountStar { n: u64 },
    /// `COUNT(col)` — increments only for non-NULL values.
    CountCol { n: u64 },
    /// `SUM(col)` — sum of non-NULL values. `None` = all values were NULL.
    Sum { acc: Option<Value> },
    /// `MIN(col)` — minimum non-NULL value.
    Min { acc: Option<Value> },
    /// `MAX(col)` — maximum non-NULL value.
    Max { acc: Option<Value> },
    /// `AVG(col)` — running sum + count; final = sum / count as Real.
    Avg { sum: Value, count: u64 },
    /// `GROUP_CONCAT(...)` — accumulates `(text_value, sort_key_values)` per row.
    GroupConcat {
        /// Accumulated rows: (coerced-to-text value, evaluated ORDER BY key values).
        rows: Vec<(String, Vec<Value>)>,
        /// Separator string placed between values in finalize.
        separator: String,
        /// Whether to deduplicate values before concatenating.
        distinct: bool,
        /// Sort directions: `true` = ASC, `false` = DESC. One per ORDER BY key.
        order_by_dirs: Vec<bool>,
    },
}

impl AggAccumulator {
    fn new(agg: &AggExpr) -> Self {
        match agg {
            AggExpr::GroupConcat {
                separator,
                distinct,
                order_by,
                ..
            } => Self::GroupConcat {
                rows: Vec::new(),
                separator: separator.clone(),
                distinct: *distinct,
                order_by_dirs: order_by
                    .iter()
                    .map(|(_, dir)| matches!(dir, crate::ast::SortOrder::Asc))
                    .collect(),
            },
            AggExpr::Simple { name, arg, .. } => match name.as_str() {
                "count" if arg.is_none() => Self::CountStar { n: 0 },
                "count" => Self::CountCol { n: 0 },
                "sum" => Self::Sum { acc: None },
                "min" => Self::Min { acc: None },
                "max" => Self::Max { acc: None },
                "avg" => Self::Avg {
                    sum: Value::Int(0),
                    count: 0,
                },
                _ => unreachable!("AggAccumulator::new called with non-aggregate"),
            },
        }
    }

    fn update(&mut self, row: &[Value], agg: &AggExpr) -> Result<(), DbError> {
        // Extract the argument expression from Simple aggregates.
        let simple_arg = match agg {
            AggExpr::Simple { arg, .. } => arg.as_ref(),
            AggExpr::GroupConcat { .. } => None,
        };

        // Phase 9.5b: fast-path for simple column refs — avoids eval() overhead.
        #[inline]
        fn fast_eval<'a>(expr: Option<&Expr>, row: &'a [Value]) -> Option<&'a Value> {
            match expr {
                Some(Expr::Column { col_idx, .. }) => row.get(*col_idx),
                _ => None,
            }
        }

        match self {
            Self::CountStar { n } => *n += 1,

            Self::CountCol { n } => {
                if let Some(v) = fast_eval(simple_arg, row) {
                    if !matches!(v, Value::Null) {
                        *n += 1;
                    }
                } else {
                    let v = eval(simple_arg.unwrap(), row)?;
                    if !matches!(v, Value::Null) {
                        *n += 1;
                    }
                }
            }

            Self::Sum { acc } => {
                let v = if let Some(v) = fast_eval(simple_arg, row) {
                    v.clone()
                } else {
                    eval(simple_arg.unwrap(), row)?
                };
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v,
                        // value_agg_add: direct arithmetic, no AST allocation.
                        Some(a) => value_agg_add(a, v)?,
                    });
                }
            }

            Self::Min { acc } => {
                let v = if let Some(v) = fast_eval(simple_arg, row) {
                    v.clone()
                } else {
                    eval(simple_arg.unwrap(), row)?
                };
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v,
                        // compare_values_null_last: direct variant match, no eval().
                        Some(a) => {
                            if compare_values_null_last(&v, &a) == std::cmp::Ordering::Less {
                                v
                            } else {
                                a
                            }
                        }
                    });
                }
            }

            Self::Max { acc } => {
                let v = if let Some(v) = fast_eval(simple_arg, row) {
                    v.clone()
                } else {
                    eval(simple_arg.unwrap(), row)?
                };
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v,
                        // compare_values_null_last: direct variant match, no eval().
                        Some(a) => {
                            if compare_values_null_last(&v, &a) == std::cmp::Ordering::Greater {
                                v
                            } else {
                                a
                            }
                        }
                    });
                }
            }

            Self::Avg { sum, count } => {
                let v = if let Some(v) = fast_eval(simple_arg, row) {
                    v.clone()
                } else {
                    eval(simple_arg.unwrap(), row)?
                };
                if !matches!(v, Value::Null) {
                    // value_agg_add: direct arithmetic, no AST allocation.
                    *sum = value_agg_add(sum.clone(), v)?;
                    *count += 1;
                }
            }

            Self::GroupConcat { rows, .. } => {
                // Extract the GROUP_CONCAT expression and ORDER BY from the AggExpr descriptor.
                let (gc_expr, gc_order_by) = match agg {
                    AggExpr::GroupConcat { expr, order_by, .. } => (expr.as_ref(), order_by),
                    _ => {
                        unreachable!("GroupConcat accumulator paired with non-GroupConcat AggExpr")
                    }
                };

                // Evaluate the concatenated expression; skip NULLs.
                let val = match eval(gc_expr, row)? {
                    Value::Null => return Ok(()),
                    v => value_to_display_string(v),
                };

                // Evaluate ORDER BY key expressions for this row.
                let keys: Vec<Value> = gc_order_by
                    .iter()
                    .map(|(e, _)| eval(e, row))
                    .collect::<Result<Vec<_>, _>>()?;

                rows.push((val, keys));
            }
        }
        Ok(())
    }

    fn finalize(self) -> Result<Value, DbError> {
        match self {
            Self::CountStar { n } => Ok(Value::BigInt(n as i64)),
            Self::CountCol { n } => Ok(Value::BigInt(n as i64)),
            Self::Sum { acc } => Ok(acc.unwrap_or(Value::Null)),
            Self::Min { acc } => Ok(acc.unwrap_or(Value::Null)),
            Self::Max { acc } => Ok(acc.unwrap_or(Value::Null)),
            Self::Avg { sum, count } => finalize_avg(sum, count),
            Self::GroupConcat {
                mut rows,
                separator,
                distinct,
                order_by_dirs,
            } => {
                if rows.is_empty() {
                    return Ok(Value::Null);
                }

                // 1. Sort if ORDER BY keys are present.
                if !order_by_dirs.is_empty() {
                    rows.sort_by(|(_, keys_a), (_, keys_b)| {
                        for (i, &asc) in order_by_dirs.iter().enumerate() {
                            let a = keys_a.get(i).unwrap_or(&Value::Null);
                            let b = keys_b.get(i).unwrap_or(&Value::Null);
                            let cmp = compare_values_null_last_session(a, b);
                            let cmp = if asc { cmp } else { cmp.reverse() };
                            if cmp != std::cmp::Ordering::Equal {
                                return cmp;
                            }
                        }
                        std::cmp::Ordering::Equal
                    });
                }

                // 2. Deduplicate if DISTINCT (preserves sorted order).
                // Uses the session collation so that folded-equal strings
                // (e.g. "José" == "jose" under Es) are treated as duplicates.
                let values: Vec<&str> = if distinct {
                    use crate::eval::current_eval_collation;
                    use crate::text_semantics::canonical_text;
                    let coll = current_eval_collation();
                    let mut seen: std::collections::HashSet<String> =
                        std::collections::HashSet::new();
                    rows.iter()
                        .filter(|(v, _)| seen.insert(canonical_text(coll, v.as_str()).into_owned()))
                        .map(|(v, _)| v.as_str())
                        .collect()
                } else {
                    rows.iter().map(|(v, _)| v.as_str()).collect()
                };

                // 3. Concatenate with separator; truncate at 1 MB (group_concat_max_len).
                const MAX_LEN: usize = 1_048_576;
                let mut result = String::new();
                for (i, val) in values.into_iter().enumerate() {
                    if i > 0 {
                        result.push_str(&separator);
                    }
                    result.push_str(val);
                    if result.len() >= MAX_LEN {
                        result.truncate(MAX_LEN);
                        break;
                    }
                }
                Ok(Value::Text(result))
            }
        }
    }
}

/// Add two numeric values for aggregation.
///
/// Direct match-on-variant arithmetic — no AST nodes, no heap allocation, no
/// `eval()` call. Replaces the old `agg_add` that constructed `Expr::BinaryOp`
/// nodes at runtime. Inspired by PostgreSQL's `nodeAgg` transition-function
/// model and DataFusion's type-specialized accumulator updates.
///
/// Widening rules (same as SQL standard):
/// - `Int  + Int`    → `Int`  (checked; Overflow on wrap)
/// - `Int  + BigInt` → `BigInt` (checked)
/// - `*    + Real`   → `Real`  (lossless widening for aggregation purposes)
/// - `Decimal + Decimal` → `Decimal` (same scale required)
fn value_agg_add(a: Value, b: Value) -> Result<Value, DbError> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x
            .checked_add(y)
            .map(Value::Int)
            .ok_or(DbError::Overflow),
        (Value::BigInt(x), Value::BigInt(y)) => x
            .checked_add(y)
            .map(Value::BigInt)
            .ok_or(DbError::Overflow),
        (Value::Real(x), Value::Real(y)) => Ok(Value::Real(x + y)),
        // Cross-type widening — always produce the wider type.
        (Value::Int(x), Value::BigInt(y)) | (Value::BigInt(y), Value::Int(x)) => (x as i64)
            .checked_add(y)
            .map(Value::BigInt)
            .ok_or(DbError::Overflow),
        (Value::Int(x), Value::Real(y)) | (Value::Real(y), Value::Int(x)) => {
            Ok(Value::Real(x as f64 + y))
        }
        (Value::BigInt(x), Value::Real(y)) | (Value::Real(y), Value::BigInt(x)) => {
            Ok(Value::Real(x as f64 + y))
        }
        (Value::Decimal(m1, s1), Value::Decimal(m2, s2)) if s1 == s2 => m1
            .checked_add(m2)
            .map(|m| Value::Decimal(m, s1))
            .ok_or(DbError::Overflow),
        (a, b) => Err(DbError::TypeMismatch {
            expected: format!("numeric, got {} and {}", a.variant_name(), b.variant_name()),
            got: "incompatible types in SUM/AVG".into(),
        }),
    }
}

/// Finalize AVG: always produces `Real`. Returns `Null` if count == 0.
///
/// Direct division — no `eval()` call, no AST allocation.
fn finalize_avg(sum: Value, count: u64) -> Result<Value, DbError> {
    if count == 0 {
        return Ok(Value::Null);
    }
    let f: f64 = match sum {
        Value::Int(n) => n as f64,
        Value::BigInt(n) => n as f64,
        Value::Real(f) => f,
        Value::Decimal(m, s) => m as f64 * 10f64.powi(-(s as i32)),
        other => {
            return Err(DbError::TypeMismatch {
                expected: "numeric".into(),
                got: other.variant_name().into(),
            })
        }
    };
    Ok(Value::Real(f / count as f64))
}

