// ── plan_select ──────────────────────────────────────────────────────────────

/// Chooses an [`AccessMethod`] for the given `WHERE` clause and available indexes.
///
/// Returns [`AccessMethod::Scan`] if no suitable index is found.
///
/// `table_stats` contains per-column statistics for the table being scanned.
/// An empty slice means "no statistics available" — the planner always uses
/// indexes (conservative: never wrong, just potentially suboptimal).
///
/// `stale_tracker` is used to:
/// 1. Register the row count baseline when stats are loaded (for Phase 6.11).
/// 2. Use `DEFAULT_NUM_DISTINCT` instead of catalog stats for stale tables.
pub fn plan_select(
    where_clause: Option<&Expr>,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
    table_id: u32,
    table_stats: &[StatsDef],
    stale_tracker: &mut StaleStatsTracker,
    // Column indices needed in the SELECT output. Empty = SELECT * → no index-only scan.
    select_col_idxs: &[u16],
) -> AccessMethod {
    use crate::key_encoding::encode_index_key;

    let expr = match where_clause {
        Some(e) => e,
        None => return AccessMethod::Scan,
    };

    // ── Rule 0: composite equality (N ≥ 2 columns) ────────────────────────
    // Must run before Rule 1 to prefer composite indexes over single-column.
    if let Some(am) = plan_composite_eq(expr, indexes, columns) {
        if let AccessMethod::IndexRange {
            ref index_def,
            ref lo,
            ..
        } = am
        {
            if !stats_cost_gate(index_def, columns, table_id, table_stats, stale_tracker) {
                return AccessMethod::Scan;
            }
            // Index-only scan upgrade: composite key covers all SELECT columns.
            if index_covers_query(index_def, select_col_idxs) {
                if let Some(lo_key) = lo.clone() {
                    return AccessMethod::IndexOnlyScan {
                        index_def: index_def.clone(),
                        lo: lo_key.clone(),
                        hi: Some(lo_key),
                        n_key_cols: index_def.columns.len(),
                        n_include_cols: index_def.include_columns.len(),
                        needed_key_positions: build_key_positions(index_def, select_col_idxs),
                    };
                }
            }
        }
        return am;
    }

    // ── Rule 1: col = literal ─────────────────────────────────────────────
    if let Some((col_name, value)) = extract_eq_col_literal(expr) {
        if let Some(idx) = find_index_on_col(col_name, indexes, columns, Some(expr), true) {
            let value = coerce_literal_to_col_type(value, col_name, columns);
            if let Ok(key) = encode_index_key(&[value]) {
                // PK equality is always worth using for SELECT: the current
                // small-table/NDV cost gate is the wrong trade-off for point
                // lookups on the primary key.
                let use_index = idx.is_primary
                    || stats_cost_gate(idx, columns, table_id, table_stats, stale_tracker);
                if use_index {
                    // Index-only scan upgrade (Phase 6.13): all SELECT cols in key.
                    if index_covers_query(idx, select_col_idxs) {
                        return AccessMethod::IndexOnlyScan {
                            index_def: idx.clone(),
                            lo: key.clone(),
                            hi: Some(key), // point lookup: lo == hi
                            n_key_cols: idx.columns.len(),
                            n_include_cols: idx.include_columns.len(),
                            needed_key_positions: build_key_positions(idx, select_col_idxs),
                        };
                    }
                    if idx.columns.len() == 1 {
                        return AccessMethod::IndexLookup {
                            index_def: idx.clone(),
                            key,
                        };
                    } else {
                        let mut hi = key.clone();
                        hi.extend_from_slice(&[0xFF; crate::key_encoding::MAX_INDEX_KEY]);
                        return AccessMethod::IndexRange {
                            index_def: idx.clone(),
                            lo: Some(key),
                            hi: Some(hi),
                        };
                    }
                }
            }
        }
    }

    // ── Rule 1b: expression index lookup ──────────────────────────────────────────
    // Handles patterns like `LOWER(email) = 'foo'` or `LOWER(email) LIKE 'foo%'`
    // where an expression index `LOWER(email)` exists.
    if let Some((idx, lo_key, hi_key)) = find_expression_index(expr, indexes, columns, Some(expr))
    {
        let use_index =
            idx.is_primary || stats_cost_gate(&idx, columns, table_id, table_stats, stale_tracker);
        if use_index {
            // Expression indexes are always single-column (by definition).
            return AccessMethod::IndexRange {
                index_def: idx.clone(),
                lo: lo_key,
                hi: hi_key,
            };
        }
    }

    // ── Rule 2: col > lo AND col < hi (or >=, <=) ─────────────────────────
    if let Some((idx, lo_val, hi_val)) = extract_range(expr, indexes, columns, Some(expr), true) {
        // Cost gate: range scans are even less selective — apply same threshold.
        let use_index =
            idx.is_primary || stats_cost_gate(idx, columns, table_id, table_stats, stale_tracker);
        if use_index {
            // Coerce range bounds to the indexed column's stored type.
            let range_col = idx.columns.first().and_then(|c| {
                columns
                    .iter()
                    .find(|col| col.col_idx == c.col_idx)
                    .map(|col| col.name.as_str())
            });
            let lo = lo_val.and_then(|v| {
                let v =
                    range_col.map_or(v.clone(), |cn| coerce_literal_to_col_type(v, cn, columns));
                encode_index_key(&[v]).ok()
            });
            let hi = hi_val.and_then(|v| {
                let v =
                    range_col.map_or(v.clone(), |cn| coerce_literal_to_col_type(v, cn, columns));
                encode_index_key(&[v]).ok()
            });
            return AccessMethod::IndexRange {
                index_def: idx.clone(),
                lo,
                hi,
            };
        }
    }

    // ── Rule 2b: single-sided range (col > val, col < val, col >= val, col <= val)
    // Handles predicates like `WHERE id > 50` that are NOT wrapped in AND.
    if let Some((col_name, bound)) = extract_range_side(expr) {
        if let Some(idx) = find_index_on_col(col_name, indexes, columns, Some(expr), true) {
            let use_index = idx.is_primary
                || stats_cost_gate(idx, columns, table_id, table_stats, stale_tracker);
            if use_index {
                // Determine if this is a lower or upper bound.
                let is_lower = matches!(
                    expr,
                    Expr::BinaryOp {
                        op: BinaryOp::Gt | BinaryOp::GtEq,
                        ..
                    }
                );
                let encoded = bound.and_then(|v| {
                    let v = coerce_literal_to_col_type(v, col_name, columns);
                    encode_index_key(&[v]).ok()
                });
                let (lo, hi) = if is_lower {
                    (encoded, None) // open upper bound
                } else {
                    (None, encoded) // open lower bound
                };
                return AccessMethod::IndexRange {
                    index_def: idx.clone(),
                    lo,
                    hi,
                };
            }
        }
    }

    // ── Rule G: GIN inverted index scan for col @> literal (Phase 11.17) ────────
    // Detects `col @> jsonb_literal` and uses the GIN index when one exists on
    // that column. Term extraction happens at plan time from the literal value.
    if let Some(am) = plan_gin_scan(expr, indexes, columns) {
        return am;
    }

    AccessMethod::Scan
}

