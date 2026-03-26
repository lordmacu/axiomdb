fn is_aggregate(name: &str) -> bool {
    matches!(name, "count" | "sum" | "min" | "max" | "avg")
}

/// Returns `true` if `expr` or any sub-expression is an aggregate call.
fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        // GROUP_CONCAT is always an aggregate — detected via the dedicated AST variant.
        Expr::GroupConcat { .. } => true,
        Expr::Function { name, .. } if is_aggregate(name.as_str()) => true,
        Expr::BinaryOp { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::UnaryOp { operand, .. } => contains_aggregate(operand),
        Expr::IsNull { expr, .. } => contains_aggregate(expr),
        Expr::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        Expr::Like { expr, pattern, .. } => contains_aggregate(expr) || contains_aggregate(pattern),
        Expr::In { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        Expr::Function { args, .. } => args.iter().any(contains_aggregate),
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            operand.as_deref().is_some_and(contains_aggregate)
                || when_thens
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_result.as_deref().is_some_and(contains_aggregate)
        }
        Expr::Cast { expr, .. } => contains_aggregate(expr),
        Expr::Literal(_) | Expr::Column { .. } | Expr::OuterColumn { .. } | Expr::Param { .. } => {
            false
        }
        // Subquery internals are analyzed independently; aggregates inside them
        // do not count as aggregates of the outer query.
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => false,
    }
}

/// Returns `true` if the SELECT list or HAVING clause contain any aggregate call.
fn has_aggregates(items: &[SelectItem], having: &Option<Expr>) -> bool {
    let in_select = items.iter().any(|item| match item {
        SelectItem::Expr { expr, .. } => contains_aggregate(expr),
        _ => false,
    });
    let in_having = having.as_ref().is_some_and(contains_aggregate);
    in_select || in_having
}

// ── Aggregate descriptor ──────────────────────────────────────────────────────

/// Descriptor for one aggregate expression in the query.
///
/// Collected from the SELECT list and HAVING clause before the scan loop.
/// Deduplicated: if `COUNT(*)` appears in both SELECT and HAVING, only one
/// `AggExpr` is created and both share the same accumulator index.
#[derive(Debug, Clone)]
enum AggExpr {
    /// Standard aggregate: COUNT, SUM, MIN, MAX, AVG.
    Simple {
        /// Lowercase function name: "count", "sum", "min", "max", "avg".
        name: String,
        /// The argument expression. `None` for `COUNT(*)`.
        arg: Option<Expr>,
        /// Position in `GroupState::accumulators`. Preserved for diagnostics.
        #[allow(dead_code)]
        agg_idx: usize,
    },
    /// GROUP_CONCAT / string_agg aggregate.
    GroupConcat {
        /// Expression to evaluate and concatenate per row.
        expr: Box<Expr>,
        /// If true, deduplicate values before concatenating.
        distinct: bool,
        /// Per-aggregate ORDER BY: (sort_expr, direction) pairs.
        order_by: Vec<(Expr, crate::ast::SortOrder)>,
        /// Separator string (default `","`).
        separator: String,
        /// Position in `GroupState::accumulators`. Preserved for diagnostics.
        #[allow(dead_code)]
        agg_idx: usize,
    },
}

impl AggExpr {
    /// Returns the accumulator index for this aggregate.
    #[allow(dead_code)]
    fn agg_idx(&self) -> usize {
        match self {
            Self::Simple { agg_idx, .. } | Self::GroupConcat { agg_idx, .. } => *agg_idx,
        }
    }

    /// Returns `true` if this descriptor matches the given simple function call.
    fn matches_simple(&self, name: &str, args: &[Expr]) -> bool {
        match self {
            Self::Simple { name: n, arg, .. } => {
                if n != name {
                    return false;
                }
                match (arg, args.first()) {
                    // Both COUNT(*): arg = None, args is empty
                    (None, None) => args.is_empty(),
                    // Both have an argument — compare by col_idx if both are Column refs
                    (
                        Some(Expr::Column { col_idx: a, .. }),
                        Some(Expr::Column { col_idx: b, .. }),
                    ) => a == b,
                    _ => false,
                }
            }
            Self::GroupConcat { .. } => false,
        }
    }

    /// Returns `true` if this descriptor matches the given GROUP_CONCAT call.
    fn matches_group_concat(
        &self,
        gc_expr: &Expr,
        distinct: bool,
        order_by: &[(Expr, crate::ast::SortOrder)],
        separator: &str,
    ) -> bool {
        match self {
            Self::GroupConcat {
                expr,
                distinct: d,
                order_by: ob,
                separator: sep,
                ..
            } => {
                expr.as_ref() == gc_expr
                    && *d == distinct
                    && ob == order_by
                    && sep.as_str() == separator
            }
            Self::Simple { .. } => false,
        }
    }
}

