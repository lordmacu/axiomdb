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

/// Phase 21.12 — Resolve positional integers in DISTINCT ON exprs
/// (e.g. `DISTINCT ON (1)` → first SELECT item), mirroring ORDER BY positional resolution.
fn resolve_positional_distinct_on(
    distinct_on: &[crate::expr::Expr],
    select_items: &[SelectItem],
) -> Vec<crate::expr::Expr> {
    distinct_on
        .iter()
        .map(|e| match e {
            crate::expr::Expr::Literal(Value::Int(n)) if *n >= 1 => {
                let idx = (*n as usize) - 1;
                if let Some(SelectItem::Expr { expr, .. }) = select_items.get(idx) {
                    expr.clone()
                } else {
                    e.clone()
                }
            }
            crate::expr::Expr::Literal(Value::BigInt(n)) if *n >= 1 => {
                let idx = (*n as usize) - 1;
                if let Some(SelectItem::Expr { expr, .. }) = select_items.get(idx) {
                    expr.clone()
                } else {
                    e.clone()
                }
            }
            _ => e.clone(),
        })
        .collect()
}

/// Phase 21.12 — DISTINCT ON deduplication.
///
/// Keeps the first row per distinct combination of the DISTINCT ON key expressions
/// (`distinct_on`). "First" is defined by the combined sort:
/// (all `distinct_on` exprs ASC NULLS LAST, then the full `order_by` clause).
///
/// The algorithm:
/// 1. Build a combined `OrderByItem` list: `distinct_on` exprs all ASC NULLS LAST,
///    followed by the caller's `order_by` items.
/// 2. Sort `rows` by that combined key (one sort pass, same as a plain ORDER BY).
/// 3. Walk sorted rows; serialize only the `distinct_on` portion as a dedup key,
///    and emit each row only on the first occurrence of that key.
///
/// The result is already in the correct final ORDER BY sequence — no second sort
/// is needed by the caller.
fn apply_distinct_on(
    rows: Vec<Row>,
    distinct_on: &[crate::expr::Expr],
    order_by: &[OrderByItem],
    select_items: &[SelectItem],
) -> Result<Vec<Row>, DbError> {
    use crate::ast::{NullsOrder, SortOrder};
    use crate::eval::eval as eval_expr;

    if distinct_on.is_empty() {
        return Ok(rows);
    }

    // Resolve any positional integer references in DISTINCT ON (e.g. DISTINCT ON (1)).
    let resolved = resolve_positional_distinct_on(distinct_on, select_items);

    // Build combined ORDER BY: DISTINCT ON exprs first (all ASC NULLS LAST),
    // then the user's ORDER BY.
    let mut combined: Vec<OrderByItem> = resolved
        .iter()
        .map(|e| OrderByItem {
            expr: e.clone(),
            order: SortOrder::Asc,
            nulls: Some(NullsOrder::Last),
        })
        .collect();
    combined.extend_from_slice(order_by);

    let sorted = apply_order_by(rows, &combined)?;

    // Walk sorted rows, keeping the first occurrence of each DISTINCT ON key.
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    let mut result: Vec<Row> = Vec::new();
    for row in sorted {
        let key: Vec<u8> = resolved
            .iter()
            .flat_map(|e| {
                let v = eval_expr(e, &row).unwrap_or(Value::Null);
                value_to_session_key_bytes(&v)
            })
            .collect();
        if seen.insert(key) {
            result.push(row);
        }
    }
    Ok(result)
}

// ── GroupState ────────────────────────────────────────────────────────────────

/// State for one GROUP BY group.
#[allow(dead_code)]
struct GroupState {
    /// Evaluated GROUP BY expression values (for future sort-based output — 4.9b).
    #[allow(dead_code)]
    key_values: Vec<Value>,
    /// One source row from this group — used by HAVING/SELECT to resolve column refs.
    representative_row: Row,
    /// One accumulator per aggregate in the query (SELECT + HAVING).
    accumulators: Vec<AggAccumulator>,
}

// ── GroupEntry — replaces GroupState in the new hash path ────────────────────

/// Per-group state used by the type-specialized group tables.
///
/// Replaces `GroupState::representative_row` with `non_agg_col_values`: a sparse
/// slice containing only the column values needed by non-aggregate SELECT items
/// and HAVING expressions. The column indices are pre-computed once before the
/// scan loop (`compute_non_agg_col_indices`) and reused for every group.
struct GroupEntry {
    /// Evaluated GROUP BY expression values (for output row key columns).
    #[allow(dead_code)]
    key_values: Vec<Value>,
    /// Values of non-aggregate column references in SELECT / HAVING.
    /// Indexed parallel to `non_agg_col_indices` computed before the scan loop.
    non_agg_col_values: Vec<Value>,
    /// One accumulator per AggExpr in the query.
    accumulators: Vec<AggAccumulator>,
}