// ── Rule G helper: GIN scan planner ──────────────────────────────────────────

/// Detects JSONB and Array predicates that a `jsonb_ops` GIN index can accelerate:
///
/// - `col @>  <jsonb_literal>` (Phase 11.17 — JSONB containment)
/// - `col @>  <array_literal>`  (Phase 20.4 Step 8 — array @> array)
/// - `col &&  <array_literal>`  (Phase 20.4 Step 8 — array overlap)
/// - `col <@  <array_literal>`  (Phase 20.4 Step 8 — array contained by)
/// - `col =   <array_literal>`  (Phase 20.4 Step 8 — array equality)
/// - `col ?   <text_literal>`   (Phase 11.18a — key/array-string exists)
///
/// Returns `AccessMethod::GinScan` with the probe terms when a GIN index
/// whose first column matches `col` exists. The executor always runs the
/// original predicate as a re-check (`recheck_required = true`) because GIN
/// posting-list intersection alone cannot confirm structural containment or
/// rule out dead rows.
fn plan_gin_scan(expr: &Expr, indexes: &[IndexDef], columns: &[ColumnDef]) -> Option<AccessMethod> {
    enum GinProbe {
        Contains,
        ArrayContains,
        ArrayOverlap,
        ArrayContainedBy,
        ArrayEquals,
        Exists,
        JsonPathExists,
        JsonPathMatch,
    }

    /// Extracts the column name from a column expression, if it references an array column.
    fn get_array_column_info<'a>(
        left: &'a Expr,
        right: &'a Expr,
        columns: &[ColumnDef],
    ) -> Option<(&'a str, &'a Value)> {
        let (col_expr, val_expr) = match (left, right) {
            (Expr::Column { name, .. }, v) => (name.as_str(), v),
            _ => return None,
        };
        // Check if column is an array type
        let col_idx = columns.iter().find(|c| c.name == col_expr)?.col_idx as usize;
        let col_def = columns.get(col_idx)?;
        if col_def.col_type != axiomdb_catalog::schema::ColumnType::Array {
            return None;
        }
        // Get the array literal value
        let arr_val = match val_expr {
            Expr::Literal(v) => v,
            _ => return None,
        };
        if !matches!(arr_val, Value::Array(_)) {
            return None;
        }
        Some((col_expr, arr_val))
    }

    let (probe, col_name, literal) = match expr {
        // Array: col @> ARRAY[...] — containment
        Expr::BinaryOp {
            op: BinaryOp::JsonContains,
            left,
            right,
        } => {
            if let Some((name, val)) = get_array_column_info(left, right, columns) {
                (GinProbe::ArrayContains, name, val)
            } else {
                match (left.as_ref(), right.as_ref()) {
                    (Expr::Column { name, .. }, Expr::Literal(v)) => {
                        (GinProbe::Contains, name.as_str(), v)
                    }
                    _ => return None,
                }
            }
        }
        // Array: col && ARRAY[...] — overlap
        Expr::BinaryOp {
            op: BinaryOp::ArrayOverlap,
            left,
            right,
        } => {
            if let Some((name, val)) = get_array_column_info(left, right, columns) {
                (GinProbe::ArrayOverlap, name, val)
            } else {
                return None;
            }
        }
        // Array: col <@ ARRAY[...] — contained by
        Expr::BinaryOp {
            op: BinaryOp::JsonContainedBy,
            left,
            right,
        } => {
            if let Some((name, val)) = get_array_column_info(left, right, columns) {
                (GinProbe::ArrayContainedBy, name, val)
            } else {
                return None;
            }
        }
        // Array: col = ARRAY[...] — equality
        Expr::BinaryOp {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            if let Some((name, val)) = get_array_column_info(left, right, columns) {
                (GinProbe::ArrayEquals, name, val)
            } else {
                return None;
            }
        }
        // JSONB: col ? <text_literal>
        Expr::BinaryOp {
            op: BinaryOp::JsonExists,
            left,
            right,
        } => match (left.as_ref(), right.as_ref()) {
            (Expr::Column { name, .. }, Expr::Literal(v)) => (GinProbe::Exists, name.as_str(), v),
            _ => return None,
        },
        // JSONB: col @? <jsonpath>
        Expr::BinaryOp {
            op: BinaryOp::JsonbPathExists,
            left,
            right,
        } => match (left.as_ref(), right.as_ref()) {
            (Expr::Column { name, .. }, Expr::Literal(v)) => {
                (GinProbe::JsonPathExists, name.as_str(), v)
            }
            _ => return None,
        },
        // JSONB: col @@ <jsonpath>
        Expr::BinaryOp {
            op: BinaryOp::JsonbPathMatch,
            left,
            right,
        } => match (left.as_ref(), right.as_ref()) {
            (Expr::Column { name, .. }, Expr::Literal(v)) => {
                (GinProbe::JsonPathMatch, name.as_str(), v)
            }
            _ => return None,
        },
        _ => return None,
    };

    // Resolve column index.
    let col_idx = columns.iter().find(|c| c.name == col_name)?.col_idx;

    // Find a GIN index (index_type == 4) whose first column matches.
    let gin_idx = indexes.iter().find(|idx| {
        idx.index_type == 4 && !idx.columns.is_empty() && idx.columns[0].col_idx == col_idx
    })?;

    // Extract query terms from the literal value at plan time.
    let query_terms = match probe {
        // Array @> containment: all query elements must be in the indexed column
        GinProbe::ArrayContains => {
            if let Value::Array(arr) = literal {
                crate::index_maintenance::gin_extract_array_keys(arr)
            } else {
                return None;
            }
        }
        // Array && overlap: at least one query element must be in the indexed column
        GinProbe::ArrayOverlap => {
            if let Value::Array(arr) = literal {
                crate::index_maintenance::gin_extract_array_keys(arr)
            } else {
                return None;
            }
        }
        // Array <@ contained-by: all indexed elements must be in the query array
        // This is the reverse check - the indexed column must be a subset of query
        GinProbe::ArrayContainedBy => {
            if let Value::Array(arr) = literal {
                crate::index_maintenance::gin_extract_array_keys(arr)
            } else {
                return None;
            }
        }
        // Array = equality: all elements must match (same as @> for identical arrays)
        GinProbe::ArrayEquals => {
            if let Value::Array(arr) = literal {
                crate::index_maintenance::gin_extract_array_keys(arr)
            } else {
                return None;
            }
        }
        GinProbe::Contains => match literal {
            // SQL text literals ('{"a":1}') are treated as JSON; Jsonb is
            // pre-encoded binary.
            Value::Jsonb(b) => axiomdb_types::jsonb::gin_extract_terms(b.as_slice()).ok()?,
            Value::Json(s) | Value::Text(s) => {
                axiomdb_types::jsonb::gin_extract_terms_from_str(s).ok()?
            }
            _ => return None,
        },
        GinProbe::Exists => match literal {
            Value::Text(s) | Value::Json(s) => vec![axiomdb_types::jsonb::gin_key_term(s)],
            _ => return None,
        },
        GinProbe::JsonPathExists | GinProbe::JsonPathMatch => {
            let key = extract_simple_jsonpath_key(literal)?;
            vec![axiomdb_types::jsonb::gin_key_term(&key)]
        }
    };

    // An empty query (`col @> '{}'`) is always true — no index help possible.
    if query_terms.is_empty() {
        return None;
    }

    Some(AccessMethod::GinScan {
        index_def: gin_idx.clone(),
        query_terms,
    })
}

