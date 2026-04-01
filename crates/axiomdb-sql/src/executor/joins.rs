/// Minimum row count to trigger hash join. Below this, nested loop is faster
/// (HashMap construction overhead > n×m comparisons for small n,m).
const HASH_JOIN_MIN_ROWS: usize = 32;

// ── Adaptive Join Selection (Phase 9.9) ─────────────────────────────────────
//
// Cost-based selection inspired by PostgreSQL joinpath.c:
//
//   1. CROSS JOIN / non-equijoin → always nested loop (no alternative)
//   2. Equijoin + both sides < 32 rows → nested loop (hash overhead not worth it)
//   3. Equijoin + INNER/LEFT/RIGHT → hash join (O(n+m), build smaller side)
//   4. Equijoin + FULL OUTER → hash join with matched-bitmap tracking
//   5. Sort-merge: available but not auto-selected yet (requires presorted detection)
//
// The selection happens at the top of apply_join based on:
//   - Join type (INNER/LEFT/RIGHT/FULL/CROSS)
//   - Condition type (equijoin detected by detect_equijoin)
//   - Table sizes (left_rows.len(), right_rows.len())

#[allow(clippy::too_many_arguments)]
fn apply_join(
    left_rows: Vec<Row>,
    right_rows: &[Row],
    left_col_count: usize,
    right_col_count: usize,
    join_type: JoinType,
    condition: &JoinCondition,
    left_schema: &[(String, usize)],
    right_col_offset: usize,
    right_columns: &[axiomdb_catalog::schema::ColumnDef],
) -> Result<Vec<Row>, DbError> {
    // Phase 9.9: Adaptive join selection.
    let is_large = left_rows.len() >= HASH_JOIN_MIN_ROWS
        || right_rows.len() >= HASH_JOIN_MIN_ROWS;

    if is_large {
        if let Some((l_idx, r_idx)) = detect_equijoin(condition, left_col_count) {
            match join_type {
                JoinType::Inner => {
                    return Ok(hash_join_inner(&left_rows, right_rows, l_idx, r_idx));
                }
                JoinType::Left => {
                    return Ok(hash_join_left(
                        &left_rows, right_rows, l_idx, r_idx, right_col_count,
                    ));
                }
                JoinType::Right => {
                    // RIGHT = LEFT with swapped sides + column reorder.
                    let swapped = hash_join_left(
                        right_rows, &left_rows, r_idx, l_idx, left_col_count,
                    );
                    return Ok(swap_columns(swapped, right_col_count, left_col_count));
                }
                JoinType::Full => {
                    return Ok(hash_join_full(
                        &left_rows, right_rows, l_idx, r_idx,
                        left_col_count, right_col_count,
                    ));
                }
                JoinType::Cross => {} // no equijoin for CROSS
            }
        }
    }

    match join_type {
        JoinType::Inner | JoinType::Cross => {
            let mut result = Vec::new();
            for left in &left_rows {
                for right in right_rows {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                    }
                }
            }
            Ok(result)
        }

        JoinType::Left => {
            let null_right: Row = vec![Value::Null; right_col_count];
            let mut result = Vec::new();
            for left in &left_rows {
                let mut matched = false;
                for right in right_rows {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                        matched = true;
                    }
                }
                if !matched {
                    result.push(concat_rows(left, &null_right));
                }
            }
            Ok(result)
        }

        JoinType::Right => {
            let null_left: Row = vec![Value::Null; left_col_count];
            let mut matched_right = vec![false; right_rows.len()];
            let mut result = Vec::new();

            for left in &left_rows {
                for (i, right) in right_rows.iter().enumerate() {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                        matched_right[i] = true;
                    }
                }
            }
            // Emit unmatched right rows with NULLs on the left side.
            for (i, right) in right_rows.iter().enumerate() {
                if !matched_right[i] {
                    result.push(concat_rows(&null_left, right));
                }
            }
            Ok(result)
        }

        JoinType::Full => {
            // FULL OUTER JOIN = matched pairs + unmatched left rows (NULL right)
            //                 + unmatched right rows (NULL left).
            //
            // A matched-right bitmap tracks which right rows were joined so the
            // second pass can emit the unmatched ones without duplicating them.
            let null_left: Row = vec![Value::Null; left_col_count];
            let null_right: Row = vec![Value::Null; right_col_count];
            let mut matched_right = vec![false; right_rows.len()];
            let mut result = Vec::new();

            for left in &left_rows {
                let mut matched = false;
                for (i, right) in right_rows.iter().enumerate() {
                    let combined = concat_rows(left, right);
                    if eval_join_cond(
                        condition,
                        &combined,
                        left_schema,
                        right_col_offset,
                        right_columns,
                    )? {
                        result.push(combined);
                        matched = true;
                        matched_right[i] = true;
                    }
                }
                if !matched {
                    // Left row had no match — emit with NULLs on the right side.
                    result.push(concat_rows(left, &null_right));
                }
            }

            // Emit right rows that were never matched with NULLs on the left side.
            for (i, right) in right_rows.iter().enumerate() {
                if !matched_right[i] {
                    result.push(concat_rows(&null_left, right));
                }
            }

            Ok(result)
        }
    }
}

