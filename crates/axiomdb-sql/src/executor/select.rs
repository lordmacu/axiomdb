fn execute_select_ctx(
    mut stmt: SelectStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // Set the session collation for all eval() calls in this ctx execution.
    // Cleared automatically when _coll_guard is dropped at function exit.
    let _coll_guard = CollationGuard::new(ctx.effective_collation());

    // SELECT without FROM: no table resolution needed.
    if stmt.from.is_none() {
        return execute_select(stmt, storage, txn);
    }

    // Subquery in FROM: no caching path yet — delegate.
    if matches!(stmt.from, Some(FromClause::Subquery { .. })) {
        return execute_select(stmt, storage, txn);
    }

    let from_table_ref = match stmt.from.take() {
        Some(FromClause::Table(tref)) => tref,
        _ => unreachable!("already handled None and Subquery above"),
    };

    if stmt.joins.is_empty() {
        // Single-table path — use cache.
        let resolved = resolve_table_cached(
            storage,
            txn,
            ctx,
            &from_table_ref,
        )?;

        let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());

        // ── Query planner: pick the best access method ────────────────────
        // Load per-column statistics for cost-based index selection (Phase 6.10).
        let table_stats: Vec<axiomdb_catalog::StatsDef> = {
            let mut reader = CatalogReader::new(storage, snap)?;
            reader.list_stats(resolved.def.id).unwrap_or_default()
        };
        // Collect SELECT column indices for index-only scan detection (Phase 6.13).
        // Returns empty slice for SELECT * (wildcard) → conservative, no index-only.
        let select_col_idxs: Vec<u16> = collect_select_col_idxs(&stmt);

        // Compute collation before the mutable borrow of ctx.stats below.
        let effective_coll = ctx.effective_collation();
        let access_method = crate::planner::plan_select_ctx(
            stmt.where_clause.as_ref(),
            &resolved.indexes,
            &resolved.columns,
            resolved.def.id,
            &table_stats,
            &mut ctx.stats,
            &select_col_idxs,
            effective_coll,
        );

        // Fetch rows via the chosen access method.
        let raw_rows: Vec<(RecordId, Vec<Value>)> = match &access_method {
            crate::planner::AccessMethod::Scan => {
                // Phase 8.1: inline WHERE filter during scan — skip result push
                // for non-matching rows, reducing allocation pressure by the
                // selectivity factor (e.g., 50% fewer allocs at 50% selectivity).
                if let Some(ref wc) = stmt.where_clause {
                    let wc_clone = wc.clone();
                    // Phase 8.3b: extract zone map predicate from WHERE clause
                    // for page-level skip during scan.
                    let zm_pred =
                        crate::planner::extract_zone_map_predicate(&wc_clone, &resolved.columns);
                    TableEngine::scan_table_filtered(
                        storage,
                        &resolved.def,
                        &resolved.columns,
                        snap,
                        |values| {
                            match eval(&wc_clone, values) {
                                Ok(v) => is_truthy(&v),
                                Err(_) => true,
                            }
                        },
                        zm_pred.as_ref(),
                    )?
                } else {
                    // No WHERE clause — scan all rows.
                    TableEngine::scan_table(
                        storage,
                        &resolved.def,
                        &resolved.columns,
                        snap,
                        None,
                    )?
                }
            }
            crate::planner::AccessMethod::IndexLookup { index_def, key } => {
                // Bloom filter: skip B-Tree read if key is definitely absent.
                // Only applied for UNIQUE indexes — non-unique indexes store key||RID in
                // the bloom (one entry per row), but the lookup key here is the bare value.
                // Checking a bare value key against a bloom populated with key||RID entries
                // produces false negatives, so we skip the bloom check for non-unique indexes.
                if index_def.is_unique && !bloom.might_exist(index_def.index_id, key) {
                    vec![]
                } else if index_def.is_unique {
                    // Unique index: exact key lookup → at most one RecordId.
                    match BTree::lookup_in(storage, index_def.root_page_id, key)? {
                        None => vec![],
                        Some(rid) => {
                            if !HeapChain::is_slot_visible(
                                storage, rid.page_id, rid.slot_id, snap,
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
                    // Non-unique index: key stored as key||RID — use range scan with
                    // [key||0x00..00, key||0xFF..FF] to find all rows with this value.
                    let lo = rid_lo(key);
                    let hi = rid_hi(key);
                    let pairs =
                        BTree::range_in(storage, index_def.root_page_id, Some(&lo), Some(&hi))?;
                    let mut result = Vec::with_capacity(pairs.len());
                    for (rid, _k) in pairs {
                        if !HeapChain::is_slot_visible(
                            storage, rid.page_id, rid.slot_id, snap,
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
            crate::planner::AccessMethod::IndexRange { index_def, lo, hi } => {
                // Range scan: B-Tree entries → batch heap reads by page.
                // Inspired by PostgreSQL's BitmapHeapScan: collect RIDs, group by
                // page_id, read each heap page ONCE, extract all matching rows.
                let (lo_adjusted, hi_adjusted);
                let (lo_ref, hi_ref) = if index_def.is_unique {
                    (lo.as_deref(), hi.as_deref())
                } else {
                    lo_adjusted = lo.as_deref().map(rid_lo);
                    hi_adjusted = hi.as_deref().map(rid_hi);
                    (lo_adjusted.as_deref(), hi_adjusted.as_deref())
                };
                // Collect only RecordIds from B-Tree (skip key cloning).
                let pairs = BTree::range_in(storage, index_def.root_page_id, lo_ref, hi_ref)?;
                let rids: Vec<RecordId> = pairs.into_iter().map(|(rid, _key)| rid).collect();

                // Batch read: group by page_id, read each page once, extract
                // visibility + row data in a single pass (eliminates the
                // is_slot_visible + read_row double-read pattern).
                let col_types = crate::table::column_data_types(&resolved.columns);
                let mut result = Vec::with_capacity(rids.len());
                let mut i = 0;
                while i < rids.len() {
                    let page_id = rids[i].page_id;
                    // Read page once.
                    let page = storage.read_page(page_id)?.into_page();
                    // Process all RIDs on this page.
                    while i < rids.len() && rids[i].page_id == page_id {
                        let rid = rids[i];
                        i += 1;
                        let slot_id = rid.slot_id;
                        // Combined visibility + data extraction from same page.
                        match axiomdb_storage::heap::read_tuple(&page, slot_id)? {
                            None => continue,
                            Some((header, data)) => {
                                if !header.is_visible(&snap) {
                                    continue;
                                }
                                let values =
                                    axiomdb_types::codec::decode_row(data, &col_types)?;
                                result.push((rid, values));
                            }
                        }
                    }
                }
                result
            }
            crate::planner::AccessMethod::IndexOnlyScan {
                index_def,
                lo,
                hi,
                n_key_cols,
                needed_key_positions: _,
            } => {
                // Index-only scan (Phase 6.13): values decoded from B-Tree key bytes.
                // Only the 24-byte heap slot header is read for MVCC visibility.
                // Non-unique: lo/hi need RID suffix for correct range bounds.
                let (lo_adj, hi_adj);
                let (lo_ref, hi_ref) = if index_def.is_unique {
                    (Some(lo.as_slice()), hi.as_deref())
                } else {
                    lo_adj = rid_lo(lo);
                    hi_adj = hi.as_deref().map(rid_hi);
                    (Some(lo_adj.as_slice()), hi_adj.as_deref())
                };
                let pairs = BTree::range_in(storage, index_def.root_page_id, lo_ref, hi_ref)?;
                let n_table_cols = resolved.columns.len();
                let mut result = Vec::with_capacity(pairs.len());
                for (rid, key_bytes) in pairs {
                    if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap)? {
                        continue;
                    }
                    let (all_key_vals, _) =
                        crate::key_encoding::decode_index_key(&key_bytes, *n_key_cols)?;
                    // Build a full-width row (Null for non-indexed cols) so that
                    // WHERE and SELECT expressions can access values by table col_idx.
                    // Populate all decoded key columns — not just the SELECT ones —
                    // so that WHERE re-evaluation can access them too.
                    let mut row_values = vec![Value::Null; n_table_cols];
                    for (key_pos, idx_col) in index_def.columns.iter().enumerate() {
                        let table_idx = idx_col.col_idx as usize;
                        if let (true, Some(val)) =
                            (table_idx < n_table_cols, all_key_vals.get(key_pos))
                        {
                            row_values[table_idx] = val.clone();
                        }
                    }
                    result.push((rid, row_values));
                }
                result
            }
        };

        let mut combined_rows: Vec<Row> = Vec::new();
        for (_rid, values) in raw_rows {
            if let Some(ref wc) = stmt.where_clause {
                let mut runner = ExecSubqueryRunner {
                    storage,
                    txn,
                    bloom,
                    ctx,
                    outer_row: &values,
                };
                if !is_truthy(&eval_with(wc, &values, &mut runner)?) {
                    continue;
                }
            }
            combined_rows.push(values);
        }

        if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
            // Single-table path: choose sorted strategy when the access method
            // already delivers rows in group-key order (Phase 4.9b).
            let strategy = choose_group_by_strategy_ctx_with_collation(
                &stmt.group_by,
                &access_method,
                effective_coll,
                &resolved.columns,
            );
            return execute_select_grouped(stmt, combined_rows, strategy);
        }

        combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

        let out_cols = build_select_column_meta(&stmt.columns, &resolved.columns, &resolved.def)?;
        let mut rows = combined_rows
            .iter()
            .map(|v| {
                let mut runner = ExecSubqueryRunner {
                    storage,
                    txn,
                    bloom,
                    ctx,
                    outer_row: v,
                };
                project_row_with(&stmt.columns, v, &mut runner)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if stmt.distinct {
            rows = apply_distinct_with_session(rows);
        }
        rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

        Ok(QueryResult::Rows {
            columns: out_cols,
            rows,
        })
    } else {
        // Multi-table JOIN path — use cache for each table.
        execute_select_with_joins_ctx(stmt, from_table_ref, storage, txn, ctx)
    }
}

fn execute_select_with_joins_ctx(
    stmt: SelectStmt,
    from_ref: crate::ast::TableRef,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // Session collation for eval()-based comparisons in join ON, WHERE, ORDER BY, etc.
    // Guard propagates from execute_select_ctx when called via the join path, but
    // we set it here too so this function can also be called independently.
    let _coll_guard = CollationGuard::new(ctx.effective_collation());

    let mut all_resolved: Vec<axiomdb_catalog::ResolvedTable> = Vec::new();
    let mut col_offsets: Vec<usize> = Vec::new();
    let mut running_offset = 0usize;

    {
        let from_t = resolve_table_cached(
            storage,
            txn,
            ctx,
            &from_ref,
        )?;
        col_offsets.push(running_offset);
        running_offset += from_t.columns.len();
        all_resolved.push(from_t);

        for join in &stmt.joins {
            match &join.table {
                FromClause::Table(tref) => {
                    let jt = resolve_table_cached(
                        storage,
                        txn,
                        ctx,
                        tref,
                    )?;
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
    }

    let snap = txn.active_snapshot()?;
    let mut scanned: Vec<Vec<Row>> = Vec::with_capacity(all_resolved.len());
    for t in &all_resolved {
        let rows = TableEngine::scan_table(storage, &t.def, &t.columns, snap, None)?;
        scanned.push(rows.into_iter().map(|(_, r)| r).collect());
    }

    let mut combined_rows: Vec<Row> = scanned[0].clone();
    let mut left_col_count = all_resolved[0].columns.len();

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

        for (j, col) in all_resolved[right_idx].columns.iter().enumerate() {
            left_schema.push((col.name.clone(), right_col_offset + j));
        }
        left_col_count += right_col_count;
    }

    if let Some(ref wc) = stmt.where_clause {
        let mut filtered = Vec::with_capacity(combined_rows.len());
        for row in combined_rows {
            if is_truthy(&eval(wc, &row)?) {
                filtered.push(row);
            }
        }
        combined_rows = filtered;
    }

    if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
        // JOIN path: no ordering guarantee — always hash aggregate.
        return execute_select_grouped(stmt, combined_rows, GroupByStrategy::Hash);
    }

    combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

    let out_cols = build_join_column_meta(&stmt.columns, &all_resolved, &stmt.joins)?;

    let mut rows = combined_rows
        .iter()
        .map(|r| project_row(&stmt.columns, r))
        .collect::<Result<Vec<_>, _>>()?;

    if stmt.distinct {
        rows = apply_distinct_with_session(rows);
    }
    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}


fn execute_select(
    mut stmt: SelectStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
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
                    return Err(DbError::NotImplemented {
                        feature: "SELECT * without FROM".into(),
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
        return execute_select_derived(stmt, storage, txn);
    }

    // Extract the FROM table reference.
    let from_table_ref = match stmt.from.take() {
        Some(FromClause::Table(tref)) => tref,
        _ => unreachable!("already handled None and Subquery above"),
    };

    if stmt.joins.is_empty() {
        // ── Single-table path (no JOIN) ───────────────────────────────────────
        let resolved = {
            let mut resolver = make_resolver(storage, txn)?;
            resolver.resolve_table(from_table_ref.schema.as_deref(), &from_table_ref.name)?
        };

        let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());

        // ── Query planner: pick the best access method (non-ctx path) ────
        // No session context available — use conservative defaults (no stats).
        let access_method = crate::planner::plan_select(
            stmt.where_clause.as_ref(),
            &resolved.indexes,
            &resolved.columns,
            resolved.def.id,
            &[], // no stats in non-ctx path — always use index (conservative)
            &mut crate::session::StaleStatsTracker::default(),
            &[], // no select_col_idxs in non-ctx path — no index-only scan
        );

        // ── Fetch rows via the chosen access method ───────────────────────
        let raw_rows: Vec<(RecordId, Vec<Value>)> = match &access_method {
            crate::planner::AccessMethod::Scan => {
                // Full sequential scan — existing behavior.
                TableEngine::scan_table(storage, &resolved.def, &resolved.columns, snap, None)?
            }
            crate::planner::AccessMethod::IndexLookup { index_def, key } => {
                // Point lookup: unique → exact match; non-unique → range with RID suffix.
                if index_def.is_unique {
                    match BTree::lookup_in(storage, index_def.root_page_id, key)? {
                        None => vec![],
                        Some(rid) => {
                            // Phase 7.3b: visibility check for dead index entries.
                            if !axiomdb_storage::heap_chain::HeapChain::is_slot_visible(
                                storage, rid.page_id, rid.slot_id, snap,
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
                            storage, rid.page_id, rid.slot_id, snap,
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
                                let values =
                                    axiomdb_types::codec::decode_row(data, &col_types)?;
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

        let mut combined_rows: Vec<Row> = Vec::new();
        for (_rid, values) in raw_rows {
            if let Some(ref wc) = stmt.where_clause {
                let mut temp_ctx = SessionContext::new();
                let temp_bloom = crate::bloom::BloomRegistry::new();
                let mut runner = ExecSubqueryRunner {
                    storage,
                    txn,
                    bloom: &temp_bloom,
                    ctx: &mut temp_ctx,
                    outer_row: &values,
                };
                if !is_truthy(&eval_with(wc, &values, &mut runner)?) {
                    continue;
                }
            }
            combined_rows.push(values);
        }

        if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
            return execute_select_grouped(stmt, combined_rows, GroupByStrategy::Hash);
        }

        combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

        let out_cols = build_select_column_meta(&stmt.columns, &resolved.columns, &resolved.def)?;
        let mut rows = combined_rows
            .iter()
            .map(|v| {
                let mut temp_ctx = SessionContext::new();
                let temp_bloom = crate::bloom::BloomRegistry::new();
                let mut runner = ExecSubqueryRunner {
                    storage,
                    txn,
                    bloom: &temp_bloom,
                    ctx: &mut temp_ctx,
                    outer_row: v,
                };
                project_row_with(&stmt.columns, v, &mut runner)
            })
            .collect::<Result<Vec<_>, _>>()?;

        if stmt.distinct {
            rows = apply_distinct_with_session(rows);
        }
        rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

        Ok(QueryResult::Rows {
            columns: out_cols,
            rows,
        })
    } else {
        // ── Multi-table JOIN path ─────────────────────────────────────────────
        execute_select_with_joins(stmt, from_table_ref, storage, txn)
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
) -> Result<QueryResult, DbError> {
    let (inner_query, _alias) = match stmt.from.take() {
        Some(FromClause::Subquery { query, alias }) => (*query, alias),
        _ => unreachable!("execute_select_derived called with non-subquery FROM"),
    };

    // Execute the inner query to materialize the derived table.
    let mut temp_ctx = SessionContext::new();
    let temp_bloom = crate::bloom::BloomRegistry::new();
    let inner_result =
        execute_select_ctx(inner_query, storage, txn, &temp_bloom, &mut temp_ctx)?;
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

    combined_rows = apply_order_by(combined_rows, &stmt.order_by)?;

    // Build output columns from SELECT list against derived column metadata.
    let out_cols = build_derived_output_columns(&stmt.columns, &derived_cols)?;
    let mut rows = combined_rows
        .iter()
        .map(|v| project_row(&stmt.columns, v))
        .collect::<Result<Vec<_>, _>>()?;

    if stmt.distinct {
        rows = apply_distinct_with_session(rows);
    }
    rows = apply_limit_offset(rows, &stmt.limit, &stmt.offset)?;

    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}

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
) -> Result<QueryResult, DbError> {
    // Resolve all tables (FROM + each JOIN table) and compute col_offsets.
    let mut all_resolved: Vec<axiomdb_catalog::ResolvedTable> = Vec::new();
    let mut col_offsets: Vec<usize> = Vec::new(); // col_offset[i] = start of table i in combined row
    let mut running_offset = 0usize;

    {
        let mut resolver = make_resolver(storage, txn)?;
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
    let snap = txn.active_snapshot()?;
    let mut scanned: Vec<Vec<Row>> = Vec::with_capacity(all_resolved.len());
    for t in &all_resolved {
        let rows = TableEngine::scan_table(storage, &t.def, &t.columns, snap, None)?;
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