fn extract_simple_jsonpath_key(literal: &Value) -> Option<String> {
    let path = match literal {
        Value::Text(s) | Value::Json(s) => s.trim(),
        _ => return None,
    };
    let key = path.strip_prefix("$.")?;
    if key.is_empty() {
        return None;
    }
    if key
        .chars()
        .any(|ch| matches!(ch, '.' | '[' | ']' | '(' | ')' | '*' | '?' | ' ' | '\t' | '\n' | '\r'))
    {
        return None;
    }
    Some(key.to_string())
}

// ── Index-only scan coverage (Phase 6.13) ────────────────────────────────────

/// Returns `true` if all columns in `select_col_idxs` are covered by the index
/// key columns or INCLUDE columns.
fn index_covers_query(index_def: &IndexDef, select_col_idxs: &[u16]) -> bool {
    if select_col_idxs.is_empty() {
        return false; // SELECT * or unknown — never use index-only
    }
    let mut covered_cols: std::collections::HashSet<u16> = index_def
        .columns
        .iter()
        .filter(|c| c.expr.is_none())
        .map(|c| c.col_idx)
        .collect();
    covered_cols.extend(index_def.include_columns.iter().copied());
    select_col_idxs.iter().all(|col| covered_cols.contains(col))
}

/// Builds the `needed_key_positions` vector for `IndexOnlyScan`.
/// For each column in `select_col_idxs`, finds its position in `index.columns`.
fn build_key_positions(index_def: &IndexDef, select_col_idxs: &[u16]) -> Vec<usize> {
    select_col_idxs
        .iter()
        .filter_map(|col_idx| index_def.columns.iter().position(|c| c.col_idx == *col_idx))
        .collect()
}

