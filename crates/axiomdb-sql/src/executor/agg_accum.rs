// ── Accumulator ───────────────────────────────────────────────────────────────

/// Per-group state for a single aggregate expression.
#[derive(Debug)]
enum AggAccumulator {
    /// `COUNT(*)` — increments for every row.
    CountStar { n: u64 },
    /// `COUNT(col)` — increments only for non-NULL values.
    CountCol { n: u64 },
    /// `COUNT(DISTINCT col)` — counts unique non-NULL values (4.9f).
    CountDistinct { seen: std::collections::HashSet<String>, n: u64 },
    /// `SUM(col)` — sum of non-NULL values. `None` = all values were NULL.
    Sum { acc: Option<Value> },
    /// `SUM(DISTINCT col)` — sum of unique non-NULL values (4.9f).
    SumDistinct { values: Vec<Value> },
    /// `MIN(col)` — minimum non-NULL value.
    Min { acc: Option<Value> },
    /// `MAX(col)` — maximum non-NULL value.
    Max { acc: Option<Value> },
    /// `AVG(col)` — running sum + count; final = sum / count as Real.
    Avg { sum: Value, count: u64 },
    /// `AVG(DISTINCT col)` — average of unique non-NULL values (4.9f).
    AvgDistinct { values: Vec<Value> },
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
    /// Phase 11.25b — `jsonb_agg` / `json_agg` / MySQL `JSON_ARRAYAGG`.
    JsonArrayAgg {
        values: Vec<serde_json::Value>,
        returns_jsonb: bool,
    },
    /// Phase 11.25b — `jsonb_object_agg` / `json_object_agg` / MySQL
    /// `JSON_OBJECTAGG`. Stores insertion-ordered key/value pairs; a later
    /// key with the same name overwrites the earlier pair (MySQL semantics;
    /// PG does the same — the last write wins and no error is raised).
    JsonObjectAgg {
        pairs: Vec<(String, serde_json::Value)>,
        returns_jsonb: bool,
    },
    Median {
        values: Vec<f64>,
    },
    /// Phase 20.4 Step 9 — `array_agg(expr [ORDER BY ...] [DISTINCT])`.
    /// Collects values into a SQL array. NULLs are included.
    ArrayAgg {
        /// Accumulated rows: (value, evaluated ORDER BY key values).
        /// NULLs are included in values for PG compatibility.
        rows: Vec<(Value, Vec<Value>)>,
        /// Whether to deduplicate values before building the array.
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
                "count_distinct" => Self::CountDistinct {
                    seen: std::collections::HashSet::new(),
                    n: 0,
                },
                "sum" => Self::Sum { acc: None },
                "sum_distinct" => Self::SumDistinct { values: Vec::new() },
                "min" => Self::Min { acc: None },
                "max" => Self::Max { acc: None },
                "avg" => Self::Avg {
                    sum: Value::Int(0),
                    count: 0,
                },
                "avg_distinct" => Self::AvgDistinct { values: Vec::new() },
                // Phase 11.25b — array aggregates. Simple dispatch by name.
                "jsonb_agg" => Self::JsonArrayAgg {
                    values: Vec::new(),
                    returns_jsonb: true,
                },
                "json_agg" | "json_arrayagg" => Self::JsonArrayAgg {
                    values: Vec::new(),
                    returns_jsonb: false,
                },
                _ => unreachable!("AggAccumulator::new called with non-aggregate"),
            },
            AggExpr::JsonbObjectAgg { returns_jsonb, .. } => Self::JsonObjectAgg {
                pairs: Vec::new(),
                returns_jsonb: *returns_jsonb,
            },
            AggExpr::Median { .. } => Self::Median { values: Vec::new() },
            AggExpr::ArrayAgg { distinct, order_by, .. } => Self::ArrayAgg {
                rows: Vec::new(),
                distinct: *distinct,
                order_by_dirs: order_by
                    .iter()
                    .map(|(_, dir)| matches!(dir, crate::ast::SortOrder::Asc))
                    .collect(),
            },
        }
    }

    fn update(&mut self, row: &[Value], agg: &AggExpr) -> Result<(), DbError> {
        // Extract the argument expression from Simple aggregates.
        let simple_arg = match agg {
            AggExpr::Simple { arg, .. } => arg.as_ref(),
            AggExpr::GroupConcat { .. } => None,
            AggExpr::JsonbObjectAgg { .. } => None,
            AggExpr::Median { .. } => None,
            AggExpr::ArrayAgg { .. } => None,
        };

        // Phase 9.5b: fast-path for simple column refs — avoids eval() overhead.
        #[inline]
        fn fast_eval<'a>(expr: Option<&Expr>, row: &'a [Value]) -> Option<&'a Value> {
            match expr {
                Some(Expr::Column { col_idx, .. }) => row.get(*col_idx),
                _ => None,
            }
        }

        /// Unwraps the aggregate argument, returning a clear error instead of
        /// panicking if the invariant is violated.
        #[inline]
        fn require_arg(arg: Option<&Expr>) -> Result<&Expr, DbError> {
            arg.ok_or_else(|| DbError::Internal {
                message: "aggregate accumulator requires an argument expression".into(),
            })
        }

        match self {
            Self::CountStar { n } => *n += 1,

            Self::CountCol { n } => {
                if let Some(v) = fast_eval(simple_arg, row) {
                    if !matches!(v, Value::Null) {
                        *n += 1;
                    }
                } else {
                    let v = eval(require_arg(simple_arg)?, row)?;
                    if !matches!(v, Value::Null) {
                        *n += 1;
                    }
                }
            }

            Self::CountDistinct { seen, n } => {
                let v = eval(require_arg(simple_arg)?, row)?;
                if !matches!(v, Value::Null) {
                    let key = value_to_display_string(v);
                    if seen.insert(key) {
                        *n += 1;
                    }
                }
            }

            Self::SumDistinct { values } => {
                let v = eval(require_arg(simple_arg)?, row)?;
                if !matches!(v, Value::Null) && !values.iter().any(|e| e == &v) {
                    values.push(v);
                }
            }

            Self::AvgDistinct { values } => {
                let v = eval(require_arg(simple_arg)?, row)?;
                if !matches!(v, Value::Null) && !values.iter().any(|e| e == &v) {
                    values.push(v);
                }
            }

            Self::Sum { acc } => {
                let v = if let Some(v) = fast_eval(simple_arg, row) {
                    v.clone()
                } else {
                    eval(require_arg(simple_arg)?, row)?
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
                    eval(require_arg(simple_arg)?, row)?
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
                    eval(require_arg(simple_arg)?, row)?
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
                    eval(require_arg(simple_arg)?, row)?
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

            Self::JsonArrayAgg { values, .. } => {
                let (val_expr,) = match agg {
                    AggExpr::Simple { arg: Some(e), .. } => (e,),
                    _ => {
                        return Err(DbError::Internal {
                            message: "JsonArrayAgg requires a Simple AggExpr with one argument"
                                .into(),
                        });
                    }
                };
                let v = eval(val_expr, row)?;
                values.push(crate::jsonb_srf::value_to_serde_for_agg(&v)?);
            }

            Self::JsonObjectAgg { pairs, .. } => {
                let (key_expr, value_expr) = match agg {
                    AggExpr::JsonbObjectAgg {
                        key_expr,
                        value_expr,
                        ..
                    } => (key_expr.as_ref(), value_expr.as_ref()),
                    _ => {
                        return Err(DbError::Internal {
                            message: "JsonObjectAgg paired with non-JsonbObjectAgg AggExpr".into(),
                        });
                    }
                };
                let k = eval(key_expr, row)?;
                if matches!(k, Value::Null) {
                    return Err(DbError::InvalidValue {
                        reason: "null key in JSON object aggregate (jsonb_object_agg / \
                                 JSON_OBJECTAGG): keys must be non-null"
                            .into(),
                    });
                }
                let k_str = value_to_display_string(k);
                let v = eval(value_expr, row)?;
                let v_sj = crate::jsonb_srf::value_to_serde_for_agg(&v)?;
                // Last-write-wins on duplicate keys (MySQL + PG agree).
                if let Some(slot) = pairs.iter_mut().find(|(k2, _)| k2 == &k_str) {
                    slot.1 = v_sj;
                } else {
                    pairs.push((k_str, v_sj));
                }
            }
            Self::Median { values } => {
                let val_expr = match agg {
                    AggExpr::Median { arg, .. } => arg,
                    _ => {
                        return Err(DbError::Internal {
                            message: "Median accumulator paired with non-Median AggExpr".into(),
                        });
                    }
                };
                let value = eval(val_expr, row)?;
                match value {
                    Value::Null => {}
                    Value::Int(n) => values.push(n as f64),
                    Value::BigInt(n) => values.push(n as f64),
                    Value::Real(f) => values.push(f),
                    Value::Decimal(mantissa, scale) => {
                        values.push((mantissa as f64) / 10f64.powi(scale as i32));
                    }
                    other => {
                        return Err(DbError::TypeMismatch {
                            expected: "numeric value for median aggregate".into(),
                            got: other.variant_name().into(),
                        });
                    }
                }
            }
            Self::ArrayAgg { rows, .. } => {
                // Extract the ARRAY_AGG expression and ORDER BY from the AggExpr descriptor.
                let (arr_expr, arr_order_by) = match agg {
                    AggExpr::ArrayAgg { arg, order_by, .. } => (arg.as_ref(), order_by),
                    _ => {
                        unreachable!("ArrayAgg accumulator paired with non-ArrayAgg AggExpr")
                    }
                };

                // Evaluate the aggregated expression (NULLs are included for PG compatibility).
                let val = eval(arr_expr, row)?;

                // Evaluate ORDER BY key expressions for this row.
                let keys: Vec<Value> = arr_order_by
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
            Self::CountDistinct { n, .. } => Ok(Value::BigInt(n as i64)),
            Self::Sum { acc } => Ok(acc.unwrap_or(Value::Null)),
            Self::SumDistinct { values } => {
                let mut acc: Option<Value> = None;
                for v in values {
                    acc = Some(match acc {
                        None => v,
                        Some(a) => value_agg_add(a, v)?,
                    });
                }
                Ok(acc.unwrap_or(Value::Null))
            }
            Self::Min { acc } => Ok(acc.unwrap_or(Value::Null)),
            Self::Max { acc } => Ok(acc.unwrap_or(Value::Null)),
            Self::Avg { sum, count } => finalize_avg(sum, count),
            Self::AvgDistinct { values } => {
                let count = values.len() as u64;
                if count == 0 {
                    return Ok(Value::Null);
                }
                let mut sum = Value::Int(0);
                for v in values {
                    sum = value_agg_add(sum, v)?;
                }
                finalize_avg(sum, count)
            }
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

            Self::JsonArrayAgg {
                values,
                returns_jsonb,
            } => {
                let arr = serde_json::Value::Array(values);
                if returns_jsonb {
                    let blob = axiomdb_types::jsonb::JsonbEncoder::encode(&arr)?;
                    Ok(Value::Jsonb(std::sync::Arc::new(blob)))
                } else {
                    Ok(Value::Json(arr.to_string()))
                }
            }

            Self::JsonObjectAgg {
                pairs,
                returns_jsonb,
            } => {
                // Preserve insertion order; JSON objects are logically
                // unordered but both PG and MySQL emit in insertion order.
                let mut obj = serde_json::Map::with_capacity(pairs.len());
                for (k, v) in pairs {
                    obj.insert(k, v);
                }
                let val = serde_json::Value::Object(obj);
                if returns_jsonb {
                    let blob = axiomdb_types::jsonb::JsonbEncoder::encode(&val)?;
                    Ok(Value::Jsonb(std::sync::Arc::new(blob)))
                } else {
                    Ok(Value::Json(val.to_string()))
                }
            }
            Self::Median { mut values } => {
                if values.is_empty() {
                    return Ok(Value::Null);
                }
                values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let mid = values.len() / 2;
                if values.len() % 2 == 1 {
                    Ok(Value::Real(values[mid]))
                } else {
                    Ok(Value::Real((values[mid - 1] + values[mid]) / 2.0))
                }
            }
            Self::ArrayAgg {
                mut rows,
                distinct,
                order_by_dirs,
            } => {
                // PostgreSQL semantics: empty group returns NULL, not empty array.
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
                // For ArrayAgg, deduplication uses Value equality.
                let values: Vec<Value> = if distinct {
                    let mut seen: std::collections::HashSet<String> =
                        std::collections::HashSet::new();
                    rows.into_iter()
                        .filter(|(v, _)| {
                            let key = value_to_display_string(v.clone());
                            seen.insert(key)
                        })
                        .map(|(v, _)| v)
                        .collect()
                } else {
                    rows.into_iter().map(|(v, _)| v).collect()
                };

                // 3. Build and return the array value.
                // The encoding to blob happens at storage time, not here.
                Ok(Value::Array(values))
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