/// Walks `expr` and registers any aggregate function calls into `result`.
fn collect_agg_exprs_from(expr: &Expr, result: &mut Vec<AggExpr>) {
    match expr {
        // GROUP_CONCAT: register as GroupConcat AggExpr and deduplicate.
        // Do NOT recurse into `gc_expr` itself (it IS the aggregate root).
        // Only recurse into ORDER BY sub-exprs (they could contain subqueries, etc.).
        Expr::GroupConcat {
            expr: gc_expr,
            distinct,
            order_by,
            separator,
        } => {
            let already = result
                .iter()
                .any(|ae| ae.matches_group_concat(gc_expr, *distinct, order_by, separator));
            if !already {
                let idx = result.len();
                result.push(AggExpr::GroupConcat {
                    expr: gc_expr.clone(),
                    distinct: *distinct,
                    order_by: order_by.clone(),
                    separator: separator.clone(),
                    agg_idx: idx,
                });
            }
            for (e, _) in order_by {
                collect_agg_exprs_from(e, result);
            }
        }
        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            let arg = args.first().cloned();
            // Deduplicate: only add if not already registered.
            let already = result
                .iter()
                .any(|ae| ae.matches_simple(name.as_str(), args));
            if !already {
                let idx = result.len();
                result.push(AggExpr::Simple {
                    name: name.clone(),
                    arg,
                    agg_idx: idx,
                });
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_agg_exprs_from(left, result);
            collect_agg_exprs_from(right, result);
        }
        Expr::UnaryOp { operand, .. } => collect_agg_exprs_from(operand, result),
        Expr::IsNull { expr, .. } => collect_agg_exprs_from(expr, result),
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_agg_exprs_from(expr, result);
            collect_agg_exprs_from(low, result);
            collect_agg_exprs_from(high, result);
        }
        Expr::In { expr, list, .. } => {
            collect_agg_exprs_from(expr, result);
            for e in list {
                collect_agg_exprs_from(e, result);
            }
        }
        Expr::Function { args, .. } => {
            for a in args {
                collect_agg_exprs_from(a, result);
            }
        }
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            if let Some(op) = operand {
                collect_agg_exprs_from(op, result);
            }
            for (w, t) in when_thens {
                collect_agg_exprs_from(w, result);
                collect_agg_exprs_from(t, result);
            }
            if let Some(e) = else_result {
                collect_agg_exprs_from(e, result);
            }
        }
        Expr::Cast { expr, .. } => collect_agg_exprs_from(expr, result),
        Expr::Like { expr, pattern, .. } => {
            collect_agg_exprs_from(expr, result);
            collect_agg_exprs_from(pattern, result);
        }
        Expr::Literal(_) | Expr::Column { .. } | Expr::OuterColumn { .. } | Expr::Param { .. } => {}
        // Aggregates inside a subquery belong to the inner query, not the outer.
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => {}
    }
}