// ── Statistics cost gate (Phase 6.10) ────────────────────────────────────────

/// Returns `true` if an index scan is worth using given the column statistics.
///
/// Uses `selectivity = 1 / NDV` for equality predicates. If selectivity is
/// above `INDEX_SELECTIVITY_THRESHOLD` (0.20 = 20% of rows), a full table
/// scan is cheaper. For small tables or when no stats exist, the function
/// conservatively returns `true` (use the index — never wrong, just possibly
/// suboptimal).
///
/// Also sets the staleness baseline if stats are loaded here for the first time.
fn stats_cost_gate(
    index_def: &IndexDef,
    _columns: &[ColumnDef],
    table_id: u32,
    table_stats: &[StatsDef],
    stale_tracker: &mut StaleStatsTracker,
) -> bool {
    // No stats → always use index (conservative default).
    if table_stats.is_empty() {
        return true;
    }

    // Find the first indexed column's col_idx.
    let col_idx = match index_def.columns.first() {
        Some(c) => c.col_idx,
        None => return true,
    };

    // Find stats for this column.
    let stats = match table_stats.iter().find(|s| s.col_idx == col_idx) {
        Some(s) => s,
        None => return true, // no stats for this column → use index
    };

    // Register baseline for Phase 6.11 staleness tracking.
    stale_tracker.set_baseline(table_id, stats.row_count);

    // No real data yet (stats bootstrapped on empty table, e.g. CREATE INDEX
    // before any INSERTs) — treat as no stats and conservatively use the index.
    if stats.row_count == 0 {
        return true;
    }

    // Small table: always scan (index overhead not worth it).
    if stats.row_count < SMALL_TABLE_THRESHOLD {
        return false;
    }

    // Compute NDV (handle dual-encoding and zero/unknown).
    let ndv = if stats.ndv > 0 {
        stats.ndv
    } else {
        DEFAULT_NUM_DISTINCT
    };

    // selectivity = 1 / NDV for equality predicates.
    let selectivity = 1.0 / (ndv.max(1) as f64);
    selectivity <= INDEX_SELECTIVITY_THRESHOLD
}

