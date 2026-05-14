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
        // GIN scan delivers rows in arbitrary RID order — always hash-group.
        crate::planner::AccessMethod::GinScan { .. } | crate::planner::AccessMethod::Scan => {
            return GroupByStrategy::Hash
        }
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
    mut stmt: SelectStmt,
    combined_rows: Vec<Row>,
    strategy: GroupByStrategy,
) -> Result<QueryResult, DbError> {
    use crate::ast::GroupByClause;

    // Resolve positional GROUP BY (e.g. GROUP BY 1) to SELECT expressions (G8).
    {
        let resolved = resolve_positional_group_by(stmt.group_by.exprs(), &stmt.columns);
        stmt.group_by = match stmt.group_by {
            GroupByClause::Simple(_) => GroupByClause::Simple(resolved),
            GroupByClause::WithRollup(_) => GroupByClause::WithRollup(resolved),
            GroupByClause::Sets { sets, .. } => GroupByClause::Sets { universe: resolved, sets },
            GroupByClause::None => GroupByClause::None,
        };
    }
    // Resolve positional ORDER BY (e.g. ORDER BY 2) to SELECT expressions (G8).
    stmt.order_by = resolve_positional_order_by(&stmt.order_by, &stmt.columns);

    match &stmt.group_by {
        GroupByClause::WithRollup(_) => {
            execute_select_grouped_rollup(stmt, combined_rows, strategy)
        }
        GroupByClause::Sets { .. } => {
            execute_select_grouped_sets(stmt, combined_rows)
        }
        _ => match strategy {
            GroupByStrategy::Hash => execute_select_grouped_hash(stmt, combined_rows),
            GroupByStrategy::Sorted { presorted } => {
                execute_select_grouped_sorted(stmt, combined_rows, presorted)
            }
        },
    }
}

/// Executes `GROUP BY ... WITH ROLLUP` (GAP-C.5).
///
/// For `GROUP BY c1, c2, ..., cN WITH ROLLUP`, produces rows at every grouping
/// level from full keys down to the grand total:
///
/// - Level N (full)   — normal groups
/// - Level N-1        — subtotal per (c1..c_{N-1}); `cN` emitted as NULL
/// - ...
/// - Level 0          — grand total; every group key emitted as NULL
///
/// Strategy: run the underlying grouped executor once per level with a
/// progressively truncated `group_by`; then null out SELECT output positions
/// that correspond to the rolled-up group-by expressions. Top-level
/// ORDER BY / LIMIT / OFFSET / DISTINCT are deferred so they apply to the
/// union of all levels (matching MySQL semantics).
fn execute_select_grouped_rollup(
    stmt: SelectStmt,
    combined_rows: Vec<Row>,
    strategy: GroupByStrategy,
) -> Result<QueryResult, DbError> {
    use crate::ast::GroupByClause;

    // Extract the expression list from the WithRollup variant.
    let exprs = match &stmt.group_by {
        GroupByClause::WithRollup(v) => v.clone(),
        _ => unreachable!("execute_select_grouped_rollup called without WithRollup"),
    };
    let n = exprs.len();
    if n == 0 {
        // `GROUP BY () WITH ROLLUP` degenerates to a single grand-total row.
        let mut stripped = stmt.clone();
        stripped.group_by = GroupByClause::None;
        return execute_select_grouped_hash(stripped, combined_rows);
    }

    // Precompute which SELECT item positions correspond to each group_by expr.
    // Used to null-out rolled-up keys at each level.
    let select_slot_for_gb: Vec<Option<usize>> = exprs
        .iter()
        .map(|gb| {
            stmt.columns.iter().position(|item| match item {
                SelectItem::Expr { expr, .. } => expr == gb,
                _ => false,
            })
        })
        .collect();

    let out_cols_template = {
        // Borrow the column meta from a single-level run (shape is identical
        // across all levels — only values change).
        let mut probe = stmt.clone();
        probe.group_by = GroupByClause::Simple(exprs.clone());
        probe.order_by.clear();
        probe.limit = None;
        probe.offset = None;
        probe.distinct = false;
        probe.calc_found_rows = false;
        let res = match strategy {
            GroupByStrategy::Hash => execute_select_grouped_hash(probe, combined_rows.clone())?,
            GroupByStrategy::Sorted { presorted } => {
                execute_select_grouped_sorted(probe, combined_rows.clone(), presorted)?
            }
        };
        match res {
            QueryResult::Rows { columns, rows: _ } => columns,
            other => return Ok(other),
        }
    };

    // Re-run per level, from full keys down to zero.
    let mut all_rows: Vec<Row> = Vec::new();
    for k in (0..=n).rev() {
        let mut level_stmt = stmt.clone();
        let level_exprs = exprs[..k].to_vec();
        level_stmt.group_by = if level_exprs.is_empty() {
            GroupByClause::None
        } else {
            GroupByClause::Simple(level_exprs)
        };
        level_stmt.order_by.clear();
        level_stmt.limit = None;
        level_stmt.offset = None;
        level_stmt.distinct = false;
        level_stmt.calc_found_rows = false;

        let level_res = match strategy {
            GroupByStrategy::Hash => {
                execute_select_grouped_hash(level_stmt, combined_rows.clone())?
            }
            GroupByStrategy::Sorted { presorted } => {
                execute_select_grouped_sorted(level_stmt, combined_rows.clone(), presorted)?
            }
        };
        let mut level_rows = match level_res {
            QueryResult::Rows { rows, .. } => rows,
            other => return Ok(other),
        };

        // Null out SELECT positions for rolled-up group-by exprs (indices k..n).
        if k < n {
            for row in level_rows.iter_mut() {
                for slot in select_slot_for_gb.iter().take(n).skip(k).flatten() {
                    if *slot < row.len() {
                        row[*slot] = Value::Null;
                    }
                }
            }
        }
        all_rows.extend(level_rows);
    }

    // Outer DISTINCT / ORDER BY / LIMIT apply to the union of all levels.
    if stmt.distinct {
        all_rows = apply_distinct_with_session(all_rows);
    }
    let remapped_ob = remap_order_by_for_grouped(&stmt.order_by, &stmt.columns);
    all_rows = apply_order_by(all_rows, &remapped_ob)?;
    if stmt.calc_found_rows {
        set_found_rows(all_rows.len() as u64);
    }
    all_rows = apply_limit_offset(all_rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols_template,
        rows: all_rows,
    })
}

