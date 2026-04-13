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
    // Resolve/materialize all join sources (FROM + each JOIN table/subquery).
    let mut all_sources: Vec<JoinSourceSchema> = Vec::new();
    let mut scanned: Vec<Vec<Row>> = Vec::new();
    let mut col_offsets: Vec<usize> = Vec::new(); // col_offset[i] = start of source i in combined row
    let mut running_offset = 0usize;
    let snap = conn_txn
        .map(|c| txn.active_snapshot(c))
        .unwrap_or_else(|| txn.snapshot());

    {
        let mut resolver = make_resolver_with_database(storage, txn, conn_txn, DEFAULT_DATABASE_NAME)?;
        let from_t = resolver.resolve_table(from_ref.schema.as_deref(), &from_ref.name)?;
        let from_rows =
            crate::table::scan_table_any_layout(storage, &from_t.def, &from_t.columns, snap.clone())?;
        col_offsets.push(running_offset);
        running_offset += from_t.columns.len();
        all_sources.push(join_source_schema_from_resolved(&from_ref, &from_t));
        scanned.push(from_rows.into_iter().map(|(_, r)| r).collect());

        for join in &stmt.joins {
            match &join.table {
                FromClause::Table(tref) => {
                    let jt = resolver.resolve_table(tref.schema.as_deref(), &tref.name)?;
                    let rows =
                        crate::table::scan_table_any_layout(storage, &jt.def, &jt.columns, snap.clone())?;
                    col_offsets.push(running_offset);
                    running_offset += jt.columns.len();
                    all_sources.push(join_source_schema_from_resolved(tref, &jt));
                    scanned.push(rows.into_iter().map(|(_, r)| r).collect());
                }
                FromClause::Subquery { query, alias } => {
                    let inner_result = execute_select((**query).clone(), storage, txn, conn_txn)?;
                    let (columns, rows) = match inner_result {
                        QueryResult::Rows { columns, rows } => (columns, rows),
                        _ => {
                            return Err(DbError::Internal {
                                message: "join-side subquery did not return rows".into(),
                            });
                        }
                    };
                    col_offsets.push(running_offset);
                    running_offset += columns.len();
                    all_sources.push(join_source_schema_from_derived(alias, columns));
                    scanned.push(rows);
                }
                FromClause::JsonTable(_) => {
                    return Err(DbError::NotImplemented {
                        feature: "JSON_TABLE JOIN via the non-ctx executor path; \
                                  use a session-bound query path"
                            .into(),
                    });
                }
                FromClause::JsonbSrf(_) => {
                    return Err(DbError::NotImplemented {
                        feature: "JSONB SRF JOIN via the non-ctx executor path; \
                                  use a session-bound query path"
                            .into(),
                    });
                }
            }
        }
    }

    // Progressive nested-loop join.
    let mut combined_rows: Vec<Row> = scanned[0].clone();
    let mut left_col_count = all_sources[0].columns.len();

    // left_schema tracks (col_name, global_col_idx) for all accumulated left columns.
    // Used by USING conditions to locate column positions by name.
    let mut left_schema: Vec<(String, usize)> = all_sources[0]
        .columns
        .iter()
        .enumerate()
        .map(|(i, col)| (col.name.clone(), i))
        .collect();

    for (i, join) in stmt.joins.iter().enumerate() {
        let right_idx = i + 1;
        let right_col_count = all_sources[right_idx].columns.len();
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
            &all_sources[right_idx].columns,
        )?;

        // Extend left_schema with the right table's columns at their global positions.
        for (j, col) in all_sources[right_idx].columns.iter().enumerate() {
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
    let resolved_ob = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
    combined_rows = apply_order_by(combined_rows, &resolved_ob)?;

    // Build output ColumnMeta.
    let out_cols = build_join_column_meta(&stmt.columns, &all_sources, &stmt.joins)?;

    // Project SELECT list.
    let mut rows = combined_rows
        .iter()
        .map(|r| project_join_row(&stmt.columns, r, &all_sources))
        .collect::<Result<Vec<_>, _>>()?;

    // DISTINCT deduplication (after projection, before LIMIT).
    if stmt.distinct {
        rows = apply_distinct_with_session(rows);
    }
    if stmt.calc_found_rows {
        set_found_rows(rows.len() as u64);
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

fn gin_scan_rows(
    storage: &dyn StorageEngine,
    resolved: &axiomdb_catalog::ResolvedTable,
    index_def: &axiomdb_catalog::IndexDef,
    query_terms: &[Vec<u8>],
    snap: TransactionSnapshot,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    use std::collections::HashSet;

    if resolved.def.is_clustered() {
        let mut candidate_keys: Option<HashSet<Vec<u8>>> = None;

        for term in query_terms {
            let (lo, hi) = crate::index_maintenance::gin_term_bounds(term);
            let pairs = BTree::range_in(
                storage,
                index_def.root_page_id,
                Some(&lo),
                Some(&hi),
            )?;
            let term_keys: HashSet<Vec<u8>> = pairs
                .into_iter()
                .filter_map(|(_rid, key)| {
                    crate::index_maintenance::gin_key_suffix(&key, term).map(|pk| pk.to_vec())
                })
                .collect();

            candidate_keys = Some(match candidate_keys.take() {
                None => term_keys,
                Some(existing) => existing.intersection(&term_keys).cloned().collect(),
            });
        }

        let pk_keys = candidate_keys.unwrap_or_default();
        let col_types = crate::table::column_data_types(&resolved.columns);
        let mut result = Vec::with_capacity(pk_keys.len());
        for pk_key in pk_keys {
            let Some(row) = axiomdb_storage::clustered_tree::lookup(
                storage,
                Some(resolved.def.root_page_id),
                &pk_key,
                &snap,
            )?
            else {
                continue;
            };
            let values = axiomdb_types::codec::decode_row(&row.row_data, &col_types)?;
            result.push((RecordId { page_id: 0, slot_id: 0 }, values));
        }
        return Ok(result);
    }

    let mut candidate_rids: Option<HashSet<RecordId>> = None;

    for term in query_terms {
        let (lo, hi) = crate::index_maintenance::gin_term_bounds(term);
        let pairs = BTree::range_in(
            storage,
            index_def.root_page_id,
            Some(&lo),
            Some(&hi),
        )?;
        let term_rids: HashSet<RecordId> = pairs.into_iter().map(|(rid, _)| rid).collect();

        candidate_rids = Some(match candidate_rids.take() {
            None => term_rids,
            Some(existing) => existing.intersection(&term_rids).copied().collect(),
        });
    }

    let rids: Vec<RecordId> = candidate_rids.unwrap_or_default().into_iter().collect();
    let mut result = Vec::with_capacity(rids.len());

    for rid in rids {
        if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap.clone())? {
            continue;
        }
        if let Some(values) = TableEngine::read_row(storage, &resolved.columns, rid)? {
            result.push((rid, values));
        }
    }

    Ok(result)
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