// ── Rule 0: composite equality planner ───────────────────────────────────────

/// Collects all atomic `col = literal` equality conditions reachable via
/// AND-clauses in `expr`. Stops at OR, NOT, or any non-equality operator.
fn collect_eq_conditions(expr: &Expr) -> Vec<(&str, Value)> {
    match expr {
        Expr::BinaryOp {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let mut v = collect_eq_conditions(left);
            v.extend(collect_eq_conditions(right));
            v
        }
        other => extract_eq_col_literal(other).into_iter().collect(),
    }
}

/// Rule 0: try to match WHERE AND-clauses to the leading columns of a composite
/// index (≥ 2 columns). Returns `IndexRange { lo=hi=composite_key }` if a
/// composite match with at least 2 columns is found.
///
/// `IndexRange lo=hi` is used instead of `IndexLookup` because composite
/// non-unique indexes may have multiple rows per composite key — range scan
/// correctly returns all of them, while `lookup_in` only returns one.
fn plan_composite_eq(
    expr: &Expr,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
) -> Option<AccessMethod> {
    use crate::key_encoding::encode_index_key;

    let eq_conds = collect_eq_conditions(expr);
    if eq_conds.len() < 2 {
        return None; // single-column → Rule 1 handles it
    }

    for idx in indexes.iter().filter(|i| {
        // Skip primary, FK auto-indexes, single-column indexes, partial indexes.
        !i.is_primary && !i.is_fk_index && i.columns.len() >= 2 && i.predicate.is_none()
    }) {
        let mut key_parts: Vec<Value> = Vec::new();

        // Try to match leading columns of the index to equality conditions.
        // Stops at the first unmatched column (prefix property).
        for idx_col in &idx.columns {
            let col_name = columns
                .iter()
                .find(|c| c.col_idx == idx_col.col_idx)
                .map(|c| c.name.as_str())?;

            match eq_conds.iter().find(|(name, _)| *name == col_name) {
                Some((_, val)) => {
                    key_parts.push(coerce_literal_to_col_type(val.clone(), col_name, columns));
                }
                None => break, // gap in leading columns — can't use this index
            }
        }

        if key_parts.len() >= 2 {
            if let Ok(key) = encode_index_key(&key_parts) {
                // Also check partial index predicate implication (same as Rule 1).
                // For Phase 6.9, we already filtered out partial indexes above (predicate.is_none()).
                return Some(AccessMethod::IndexRange {
                    index_def: idx.clone(),
                    lo: Some(key.clone()),
                    hi: Some(key),
                });
            }
        }
    }
    None
}

// ── Helper: extract col = literal from WHERE ─────────────────────────────────

/// Returns `(col_name, value)` if `expr` is `col = literal` or `literal = col`.
fn extract_eq_col_literal(expr: &Expr) -> Option<(&str, Value)> {
    if let Expr::BinaryOp {
        op: BinaryOp::Eq,
        left,
        right,
    } = expr
    {
        // col = literal
        if let (Expr::Column { name, .. }, Expr::Literal(v)) = (left.as_ref(), right.as_ref()) {
            return Some((name.as_str(), v.clone()));
        }
        // literal = col
        if let (Expr::Literal(v), Expr::Column { name, .. }) = (left.as_ref(), right.as_ref()) {
            return Some((name.as_str(), v.clone()));
        }
    }
    None
}

// ── Expression index matching (Phase 21.8E) ─────────────────────────────────────

