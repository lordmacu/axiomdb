fn execute_select(
    mut stmt: SelectStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: Option<&ConnectionTxn>,
) -> Result<QueryResult, DbError> {
    // Dispatch based on FROM clause type and whether JOINs are present.
    if stmt.from.is_none() {
        // ── SELECT without FROM ───────────────────────────────────────────────
        // Subqueries in the SELECT list (EXISTS, IN subquery, scalar subquery)
        // require a runner; we use a temporary SessionContext and a temporary bloom.
        let mut temp_ctx = SessionContext::new();
        let temp_bloom = crate::bloom::BloomRegistry::new();
        let mut runner = ExecSubqueryRunner {
            storage,
            txn,
            bloom: &temp_bloom,
            ctx: &mut temp_ctx,
            outer_row: &[],
            cache: None,
            in_set_cache: None,
            correlated_cache: None,
        };
        let mut out_row: Row = Vec::new();
        let mut out_cols: Vec<ColumnMeta> = Vec::new();
        for item in &stmt.columns {
            match item {
                SelectItem::Expr { expr, alias } => {
                    let v = eval_with(expr, &[], &mut runner)?;
                    let name = alias
                        .clone()
                        .unwrap_or_else(|| expr_column_name(expr, None));
                    let dt = datatype_of_value(&v);
                    out_cols.push(ColumnMeta::computed(name, dt));
                    out_row.push(v);
                }
                SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                    // SELECT * with no FROM → empty result (0 columns, 0 rows), MySQL-compatible.
                    return Ok(QueryResult::Rows {
                        columns: vec![],
                        rows: vec![],
                    });
                }
            }
        }
        let rows = if stmt.distinct {
            apply_distinct_with_session(vec![out_row])
        } else {
            vec![out_row]
        };
        return Ok(QueryResult::Rows {
            columns: out_cols,
            rows,
        });
    }

    // FROM is present — handle derived table (subquery in FROM) or real table.
    if matches!(stmt.from, Some(FromClause::Subquery { .. })) {
        return execute_select_derived(stmt, storage, txn, conn_txn);
    }

    // Extract the FROM table reference.
    let from_table_ref = match stmt.from.take() {
        Some(FromClause::Table(tref)) => tref,
        _ => unreachable!("already handled None and Subquery above"),
    };

    // INFORMATION_SCHEMA virtual tables (4.20c).
    if from_table_ref
        .schema
        .as_deref()
        .map(crate::information_schema::is_information_schema)
        .unwrap_or(false)
    {
        return execute_information_schema_select(
            stmt,
            from_table_ref,
            storage,
            txn,
            conn_txn,
            DEFAULT_DATABASE_NAME,
        );
    }

    if stmt.joins.is_empty() {
        // ── Single-table path (no JOIN) ───────────────────────────────────────
        let resolved = {
            let mut resolver = make_resolver_with_database(storage, txn, conn_txn, DEFAULT_DATABASE_NAME)?;
            resolver.resolve_table(from_table_ref.schema.as_deref(), &from_table_ref.name)?
        };
        let snap = conn_txn
            .map(|c| txn.active_snapshot(c))
            .unwrap_or_else(|| txn.snapshot());

        // ── Query planner: pick the best access method (non-ctx path) ────
        // No session context available — use conservative defaults (no stats).
        let access_method = normalize_clustered_access_method(
            crate::planner::plan_select(
                stmt.where_clause.as_ref(),
                &resolved.indexes,
                &resolved.columns,
                resolved.def.id,
                &[], // no stats in non-ctx path — always use index (conservative)
                &mut crate::session::StaleStatsTracker::default(),
                &[], // no select_col_idxs in non-ctx path — no index-only scan
            ),
            resolved.def.is_clustered(),
        );

        // ── Fetch rows via the chosen access method ───────────────────────
        let raw_rows: Vec<(RecordId, Vec<Value>)> = match &access_method {
            crate::planner::AccessMethod::Scan if resolved.def.is_clustered() => {
                crate::table::scan_clustered_table(storage, &resolved.def, &resolved.columns, snap)?
            }
            crate::planner::AccessMethod::Scan => {
                // Full sequential scan — existing behavior.
                TableEngine::scan_table(storage, &resolved.def, &resolved.columns, snap, None)?
            }
            crate::planner::AccessMethod::IndexLookup { index_def, key }
                if resolved.def.is_clustered() && index_def.is_primary =>
            {
                // Clustered PK point lookup (non-ctx path).
                match crate::table::lookup_clustered_row(
                    storage,
                    &resolved.def,
                    &resolved.columns,
                    key,
                    snap,
                )? {
                    Some(pair) => vec![pair],
                    None => vec![],
                }
            }
            crate::planner::AccessMethod::IndexLookup { index_def, key }
                if resolved.def.is_clustered() =>
            {
                clustered_secondary_rows_for_lookup(storage, &resolved, index_def, key, snap)?
            }
            crate::planner::AccessMethod::IndexLookup { index_def, key } => {
                // Point lookup: unique → exact match; non-unique → range with RID suffix.
                if index_def.is_unique {
                    match BTree::lookup_in(storage, index_def.root_page_id, key)? {
                        None => vec![],
                        Some(rid) => {
                            // Phase 7.3b: visibility check for dead index entries.
                            if !axiomdb_storage::heap_chain::HeapChain::is_slot_visible(
                                storage,
                                rid.page_id,
                                rid.slot_id,
                                snap,
                            )? {
                                vec![]
                            } else {
                                match TableEngine::read_row(storage, &resolved.columns, rid)? {
                                    None => vec![],
                                    Some(values) => vec![(rid, values)],
                                }
                            }
                        }
                    }
                } else {
                    let lo = rid_lo(key);
                    let hi = rid_hi(key);
                    let pairs =
                        BTree::range_in(storage, index_def.root_page_id, Some(&lo), Some(&hi))?;
                    let mut result = Vec::with_capacity(pairs.len());
                    for (rid, _k) in pairs {
                        // Phase 7.3b: filter dead index entries by heap visibility.
                        if !axiomdb_storage::heap_chain::HeapChain::is_slot_visible(
                            storage,
                            rid.page_id,
                            rid.slot_id,
                            snap.clone(),
                        )? {
                            continue;
                        }
                        if let Some(values) =
                            TableEngine::read_row(storage, &resolved.columns, rid)?
                        {
                            result.push((rid, values));
                        }
                    }
                    result
                }
            }
            crate::planner::AccessMethod::IndexRange { index_def, lo, hi }
                if resolved.def.is_clustered() && index_def.is_primary =>
            {
                // Clustered PK range scan (non-ctx path).
                crate::table::range_clustered_table(
                    storage,
                    &resolved.def,
                    &resolved.columns,
                    lo.as_deref(),
                    hi.as_deref(),
                    snap,
                )?
            }
            crate::planner::AccessMethod::IndexRange { index_def, lo, hi }
                if resolved.def.is_clustered() =>
            {
                clustered_secondary_rows_for_range(
                    storage,
                    &resolved,
                    index_def,
                    lo.as_deref(),
                    hi.as_deref(),
                    snap,
                )?
            }
            crate::planner::AccessMethod::IndexRange { index_def, lo, hi } => {
                // Range scan: iterate B-Tree entries → heap reads.
                let (lo_adjusted, hi_adjusted);
                let (lo_ref, hi_ref) = if index_def.is_unique {
                    (lo.as_deref(), hi.as_deref())
                } else {
                    lo_adjusted = lo.as_deref().map(rid_lo);
                    hi_adjusted = hi.as_deref().map(rid_hi);
                    (lo_adjusted.as_deref(), hi_adjusted.as_deref())
                };
                let pairs = BTree::range_in(storage, index_def.root_page_id, lo_ref, hi_ref)?;
                let rids: Vec<RecordId> = pairs.into_iter().map(|(rid, _)| rid).collect();
                let col_types = crate::table::column_data_types(&resolved.columns);
                let mut result = Vec::with_capacity(rids.len());
                let mut i = 0;
                while i < rids.len() {
                    let page_id = rids[i].page_id;
                    let page = storage.read_page(page_id)?.into_page();
                    while i < rids.len() && rids[i].page_id == page_id {
                        let rid = rids[i];
                        i += 1;
                        match axiomdb_storage::heap::read_tuple(&page, rid.slot_id)? {
                            None => continue,
                            Some((header, data)) => {
                                if !header.is_visible(&snap) {
                                    continue;
                                }
                                let values = axiomdb_types::codec::decode_row(data, &col_types)?;
                                result.push((rid, values));
                            }
                        }
                    }
                }
                result
            }
            // IndexOnlyScan not used in non-ctx path (select_col_idxs = &[] above).
            crate::planner::AccessMethod::IndexOnlyScan { .. } => {
                unreachable!("IndexOnlyScan only emitted when select_col_idxs is non-empty")
            }
        };

        // ── EXISTS decorrelation fast-path (Phase 9 optimization) ────────────
        // Before entering the per-row WHERE loop, check if the WHERE clause is
        // a simple correlated EXISTS that can be decorrelated into a hash
        // semi-join. This turns O(n × cost(inner)) into O(n + m).
        // Inspired by PostgreSQL's hash semi-join and DataFusion's
        // decorrelate_predicate_subquery optimizer pass.
        let mut combined_rows: Vec<Row> = if let Some(ref wc) = stmt.where_clause {
            if let Some(decorr) = try_extract_exists_decorrelation(wc) {
                apply_exists_semijoin(raw_rows, &decorr, storage, txn)?
            } else {
                // Fall back to per-row evaluation.
                let mut rows = Vec::new();
                let mut sq_cache: SubqueryCache = HashMap::new();
                let mut in_set_cache: InSetCache = HashMap::new();
                let mut corr_cache: CorrelatedCache = HashMap::new();
                for (_rid, values) in raw_rows {
                    let mut temp_ctx = SessionContext::new();
                    let temp_bloom = crate::bloom::BloomRegistry::new();
                    let mut runner = ExecSubqueryRunner {
                        storage,
                        txn,
                        bloom: &temp_bloom,
                        ctx: &mut temp_ctx,
                        outer_row: &values,
                        cache: Some(&mut sq_cache),
                        in_set_cache: Some(&mut in_set_cache),
                        correlated_cache: Some(&mut corr_cache),
                    };
                    if !is_truthy(&eval_with(wc, &values, &mut runner)?) {
                        continue;
                    }
                    rows.push(values);
                }
                rows
            }
        } else {
            raw_rows.into_iter().map(|(_rid, v)| v).collect()
        };

        if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
            return execute_select_grouped(stmt, combined_rows, GroupByStrategy::Hash);
        }

        let resolved_ob = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
        // Top-N optimization: if ORDER BY + LIMIT, use partial sort (O(n log k))
        // instead of full sort (O(n log n)). Inspired by PostgreSQL's bounded
        // heapsort and DuckDB's TopN physical operator.
        if !resolved_ob.is_empty()
            && stmt.limit.is_some()
            && !stmt.distinct
            && !stmt.calc_found_rows
        {
            let (limit_n, offset_n) = eval_limit_offset_usize(&stmt.limit, &stmt.offset)?;
            let top_n = offset_n + limit_n.unwrap_or(usize::MAX).min(usize::MAX - offset_n);
            combined_rows = apply_order_by_top_n(combined_rows, &resolved_ob, top_n)?;
        } else {
            combined_rows = apply_order_by(combined_rows, &resolved_ob)?;
        }

        let out_cols = build_select_column_meta(&stmt.columns, &resolved.columns, &resolved.def)?;
        let mut rows: Vec<Row> = Vec::with_capacity(combined_rows.len());
        let mut proj_cache: SubqueryCache = HashMap::new();
        let mut proj_in_set_cache: InSetCache = HashMap::new();
        let mut proj_corr_cache: CorrelatedCache = HashMap::new();
        for v in &combined_rows {
            let mut temp_ctx = SessionContext::new();
            let temp_bloom = crate::bloom::BloomRegistry::new();
            let mut runner = ExecSubqueryRunner {
                storage,
                txn,
                bloom: &temp_bloom,
                ctx: &mut temp_ctx,
                outer_row: v,
                cache: Some(&mut proj_cache),
                in_set_cache: Some(&mut proj_in_set_cache),
                correlated_cache: Some(&mut proj_corr_cache),
            };
            rows.push(project_row_with(&stmt.columns, v, &mut runner)?);
        }

        if stmt.distinct {
            rows = apply_distinct_with_session(rows);
        }
        if stmt.calc_found_rows {
            set_found_rows(rows.len() as u64);
        }
        rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

        Ok(QueryResult::Rows {
            columns: out_cols,
            rows,
        })
    } else {
        // ── Multi-table JOIN path ─────────────────────────────────────────────
        execute_select_with_joins(stmt, from_table_ref, storage, txn, conn_txn)
    }
}