/// Builds the deduplicated list of aggregate expressions from SELECT + HAVING.
fn collect_agg_exprs(items: &[SelectItem], having: &Option<Expr>) -> Vec<AggExpr> {
    let mut result = Vec::new();
    for item in items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_agg_exprs_from(expr, &mut result);
        }
    }
    if let Some(h) = having {
        collect_agg_exprs_from(h, &mut result);
    }
    result
}

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

        match self {
            Self::CountStar { n } => *n += 1,

            Self::CountCol { n } => {
                let v = eval(simple_arg.unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *n += 1;
                }
            }

            Self::Sum { acc } => {
                let v = eval(simple_arg.unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v,
                        Some(a) => agg_add(a, v)?,
                    });
                }
            }

            Self::Min { acc } => {
                let v = eval(simple_arg.unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v.clone(),
                        Some(a) => {
                            if agg_compare(&v, &a)? == std::cmp::Ordering::Less {
                                v
                            } else {
                                a
                            }
                        }
                    });
                }
            }

            Self::Max { acc } => {
                let v = eval(simple_arg.unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *acc = Some(match acc.take() {
                        None => v.clone(),
                        Some(a) => {
                            if agg_compare(&v, &a)? == std::cmp::Ordering::Greater {
                                v
                            } else {
                                a
                            }
                        }
                    });
                }
            }

            Self::Avg { sum, count } => {
                let v = eval(simple_arg.unwrap(), row)?;
                if !matches!(v, Value::Null) {
                    *sum = agg_add(sum.clone(), v)?;
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

/// Add two values for aggregation (reuses `eval` for type handling and coercion).
fn agg_add(a: Value, b: Value) -> Result<Value, DbError> {
    eval(
        &Expr::BinaryOp {
            op: BinaryOp::Add,
            left: Box::new(Expr::Literal(a)),
            right: Box::new(Expr::Literal(b)),
        },
        &[],
    )
}

/// Compare two values for MIN/MAX (returns Ordering).
fn agg_compare(a: &Value, b: &Value) -> Result<std::cmp::Ordering, DbError> {
    // Delegate to eval: if a < b → Less, if a = b → Equal, else Greater.
    let lt = eval(
        &Expr::BinaryOp {
            op: BinaryOp::Lt,
            left: Box::new(Expr::Literal(a.clone())),
            right: Box::new(Expr::Literal(b.clone())),
        },
        &[],
    )?;
    if is_truthy(&lt) {
        return Ok(std::cmp::Ordering::Less);
    }
    let eq = eval(
        &Expr::BinaryOp {
            op: BinaryOp::Eq,
            left: Box::new(Expr::Literal(a.clone())),
            right: Box::new(Expr::Literal(b.clone())),
        },
        &[],
    )?;
    if is_truthy(&eq) {
        Ok(std::cmp::Ordering::Equal)
    } else {
        Ok(std::cmp::Ordering::Greater)
    }
}

/// Finalize AVG: always produces `Real`. Returns `Null` if count == 0.
fn finalize_avg(sum: Value, count: u64) -> Result<Value, DbError> {
    if count == 0 {
        return Ok(Value::Null);
    }
    // Convert sum to Real.
    let sum_real = match sum {
        Value::Int(n) => Value::Real(n as f64),
        Value::BigInt(n) => Value::Real(n as f64),
        Value::Real(f) => Value::Real(f),
        Value::Decimal(m, s) => Value::Real(m as f64 * 10f64.powi(-(s as i32))),
        other => {
            return Err(DbError::TypeMismatch {
                expected: "numeric".into(),
                got: other.variant_name().into(),
            })
        }
    };
    eval(
        &Expr::BinaryOp {
            op: BinaryOp::Div,
            left: Box::new(Expr::Literal(sum_real)),
            right: Box::new(Expr::Literal(Value::Real(count as f64))),
        },
        &[],
    )
}

// ── GROUP_CONCAT helpers ──────────────────────────────────────────────────────

/// Converts a non-NULL `Value` to its text representation for GROUP_CONCAT.
///
/// Mirrors MySQL's `val_str()` coercion rules:
/// - `Text` → unchanged
/// - `Int`/`BigInt` → decimal representation
/// - `Real` → Rust default float formatting
/// - `Bool` → `"1"` (true) or `"0"` (false) — MySQL behavior
/// - Others → debug representation (fallback; should not occur in practice)
fn value_to_display_string(v: Value) -> String {
    match v {
        Value::Text(s) => s,
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Real(f) => format!("{f}"),
        Value::Bool(b) => {
            if b {
                "1".into()
            } else {
                "0".into()
            }
        }
        Value::Null => String::new(), // should not be reached (callers skip NULLs)
        other => format!("{other:?}"),
    }
}

/// Compares two `Value`s for ORDER BY inside GROUP_CONCAT.
///
/// Uses proper type-aware comparison:
/// - `NULL` sorts last (greater than any non-NULL), matching MySQL behavior.
/// - Numeric types compared numerically.
/// - `Text` compared lexicographically (not by length).
/// - Other types fall back to `value_to_key_bytes` for a stable total order.
fn compare_values_null_last(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        // Numeric types — proper numeric ordering.
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::BigInt(x), Value::BigInt(y)) => x.cmp(y),
        (Value::Int(x), Value::BigInt(y)) => (*x as i64).cmp(y),
        (Value::BigInt(x), Value::Int(y)) => x.cmp(&(*y as i64)),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        // Text — lexicographic (not length-prefixed).
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        // All other types — stable fallback via key-bytes.
        _ => value_to_key_bytes(a).cmp(&value_to_key_bytes(b)),
    }
}

/// Session-aware version of [`compare_values_null_last`].
///
/// For `Text` values, uses the active thread-local session collation (set by
/// [`CollationGuard`]) instead of binary ordering. Used in GROUP_CONCAT ORDER BY.
fn compare_values_null_last_session(a: &Value, b: &Value) -> std::cmp::Ordering {
    use crate::eval::current_eval_collation;
    match (a, b) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::BigInt(x), Value::BigInt(y)) => x.cmp(y),
        (Value::Int(x), Value::BigInt(y)) => (*x as i64).cmp(y),
        (Value::BigInt(x), Value::Int(y)) => x.cmp(&(*y as i64)),
        (Value::Real(x), Value::Real(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        (Value::Text(x), Value::Text(y)) => compare_text(current_eval_collation(), x, y),
        _ => value_to_key_bytes(a).cmp(&value_to_key_bytes(b)),
    }
}

/// Session-aware serialization for GROUP BY hash keys and DISTINCT deduplication.
///
/// For `Text` values, uses the canonical fold from the active thread-local
/// session collation so that `jose` and `José` map to the same group key under `Es`.
/// All non-text types use the binary serialization unchanged.
fn value_to_session_key_bytes(v: &Value) -> Vec<u8> {
    use crate::eval::current_eval_collation;
    use crate::text_semantics::canonical_text;
    let coll = current_eval_collation();
    if coll == SessionCollation::Binary {
        return value_to_key_bytes(v);
    }
    let mut buf = Vec::new();
    match v {
        Value::Text(s) => {
            let key = canonical_text(coll, s);
            buf.push(0x06);
            buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
            buf.extend_from_slice(key.as_bytes());
        }
        other => return value_to_key_bytes(other),
    }
    buf
}