/// Produces a canonical SQL string for an expression by:
/// - Lowercasing function names and column names
/// - Removing extra whitespace
/// - Using consistent parentheses
///
/// This allows direct string comparison with the stored SQL expression
/// from a `CREATE INDEX ... (LOWER(col))` statement.
fn normalize_expr_sql(e: &Expr) -> String {
    match e {
        Expr::Literal(v) => format!("{:?}", v),
        Expr::Column { name, .. } => name.to_lowercase(),
        Expr::Function { name, args } => {
            let args_sql: Vec<String> = args.iter().map(normalize_expr_sql).collect();
            format!("{}({})", name.to_lowercase(), args_sql.join(", "))
        }
        Expr::BinaryOp { op, left, right } => {
            let op_str = match op {
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                BinaryOp::Div => "/",
                BinaryOp::Mod => "%",
                BinaryOp::And => "AND",
                BinaryOp::Or => "OR",
                BinaryOp::Eq => "=",
                BinaryOp::NotEq => "<>",
                BinaryOp::Lt => "<",
                BinaryOp::LtEq => "<=",
                BinaryOp::Gt => ">",
                BinaryOp::GtEq => ">=",
                BinaryOp::Concat => "||",
                _ => return String::new(), // Unsupported ops give no match
            };
            format!(
                "({} {} {})",
                normalize_expr_sql(left),
                op_str,
                normalize_expr_sql(right)
            )
        }
        Expr::UnaryOp { op, operand } => {
            let op_str = match op {
                UnaryOp::Neg => "-",
                UnaryOp::Not => "NOT",
                UnaryOp::BitNot => "~",
            };
            format!("({}{})", op_str, normalize_expr_sql(operand))
        }
        Expr::Cast { expr, target } => {
            format!("cast({} AS {:?})", normalize_expr_sql(expr), target)
        }
        // Partial coverage — other expression types return empty (no match)
        _ => String::new(),
    }
}

/// Extracts the indexable expression from a comparison LHS, returning the
/// inner-most function call or column if there's no wrapping function.
fn extract_indexable_expr(e: &Expr) -> Option<&Expr> {
    match e {
        Expr::Function { .. } => Some(e),
        Expr::Column { .. } => Some(e),
        Expr::Cast { expr, .. } => extract_indexable_expr(expr),
        Expr::UnaryOp { operand, .. } => extract_indexable_expr(operand),
        _ => None,
    }
}