// ── GroupTablePrimitive — zero-serialization INT/BIGINT GROUP BY ─────────────

/// Hash group table for single-column INT or BIGINT GROUP BY keys.
///
/// Stores the native `i64` value directly as the hash key, bypassing
/// `value_to_session_key_bytes` serialization entirely. Uses `hashbrown::HashMap`
/// which memoizes the `u64` hash in its raw table (SIMD-accelerated Robin Hood
/// probing), so hash recomputation on lookup probes is avoided — the same technique
/// used in DataFusion's `GroupValuesPrimitive<T>`.
struct GroupTablePrimitive {
    /// i64 key → index into `entries`.
    map: hashbrown::HashMap<i64, usize>,
    /// NULL values form their own group (SQL: NULLs are equal under GROUP BY).
    null_group: Option<usize>,
    entries: Vec<GroupEntry>,
}

impl GroupTablePrimitive {
    fn new() -> Self {
        Self {
            map: hashbrown::HashMap::new(),
            null_group: None,
            entries: Vec::new(),
        }
    }

    /// Look up or create the group for `key`. Returns `(group_index, is_new)`.
    fn get_or_insert(
        &mut self,
        key: Option<i64>,
        key_value: Value,
        agg_exprs: &[AggExpr],
        non_agg_col_indices: &[usize],
        row: &[Value],
    ) -> usize {
        match key {
            None => {
                if let Some(idx) = self.null_group {
                    idx
                } else {
                    let idx = self.entries.len();
                    self.entries.push(GroupEntry {
                        key_values: vec![Value::Null],
                        non_agg_col_values: extract_non_agg_cols(non_agg_col_indices, row),
                        accumulators: agg_exprs.iter().map(AggAccumulator::new).collect(),
                    });
                    self.null_group = Some(idx);
                    idx
                }
            }
            Some(k) => {
                let next_idx = self.entries.len();
                let idx = *self.map.entry(k).or_insert(next_idx);
                if idx == next_idx {
                    self.entries.push(GroupEntry {
                        key_values: vec![key_value],
                        non_agg_col_values: extract_non_agg_cols(non_agg_col_indices, row),
                        accumulators: agg_exprs.iter().map(AggAccumulator::new).collect(),
                    });
                }
                idx
            }
        }
    }
}

// ── GroupTableGeneric — serialized keys, hashbrown backend ───────────────────

/// Hash group table for all other GROUP BY cases (multi-column, TEXT, composite).
///
/// Keeps the existing `value_to_session_key_bytes` serialization but replaces
/// `std::collections::HashMap` with `hashbrown::HashMap`. hashbrown memoizes
/// hashes in its raw table and uses SIMD-accelerated linear probing (Robin Hood
/// hashing), giving 20–40% faster lookups on realistic workloads compared to
/// the standard library implementation.
struct GroupTableGeneric {
    /// Serialized key bytes → index into `entries`.
    map: hashbrown::HashMap<Vec<u8>, usize>,
    entries: Vec<GroupEntry>,
}

impl GroupTableGeneric {
    fn new() -> Self {
        Self {
            map: hashbrown::HashMap::new(),
            entries: Vec::new(),
        }
    }

    /// Look up or create the group for `key_buf`. Returns `group_index`.
    /// `key_buf` is borrowed; only cloned if a new group is inserted.
    fn get_or_insert(
        &mut self,
        key_buf: &[u8],
        key_values: Vec<Value>,
        agg_exprs: &[AggExpr],
        non_agg_col_indices: &[usize],
        row: &[Value],
    ) -> usize {
        if let Some(&idx) = self.map.get(key_buf) {
            return idx;
        }
        let idx = self.entries.len();
        self.entries.push(GroupEntry {
            key_values,
            non_agg_col_values: extract_non_agg_cols(non_agg_col_indices, row),
            accumulators: agg_exprs.iter().map(AggAccumulator::new).collect(),
        });
        self.map.insert(key_buf.to_vec(), idx);
        idx
    }
}

// ── GroupTableKind — zero-cost dispatch enum ──────────────────────────────────

enum GroupTableKind {
    Primitive(GroupTablePrimitive),
    Generic(GroupTableGeneric),
}

impl GroupTableKind {
    fn entries_mut(&mut self) -> &mut Vec<GroupEntry> {
        match self {
            Self::Primitive(t) => &mut t.entries,
            Self::Generic(t) => &mut t.entries,
        }
    }

