fn execute_select_ctx(
    mut stmt: SelectStmt,
    exec_ctx: &ExecutionContext,
    conn_txn: Option<&ConnectionTxn>,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let bloom = exec_ctx.bloom();
    // Set the session collation for all eval() calls in this ctx execution.
    // Cleared automatically when _coll_guard is dropped at function exit.
    let _coll_guard = CollationGuard::new(ctx.effective_collation());

    // SELECT without FROM: no table resolution needed.
    if stmt.from.is_none() {
        return execute_select(stmt, storage, txn, conn_txn);
    }

    // Subquery in FROM: no caching path yet — delegate.
    if matches!(stmt.from, Some(FromClause::Subquery { .. })) {
        return execute_select(stmt, storage, txn, conn_txn);
    }

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
        let default_db = ctx.selected_database().unwrap_or(DEFAULT_DATABASE_NAME);
        return execute_information_schema_select(
            stmt,
            from_table_ref,
            storage,
            txn,
            conn_txn,
            default_db,
        );
    }

    if stmt.joins.is_empty() {
        // Single-table path — use cache.
        let resolved = resolve_table_cached(storage, txn, ctx, conn_txn, &from_table_ref)?;
        let snap = if let Some(ct) = conn_txn {
            txn.active_snapshot(ct)
        } else {
            txn.snapshot()
        };

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
            let mut reader = CatalogReader::new(storage, snap.clone())?;
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
                crate::expr::Expr::IsBoolean { expr, .. } => collect_expr_columns(expr, mask),
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
                | crate::expr::Expr::Default
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
                        && !stmt.calc_found_rows
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
                        if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap.clone())? {
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
                    if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap.clone())? {
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
            crate::planner::AccessMethod::GinScan {
                index_def,
                query_terms,
            } => gin_scan_rows(storage, &resolved, index_def, query_terms, snap.clone())?,
        };

        // ── EXISTS decorrelation fast-path ────────────────────────────────────
        let mut combined_rows: Vec<Row> = if !where_already_applied {
            if let Some(ref wc) = stmt.where_clause {
                if let Some(decorr) = try_extract_exists_decorrelation(wc) {
                    apply_exists_semijoin(raw_rows, &decorr, exec_ctx.storage(), exec_ctx.coord())?
                } else {
                    let mut rows = Vec::new();
                    let mut sq_cache_ctx: SubqueryCache = HashMap::new();
                    let mut in_set_cache_ctx: InSetCache = HashMap::new();
                    let mut corr_cache_ctx: CorrelatedCache = HashMap::new();
                    let mut mat_cache_ctx: MaterializedCache = HashMap::new();
                    for (_rid, values) in raw_rows {
                        let mut runner = ExecSubqueryRunner {
                            storage: exec_ctx.storage(),
                            txn: exec_ctx.coord(),
                            bloom: exec_ctx.bloom(),
                            ctx,
                            outer_row: &values,
                            cache: Some(&mut sq_cache_ctx),
                            in_set_cache: Some(&mut in_set_cache_ctx),
                            correlated_cache: Some(&mut corr_cache_ctx),
                            materialized: Some(&mut mat_cache_ctx),
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
            }
        } else {
            raw_rows.into_iter().map(|(_rid, v)| v).collect()
        };

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

        let resolved_ob = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
        // Top-N optimization: partial sort when ORDER BY + LIMIT present.
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
        let mut proj_cache_ctx: SubqueryCache = HashMap::new();
        let mut proj_in_set_ctx: InSetCache = HashMap::new();
        let mut proj_corr_ctx: CorrelatedCache = HashMap::new();
        let mut proj_mat_ctx: MaterializedCache = HashMap::new();
        let mut rows: Vec<Row> = Vec::with_capacity(combined_rows.len());
        for v in &combined_rows {
            let mut runner = ExecSubqueryRunner {
                storage: exec_ctx.storage(),
                txn: exec_ctx.coord(),
                bloom: exec_ctx.bloom(),
                ctx,
                outer_row: v,
                cache: Some(&mut proj_cache_ctx),
                in_set_cache: Some(&mut proj_in_set_ctx),
                correlated_cache: Some(&mut proj_corr_ctx),
                materialized: Some(&mut proj_mat_ctx),
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
        // Multi-table JOIN path — use cache for each table.
        execute_select_with_joins_ctx(stmt, from_table_ref, exec_ctx, conn_txn, ctx)
    }
}