/// Evaluates a join condition against a combined row.
///
/// - `On(expr)`: evaluates the expression directly (`col_idx` already resolved by analyzer).
/// - `Using(names)`: for each name, finds its index in the left schema and in the right
///   table, then checks equality. NULL = NULL is UNKNOWN (returns false per SQL semantics).
///
/// `left_schema` is a `(column_name, global_col_idx)` list for every column in the
/// accumulated left side of this join stage.
fn eval_join_cond(
    cond: &JoinCondition,
    combined: &[Value],
    left_schema: &[(String, usize)],
    right_col_offset: usize,
    right_columns: &[axiomdb_catalog::schema::ColumnDef],
) -> Result<bool, DbError> {
    match cond {
        JoinCondition::On(expr) => Ok(is_truthy(&eval(expr, combined)?)),

        JoinCondition::Using(names) => {
            for col_name in names {
                // Find col_idx in the accumulated left schema.
                let left_idx = left_schema
                    .iter()
                    .find(|(name, _)| name == col_name)
                    .map(|(_, idx)| *idx)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: col_name.clone(),
                        table: "left side (USING)".into(),
                    })?;

                // Find col_idx in the right table.
                let right_pos = right_columns
                    .iter()
                    .position(|c| &c.name == col_name)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        name: col_name.clone(),
                        table: "right table (USING)".into(),
                    })?;
                let right_idx = right_col_offset + right_pos;

                let left_val = combined
                    .get(left_idx)
                    .ok_or(DbError::ColumnIndexOutOfBounds {
                        idx: left_idx,
                        len: combined.len(),
                    })?;
                let right_val = combined
                    .get(right_idx)
                    .ok_or(DbError::ColumnIndexOutOfBounds {
                        idx: right_idx,
                        len: combined.len(),
                    })?;

                // NULL = NULL is UNKNOWN in SQL 3-valued logic — no match.
                if matches!(left_val, Value::Null) || matches!(right_val, Value::Null) {
                    return Ok(false);
                }
                if left_val != right_val {
                    return Ok(false);
                }
            }
            Ok(true)
        }
    }
}

// ── Hash Join (Phase 9.6) ───────────────────────────────────────────────────