/// Executes a SELECT whose FROM clause is a derived table: `FROM (SELECT ...) AS alias`.
///
/// The inner query is executed to produce a materialized set of rows, which are
/// then treated as a virtual table for the outer query's WHERE / GROUP BY / ORDER BY.
fn execute_select_derived(
    mut stmt: SelectStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    _conn_txn: Option<&ConnectionTxn>,
) -> Result<QueryResult, DbError> {
    let (inner_query, _alias) = match stmt.from.take() {
        Some(FromClause::Subquery { query, alias }) => (*query, alias),
        _ => unreachable!("execute_select_derived called with non-subquery FROM"),
    };

    // Execute the inner query to materialize the derived table.
    let mut temp_ctx = SessionContext::new();
    let temp_bloom = crate::bloom::BloomRegistry::new();
    let temp_exec_ctx = ExecutionContext::new(storage, txn, &temp_bloom);
    let inner_result = execute_select_ctx(inner_query, &temp_exec_ctx, None, &mut temp_ctx)?;
    let (derived_cols, derived_rows) = match inner_result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        _ => {
            return Err(DbError::Internal {
                message: "derived table inner query did not return rows".into(),
            })
        }
    };

    // Apply outer WHERE.
    let mut combined_rows: Vec<Row> = Vec::new();
    let mut sq_cache_derived: SubqueryCache = HashMap::new();
    let mut in_set_cache_derived: InSetCache = HashMap::new();
    let mut corr_cache_derived: CorrelatedCache = HashMap::new();
    for values in derived_rows {
        if let Some(ref wc) = stmt.where_clause {
            let mut temp_ctx2 = SessionContext::new();
            let temp_bloom2 = crate::bloom::BloomRegistry::new();
            let mut runner = ExecSubqueryRunner {
                storage,
                txn,
                bloom: &temp_bloom2,
                ctx: &mut temp_ctx2,
                outer_row: &values,
                cache: Some(&mut sq_cache_derived),
                in_set_cache: Some(&mut in_set_cache_derived),
                correlated_cache: Some(&mut corr_cache_derived),
            };
            if !is_truthy(&eval_with(wc, &values, &mut runner)?) {
                continue;
            }
        }
        combined_rows.push(values);
    }

    // GROUP BY / aggregation.
    if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
        return execute_select_grouped(stmt, combined_rows, GroupByStrategy::Hash);
    }

    let resolved_ob = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
    if !resolved_ob.is_empty() && stmt.limit.is_some() && !stmt.distinct && !stmt.calc_found_rows
    {
        let (limit_n, offset_n) = eval_limit_offset_usize(&stmt.limit, &stmt.offset)?;
        let top_n = offset_n + limit_n.unwrap_or(usize::MAX).min(usize::MAX - offset_n);
        combined_rows = apply_order_by_top_n(combined_rows, &resolved_ob, top_n)?;
    } else {
        combined_rows = apply_order_by(combined_rows, &resolved_ob)?;
    }

    // Build output columns from SELECT list against derived column metadata.
    let out_cols = build_derived_output_columns(&stmt.columns, &derived_cols)?;
    let mut rows = combined_rows
        .iter()
        .map(|v| project_row(&stmt.columns, v))
        .collect::<Result<Vec<_>, _>>()?;

    if stmt.distinct {
        rows = apply_distinct_with_session(rows);
    }
    if stmt.calc_found_rows {
        set_found_rows(rows.len() as u64);
    }
    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}