/// Finds an expression index whose stored SQL matches a WHERE expression.
///
/// Returns `(index, lo_key, hi_key)` where `lo_key` and `hi_key` are the
/// encoded index key bounds. For equality, `hi_key = Some(lo_key.clone())`.
/// For prefix LIKE (`'foo%'`), `lo_key` = encoded 'foo', `hi_key` = encoded 'foo\xFF'.
/// Returns `None` if no expression index matches or the predicate is not indexable.
///
/// Currently handles:
/// - `func(col) = literal` → IndexLookup on expression index `func(col)`
/// - `func(col) LIKE 'prefix%'` → IndexRange with prefix bounds
#[allow(clippy::type_complexity)]
fn find_expression_index(
    where_expr: &Expr,
    indexes: &[IndexDef],
    columns: &[ColumnDef],
    query_where: Option<&Expr>,
) -> Option<(IndexDef, Option<Vec<u8>>, Option<Vec<u8>>)> {
    use crate::key_encoding::encode_index_key;

    if let Expr::BinaryOp {
        op: BinaryOp::And,
        left,
        right,
    } = where_expr
    {
        return find_expression_index(left, indexes, columns, query_where)
            .or_else(|| find_expression_index(right, indexes, columns, query_where));
    }

    // Match either BinaryOp::Eq or Expr::Like.
    let (indexable_expr, literal_expr, is_eq) = match where_expr {
        // func(col) = literal
        Expr::BinaryOp {
            op: BinaryOp::Eq,
            left,
            right,
        } => {
            let (lhs, rhs) = (left.as_ref(), right.as_ref());
            // Only handle func(col) = literal, not literal = func(col) (swap later).
            if matches!(lhs, Expr::Function { .. } | Expr::Cast { .. }) {
                (Some(lhs), Some(rhs), true)
            } else {
                return None;
            }
        }
        // LIKE pattern
        Expr::Like {
            expr,
            pattern,
            negated,
            ..
        } if !negated => {
            // Only handle non-negated LIKE.
            (Some(expr.as_ref()), Some(pattern.as_ref()), false)
        }
        _ => return None,
    };

    let (indexable_expr, literal_expr, is_eq) = match (indexable_expr, literal_expr) {
        (Some(a), Some(b)) => (a, b, is_eq),
        _ => return None,
    };

    // Extract the function/column expression.
    let indexable = extract_indexable_expr(indexable_expr)?;
    let normalized_query = normalize_expr_sql(indexable);
    if normalized_query.is_empty() {
        return None;
    }

    // Find an expression index whose stored SQL matches the query expression.
    let matching_idx = indexes.iter().find(|idx| {
        if idx.is_primary || idx.is_fk_index {
            return false;
        }
        // Expression indexes are always single-column.
        if idx.columns.len() != 1 {
            return false;
        }
        // Partial-index guard: expression indexes remain usable only when the
        // query WHERE implies the stored predicate.
        if let Some(pred_sql) = &idx.predicate {
            if !crate::partial_index::predicate_implied_by_query(pred_sql, query_where, columns) {
                return false;
            }
        }
        let idx_col = &idx.columns[0];
        let Some(stored_expr) = &idx_col.expr else {
            return false; // Not an expression index
        };
        // Normalize the stored expression for comparison.
        // The stored SQL may have mixed case (e.g., "LOWER(email)") so we
        // normalize both sides before comparing.
        let stored_normalized = stored_expr
            .to_lowercase()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>();
        let query_normalized: String = normalized_query
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        stored_normalized == query_normalized
    })?;

    // Extract the literal value from the RHS for encoding.
    let pat = match literal_expr {
        Expr::Literal(Value::Text(t)) => t,
        // JSON patterns for LIKE are not yet supported.
        Expr::Literal(Value::Json(_)) => return None,
        _ => return None,
    };

    if is_eq {
        // func(col) = 'literal' → point lookup.
        // We need to encode the literal as an index key.
        // The value type must match the expression's result type (TEXT).
        let literal_val = Value::Text(pat.clone());
        if let Ok(key) = encode_index_key(&[literal_val]) {
            return Some((matching_idx.clone(), Some(key.clone()), Some(key)));
        }
        None
    } else {
        // LIKE 'prefix%' → prefix range scan.
        // `pat` here is from the Value::Text branch; Value::Json is not supported for LIKE.
        if pat.ends_with('%') && !pat.starts_with('%') {
            // Prefix pattern: 'foo%' → lo='foo', hi='foo\xFF' (0xFF as last possible char)
            let prefix = pat.trim_end_matches('%');
            let lo_val = Value::Text(prefix.to_string());
            let hi_val = Value::Text(format!("{}\u{FF}", prefix));
            let lo = encode_index_key(&[lo_val]).ok();
            let hi = encode_index_key(&[hi_val]).ok();
            if lo.is_some() {
                return Some((matching_idx.clone(), lo, hi));
            }
        }
        // '%suffix' or '%infix%' — would need reverse index or full scan.
        // Return None to fall through to regular evaluation.
        None
    }
}

/// Returns the first usable index whose first column matches `col_name` and
/// whose partial index predicate (if any) is implied by the query WHERE.
///
/// `query_where` is the full WHERE clause of the query, used for partial index
/// predicate implication checking (Phase 6.7).
fn find_index_on_col<'a>(
    col_name: &str,
    indexes: &'a [IndexDef],
    columns: &[ColumnDef],
    query_where: Option<&Expr>,
    allow_primary: bool,
) -> Option<&'a IndexDef> {
    // Find the col_idx for this column name.
    let col_idx = columns.iter().find(|c| c.name == col_name)?.col_idx;

    // Find a non-primary index whose first column is this col_idx AND whose
    // predicate (if any) is implied by the query WHERE clause.
    indexes.iter().find(|idx| {
        if idx.is_primary && !allow_primary {
            return false;
        }
        // FK auto-indexes use composite keys (fk_val | RecordId) — never usable
        // for plain SELECT column = value lookups.
        if idx.is_fk_index {
            return false;
        }
        if idx.columns.first().map(|c| c.col_idx) != Some(col_idx) {
            return false;
        }
        // Partial index guard (Phase 6.7): only use if predicate is implied.
        if let Some(pred_sql) = &idx.predicate {
            crate::partial_index::predicate_implied_by_query(pred_sql, query_where, columns)
        } else {
            true // full index — always usable
        }
    })
}

// ── Helper: extract range predicate ──────────────────────────────────────────

