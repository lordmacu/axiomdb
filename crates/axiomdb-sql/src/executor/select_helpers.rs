// ── JOIN execution ───────────────────────────────────────────────────────────

/// Executes a SELECT with one or more JOINs using nested-loop strategy.
///
/// All tables are pre-scanned once. The combined row is built progressively:
/// - Stage 0: rows from the FROM table
/// - Stage i: `apply_join(stage_{i-1}, scan(JOIN[i].table), ...)`
///
/// WHERE is applied to the fully combined row after all joins.
fn execute_select_with_joins(
    stmt: SelectStmt,
    from_ref: crate::ast::TableRef,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: Option<&ConnectionTxn>,
) -> Result<QueryResult, DbError> {
    // Resolve all tables (FROM + each JOIN table) and compute col_offsets.
    let mut all_resolved: Vec<axiomdb_catalog::ResolvedTable> = Vec::new();
    let mut col_offsets: Vec<usize> = Vec::new(); // col_offset[i] = start of table i in combined row
    let mut running_offset = 0usize;

    {
        let mut resolver = make_resolver_with_database(storage, txn, conn_txn, DEFAULT_DATABASE_NAME)?;
        let from_t = resolver.resolve_table(from_ref.schema.as_deref(), &from_ref.name)?;
        col_offsets.push(running_offset);
        running_offset += from_t.columns.len();
        all_resolved.push(from_t);

        for join in &stmt.joins {
            match &join.table {
                FromClause::Table(tref) => {
                    let jt = resolver.resolve_table(tref.schema.as_deref(), &tref.name)?;
                    col_offsets.push(running_offset);
                    running_offset += jt.columns.len();
                    all_resolved.push(jt);
                }
                FromClause::Subquery { .. } => {
                    return Err(DbError::NotImplemented {
                        feature: "subquery in JOIN — Phase 4.11".into(),
                    })
                }
            }
        }
    } // resolver dropped — storage immutable borrow released

    // Pre-scan all tables once (consistent snapshot for all).
    let snap = conn_txn
        .map(|c| txn.active_snapshot(c))
        .unwrap_or_else(|| txn.snapshot());
    let mut scanned: Vec<Vec<Row>> = Vec::with_capacity(all_resolved.len());
    for t in &all_resolved {
        let rows = crate::table::scan_table_any_layout(storage, &t.def, &t.columns, snap.clone())?;
        scanned.push(rows.into_iter().map(|(_, r)| r).collect());
    }

    // Progressive nested-loop join.
    let mut combined_rows: Vec<Row> = scanned[0].clone();
    let mut left_col_count = all_resolved[0].columns.len();

    // left_schema tracks (col_name, global_col_idx) for all accumulated left columns.
    // Used by USING conditions to locate column positions by name.
    let mut left_schema: Vec<(String, usize)> = all_resolved[0]
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.clone(), i))
        .collect();

    for (i, join) in stmt.joins.iter().enumerate() {
        let right_idx = i + 1;
        let right_col_count = all_resolved[right_idx].columns.len();
        let right_col_offset = col_offsets[right_idx];

        combined_rows = apply_join(
            combined_rows,
            &scanned[right_idx],
            left_col_count,
            right_col_count,
            join.join_type,
            &join.condition,
            &left_schema,
            right_col_offset,
            &all_resolved[right_idx].columns,
        )?;

        // Extend left_schema with the right table's columns at their global positions.
        for (j, col) in all_resolved[right_idx].columns.iter().enumerate() {
            left_schema.push((col.name.clone(), right_col_offset + j));
        }
        left_col_count += right_col_count;
    }

    // Apply WHERE against the full combined row.
    if let Some(ref wc) = stmt.where_clause {
        let mut filtered = Vec::with_capacity(combined_rows.len());
        for row in combined_rows {
            if is_truthy(&eval(wc, &row)?) {
                filtered.push(row);
            }
        }
        combined_rows = filtered;
    }

    // Branch: aggregation (GROUP BY / aggregate functions) or direct projection.
    if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
        return execute_select_grouped(stmt, combined_rows, GroupByStrategy::Hash);
    }

    // Sort source rows before projection.
    combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

    // Build output ColumnMeta.
    let out_cols = build_join_column_meta(&stmt.columns, &all_resolved, &stmt.joins)?;

    // Project SELECT list.
    let mut rows = combined_rows
        .iter()
        .map(|r| project_row(&stmt.columns, r))
        .collect::<Result<Vec<_>, _>>()?;

    // DISTINCT deduplication (after projection, before LIMIT).
    if stmt.distinct {
        rows = apply_distinct_with_session(rows);
    }
    // LIMIT/OFFSET applied after deduplication.
    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

fn collect_select_col_idxs(stmt: &SelectStmt) -> Vec<u16> {
    let mut col_idxs = Vec::new();
    for item in &stmt.columns {
        match item {
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                return vec![]; // wildcard → conservative, no index-only scan
            }
            SelectItem::Expr { expr, .. } => match expr {
                // Plain column reference: directly use its col_idx.
                Expr::Column { col_idx, .. } => {
                    col_idxs.push(*col_idx as u16);
                }
                // Any other expression (function call, literal, etc.) → conservative.
                _ => return vec![],
            },
        }
    }
    col_idxs
}

fn normalize_clustered_access_method(
    access_method: crate::planner::AccessMethod,
    is_clustered: bool,
) -> crate::planner::AccessMethod {
    if !is_clustered {
        return access_method;
    }

    match access_method {
        crate::planner::AccessMethod::IndexOnlyScan {
            index_def, lo, hi, ..
        } => {
            let is_single_key_point = index_def.columns.len() == 1
                && hi
                    .as_ref()
                    .map(|bound| bound.as_slice() == lo.as_slice())
                    .unwrap_or(false);

            if is_single_key_point {
                crate::planner::AccessMethod::IndexLookup { index_def, key: lo }
            } else {
                crate::planner::AccessMethod::IndexRange {
                    index_def,
                    lo: Some(lo),
                    hi,
                }
            }
        }
        other => other,
    }
}

fn clustered_secondary_rows_for_lookup(
    storage: &dyn StorageEngine,
    resolved: &axiomdb_catalog::ResolvedTable,
    index_def: &axiomdb_catalog::IndexDef,
    key: &[u8],
    snap: TransactionSnapshot,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    let primary_idx = clustered_primary_index(resolved)?;
    crate::table::lookup_clustered_secondary_rows(
        storage,
        &resolved.def,
        &resolved.columns,
        primary_idx,
        index_def,
        key,
        snap,
    )
}

fn clustered_secondary_rows_for_range(
    storage: &dyn StorageEngine,
    resolved: &axiomdb_catalog::ResolvedTable,
    index_def: &axiomdb_catalog::IndexDef,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    snap: TransactionSnapshot,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    let primary_idx = clustered_primary_index(resolved)?;
    crate::table::range_clustered_secondary_rows(
        storage,
        &resolved.def,
        &resolved.columns,
        primary_idx,
        index_def,
        lo,
        hi,
        snap,
    )
}

fn clustered_primary_index(
    resolved: &axiomdb_catalog::ResolvedTable,
) -> Result<&axiomdb_catalog::IndexDef, DbError> {
    resolved
        .indexes
        .iter()
        .find(|idx| idx.is_primary && !idx.columns.is_empty())
        .ok_or_else(|| DbError::Internal {
            message: format!(
                "clustered table {}.{} is missing primary-index metadata",
                resolved.def.schema_name, resolved.def.table_name
            ),
        })
}