/// Session-aware DISTINCT deduplication.
///
/// Uses [`value_to_session_key_bytes`] so that folded-equal text strings are
/// treated as duplicates under `Es` session collation.
fn apply_distinct_with_session(rows: Vec<Row>) -> Vec<Row> {
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    rows.into_iter()
        .filter(|row| {
            let key: Vec<u8> = row.iter().flat_map(value_to_session_key_bytes).collect();
            seen.insert(key)
        })
        .collect()
}

// ── GroupState ────────────────────────────────────────────────────────────────

/// State for one GROUP BY group.
struct GroupState {
    /// Evaluated GROUP BY expression values (for future sort-based output — 4.9b).
    #[allow(dead_code)]
    key_values: Vec<Value>,
    /// One source row from this group — used by HAVING/SELECT to resolve column refs.
    representative_row: Row,
    /// One accumulator per aggregate in the query (SELECT + HAVING).
    accumulators: Vec<AggAccumulator>,
}

// ── GROUP BY key hashing ──────────────────────────────────────────────────────

/// Serializes a `Value` to a self-describing byte sequence for use as a
/// GROUP BY hash key.
///
/// Properties:
/// - Two `NULL` values produce identical bytes `[0x00]` → they form one group
///   (SQL grouping semantics: NULLs are considered equal for GROUP BY).
/// - `Real(f64)` uses `to_bits()` for bit-exact representation. `NaN` would
///   produce a fixed bit pattern, but NaN is forbidden in stored values.
/// - The tag byte guarantees values of different types never collide.
fn value_to_key_bytes(v: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    match v {
        Value::Null => buf.push(0x00),
        Value::Bool(b) => {
            buf.push(0x01);
            buf.push(*b as u8);
        }
        Value::Int(n) => {
            buf.push(0x02);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::BigInt(n) => {
            buf.push(0x03);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        Value::Real(f) => {
            buf.push(0x04);
            buf.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Value::Decimal(m, s) => {
            buf.push(0x05);
            buf.extend_from_slice(&m.to_le_bytes());
            buf.push(*s);
        }
        Value::Text(s) => {
            buf.push(0x06);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            buf.push(0x07);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Date(d) => {
            buf.push(0x08);
            buf.extend_from_slice(&d.to_le_bytes());
        }
        Value::Timestamp(t) => {
            buf.push(0x09);
            buf.extend_from_slice(&t.to_le_bytes());
        }
        Value::Uuid(u) => {
            buf.push(0x0A);
            buf.extend_from_slice(u.as_slice());
        }
    }
    buf
}

/// Session-aware GROUP BY key serialization.
///
/// Uses [`value_to_session_key_bytes`] so that text values are canonicalized
/// according to the active session collation (e.g. `es` folds `José` = `jose`).
fn group_key_bytes_session(key_values: &[Value]) -> Vec<u8> {
    key_values
        .iter()
        .flat_map(value_to_session_key_bytes)
        .collect()
}

// ── HAVING evaluator ──────────────────────────────────────────────────────────

/// Evaluates a HAVING expression against a finalized group.
///
/// `Expr::Column` references are evaluated against `representative_row`
/// (the original source row, so `col_idx` values from the analyzer are valid).
///
/// `Expr::Function` aggregate calls are looked up in `agg_values` by name + arg.
///
/// All other expressions are evaluated by delegating sub-expression results to
/// the standard `eval()` via synthetic `Expr::Literal` nodes.
fn eval_with_aggs(
    expr: &Expr,
    representative_row: &[Value],
    agg_values: &[Value],
    agg_exprs: &[AggExpr],
) -> Result<Value, DbError> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),

        Expr::Column { col_idx, .. } => {
            representative_row
                .get(*col_idx)
                .cloned()
                .ok_or(DbError::ColumnIndexOutOfBounds {
                    idx: *col_idx,
                    len: representative_row.len(),
                })
        }

        // GROUP_CONCAT: look up the pre-computed finalized value by structural match.
        Expr::GroupConcat {
            expr: gc_expr,
            distinct,
            order_by,
            separator,
        } => {
            let idx = agg_exprs
                .iter()
                .position(|ae| ae.matches_group_concat(gc_expr, *distinct, order_by, separator))
                .ok_or_else(|| {
                    DbError::Other("GROUP_CONCAT not pre-registered — internal error".to_string())
                })?;
            Ok(agg_values[idx].clone())
        }

        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            let idx = agg_exprs
                .iter()
                .position(|ae| ae.matches_simple(name.as_str(), args))
                .ok_or_else(|| {
                    DbError::Other(format!(
                        "aggregate '{name}' not pre-registered — internal error"
                    ))
                })?;
            Ok(agg_values[idx].clone())
        }

        // AND: short-circuit
        Expr::BinaryOp {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let l = eval_with_aggs(left, representative_row, agg_values, agg_exprs)?;
            match l {
                Value::Bool(false) => Ok(Value::Bool(false)),
                Value::Bool(true) => {
                    eval_with_aggs(right, representative_row, agg_values, agg_exprs)
                }
                Value::Null => {
                    let r = eval_with_aggs(right, representative_row, agg_values, agg_exprs)?;
                    Ok(if matches!(r, Value::Bool(false)) {
                        Value::Bool(false)
                    } else {
                        Value::Null
                    })
                }
                other => Err(DbError::TypeMismatch {
                    expected: "Bool".into(),
                    got: other.variant_name().into(),
                }),
            }
        }

        // OR: short-circuit
        Expr::BinaryOp {
            op: BinaryOp::Or,
            left,
            right,
        } => {
            let l = eval_with_aggs(left, representative_row, agg_values, agg_exprs)?;
            match l {
                Value::Bool(true) => Ok(Value::Bool(true)),
                Value::Bool(false) => {
                    eval_with_aggs(right, representative_row, agg_values, agg_exprs)
                }
                Value::Null => {
                    let r = eval_with_aggs(right, representative_row, agg_values, agg_exprs)?;
                    Ok(if matches!(r, Value::Bool(true)) {
                        Value::Bool(true)
                    } else {
                        Value::Null
                    })
                }
                other => Err(DbError::TypeMismatch {
                    expected: "Bool".into(),
                    got: other.variant_name().into(),
                }),
            }
        }

        // Other binary ops: evaluate both sides, delegate to eval() via Literal.
        Expr::BinaryOp { op, left, right } => {
            let l = eval_with_aggs(left, representative_row, agg_values, agg_exprs)?;
            let r = eval_with_aggs(right, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::BinaryOp {
                    op: *op,
                    left: Box::new(Expr::Literal(l)),
                    right: Box::new(Expr::Literal(r)),
                },
                &[],
            )
        }

        Expr::UnaryOp { op, operand } => {
            let v = eval_with_aggs(operand, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::UnaryOp {
                    op: *op,
                    operand: Box::new(Expr::Literal(v)),
                },
                &[],
            )
        }

        Expr::IsNull { expr, negated } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::IsNull {
                    expr: Box::new(Expr::Literal(v)),
                    negated: *negated,
                },
                &[],
            )
        }

        // CASE WHEN in HAVING context — recurse through eval_with_aggs for sub-exprs.
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => {
            match operand {
                None => {
                    for (when_expr, then_expr) in when_thens {
                        let cond =
                            eval_with_aggs(when_expr, representative_row, agg_values, agg_exprs)?;
                        if is_truthy(&cond) {
                            return eval_with_aggs(
                                then_expr,
                                representative_row,
                                agg_values,
                                agg_exprs,
                            );
                        }
                    }
                }
                Some(base_expr) => {
                    let base_val =
                        eval_with_aggs(base_expr, representative_row, agg_values, agg_exprs)?;
                    for (val_expr, then_expr) in when_thens {
                        let val =
                            eval_with_aggs(val_expr, representative_row, agg_values, agg_exprs)?;
                        let eq = eval(
                            &Expr::BinaryOp {
                                op: BinaryOp::Eq,
                                left: Box::new(Expr::Literal(base_val.clone())),
                                right: Box::new(Expr::Literal(val)),
                            },
                            &[],
                        )?;
                        if is_truthy(&eq) {
                            return eval_with_aggs(
                                then_expr,
                                representative_row,
                                agg_values,
                                agg_exprs,
                            );
                        }
                    }
                }
            }
            match else_result {
                Some(else_expr) => {
                    eval_with_aggs(else_expr, representative_row, agg_values, agg_exprs)
                }
                None => Ok(Value::Null),
            }
        }

        // Compound predicates that may contain aggregates in sub-expressions.
        Expr::Like {
            expr,
            pattern,
            negated,
        } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            let p = eval_with_aggs(pattern, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::Like {
                    expr: Box::new(Expr::Literal(v)),
                    pattern: Box::new(Expr::Literal(p)),
                    negated: *negated,
                },
                &[],
            )
        }

        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            let lo = eval_with_aggs(low, representative_row, agg_values, agg_exprs)?;
            let hi = eval_with_aggs(high, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::Between {
                    expr: Box::new(Expr::Literal(v)),
                    low: Box::new(Expr::Literal(lo)),
                    high: Box::new(Expr::Literal(hi)),
                    negated: *negated,
                },
                &[],
            )
        }

        Expr::In {
            expr,
            list,
            negated,
        } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            let evaluated_list: Result<Vec<Expr>, _> = list
                .iter()
                .map(|e| {
                    eval_with_aggs(e, representative_row, agg_values, agg_exprs).map(Expr::Literal)
                })
                .collect();
            eval(
                &Expr::In {
                    expr: Box::new(Expr::Literal(v)),
                    list: evaluated_list?,
                    negated: *negated,
                },
                &[],
            )
        }

        Expr::Cast { expr, target } => {
            let v = eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?;
            eval(
                &Expr::Cast {
                    expr: Box::new(Expr::Literal(v)),
                    target: *target,
                },
                &[],
            )
        }

        // For remaining variants: fall back to standard eval against representative_row.
        other => eval(other, representative_row),
    }
}