// ── GROUPING SETS executor (Phase 21.21) ─────────────────────────────────────

/// Executes `GROUP BY ROLLUP(...) / CUBE(...) / GROUPING SETS(...)`.
///
/// For each grouping set: builds a simple `SELECT … GROUP BY <set_exprs>`,
/// runs the hash aggregator (HAVING applied per-pass per SQL standard), nulls
/// out SELECT positions for universe exprs absent from the set, then appends
/// a hidden `Value::BigInt(mask)` where bit `i` = 1 iff `universe[i]` is absent.
/// All per-set rows are unioned, ORDER BY runs (GROUPING() can read the mask),
/// the mask is stripped, then LIMIT / OFFSET are applied.
fn execute_select_grouped_sets(
    stmt: SelectStmt,
    combined_rows: Vec<Row>,
) -> Result<QueryResult, DbError> {
    use crate::ast::GroupByClause;

    let (universe, sets) = match &stmt.group_by {
        GroupByClause::Sets { universe, sets } => (universe.clone(), sets.clone()),
        _ => unreachable!("execute_select_grouped_sets called without GroupByClause::Sets"),
    };

    let n_universe = universe.len();

    // Precompute: for each universe[i], which SELECT output positions map to it?
    // A universe expression may appear multiple times in the SELECT list.
    let select_slots_for_universe: Vec<Vec<usize>> = universe
        .iter()
        .map(|ue| {
            stmt.columns
                .iter()
                .enumerate()
                .filter_map(|(pos, item)| match item {
                    SelectItem::Expr { expr, .. } if expr == ue => Some(pos),
                    _ => None,
                })
                .collect()
        })
        .collect();

    // Get column meta from a probe run (use the first non-empty set, or grand total).
    let probe_set = sets
        .iter()
        .find(|s| !s.is_empty())
        .or_else(|| sets.first())
        .cloned()
        .unwrap_or_default();
    let probe_exprs: Vec<Expr> = probe_set.iter().map(|&i| universe[i].clone()).collect();
    let mut probe_stmt = stmt.clone();
    probe_stmt.group_by = if probe_exprs.is_empty() {
        GroupByClause::None
    } else {
        GroupByClause::Simple(probe_exprs)
    };
    probe_stmt.order_by.clear();
    probe_stmt.limit = None;
    probe_stmt.offset = None;
    probe_stmt.distinct = false;
    probe_stmt.calc_found_rows = false;
    let out_cols_template = match execute_select_grouped_hash(probe_stmt, combined_rows.clone())? {
        QueryResult::Rows { columns, .. } => columns,
        other => return Ok(other),
    };

    // If HAVING contains GROUPING() calls, the mask must be present when HAVING
    // is evaluated. The mask is appended AFTER the hash executor runs, so we
    // cannot pass such HAVING to the per-pass hash executor. Instead we strip
    // HAVING from the per-pass stmt and apply it post-mask in the post-union step.
    let having_has_grouping = stmt
        .having
        .as_ref()
        .map(expr_contains_grouping)
        .unwrap_or(false);

    // Precompute: which SELECT output positions hold a GROUPING() expression?
    // These positions are filled with 0 by the hash executor (no mask yet).
    // After mask injection we re-evaluate them with the real mask.
    let grouping_select_slots: Vec<(usize, Expr)> = stmt
        .columns
        .iter()
        .enumerate()
        .filter_map(|(pos, item)| match item {
            SelectItem::Expr { expr, .. } if expr_contains_grouping(expr) => {
                Some((pos, expr.clone()))
            }
            _ => None,
        })
        .collect();

    // Run one pass per grouping set.
    let mut all_rows: Vec<Row> = Vec::new();

    for set_indices in &sets {
        let set_exprs: Vec<Expr> = set_indices.iter().map(|&i| universe[i].clone()).collect();
        let mut pass_stmt = stmt.clone();
        pass_stmt.group_by = if set_exprs.is_empty() {
            GroupByClause::None
        } else {
            GroupByClause::Simple(set_exprs)
        };
        pass_stmt.order_by.clear();
        pass_stmt.limit = None;
        pass_stmt.offset = None;
        pass_stmt.distinct = false;
        pass_stmt.calc_found_rows = false;
        // HAVING intentionally kept — applied per-pass (SQL standard semantics).
        // Exception: if HAVING references GROUPING(), defer it to post-mask step.
        if having_has_grouping {
            pass_stmt.having = None;
        }

        let pass_rows = match execute_select_grouped_hash(pass_stmt, combined_rows.clone())? {
            QueryResult::Rows { rows, .. } => rows,
            QueryResult::Affected { .. } => vec![],
            other => return Ok(other),
        };

        // Compute grouping mask: bit i = 1 when universe[i] is absent from this set.
        let mut mask: u64 = 0;
        for i in 0..n_universe {
            if !set_indices.contains(&i) {
                mask |= 1u64 << i;
            }
        }

        // Null-out SELECT positions for absent universe exprs; inject hidden mask.
        let mut pass_rows = pass_rows;
        for row in pass_rows.iter_mut() {
            for (ui, slots) in select_slots_for_universe.iter().enumerate() {
                if !set_indices.contains(&ui) {
                    for &slot in slots {
                        if slot < row.len() {
                            row[slot] = Value::Null;
                        }
                    }
                }
            }
            row.push(Value::BigInt(mask as i64));
            // Re-evaluate GROUPING() SELECT slots now that the mask is present.
            for (pos, grouping_expr) in &grouping_select_slots {
                if *pos < row.len() - 1 {
                    // row.last() is the mask; eval reads it directly.
                    if let Ok(v) = crate::eval::eval(grouping_expr, row) {
                        row[*pos] = v;
                    }
                    // on error: leave as 0
                }
            }
        }
        all_rows.extend(pass_rows);
    }

    // Post-mask HAVING filter: apply when HAVING contains GROUPING() and was
    // deferred from per-pass execution.  The hidden mask is still in row.last().
    if having_has_grouping {
        if let Some(ref having_expr) = stmt.having {
            let mut filtered = Vec::with_capacity(all_rows.len());
            for row in all_rows {
                match crate::eval::eval(having_expr, &row) {
                    Ok(v) if crate::eval::is_truthy(&v) => filtered.push(row),
                    Ok(_) => {}
                    Err(e) => return Err(e),
                }
            }
            all_rows = filtered;
        }
    }

    // Post-union operations: DISTINCT, ORDER BY (may reference GROUPING()), LIMIT/OFFSET.
    if stmt.distinct {
        // Strip mask before dedup — it's an internal marker.
        for row in all_rows.iter_mut() {
            row.pop();
        }
        all_rows = apply_distinct_with_session(all_rows);
        // Mask gone — GROUPING() in ORDER BY is not supported after DISTINCT.
        let remapped_ob = remap_order_by_for_grouped(&stmt.order_by, &stmt.columns);
        all_rows = apply_order_by(all_rows, &remapped_ob)?;
    } else {
        // ORDER BY before strip so GROUPING() exprs can read the hidden mask.
        let remapped_ob = remap_order_by_for_grouped(&stmt.order_by, &stmt.columns);
        all_rows = apply_order_by(all_rows, &remapped_ob)?;
        // Strip mask after ORDER BY.
        for row in all_rows.iter_mut() {
            row.pop();
        }
    }

    if stmt.calc_found_rows {
        set_found_rows(all_rows.len() as u64);
    }
    all_rows = apply_limit_offset(all_rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols_template,
        rows: all_rows,
    })
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
        .exprs()
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
    let mut key_values_buf: Vec<Value> = Vec::with_capacity(stmt.group_by.exprs().len().max(1));

    // ── One-pass scan ─────────────────────────────────────────────────────────

    for row in &combined_rows {
        // Evaluate GROUP BY expressions (fast-path: direct col_idx indexing).
        key_values_buf.clear();
        if let Some(ref idxs) = group_by_col_idxs {
            for &i in idxs {
                key_values_buf.push(row.get(i).cloned().unwrap_or(Value::Null));
            }
        } else {
            for e in stmt.group_by.exprs() {
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
            let resolved_having =
                resolve_having_aliases(having.clone(), &stmt.columns);
            let v = eval_with_aggs(&resolved_having, &virtual_row, &agg_values, &agg_exprs)?;
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

/// Returns `true` if `expr` or any sub-expression is `Expr::Grouping`.
///
/// Used to detect whether a HAVING clause references GROUPING(), in which case
/// evaluation must be deferred until after the hidden mask column is appended.
fn expr_contains_grouping(expr: &Expr) -> bool {
    match expr {
        Expr::Grouping { .. } => true,
        Expr::BinaryOp { left, right, .. } => {
            expr_contains_grouping(left) || expr_contains_grouping(right)
        }
        Expr::UnaryOp { operand, .. } => expr_contains_grouping(operand),
        Expr::IsNull { expr, .. } | Expr::IsBoolean { expr, .. } => expr_contains_grouping(expr),
        Expr::Between { expr, low, high, .. } => {
            expr_contains_grouping(expr)
                || expr_contains_grouping(low)
                || expr_contains_grouping(high)
        }
        Expr::Like { expr, pattern, escape, .. } => {
            expr_contains_grouping(expr)
                || expr_contains_grouping(pattern)
                || escape.as_ref().map(|e| expr_contains_grouping(e)).unwrap_or(false)
        }
        Expr::In { expr, list, .. } => {
            expr_contains_grouping(expr) || list.iter().any(expr_contains_grouping)
        }
        Expr::Function { args, .. } => args.iter().any(expr_contains_grouping),
        Expr::Case { operand, when_thens, else_result } => {
            operand.as_ref().map(|e| expr_contains_grouping(e)).unwrap_or(false)
                || when_thens.iter().any(|(w, t)| expr_contains_grouping(w) || expr_contains_grouping(t))
                || else_result.as_ref().map(|e| expr_contains_grouping(e)).unwrap_or(false)
        }
        Expr::Cast { expr, .. } => expr_contains_grouping(expr),
        Expr::GroupConcat { expr, order_by, .. } => {
            expr_contains_grouping(expr) || order_by.iter().any(|(e, _)| expr_contains_grouping(e))
        }
        Expr::ArrayAgg { expr, order_by, .. } => {
            expr_contains_grouping(expr) || order_by.iter().any(|(e, _)| expr_contains_grouping(e))
        }
        _ => false,
    }
}
