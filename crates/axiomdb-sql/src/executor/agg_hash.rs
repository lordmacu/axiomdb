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

// ── Hash aggregation ─────────────────────────────────────────────────────────

fn execute_select_grouped_hash(
    stmt: SelectStmt,
    combined_rows: Vec<Row>,
) -> Result<QueryResult, DbError> {
    // ── Pre-scan setup ────────────────────────────────────────────────────────

    let agg_exprs = collect_agg_exprs(&stmt.columns, &stmt.having);

    // Fast-path: detect when all GROUP BY exprs are simple column refs.
    let group_by_col_idxs: Option<Vec<usize>> = stmt
        .group_by
        .iter()
        .map(|e| match e {
            Expr::Column { col_idx, .. } => Some(*col_idx),
            _ => None,
        })
        .collect();

    // Compute which column indices are referenced by non-aggregate SELECT items
    // and HAVING. Only these need to be stored per group (sparse representation).
    let non_agg_col_indices = compute_non_agg_col_indices(&stmt);

    // Row length for virtual_row construction at finalization time.
    let row_len = combined_rows.first().map(|r| r.len()).unwrap_or(0);

    // ── Choose group table variant ────────────────────────────────────────────
    //
    // GroupTablePrimitive: single INT/BIGINT column GROUP BY.
    //   Key = native i64 — zero serialization, zero allocation per row.
    //   hashbrown memoizes the u64 hash in its raw table (DataFusion technique).
    //
    // GroupTableGeneric: all other cases.
    //   Key = Vec<u8> from value_to_session_key_bytes (collation-aware).
    //   hashbrown replaces std::HashMap: SIMD Robin Hood probing, ~20-40% faster.

    let use_primitive = group_by_col_idxs
        .as_ref()
        .map(|idxs| idxs.len() == 1)
        .unwrap_or(false)
        && combined_rows.first().map(|row| {
            let col_idx = group_by_col_idxs.as_ref().unwrap()[0];
            matches!(
                row.get(col_idx).unwrap_or(&Value::Null),
                Value::Int(_) | Value::BigInt(_) | Value::Null
            )
        }).unwrap_or(false);

    let mut table = if use_primitive {
        GroupTableKind::Primitive(GroupTablePrimitive::new())
    } else {
        GroupTableKind::Generic(GroupTableGeneric::new())
    };

    // Reused buffers — cleared each iteration, cloned only on new group.
    let mut key_buf: Vec<u8> = Vec::with_capacity(64);
    let mut key_values_buf: Vec<Value> = Vec::with_capacity(stmt.group_by.len().max(1));

    // ── One-pass scan ─────────────────────────────────────────────────────────

    for row in &combined_rows {
        // Evaluate GROUP BY expressions (fast-path: direct col_idx indexing).
        key_values_buf.clear();
        if let Some(ref idxs) = group_by_col_idxs {
            for &i in idxs {
                key_values_buf.push(row.get(i).cloned().unwrap_or(Value::Null));
            }
        } else {
            for e in &stmt.group_by {
                key_values_buf.push(eval(e, row)?);
            }
        }

        // Get-or-insert the group, then update its accumulators.
        let group_idx = match &mut table {
            GroupTableKind::Primitive(t) => {
                let col_idx = group_by_col_idxs.as_ref().unwrap()[0];
                let key = match row.get(col_idx).unwrap_or(&Value::Null) {
                    Value::Int(n) => Some(*n as i64),
                    Value::BigInt(n) => Some(*n),
                    _ => None,
                };
                t.get_or_insert(
                    key,
                    key_values_buf[0].clone(),
                    &agg_exprs,
                    &non_agg_col_indices,
                    row,
                )
            }
            GroupTableKind::Generic(t) => {
                // Serialize key into reused buffer (no allocation if capacity fits).
                key_buf.clear();
                for v in &key_values_buf {
                    key_buf.extend_from_slice(&value_to_session_key_bytes(v));
                }
                t.get_or_insert(
                    &key_buf,
                    key_values_buf.clone(),
                    &agg_exprs,
                    &non_agg_col_indices,
                    row,
                )
            }
        };

        // Update accumulators for this group (zero allocation for existing groups).
        let entries = table.entries_mut();
        for (acc, agg) in entries[group_idx].accumulators.iter_mut().zip(&agg_exprs) {
            acc.update(row, agg)?;
        }
    }

    // ── Ungrouped aggregate: emit one group even with no input rows ───────────
    // e.g., SELECT COUNT(*) FROM empty_table → [(0)], not 0 rows.

    if stmt.group_by.is_empty() && table.entries_mut().is_empty() {
        let entries = table.entries_mut();
        entries.push(GroupEntry {
            key_values: vec![],
            non_agg_col_values: vec![],
            accumulators: agg_exprs.iter().map(AggAccumulator::new).collect(),
        });
    }

    // ── Finalize ──────────────────────────────────────────────────────────────

    let out_cols = build_grouped_column_meta(&stmt.columns, &agg_exprs)?;

    let mut rows: Vec<Row> = Vec::new();
    for entry in table.into_entries() {
        let agg_values: Vec<Value> = entry
            .accumulators
            .into_iter()
            .map(|acc| acc.finalize())
            .collect::<Result<_, _>>()?;

        // Reconstruct a virtual row for eval_with_aggs / project_grouped_row.
        // Only the columns in non_agg_col_indices have real values; others are Null.
        // This preserves the existing function signatures without change.
        let virtual_row = build_virtual_row(
            &entry.non_agg_col_values,
            &non_agg_col_indices,
            row_len,
        );

        if let Some(ref having) = stmt.having {
            let v = eval_with_aggs(having, &virtual_row, &agg_values, &agg_exprs)?;
            if !is_truthy(&v) {
                continue;
            }
        }

        let out_row =
            project_grouped_row(&stmt.columns, &virtual_row, &agg_values, &agg_exprs)?;
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

/// Builds a virtual row of length `row_len` filled with `Value::Null`, then
/// fills in the stored `non_agg_col_values` at their original column indices.
///
/// This allows `eval_with_aggs` and `project_grouped_row` to use the existing
/// `col_idx`-based lookup without any signature change.
#[inline]
fn build_virtual_row(
    non_agg_col_values: &[Value],
    non_agg_col_indices: &[usize],
    row_len: usize,
) -> Vec<Value> {
    let mut vrow = vec![Value::Null; row_len];
    for (val, &idx) in non_agg_col_values.iter().zip(non_agg_col_indices) {
        if idx < vrow.len() {
            vrow[idx] = val.clone();
        }
    }
    vrow
}