// ── execute_select_grouped ────────────────────────────────────────────────────

// ── GROUP BY strategy ────────────────────────────────────────────────────────

/// Controls which GROUP BY execution algorithm is used.
#[derive(Debug, Clone, Copy)]
enum GroupByStrategy {
    /// Default: one-pass hash aggregation (always correct, no ordering required).
    Hash,
    /// Stream adjacent equal groups from an already-ordered input.
    ///
    /// `presorted = true`  → caller guarantees input is in group-key order.
    /// `presorted = false` → executor sorts the input by group keys first.
    Sorted { presorted: bool },
}

/// Collation-aware GROUP BY strategy selection.
///
/// When the effective session collation is non-binary AND any GROUP BY expression
/// references a TEXT column, the presorted strategy must be rejected because the
/// index uses binary key order while the session uses a different text ordering.
///
/// `columns` should be the resolved columns of the FROM table; pass `&[]` when
/// they are unavailable (conservative: binary GROUP BY path is still available).
fn choose_group_by_strategy_ctx_with_collation(
    group_by: &[Expr],
    access_method: &crate::planner::AccessMethod,
    collation: SessionCollation,
    columns: &[axiomdb_catalog::schema::ColumnDef],
) -> GroupByStrategy {
    if group_by.is_empty() {
        return GroupByStrategy::Hash;
    }

    // Safety gate: if collation is non-binary and any GROUP BY key is a TEXT
    // column, the index-ordered GROUP BY would produce wrong groupings.
    if collation != SessionCollation::Binary && !columns.is_empty() {
        let has_text_key = group_by.iter().any(|expr| {
            if let Expr::Column { col_idx, .. } = expr {
                columns
                    .get(*col_idx)
                    .map(|col| col.col_type == axiomdb_catalog::schema::ColumnType::Text)
                    .unwrap_or(false)
            } else {
                false
            }
        });
        if has_text_key {
            return GroupByStrategy::Hash;
        }
    }

    let index_def = match access_method {
        crate::planner::AccessMethod::IndexLookup { index_def, .. }
        | crate::planner::AccessMethod::IndexRange { index_def, .. }
        | crate::planner::AccessMethod::IndexOnlyScan { index_def, .. } => index_def,
        crate::planner::AccessMethod::Scan => return GroupByStrategy::Hash,
    };

    if group_by_matches_index_prefix(group_by, index_def) {
        GroupByStrategy::Sorted { presorted: true }
    } else {
        GroupByStrategy::Hash
    }
}