/// Attempts to detect an equijoin condition and extract the left/right column
/// indices. Returns `None` for non-equijoin conditions (OR, complex exprs, etc.).
///
/// Supports: `ON a.col = b.col` where both sides are simple column references.
/// The analyzer resolves these to `Expr::Column { col_idx }` in the combined row.
fn detect_equijoin(cond: &JoinCondition, left_col_count: usize) -> Option<(usize, usize)> {
    match cond {
        JoinCondition::On(Expr::BinaryOp {
            op: BinaryOp::Eq,
            left,
            right,
        }) => {
            let (l_idx, r_idx) = match (left.as_ref(), right.as_ref()) {
                (Expr::Column { col_idx: l, .. }, Expr::Column { col_idx: r, .. }) => (*l, *r),
                _ => return None,
            };
            // Left column must be from left table, right from right table.
            if l_idx < left_col_count && r_idx >= left_col_count {
                Some((l_idx, r_idx))
            } else if r_idx < left_col_count && l_idx >= left_col_count {
                Some((r_idx, l_idx)) // swapped
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Hash join for INNER JOIN with equijoin condition: O(n + m) instead of O(n × m).
///
/// **Build side selection** (PostgreSQL pattern): always hash the SMALLER table
/// to minimize memory usage and hash table size. The larger table becomes the
/// probe side. For INNER JOIN the result is commutative — row order in output
/// may differ but the set of matched pairs is identical.
///
/// Inspired by DuckDB's `PhysicalHashJoin` and PostgreSQL's `nodeHashjoin.c`.
fn hash_join_inner(
    left_rows: &[Row],
    right_rows: &[Row],
    left_key_idx: usize,
    right_key_idx: usize,
) -> Vec<Row> {
    use std::collections::HashMap;

    // PostgreSQL optimization: build on STRICTLY smaller side, probe on larger.
    // Equal sizes → build right (deterministic, matches query order).
    let (build_rows, build_key, probe_rows, probe_key, build_is_left) =
        if left_rows.len() < right_rows.len() {
            (left_rows, left_key_idx, right_rows, right_key_idx, true)
        } else {
            (right_rows, right_key_idx, left_rows, left_key_idx, false)
        };

    // Build phase: hash smaller table by join key.
    let mut ht: HashMap<HashableValue, Vec<usize>> = HashMap::with_capacity(build_rows.len());
    for (i, row) in build_rows.iter().enumerate() {
        if let Some(key) = row.get(build_key) {
            if !matches!(key, Value::Null) {
                ht.entry(HashableValue(key.clone())).or_default().push(i);
            }
        }
    }

    // Probe phase: look up each larger-table row's key in the hash table.
    let mut result = Vec::new();
    for probe in probe_rows {
        if let Some(key) = probe.get(probe_key) {
            if matches!(key, Value::Null) {
                continue;
            }
            if let Some(indices) = ht.get(&HashableValue(key.clone())) {
                for &bi in indices {
                    // Maintain left-right column order regardless of build side.
                    if build_is_left {
                        result.push(concat_rows(&build_rows[bi], probe));
                    } else {
                        result.push(concat_rows(probe, &build_rows[bi]));
                    }
                }
            }
        }
    }
    result
}

/// Hash join for LEFT JOIN: all left rows appear; unmatched get NULL right side.
///
/// PostgreSQL optimization: build hash table on the RIGHT side (which may be
/// smaller), probe with LEFT. For LEFT JOIN we cannot swap sides freely —
/// every left row must appear in the output. So we always probe with left
/// and build on right.
fn hash_join_left(
    left_rows: &[Row],
    right_rows: &[Row],
    left_key_idx: usize,
    right_key_idx: usize,
    right_col_count: usize,
) -> Vec<Row> {
    use std::collections::HashMap;

    // Build on right side (LEFT JOIN semantics require probing every left row).
    let mut ht: HashMap<HashableValue, Vec<usize>> = HashMap::with_capacity(right_rows.len());
    for (i, row) in right_rows.iter().enumerate() {
        if let Some(key) = row.get(right_key_idx) {
            if !matches!(key, Value::Null) {
                ht.entry(HashableValue(key.clone())).or_default().push(i);
            }
        }
    }

    let null_right: Row = vec![Value::Null; right_col_count];
    let mut result = Vec::new();
    for left in left_rows {
        let key = match left.get(left_key_idx) {
            Some(k) if !matches!(k, Value::Null) => k,
            _ => {
                // NULL key or missing → never matches, emit with NULL right.
                result.push(concat_rows(left, &null_right));
                continue;
            }
        };
        if let Some(indices) = ht.get(&HashableValue(key.clone())) {
            for &ri in indices {
                result.push(concat_rows(left, &right_rows[ri]));
            }
        } else {
            result.push(concat_rows(left, &null_right));
        }
    }
    result
}

/// Hash join for FULL OUTER JOIN: matched pairs + unmatched left (NULL right)
/// + unmatched right (NULL left). Uses a matched-right bitmap like PostgreSQL.
fn hash_join_full(
    left_rows: &[Row],
    right_rows: &[Row],
    left_key_idx: usize,
    right_key_idx: usize,
    left_col_count: usize,
    right_col_count: usize,
) -> Vec<Row> {
    use std::collections::HashMap;

    // Build on right side (need matched bitmap for unmatched-right emission).
    let mut ht: HashMap<HashableValue, Vec<usize>> = HashMap::with_capacity(right_rows.len());
    for (i, row) in right_rows.iter().enumerate() {
        if let Some(key) = row.get(right_key_idx) {
            if !matches!(key, Value::Null) {
                ht.entry(HashableValue(key.clone())).or_default().push(i);
            }
        }
    }

    let null_left: Row = vec![Value::Null; left_col_count];
    let null_right: Row = vec![Value::Null; right_col_count];
    let mut matched_right = vec![false; right_rows.len()];
    let mut result = Vec::new();

    // Probe: emit matched pairs + unmatched left rows.
    for left in left_rows {
        if let Some(key) = left.get(left_key_idx) {
            if !matches!(key, Value::Null) {
                if let Some(indices) = ht.get(&HashableValue(key.clone())) {
                    for &ri in indices {
                        result.push(concat_rows(left, &right_rows[ri]));
                        matched_right[ri] = true;
                    }
                    continue;
                }
            }
        }
        // Unmatched left → NULL right side.
        result.push(concat_rows(left, &null_right));
    }

    // Emit unmatched right rows → NULL left side.
    for (i, right) in right_rows.iter().enumerate() {
        if !matched_right[i] {
            result.push(concat_rows(&null_left, right));
        }
    }

    result
}

/// Reorders columns in rows from [right_cols, left_cols] to [left_cols, right_cols].
/// Used by RIGHT JOIN hash path (swap sides then fix column order).
fn swap_columns(rows: Vec<Row>, right_count: usize, left_count: usize) -> Vec<Row> {
    rows.into_iter()
        .map(|row| {
            let mut fixed = Vec::with_capacity(row.len());
            fixed.extend_from_slice(&row[right_count..right_count + left_count]);
            fixed.extend_from_slice(&row[..right_count]);
            fixed
        })
        .collect()
}

/// Wrapper for `Value` that implements `Hash + Eq` for use as HashMap key.
/// NaN handling: f64 NaN is treated as equal to itself (not IEEE-compliant,
/// but correct for SQL grouping semantics where NaN = NaN within a group).
#[derive(Clone)]
struct HashableValue(Value);

impl std::hash::Hash for HashableValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(&self.0).hash(state);
        match &self.0 {
            Value::Null => {}
            Value::Bool(b) => b.hash(state),
            Value::Int(n) => n.hash(state),
            Value::BigInt(n) => n.hash(state),
            Value::Real(f) => f.to_bits().hash(state),
            Value::Text(s) => s.hash(state),
            Value::Bytes(b) => b.hash(state),
            Value::Decimal(m, s) => {
                m.hash(state);
                s.hash(state);
            }
            Value::Date(d) => d.hash(state),
            Value::Timestamp(t) => t.hash(state),
            Value::Uuid(u) => u.hash(state),
        }
    }
}

impl PartialEq for HashableValue {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for HashableValue {}

// ── Sort-Merge Join (Phase 9.7) ─────────────────────────────────────────────

/// Sort-merge join for INNER JOIN: sort both sides by join key, then merge.
/// O(n log n + m log m) — optimal when data is already sorted (e.g., from index).
///
/// PostgreSQL uses this when both sides can deliver sorted output
/// (nodeMergejoin.c). For AxiomDB, this is a fallback when hash join is
/// not preferred (e.g., very large tables that don't fit in memory).
#[allow(dead_code)] // Available for Phase 9.9 (adaptive join selection)
fn sort_merge_join_inner(
    left_rows: &mut [Row],
    right_rows: &mut [Row],
    left_key_idx: usize,
    right_key_idx: usize,
) -> Vec<Row> {
    // Sort both sides by join key.
    left_rows.sort_by(|a, b| cmp_values_for_join(&a[left_key_idx], &b[left_key_idx]));
    right_rows.sort_by(|a, b| cmp_values_for_join(&a[right_key_idx], &b[right_key_idx]));

    let mut result = Vec::new();
    let mut ri = 0;

    for left in left_rows.iter() {
        let lk = &left[left_key_idx];
        if matches!(lk, Value::Null) {
            continue;
        }

        // Advance right pointer past smaller keys.
        while ri < right_rows.len() {
            let rk = &right_rows[ri][right_key_idx];
            if matches!(rk, Value::Null) {
                ri += 1;
                continue;
            }
            if cmp_values_for_join(rk, lk) == std::cmp::Ordering::Less {
                ri += 1;
            } else {
                break;
            }
        }

        // Emit all right rows with equal key (handle duplicates — mark and restore).
        let mark = ri;
        while ri < right_rows.len() {
            let rk = &right_rows[ri][right_key_idx];
            if cmp_values_for_join(rk, lk) != std::cmp::Ordering::Equal {
                break;
            }
            result.push(concat_rows(left, &right_rows[ri]));
            ri += 1;
        }
        // Restore to mark for next left row with same key (PostgreSQL mark/restore pattern).
        ri = mark;
    }

    result
}

/// Sort-merge join for LEFT JOIN: all left rows appear, unmatched get NULLs.
#[allow(dead_code)] // Available for Phase 9.9 (adaptive join selection)
fn sort_merge_join_left(
    left_rows: &mut [Row],
    right_rows: &mut [Row],
    left_key_idx: usize,
    right_key_idx: usize,
    right_col_count: usize,
) -> Vec<Row> {
    left_rows.sort_by(|a, b| cmp_values_for_join(&a[left_key_idx], &b[left_key_idx]));
    right_rows.sort_by(|a, b| cmp_values_for_join(&a[right_key_idx], &b[right_key_idx]));

    let null_right: Row = vec![Value::Null; right_col_count];
    let mut result = Vec::new();
    let mut ri = 0;

    for left in left_rows.iter() {
        let lk = &left[left_key_idx];
        if matches!(lk, Value::Null) {
            result.push(concat_rows(left, &null_right));
            continue;
        }

        while ri < right_rows.len() {
            let rk = &right_rows[ri][right_key_idx];
            if matches!(rk, Value::Null) {
                ri += 1;
                continue;
            }
            if cmp_values_for_join(rk, lk) == std::cmp::Ordering::Less {
                ri += 1;
            } else {
                break;
            }
        }

        let mark = ri;
        let mut matched = false;
        while ri < right_rows.len() {
            let rk = &right_rows[ri][right_key_idx];
            if cmp_values_for_join(rk, lk) != std::cmp::Ordering::Equal {
                break;
            }
            result.push(concat_rows(left, &right_rows[ri]));
            matched = true;
            ri += 1;
        }
        // PostgreSQL optimization: only restore to mark if we matched something.
        // If no match, ri is already past the non-equal region — advancing is correct.
        if matched {
            ri = mark;
        }

        if !matched {
            result.push(concat_rows(left, &null_right));
        }
    }

    result
}

/// Compares two Values for sort-merge join ordering.
/// NULL is ordered last (greatest) to match SQL semantics.
#[allow(dead_code)]
fn cmp_values_for_join(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::BigInt(a), Value::BigInt(b)) => a.cmp(b),
        (Value::Real(a), Value::Real(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Date(a), Value::Date(b)) => a.cmp(b),
        (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
        (Value::Decimal(am, _as), Value::Decimal(bm, _bs)) => am.cmp(bm),
        _ => Ordering::Equal, // mixed types: treat as equal (shouldn't happen in well-typed SQL)
    }
}

/// Concatenates two row slices into a new combined row.
#[inline]
fn concat_rows(left: &[Value], right: &[Value]) -> Row {
    let mut combined = Vec::with_capacity(left.len() + right.len());
    combined.extend_from_slice(left);
    combined.extend_from_slice(right);
    combined
}

/// Builds `ColumnMeta` for the output of a JOIN query.
fn build_join_column_meta(
    items: &[SelectItem],
    all_tables: &[axiomdb_catalog::ResolvedTable],
    joins: &[JoinClause],
) -> Result<Vec<ColumnMeta>, DbError> {
    // Precompute outer-join nullability once for the whole chain.
    // This correctly handles LEFT, RIGHT, FULL, and mixed chains.
    let nullable_tables = compute_outer_nullable(all_tables.len(), joins);
    let mut out = Vec::new();

    for item in items {
        match item {
            SelectItem::Wildcard => {
                // Expand all columns from all tables in order.
                for (t_idx, table) in all_tables.iter().enumerate() {
                    let outer_nullable = nullable_tables[t_idx];
                    for col in &table.columns {
                        out.push(ColumnMeta {
                            name: col.name.clone(),
                            data_type: column_type_to_datatype(col.col_type),
                            nullable: col.nullable || outer_nullable,
                            table_name: Some(table.def.table_name.clone()),
                        });
                    }
                }
            }

            SelectItem::QualifiedWildcard(qualifier) => {
                // Expand only the columns from the matching table.
                let t_idx = all_tables
                    .iter()
                    .position(|t| t.def.table_name == *qualifier || t.def.schema_name == *qualifier)
                    .ok_or_else(|| DbError::TableNotFound {
                        name: qualifier.clone(),
                    })?;
                let table = &all_tables[t_idx];
                let outer_nullable = nullable_tables[t_idx];
                for col in &table.columns {
                    out.push(ColumnMeta {
                        name: col.name.clone(),
                        data_type: column_type_to_datatype(col.col_type),
                        nullable: col.nullable || outer_nullable,
                        table_name: Some(table.def.table_name.clone()),
                    });
                }
            }

            SelectItem::Expr { expr, alias } => {
                let name = expr_column_name(expr, alias.as_deref());
                // Infer type: plain column reference uses catalog type; others use Text fallback.
                let (dt, nullable) = infer_expr_type_join(expr, all_tables, &nullable_tables);
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

/// Computes per-table outer-join nullability for a join chain.
///
/// Returns a `Vec<bool>` of length `table_count` where `[i]` is `true` if
/// table `i` can be null-extended by any join in the chain:
///
/// - `LEFT JOIN`: the right table becomes nullable.
/// - `RIGHT JOIN`: all accumulated left tables (0..=join_idx) become nullable.
/// - `FULL JOIN`: both sides become nullable.
/// - `INNER` / `CROSS`: no side becomes nullable.
///
/// This replaces the old `is_outer_nullable(t_idx, joins)` helper which only
/// looked at a single join and therefore produced wrong metadata for mixed
/// outer-join chains and for `FULL JOIN`.
fn compute_outer_nullable(table_count: usize, joins: &[JoinClause]) -> Vec<bool> {
    let mut nullable = vec![false; table_count];
    for (join_idx, join) in joins.iter().enumerate() {
        let right_table = join_idx + 1;
        match join.join_type {
            JoinType::Inner | JoinType::Cross => {}
            JoinType::Left => {
                if right_table < table_count {
                    nullable[right_table] = true;
                }
            }
            JoinType::Right => {
                nullable[..right_table.min(table_count)].fill(true);
            }
            JoinType::Full => {
                nullable[..right_table.min(table_count)].fill(true);
                if right_table < table_count {
                    nullable[right_table] = true;
                }
            }
        }
    }
    nullable
}

/// Infers (DataType, nullable) for an expression in a JOIN context.
fn infer_expr_type_join(
    expr: &Expr,
    all_tables: &[axiomdb_catalog::ResolvedTable],
    nullable_tables: &[bool],
) -> (DataType, bool) {
    if let Expr::Column { col_idx, .. } = expr {
        // Find which table owns this col_idx and what the column type is.
        let mut offset = 0;
        for (t_idx, table) in all_tables.iter().enumerate() {
            let end = offset + table.columns.len();
            if *col_idx < end {
                let local_pos = col_idx - offset;
                if let Some(col) = table.columns.get(local_pos) {
                    let outer_nullable = nullable_tables.get(t_idx).copied().unwrap_or(false);
                    let nullable = col.nullable || outer_nullable;
                    return (column_type_to_datatype(col.col_type), nullable);
                }
            }
            offset = end;
        }
    }
    (DataType::Text, true) // safe fallback for computed expressions
}