    fn into_entries(self) -> Vec<GroupEntry> {
        match self {
            Self::Primitive(t) => t.entries,
            Self::Generic(t) => t.entries,
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Extracts the values at `indices` from `row` into a compact `Vec<Value>`.
/// Called once per new group; skips columns not referenced in SELECT/HAVING.
#[inline]
fn extract_non_agg_cols(indices: &[usize], row: &[Value]) -> Vec<Value> {
    indices
        .iter()
        .map(|&i| row.get(i).cloned().unwrap_or(Value::Null))
        .collect()
}

/// Collects all `col_idx` values referenced by non-aggregate expressions
/// in the SELECT list and HAVING clause. Deduplicated and sorted.
///
/// These are the only column values that need to be retained per group for
/// finalization. All other column values from source rows are discarded.
fn compute_non_agg_col_indices(stmt: &SelectStmt) -> Vec<usize> {
    let mut idxs: Vec<usize> = Vec::new();

    for item in &stmt.columns {
        if let SelectItem::Expr { expr, .. } = item {
            if !contains_aggregate(expr) {
                collect_col_idxs_non_agg(expr, &mut idxs);
            }
        }
    }
    if let Some(having) = &stmt.having {
        collect_non_agg_col_idxs_in_expr(having, false, &mut idxs);
    }
    idxs.sort_unstable();
    idxs.dedup();
    idxs
}

/// Recursively collects `col_idx` values from `expr`, skipping inside aggregates.
fn collect_col_idxs_non_agg(expr: &Expr, out: &mut Vec<usize>) {
    match expr {
        Expr::Column { col_idx, .. } => out.push(*col_idx),
        Expr::Function { name, args, .. } if is_aggregate(name.as_str()) => {
            // Do not descend into aggregate arguments — they are handled by accumulators.
            let _ = args;
        }
        Expr::GroupConcat { .. } => {}
        Expr::BinaryOp { left, right, .. } => {
            collect_col_idxs_non_agg(left, out);
            collect_col_idxs_non_agg(right, out);
        }
        Expr::UnaryOp { operand, .. } => collect_col_idxs_non_agg(operand, out),
        Expr::Collate { expr, .. } => collect_col_idxs_non_agg(expr, out),
        Expr::Function { args, .. } => {
            for a in args {
                collect_col_idxs_non_agg(a, out);
            }
        }
        Expr::Window { spec, .. } => {
            for e in &spec.partition_by {
                collect_col_idxs_non_agg(e, out);
            }
            for item in &spec.order_by {
                collect_col_idxs_non_agg(&item.expr, out);
            }
        }
        Expr::Cast { expr, .. } => collect_col_idxs_non_agg(expr, out),
        Expr::IsNull { expr, .. } => collect_col_idxs_non_agg(expr, out),
        Expr::IsBoolean { expr, .. } => collect_col_idxs_non_agg(expr, out),
        Expr::Between { expr, low, high, .. } => {
            collect_col_idxs_non_agg(expr, out);
            collect_col_idxs_non_agg(low, out);
            collect_col_idxs_non_agg(high, out);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_col_idxs_non_agg(expr, out);
            collect_col_idxs_non_agg(pattern, out);
        }
        Expr::In { expr, list, .. } => {
            collect_col_idxs_non_agg(expr, out);
            for e in list {
                collect_col_idxs_non_agg(e, out);
            }
        }
        Expr::Case { operand, when_thens, else_result } => {
            if let Some(e) = operand {
                collect_col_idxs_non_agg(e, out);
            }
            for (w, t) in when_thens {
                collect_col_idxs_non_agg(w, out);
                collect_col_idxs_non_agg(t, out);
            }
            if let Some(e) = else_result {
                collect_col_idxs_non_agg(e, out);
            }
        }
        // GROUPING() args may reference non-aggregate columns.
        Expr::Grouping { args, .. } => {
            for a in args { collect_col_idxs_non_agg(a, out); }
        }
        // Phase 20.4 — ARRAY[expr, ...]: recurse into elements.
        Expr::ArrayConstructor { elements } => {
            for e in elements { collect_col_idxs_non_agg(e, out); }
        }
        // Phase 20.4, Step 5 — array subscript: recurse into array and index.
        Expr::Subscript { array, index, slice } => {
            collect_col_idxs_non_agg(array, out);
            collect_col_idxs_non_agg(index, out);
            if let Some(s) = slice {
                collect_col_idxs_non_agg(s, out);
            }
        }
        Expr::Literal(_)
        | Expr::Default
        | Expr::OuterColumn { .. }
        | Expr::InsertValue { .. }
        | Expr::ExcludedValue { .. }
        | Expr::SqlJsonQuery { .. }
        | Expr::Param { .. }
        | Expr::Subquery(_)
        | Expr::InSubquery { .. }
        | Expr::Exists { .. } => {}
    }
}

/// Walk `expr` collecting col_idx values outside aggregate calls.
/// `inside_agg`: if true we are already inside an aggregate — stop collecting.
fn collect_non_agg_col_idxs_in_expr(expr: &Expr, inside_agg: bool, out: &mut Vec<usize>) {
    match expr {
        Expr::Column { col_idx, .. } if !inside_agg => out.push(*col_idx),
        Expr::Function { name, args, .. } if is_aggregate(name.as_str()) => {
            // Descend but mark as inside_agg so sub-columns are not collected.
            for a in args {
                collect_non_agg_col_idxs_in_expr(a, true, out);
            }
        }
        Expr::GroupConcat { .. } => {}
        Expr::BinaryOp { left, right, .. } => {
            collect_non_agg_col_idxs_in_expr(left, inside_agg, out);
            collect_non_agg_col_idxs_in_expr(right, inside_agg, out);
        }
        Expr::UnaryOp { operand, .. } => collect_non_agg_col_idxs_in_expr(operand, inside_agg, out),
        Expr::Collate { expr, .. } => collect_non_agg_col_idxs_in_expr(expr, inside_agg, out),
        Expr::Function { args, .. } => {
            for a in args {
                collect_non_agg_col_idxs_in_expr(a, inside_agg, out);
            }
        }
        Expr::Window { spec, .. } => {
            for e in &spec.partition_by {
                collect_non_agg_col_idxs_in_expr(e, inside_agg, out);
            }
            for item in &spec.order_by {
                collect_non_agg_col_idxs_in_expr(&item.expr, inside_agg, out);
            }
        }
        Expr::Cast { expr, .. } => collect_non_agg_col_idxs_in_expr(expr, inside_agg, out),
        Expr::IsNull { expr, .. } => collect_non_agg_col_idxs_in_expr(expr, inside_agg, out),
        Expr::IsBoolean { expr, .. } => collect_non_agg_col_idxs_in_expr(expr, inside_agg, out),
        Expr::Between { expr, low, high, .. } => {
            collect_non_agg_col_idxs_in_expr(expr, inside_agg, out);
            collect_non_agg_col_idxs_in_expr(low, inside_agg, out);
            collect_non_agg_col_idxs_in_expr(high, inside_agg, out);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_non_agg_col_idxs_in_expr(expr, inside_agg, out);
            collect_non_agg_col_idxs_in_expr(pattern, inside_agg, out);
        }
        Expr::In { expr, list, .. } => {
            collect_non_agg_col_idxs_in_expr(expr, inside_agg, out);
            for e in list {
                collect_non_agg_col_idxs_in_expr(e, inside_agg, out);
            }
        }
        Expr::Case { operand, when_thens, else_result } => {
            if let Some(e) = operand {
                collect_non_agg_col_idxs_in_expr(e, inside_agg, out);
            }
            for (w, t) in when_thens {
                collect_non_agg_col_idxs_in_expr(w, inside_agg, out);
                collect_non_agg_col_idxs_in_expr(t, inside_agg, out);
            }
            if let Some(e) = else_result {
                collect_non_agg_col_idxs_in_expr(e, inside_agg, out);
            }
        }
        // GROUPING() args may reference non-aggregate columns.
        Expr::Grouping { args, .. } => {
            for a in args { collect_non_agg_col_idxs_in_expr(a, inside_agg, out); }
        }
        // Phase 20.4 — ARRAY[expr, ...]: recurse into elements.
        Expr::ArrayConstructor { elements } => {
            for e in elements { collect_non_agg_col_idxs_in_expr(e, inside_agg, out); }
        }
        // Phase 20.4, Step 5 — array subscript: recurse into array and index.
        Expr::Subscript { array, index, slice } => {
            collect_non_agg_col_idxs_in_expr(array, inside_agg, out);
            collect_non_agg_col_idxs_in_expr(index, inside_agg, out);
            if let Some(s) = slice {
                collect_non_agg_col_idxs_in_expr(s, inside_agg, out);
            }
        }
        Expr::Column { .. }
        | Expr::Literal(_)
        | Expr::Default
        | Expr::OuterColumn { .. }
        | Expr::InsertValue { .. }
        | Expr::ExcludedValue { .. }
        | Expr::SqlJsonQuery { .. }
        | Expr::Param { .. }
        | Expr::Subquery(_)
        | Expr::InSubquery { .. }
        | Expr::Exists { .. } => {}
    }
}