/// Returns `(index, lo_value, hi_value)` if `expr` is `col > lo AND col < hi`
/// (or with `>=` / `<=`).
fn extract_range<'a>(
    expr: &Expr,
    indexes: &'a [IndexDef],
    columns: &[ColumnDef],
    query_where: Option<&Expr>,
    allow_primary: bool,
) -> Option<(&'a IndexDef, Option<Value>, Option<Value>)> {
    // expr must be `AND(left, right)`.
    let (lhs, rhs) = match expr {
        Expr::BinaryOp {
            op: BinaryOp::And,
            left,
            right,
        } => (left.as_ref(), right.as_ref()),
        _ => return None,
    };

    // Each side must be a comparison: col >/< literal.
    let (col1, bound1) = extract_range_side(lhs)?;
    let (col2, bound2) = extract_range_side(rhs)?;

    // Both sides must reference the same column.
    if col1 != col2 {
        return None;
    }

    let idx = find_index_on_col(col1, indexes, columns, query_where, allow_primary)?;
    // bound1 = lo side, bound2 = hi side (order may be loose but correct for 6.3)
    Some((idx, bound1, bound2))
}

/// Coerces a literal value to match the column's stored type so that the
/// encoded index key uses the same type tag as the stored key.
///
/// Without this coercion, a literal `0` (parsed as `Value::Int`) compared
/// against a `BIGINT` column (stored as `Value::BigInt`) would encode with
/// tag `0x02` instead of `0x03`, causing the B-Tree lookup to miss.
fn coerce_literal_to_col_type(value: Value, col_name: &str, columns: &[ColumnDef]) -> Value {
    use axiomdb_catalog::ColumnType;
    use axiomdb_types::{coerce, CoercionMode, DataType};

    let col = match columns.iter().find(|c| c.name == col_name) {
        Some(c) => c,
        None => return value,
    };
    let target = match col.col_type {
        ColumnType::Bool => DataType::Bool,
        ColumnType::TinyInt => DataType::TinyInt,
        ColumnType::SmallInt => DataType::SmallInt,
        ColumnType::Int => DataType::Int,
        ColumnType::BigInt => DataType::BigInt,
        ColumnType::Float32 => DataType::Float,
        ColumnType::Float => DataType::Real,
        ColumnType::Decimal => DataType::Decimal,
        ColumnType::Text => DataType::Text,
        ColumnType::Json => DataType::Json,
        ColumnType::Jsonb => DataType::Jsonb,
        ColumnType::Bytes => DataType::Bytes,
        ColumnType::Date => DataType::Date,
        ColumnType::Timestamp => DataType::Timestamp,
        ColumnType::Uuid => DataType::Uuid,
        ColumnType::Array => {
            let elem_ct = col.array_element_type.unwrap_or(ColumnType::Text);
            let elem_dt = crate::table::column_type_to_data_type(elem_ct);
            DataType::Array(Box::new(elem_dt))
        }
        ColumnType::Range => {
            // Range columns are not used as index scan keys; return value as-is.
            return value;
        }
        ColumnType::Money => DataType::Money,
        ColumnType::Composite => {
            return value;
        }
        ColumnType::Ltree => DataType::Ltree,
        ColumnType::Xml => DataType::Xml,
        ColumnType::TimestampTz => DataType::TimestampTz,
    };
    coerce(value.clone(), target, CoercionMode::Strict).unwrap_or(value)
}

/// Returns `(col_name, bound_value)` for range comparison operators.
fn extract_range_side(expr: &Expr) -> Option<(&str, Option<Value>)> {
    if let Expr::BinaryOp { op, left, right } = expr {
        match op {
            BinaryOp::Gt | BinaryOp::GtEq => {
                // col > literal  →  lo = literal
                if let (Expr::Column { name, .. }, Expr::Literal(v)) =
                    (left.as_ref(), right.as_ref())
                {
                    return Some((name.as_str(), Some(v.clone())));
                }
                // literal < col  →  lo = literal (mirrored)
                if let (Expr::Literal(v), Expr::Column { name, .. }) =
                    (left.as_ref(), right.as_ref())
                {
                    return Some((name.as_str(), Some(v.clone())));
                }
            }
            BinaryOp::Lt | BinaryOp::LtEq => {
                // col < literal  →  hi = literal
                if let (Expr::Column { name, .. }, Expr::Literal(v)) =
                    (left.as_ref(), right.as_ref())
                {
                    return Some((name.as_str(), Some(v.clone())));
                }
                // literal > col  →  hi = literal (mirrored)
                if let (Expr::Literal(v), Expr::Column { name, .. }) =
                    (left.as_ref(), right.as_ref())
                {
                    return Some((name.as_str(), Some(v.clone())));
                }
            }
            _ => {}
        }
    }
    None
}