/// Returns `true` iff every element of `group_by` is a plain `Expr::Column`
/// whose `col_idx` matches the corresponding leading column of `index_def`,
/// in the same order, without gaps.
fn group_by_matches_index_prefix(group_by: &[Expr], index_def: &IndexDef) -> bool {
    if group_by.len() > index_def.columns.len() {
        return false;
    }
    for (gb_expr, idx_col) in group_by.iter().zip(&index_def.columns) {
        match gb_expr {
            Expr::Column { col_idx, .. } => {
                if *col_idx as u16 != idx_col.col_idx {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Compare two group-key value lists lexicographically, NULL last.
fn compare_group_key_lists(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = compare_values_null_last(x, y);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// Returns `true` iff `a` and `b` are considered the same GROUP BY group.
///
/// NULL == NULL for grouping purposes (matches SQL GROUP BY semantics).
fn group_keys_equal(a: &[Value], b: &[Value]) -> bool {
    compare_group_key_lists(a, b) == std::cmp::Ordering::Equal
}

// ── Grouped executor entry point ─────────────────────────────────────────────

/// Executes the GROUP BY + aggregation path.
///
/// `combined_rows` are the post-scan, post-WHERE rows (not yet projected).
/// `strategy` controls whether hash or sorted streaming aggregation is used.
fn execute_select_grouped(
    stmt: SelectStmt,
    combined_rows: Vec<Row>,
    strategy: GroupByStrategy,
) -> Result<QueryResult, DbError> {
    match strategy {
        GroupByStrategy::Hash => execute_select_grouped_hash(stmt, combined_rows),
        GroupByStrategy::Sorted { presorted } => {
            execute_select_grouped_sorted(stmt, combined_rows, presorted)
        }
    }
}

// ── Hash aggregation (original 4.9a implementation) ──────────────────────────

fn execute_select_grouped_hash(
    stmt: SelectStmt,
    combined_rows: Vec<Row>,
) -> Result<QueryResult, DbError> {
    // Build aggregate registry.
    let agg_exprs = collect_agg_exprs(&stmt.columns, &stmt.having);

    // One-pass hash aggregation.
    let mut groups: HashMap<Vec<u8>, GroupState> = HashMap::new();

    for row in &combined_rows {
        // Evaluate GROUP BY expressions → key values.
        let key_values: Vec<Value> = stmt
            .group_by
            .iter()
            .map(|e| eval(e, row))
            .collect::<Result<_, _>>()?;

        // Session-aware: folds text under Es so "José" and "jose" share a group.
        let key_bytes = group_key_bytes_session(&key_values);

        let state = groups.entry(key_bytes).or_insert_with(|| GroupState {
            key_values: key_values.clone(),
            representative_row: row.clone(),
            accumulators: agg_exprs.iter().map(AggAccumulator::new).collect(),
        });

        // Update each accumulator.
        for (acc, agg) in state.accumulators.iter_mut().zip(&agg_exprs) {
            acc.update(row, agg)?;
        }
    }

    // Ungrouped aggregate: if no GROUP BY and no rows, still emit one output group.
    // (e.g., SELECT COUNT(*) FROM empty_table → returns (0), not 0 rows)
    if stmt.group_by.is_empty() && groups.is_empty() {
        groups.insert(
            vec![],
            GroupState {
                key_values: vec![],
                representative_row: vec![],
                accumulators: agg_exprs.iter().map(AggAccumulator::new).collect(),
            },
        );
    }

    // Build output column metadata.
    let out_cols = build_grouped_column_meta(&stmt.columns, &agg_exprs)?;

    // Finalize, HAVING filter, project.
    let mut rows: Vec<Row> = Vec::new();
    for (_, state) in groups {
        let agg_values: Vec<Value> = state
            .accumulators
            .into_iter()
            .map(|acc| acc.finalize())
            .collect::<Result<_, _>>()?;

        if let Some(ref having) = stmt.having {
            let v = eval_with_aggs(having, &state.representative_row, &agg_values, &agg_exprs)?;
            if !is_truthy(&v) {
                continue;
            }
        }

        let out_row = project_grouped_row(
            &stmt.columns,
            &state.representative_row,
            &agg_values,
            &agg_exprs,
        )?;
        rows.push(out_row);
    }

    if stmt.distinct {
        rows = apply_distinct_with_session(rows);
    }
    let remapped_ob = remap_order_by_for_grouped(&stmt.order_by, &stmt.columns);
    rows = apply_order_by(rows, &remapped_ob)?;
    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

// ── Sorted streaming aggregation (4.9b) ──────────────────────────────────────

/// Sorted streaming GROUP BY.
///
/// When `presorted = true`, input rows are already in group-key order
/// (guaranteed by the B-Tree access method). Groups are formed by streaming
/// adjacent equal-key rows without building any hash table.
///
/// When `presorted = false`, the input is sorted by group keys first, then
/// streamed. This path is not auto-selected in 4.9b but is available for
/// testing and future use.
fn execute_select_grouped_sorted(
    stmt: SelectStmt,
    mut combined_rows: Vec<Row>,
    presorted: bool,
) -> Result<QueryResult, DbError> {
    let agg_exprs = collect_agg_exprs(&stmt.columns, &stmt.having);
    let out_cols = build_grouped_column_meta(&stmt.columns, &agg_exprs)?;

    // Evaluate GROUP BY expressions for every row up front.
    // This avoids re-evaluating the same expressions during boundary detection.
    struct KeyedRow {
        row: Row,
        key_values: Vec<Value>,
    }
    let mut keyed: Vec<KeyedRow> = combined_rows
        .drain(..)
        .map(|row| {
            let key_values: Vec<Value> = stmt
                .group_by
                .iter()
                .map(|e| eval(e, &row))
                .collect::<Result<_, _>>()?;
            Ok(KeyedRow { row, key_values })
        })
        .collect::<Result<Vec<_>, DbError>>()?;

    if !presorted {
        // Stable sort by group keys — NULL last, same as hash path output order.
        keyed.sort_by(|a, b| compare_group_key_lists(&a.key_values, &b.key_values));
    }

    // Stream adjacent equal groups.
    let mut output_rows: Vec<Row> = Vec::new();

    if keyed.is_empty() {
        // Ungrouped aggregate on empty input: emit one row (e.g., COUNT(*) → 0).
        if stmt.group_by.is_empty() {
            let accumulators: Vec<AggAccumulator> =
                agg_exprs.iter().map(AggAccumulator::new).collect();
            let agg_values: Vec<Value> = accumulators
                .into_iter()
                .map(|acc| acc.finalize())
                .collect::<Result<_, _>>()?;
            let out_row = project_grouped_row(&stmt.columns, &[], &agg_values, &agg_exprs)?;
            output_rows.push(out_row);
        }
    } else {
        // Initialize first group.
        let first = &keyed[0];
        let mut current_key = first.key_values.clone();
        let mut representative_row = first.row.clone();
        let mut accumulators: Vec<AggAccumulator> =
            agg_exprs.iter().map(AggAccumulator::new).collect();
        for (acc, agg) in accumulators.iter_mut().zip(&agg_exprs) {
            acc.update(&first.row, agg)?;
        }

        for kr in &keyed[1..] {
            if group_keys_equal(&current_key, &kr.key_values) {
                // Same group — accumulate.
                for (acc, agg) in accumulators.iter_mut().zip(&agg_exprs) {
                    acc.update(&kr.row, agg)?;
                }
            } else {
                // Group boundary — drain current accumulators by value, finalize, emit.
                let finished: Vec<AggAccumulator> = std::mem::replace(
                    &mut accumulators,
                    agg_exprs.iter().map(AggAccumulator::new).collect(),
                );
                let agg_values: Vec<Value> = finished
                    .into_iter()
                    .map(|acc| acc.finalize())
                    .collect::<Result<_, _>>()?;
                if let Some(ref having) = stmt.having {
                    let v = eval_with_aggs(having, &representative_row, &agg_values, &agg_exprs)?;
                    if is_truthy(&v) {
                        let out_row = project_grouped_row(
                            &stmt.columns,
                            &representative_row,
                            &agg_values,
                            &agg_exprs,
                        )?;
                        output_rows.push(out_row);
                    }
                } else {
                    let out_row = project_grouped_row(
                        &stmt.columns,
                        &representative_row,
                        &agg_values,
                        &agg_exprs,
                    )?;
                    output_rows.push(out_row);
                }

                // Start next group (accumulators already reset by mem::replace above).
                current_key = kr.key_values.clone();
                representative_row = kr.row.clone();
                for (acc, agg) in accumulators.iter_mut().zip(&agg_exprs) {
                    acc.update(&kr.row, agg)?;
                }
            }
        }

        // Finalize the last group.
        let agg_values: Vec<Value> = accumulators
            .into_iter()
            .map(|acc| acc.finalize())
            .collect::<Result<_, _>>()?;
        if let Some(ref having) = stmt.having {
            let v = eval_with_aggs(having, &representative_row, &agg_values, &agg_exprs)?;
            if is_truthy(&v) {
                let out_row = project_grouped_row(
                    &stmt.columns,
                    &representative_row,
                    &agg_values,
                    &agg_exprs,
                )?;
                output_rows.push(out_row);
            }
        } else {
            let out_row =
                project_grouped_row(&stmt.columns, &representative_row, &agg_values, &agg_exprs)?;
            output_rows.push(out_row);
        }
    }

    let mut rows = output_rows;
    if stmt.distinct {
        rows = apply_distinct_with_session(rows);
    }
    let remapped_ob = remap_order_by_for_grouped(&stmt.order_by, &stmt.columns);
    rows = apply_order_by(rows, &remapped_ob)?;
    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

/// Projects one output row for the grouped path.
///
/// For each `SelectItem::Expr`:
/// - If the expression contains an aggregate → `eval_with_aggs`
/// - Otherwise → standard `eval` against `representative_row`
fn project_grouped_row(
    items: &[SelectItem],
    representative_row: &[Value],
    agg_values: &[Value],
    agg_exprs: &[AggExpr],
) -> Result<Row, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                return Err(DbError::TypeMismatch {
                    expected: "column in GROUP BY or aggregate function".into(),
                    got: "SELECT * (wildcard) with GROUP BY".into(),
                });
            }
            SelectItem::Expr { expr, .. } => {
                let v = if contains_aggregate(expr) {
                    eval_with_aggs(expr, representative_row, agg_values, agg_exprs)?
                } else {
                    eval(expr, representative_row)?
                };
                out.push(v);
            }
        }
    }
    Ok(out)
}

/// Builds `ColumnMeta` for the output of a grouped SELECT.
fn build_grouped_column_meta(
    items: &[SelectItem],
    agg_exprs: &[AggExpr],
) -> Result<Vec<ColumnMeta>, DbError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                return Err(DbError::TypeMismatch {
                    expected: "column in GROUP BY or aggregate function".into(),
                    got: "SELECT * (wildcard) with GROUP BY".into(),
                });
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias
                    .clone()
                    .unwrap_or_else(|| grouped_expr_name(expr, agg_exprs));
                let (dt, nullable) = grouped_expr_type(expr, agg_exprs);
                out.push(ColumnMeta {
                    name,
                    data_type: dt,
                    nullable,
                    table_name: None,
                });
            }
        }
    }
    Ok(out)
}

