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
        let resolved = resolve_table_cached(storage, txn, ctx, &from_table_ref)?;
        let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());

        // ── COUNT(*) fast path (Phase 8) ─────────────────────────────────
        // Detect `SELECT COUNT(*) FROM table` with no WHERE, no GROUP BY,
        // no HAVING, no JOIN. For heap tables use HeapChain::count_visible().
        // For clustered tables use count_clustered_visible() (header-only scan).
        // Both paths: zero column decode, zero allocs.
        if stmt.where_clause.is_none()
            && stmt.group_by.is_empty()
            && stmt.having.is_none()
            && stmt.columns.len() == 1
        {
            if let Some(crate::ast::SelectItem::Expr {
                expr: crate::expr::Expr::Function { ref name, ref args },
                ..
            }) = stmt.columns.first()
            {
                if name.eq_ignore_ascii_case("count") && args.is_empty() {
                    let count = if resolved.def.is_clustered() {
                        crate::table::count_clustered_visible(
                            storage,
                            resolved.def.root_page_id,
                            snap,
                        )?
                    } else {
                        HeapChain::count_visible(storage, resolved.def.root_page_id, snap)?
                    };
                    let columns = vec![ColumnMeta::computed("count(*)", DataType::BigInt)];
                    let rows = vec![vec![Value::BigInt(count as i64)]];
                    return Ok(QueryResult::Rows { columns, rows });
                }
            }
        }

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
        let access_method = normalize_clustered_access_method(
            crate::planner::plan_select_ctx(
                stmt.where_clause.as_ref(),
                &resolved.indexes,
                &resolved.columns,
                resolved.def.id,
                &table_stats,
                &mut ctx.stats,
                &select_col_idxs,
                effective_coll,
            ),
            resolved.def.is_clustered(),
        );

        /// Collects column indices referenced by an expression into a mask.
        fn collect_expr_columns(e: &crate::expr::Expr, mask: &mut [bool]) {
            match e {
                crate::expr::Expr::Column { col_idx, .. } => {
                    if *col_idx < mask.len() {
                        mask[*col_idx] = true;
                    }
                }
                crate::expr::Expr::BinaryOp { left, right, .. } => {
                    collect_expr_columns(left, mask);
                    collect_expr_columns(right, mask);
                }
                crate::expr::Expr::UnaryOp { operand, .. } => {
                    collect_expr_columns(operand, mask);
                }
                crate::expr::Expr::Function { args, .. } => {
                    for arg in args {
                        collect_expr_columns(arg, mask);
                    }
                }
                // GROUP_CONCAT has its own expression + ORDER BY expressions inside it.
                // These must be included in the mask so the referenced columns are decoded.
                crate::expr::Expr::GroupConcat { expr, order_by, .. } => {
                    collect_expr_columns(expr, mask);
                    for (ob_expr, _) in order_by {
                        collect_expr_columns(ob_expr, mask);
                    }
                }
                crate::expr::Expr::Cast { expr, .. } => collect_expr_columns(expr, mask),
                crate::expr::Expr::IsNull { expr, .. } => collect_expr_columns(expr, mask),
                crate::expr::Expr::Between {
                    expr, low, high, ..
                } => {
                    collect_expr_columns(expr, mask);
                    collect_expr_columns(low, mask);
                    collect_expr_columns(high, mask);
                }
                crate::expr::Expr::Like { expr, pattern, .. } => {
                    collect_expr_columns(expr, mask);
                    collect_expr_columns(pattern, mask);
                }
                crate::expr::Expr::In { expr, list, .. } => {
                    collect_expr_columns(expr, mask);
                    for item in list {
                        collect_expr_columns(item, mask);
                    }
                }
                crate::expr::Expr::Case {
                    operand,
                    when_thens,
                    else_result,
                } => {
                    if let Some(op) = operand {
                        collect_expr_columns(op, mask);
                    }
                    for (w, t) in when_thens {
                        collect_expr_columns(w, mask);
                        collect_expr_columns(t, mask);
                    }
                    if let Some(el) = else_result {
                        collect_expr_columns(el, mask);
                    }
                }
                // Subquery internals are not scanned — they run as separate queries.
                crate::expr::Expr::Literal(_)
                | crate::expr::Expr::Param { .. }
                | crate::expr::Expr::OuterColumn { .. }
                | crate::expr::Expr::Subquery(_)
                | crate::expr::Expr::InSubquery { .. }
                | crate::expr::Expr::Exists { .. } => {}
            }
        }

        /// Returns true if the expression contains a subquery node.
        fn expr_has_subquery(e: &crate::expr::Expr) -> bool {
            match e {
                crate::expr::Expr::Subquery(_)
                | crate::expr::Expr::InSubquery { .. }
                | crate::expr::Expr::Exists { .. } => true,
                crate::expr::Expr::BinaryOp { left, right, .. } => {
                    expr_has_subquery(left) || expr_has_subquery(right)
                }
                crate::expr::Expr::UnaryOp { operand, .. } => expr_has_subquery(operand),
                _ => false,
            }
        }

        // Tracks whether the WHERE clause was already evaluated inline during
        // scan (Phase 8.1). If true, skip the redundant re-evaluation in
        // combined_rows below — saves ~2500 eval calls at 50% selectivity.
        let mut where_already_applied = false;

        // Fetch rows via the chosen access method.
        let raw_rows: Vec<(RecordId, Vec<Value>)> = match &access_method {
            crate::planner::AccessMethod::Scan if resolved.def.is_clustered() => {
                // ── Clustered full scan (Phase 39.15) ────────────────────────
                // Iterate all clustered B-tree leaves. MVCC visibility is handled
                // inside ClusteredRangeIter. WHERE evaluated on decoded values.
                //
                // Phase 9.2 column projection: only decode columns referenced in
                // SELECT, WHERE, ORDER BY, GROUP BY, HAVING. For aggregate queries
                // on wide tables this avoids decoding TEXT columns not needed by
                // the query (e.g. name, email when aggregating over age, score).
                let n_cols = resolved.columns.len();
                let clustered_decode_mask: Option<Vec<bool>> = {
                    let mut mask = vec![false; n_cols];
                    // WHERE columns
                    if let Some(ref wc) = stmt.where_clause {
                        collect_expr_columns(wc, &mut mask);
                    }
                    // SELECT columns
                    for item in &stmt.columns {
                        if let crate::ast::SelectItem::Expr { expr, .. } = item {
                            collect_expr_columns(expr, &mut mask);
                        } else {
                            mask.iter_mut().for_each(|m| *m = true);
                        }
                    }
                    // ORDER BY, GROUP BY, HAVING
                    for ob in &stmt.order_by { collect_expr_columns(&ob.expr, &mut mask); }
                    for gb in &stmt.group_by { collect_expr_columns(gb, &mut mask); }
                    if let Some(ref having) = stmt.having {
                        collect_expr_columns(having, &mut mask);
                    }
                    if mask.iter().all(|&b| b) { None } else { Some(mask) }
                };
                let mut rows = crate::table::scan_clustered_table_masked(
                    storage,
                    &resolved.def,
                    &resolved.columns,
                    snap,
                    clustered_decode_mask.as_deref(),
                )?;
                // Apply WHERE filter on decoded values (clustered scan doesn't
                // support inline BatchPredicate yet — Phase 39.20 optimization).
                if let Some(ref wc) = stmt.where_clause {
                    if !expr_has_subquery(wc) {
                        where_already_applied = true;
                    }
                    rows.retain(|(_, values)| match eval(wc, values) {
                        Ok(v) => is_truthy(&v),
                        Err(_) => true,
                    });
                }
                rows
            }
            crate::planner::AccessMethod::Scan => {
                // Phase 8.1: inline WHERE filter during scan — skip result push
                // for non-matching rows, reducing allocation pressure by the
                // selectivity factor (e.g., 50% fewer allocs at 50% selectivity).
                if let Some(ref wc) = stmt.where_clause {
                    let wc_clone = wc.clone();
                    let zm_pred =
                        crate::planner::extract_zone_map_predicate(&wc_clone, &resolved.columns);
                    // Only skip re-eval if the WHERE has no subqueries.
                    // Subqueries need eval_with() which isn't available in the scan closure.
                    if !expr_has_subquery(&wc_clone) {
                        where_already_applied = true;
                    }
                    // Build WHERE column mask for two-phase decode:
                    // only decode columns referenced in WHERE first.
                    let n_cols = resolved.columns.len();
                    let _where_mask = {
                        let mut mask = vec![false; n_cols];
                        collect_expr_columns(&wc_clone, &mut mask);
                        // Only use two-phase if mask is selective (not all cols)
                        if mask.iter().filter(|&&b| b).count() < n_cols {
                            Some(mask)
                        } else {
                            None
                        }
                    };
                    // Phase 8.1: try to compile a BatchPredicate for zero-alloc
                    // raw-byte evaluation. Falls back to eval() for complex
                    // expressions (OR, LIKE, IN, subqueries, Text/Bytes, etc.).
                    let col_types: Vec<axiomdb_types::DataType> = resolved
                        .columns
                        .iter()
                        .map(|c| crate::table::column_type_to_data_type(c.col_type))
                        .collect();
                    let batch_pred = crate::eval::batch::try_compile(&wc_clone, &col_types);

                    // Phase 9.2: Operator fusion — build unified decode mask
                    // (SELECT ∪ WHERE ∪ ORDER BY ∪ GROUP BY columns).
                    // Only decode columns that are actually referenced anywhere
                    // in the query. Non-referenced columns get Value::Null,
                    // saving String/Text allocation for wide tables.
                    let decode_mask = {
                        let mut mask = vec![false; n_cols];
                        // WHERE columns
                        collect_expr_columns(&wc_clone, &mut mask);
                        // SELECT columns
                        for item in &stmt.columns {
                            if let crate::ast::SelectItem::Expr { expr, .. } = item {
                                collect_expr_columns(expr, &mut mask);
                            } else {
                                // Wildcard — need all columns
                                mask.iter_mut().for_each(|m| *m = true);
                            }
                        }
                        // ORDER BY columns
                        for ob in &stmt.order_by {
                            collect_expr_columns(&ob.expr, &mut mask);
                        }
                        // GROUP BY columns
                        for gb in &stmt.group_by {
                            collect_expr_columns(gb, &mut mask);
                        }
                        // HAVING columns
                        if let Some(ref having) = stmt.having {
                            collect_expr_columns(having, &mut mask);
                        }
                        // Only use mask if it's selective (not all cols needed)
                        if mask.iter().all(|&b| b) {
                            None
                        } else {
                            Some(mask)
                        }
                    };

                    // Phase 9.11: early-exit scan for LIMIT without ORDER BY.
                    // PostgreSQL's ExecutePlan(count) pattern — stop scanning
                    // after limit rows are collected. Only safe when no ORDER BY
                    // (sorting requires all rows first) and no GROUP BY.
                    let scan_limit = if stmt.order_by.is_empty()
                        && stmt.group_by.is_empty()
                        && stmt.having.is_none()
                    {
                        stmt.limit.as_ref().and_then(|expr| match expr {
                            Expr::Literal(Value::Int(n)) => Some(*n as usize),
                            Expr::Literal(Value::BigInt(n)) => Some(*n as usize),
                            _ => None,
                        })
                    } else {
                        None
                    };

                    TableEngine::scan_table_filtered_parallel(
                        storage,
                        &resolved.def,
                        &resolved.columns,
                        snap,
                        |values| match eval(&wc_clone, values) {
                            Ok(v) => is_truthy(&v),
                            Err(_) => true,
                        },
                        zm_pred.as_ref().map(|(ci, p)| (*ci, p)),
                        batch_pred.as_ref(),
                        decode_mask.as_deref(),
                        scan_limit,
                    )?
                } else {
                    // No WHERE clause — scan all rows with column projection mask.
                    // Phase 9.2 applies here too: only decode columns referenced in
                    // SELECT, ORDER BY, GROUP BY, HAVING. Skipping unreferenced TEXT/Bytes
                    // columns (e.g. name, email in aggregate queries) saves ~2 string
                    // allocations per row, reducing pressure for aggregate workloads that
                    // only need a small subset of a wide table's columns.
                    let n_cols = resolved.columns.len();
                    let decode_mask_no_where: Option<Vec<bool>> = {
                        let mut mask = vec![false; n_cols];
                        for item in &stmt.columns {
                            if let crate::ast::SelectItem::Expr { expr, .. } = item {
                                collect_expr_columns(expr, &mut mask);
                            } else {
                                mask.iter_mut().for_each(|m| *m = true);
                            }
                        }
                        for ob in &stmt.order_by {
                            collect_expr_columns(&ob.expr, &mut mask);
                        }
                        for gb in &stmt.group_by {
                            collect_expr_columns(gb, &mut mask);
                        }
                        if let Some(ref having) = stmt.having {
                            collect_expr_columns(having, &mut mask);
                        }
                        if mask.iter().all(|&b| b) {
                            None
                        } else {
                            Some(mask)
                        }
                    };
                    TableEngine::scan_table(
                        storage,
                        &resolved.def,
                        &resolved.columns,
                        snap,
                        decode_mask_no_where.as_deref(),
                    )?
                }
            }
            crate::planner::AccessMethod::IndexLookup { index_def, key }
                if resolved.def.is_clustered() && index_def.is_primary =>
            {
                // ── Clustered PK point lookup (Phase 39.15) ──────────────────
                // Direct B-tree search returns full row inline — no heap fetch.
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
                // Bloom filter: skip B-Tree read if key is definitely absent.
                // Only applied for UNIQUE indexes — non-unique indexes store key||RID in
                // the bloom (one entry per row), but the lookup key here is the bare value.
                // Checking a bare value key against a bloom populated with key||RID entries
                // produces false negatives, so we skip the bloom check for non-unique indexes.
                // Skip bloom for primary key: deferred deletion model guarantees the
                // key is in the B-Tree after INSERT, so the bloom check is wasted cycles.
                if index_def.is_unique
                    && !index_def.is_primary
                    && !bloom.might_exist(index_def.index_id, key)
                {
                    vec![]
                } else if index_def.is_unique {
                    // Unique index: exact key lookup → at most one RecordId.
                    match BTree::lookup_in(storage, index_def.root_page_id, key)? {
                        None => vec![],
                        Some(rid) => {
                            if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap)?
                            {
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
                        if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap)? {
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
                // ── Clustered PK range scan (Phase 39.15) ────────────────────
                // Single pass through clustered leaves. No heap indirection.
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
                                let values = axiomdb_types::codec::decode_row(data, &col_types)?;
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
            // Skip redundant WHERE re-evaluation when scan_table_filtered
            // already applied the predicate (Phase 8.1 optimization).
            if !where_already_applied {
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
        let from_t = resolve_table_cached(storage, txn, ctx, &from_ref)?;
        col_offsets.push(running_offset);
        running_offset += from_t.columns.len();
        all_resolved.push(from_t);

        for join in &stmt.joins {
            match &join.table {
                FromClause::Table(tref) => {
                    let jt = resolve_table_cached(storage, txn, ctx, tref)?;
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

    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let mut scanned: Vec<Vec<Row>> = Vec::with_capacity(all_resolved.len());
    for t in &all_resolved {
        let rows = crate::table::scan_table_any_layout(storage, &t.def, &t.columns, snap)?;
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
                            snap,
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
    let inner_result = execute_select_ctx(inner_query, storage, txn, &temp_bloom, &mut temp_ctx)?;
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
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let mut scanned: Vec<Vec<Row>> = Vec::with_capacity(all_resolved.len());
    for t in &all_resolved {
        let rows = crate::table::scan_table_any_layout(storage, &t.def, &t.columns, snap)?;
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