/// Returns a display name for a grouped SELECT expression.
fn grouped_expr_name(expr: &Expr, _agg_exprs: &[AggExpr]) -> String {
    match expr {
        Expr::Column { name, .. } => name.clone(),
        Expr::GroupConcat { .. } => "GROUP_CONCAT(...)".into(),
        Expr::Function { name, args } if is_aggregate(name.as_str()) => {
            if args.is_empty() {
                format!("{name}(*)")
            } else {
                format!("{name}(...)")
            }
        }
        _ => "?column?".into(),
    }
}

/// Infers `(DataType, nullable)` for a grouped SELECT expression.
/// Aggregate results: COUNT → BigInt non-null; SUM/MIN/MAX/AVG → nullable.
fn grouped_expr_type(expr: &Expr, _agg_exprs: &[AggExpr]) -> (DataType, bool) {
    match expr {
        // GROUP_CONCAT always produces TEXT; nullable (empty group → NULL).
        Expr::GroupConcat { .. } => (DataType::Text, true),
        Expr::Function { name, .. } if is_aggregate(name.as_str()) => match name.as_str() {
            "count" => (DataType::BigInt, false),
            "avg" => (DataType::Real, true),
            _ => (DataType::Text, true), // SUM/MIN/MAX: type depends on column — Text fallback
        },
        Expr::Column { .. } => (DataType::Text, true), // Column refs: safe fallback
        _ => (DataType::Text, true),
    }
}
