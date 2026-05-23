/// Benchmark-fairness knob: when `AXIOMDB_NO_COUNT_CACHE` is set in the
/// environment, the per-session `SELECT COUNT(*)` memoization is disabled so the
/// query always scans fresh. Engines like SQLite and PostgreSQL never memoize
/// COUNT(*), so a like-for-like scan-vs-scan comparison must turn ours off;
/// otherwise the bench measures an O(1) cache hit against their O(n) scan.
/// Mirrors the `AXIOMDB_BENCH_REDO` durability knob. Read once and cached.
fn count_cache_disabled() -> bool {
    use std::sync::OnceLock;
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var_os("AXIOMDB_NO_COUNT_CACHE").is_some())
}

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

    // Phase 21.9b: subquery with embedded set operation (UNION/INTERSECT/EXCEPT).
    // The parser folds tails into `set_op_rest` when inside a FROM/JOIN subquery.
    if !stmt.set_op_rest.is_empty() {
        let rest = std::mem::take(&mut stmt.set_op_rest);
        return execute_set_op_ctx(stmt, rest, exec_ctx, conn_txn, ctx);
    }

    // SELECT without FROM: no table resolution needed.
    if stmt.from.is_none() {
        return execute_select_no_from_ctx(stmt, exec_ctx, ctx);
    }

    // Subquery in FROM: no caching path yet — delegate.
    if matches!(stmt.from, Some(FromClause::Subquery { .. })) {
        return execute_select(stmt, storage, txn, conn_txn);
    }

    // JSON_TABLE in FROM (Phase 11.20a): no caching path — delegate.
    if matches!(stmt.from, Some(FromClause::JsonTable(_))) {
        return execute_select(stmt, storage, txn, conn_txn);
    }

    // JSONB SRF in FROM (Phase 11.25a): no caching path — delegate.
    if matches!(stmt.from, Some(FromClause::JsonbSrf(_))) {
        return execute_select(stmt, storage, txn, conn_txn);
    }

    // VALUES in FROM (Phase 21.22): no caching path — delegate.
    if matches!(stmt.from, Some(FromClause::Values(_))) {
        return execute_select(stmt, storage, txn, conn_txn);
    }

    // Recursive CTE in FROM (Phase 21.3b).
    if matches!(stmt.from, Some(FromClause::RecursiveCte(_))) {
        return execute_select_recursive_cte_ctx(stmt, exec_ctx, conn_txn, ctx);
    }

    // UNNEST in FROM (Phase 20.4 Step 7): delegate to execute_select which handles it.
    if matches!(stmt.from, Some(FromClause::Unnest(_))) {
        return execute_select(stmt, storage, txn, conn_txn);
    }

    // GENERATE_SERIES in FROM (Phase 20.10): delegate to execute_select which handles it.
    if matches!(stmt.from, Some(FromClause::GenerateSeries(_))) {
        return execute_select(stmt, storage, txn, conn_txn);
    }

    // READ_PARQUET in FROM (Phase 20.6): delegate to execute_select which handles it.
    if matches!(stmt.from, Some(FromClause::ReadParquet(_))) {
        return execute_select(stmt, storage, txn, conn_txn);
    }

    // XMLTABLE in FROM (Phase 20.20): delegate to execute_select which handles it.
    if matches!(stmt.from, Some(FromClause::XmlTable(_))) {
        return execute_select(stmt, storage, txn, conn_txn);
    }

    let from_table_ref = match stmt.from.take() {
        Some(FromClause::Table(tref)) => tref,
        _ => unreachable!(
            "already handled None, Subquery, JsonTable, JsonbSrf, Values, RecursiveCte, GenerateSeries, ReadParquet, XmlTable above"
        ),
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
            ctx.temp_schema_name(),
        );
    }

    if !stmt.joins.is_empty()
        && stmt
            .hints
            .iter()
            .any(|hint| matches!(hint, crate::ast::SelectHint::Index { .. }))
    {
        return Err(DbError::NotImplemented {
            feature: "INDEX(table index) hint on joined SELECT — single-table MVP only".into(),
        });
    }

    if stmt.joins.is_empty() {
        // Single-table path — use cache.
        let resolved = crate::time_select_phase!(
            resolve_ns,
            resolve_table_cached(storage, txn, ctx, conn_txn, &from_table_ref)?
        );
        let snap = if let Some(ct) = conn_txn {
            txn.active_snapshot(ct)
        } else {
            txn.snapshot()
        };

        // Phase 22b.2 + 22b.6: if this is a foreign table, hand off to the FDW
        // scan path which issues an HTTP GET and materialises the result.
        // Phase 22b.6 extracts equality predicates for URL construction. The full
        // WHERE is always preserved for local filtering (correctness guarantee).
        if resolved.def.id >= FOREIGN_TABLE_ID_BASE {
            let (pushed, _) = extract_fdw_pushable(stmt.where_clause.as_ref(), &resolved.columns);

            let pushed_limit: Option<u64> = stmt.limit.as_ref().and_then(|e| match e {
                Expr::Literal(Value::Int(n)) if *n >= 0 => Some(*n as u64),
                Expr::Literal(Value::BigInt(n)) if *n >= 0 => Some(*n as u64),
                _ => None,
            });

            let fdw_rows = fdw_scan_table(
                storage,
                snap,
                resolved.def.id,
                &resolved.columns,
                &pushed,
                pushed_limit,
            )?;

            let first_source = join_source_schema_from_resolved(&from_table_ref, &resolved);
            let first_rows: Vec<Row> = fdw_rows.into_iter().map(|(_, r)| r).collect();

            return execute_select_with_joins_first_materialized(
                stmt,
                first_source,
                first_rows,
                exec_ctx,
                conn_txn,
                ctx,
            );
        }

        // ── COUNT(*) fast path (Phase 8) ─────────────────────────────────
        // Detect `SELECT COUNT(*) FROM table` with no WHERE, no GROUP BY,
        // no HAVING, no JOIN. For heap tables use HeapChain::count_visible().
        // For clustered tables use count_clustered_visible() (header-only scan).
        // Both paths: zero column decode, zero allocs.
        if stmt.where_clause.is_none()
            && stmt.group_by.is_empty()
            && stmt.having.is_none()
            && stmt.columns.len() == 1
            // TABLESAMPLE must actually sample — skip the count fast path so the
            // query falls through to the sampled scan below (Phase 20.11).
            && from_table_ref.tablesample.is_none()
        {
            if let Some(crate::ast::SelectItem::Expr {
                expr: crate::expr::Expr::Function { ref name, ref args },
                ..
            }) = stmt.columns.first()
            {
                if name.eq_ignore_ascii_case("count") && args.is_empty() {
                    // Attack 17b: per-session COUNT(*) cache. Hits when no
                    // writes have hit this table since cache time AND no
                    // DDL has bumped its schema_version. Only used in
                    // autocommit mode — inside an explicit BEGIN..ROLLBACK,
                    // `stats.changes_for` doesn't unwind on rollback so
                    // caching there would risk returning a stale count
                    // (post-rollback). Autocommit queries each commit
                    // immediately, so the counter always reflects
                    // committed state.
                    let cacheable =
                        ctx.autocommit && !ctx.in_explicit_txn && !count_cache_disabled();
                    let count = match cacheable
                        .then(|| {
                            ctx.get_count_star(
                                resolved.def.id,
                                resolved.def.schema_version,
                                txn.write_commit_seq(),
                            )
                        })
                        .flatten()
                    {
                        Some(c) => c,
                        None => {
                            let c = if resolved.def.is_clustered() {
                                crate::table::count_clustered_visible(
                                    storage,
                                    resolved.def.root_page_id,
                                    snap,
                                )?
                            } else {
                                HeapChain::count_visible(storage, resolved.def.root_page_id, snap)?
                            };
                            if cacheable {
                                ctx.cache_count_star(
                                    resolved.def.id,
                                    resolved.def.schema_version,
                                    c,
                                    txn.write_commit_seq(),
                                );
                            }
                            c
                        }
                    };
                    let columns = vec![ColumnMeta::computed("count(*)", DataType::BigInt)];
                    let rows = vec![vec![Value::BigInt(count as i64)]];
                    return Ok(QueryResult::Rows { columns, rows });
                }
            }
        }

        // ── Query planner: pick the best access method ────────────────────
        // Load per-column statistics for cost-based index selection (Phase 6.10).
        // Per-table stats cache: the planner reads these once per statement for
        // cost-based index selection, but they change only on ANALYZE / DDL.
        // Cache by (table_id, schema_version) so warm queries skip rebuilding a
        // CatalogReader + rescanning the stats heap (≈580 ns). ANALYZE drops the
        // entry via invalidate_table; DDL bumps schema_version → version miss.
        let table_stats: Arc<Vec<axiomdb_catalog::StatsDef>> =
            crate::time_select_phase!(stats_ns, {
                let tid = resolved.def.id;
                let ver = resolved.def.schema_version;
                if let Some(cached) = ctx.get_stats_cached(tid, ver) {
                    cached
                } else {
                    let mut reader = CatalogReader::new(storage, snap.clone())?;
                    let stats = reader.list_stats(tid).unwrap_or_default();
                    ctx.cache_stats(tid, ver, stats)
                }
            });
        // Collect SELECT column indices for index-only scan detection (Phase 6.13).
        // Returns empty slice for SELECT * (wildcard) → conservative, no index-only.
        let select_col_idxs: Vec<u16> = collect_select_col_idxs(&stmt);

        // Compute collation before the mutable borrow of ctx.stats below.
        let effective_coll = ctx.effective_collation();
        let mut access_method = crate::time_select_phase!(
            plan_ns,
            crate::planner::plan_select_ctx(
                stmt.where_clause.as_ref(),
                &resolved.indexes,
                &resolved.columns,
                resolved.def.id,
                &table_stats,
                &mut ctx.stats,
                &select_col_idxs,
                effective_coll,
            )
        );
        if let Some(hinted_index) =
            stmt.hinted_index_name_for_table(&from_table_ref, &resolved.def.table_name)?
        {
            access_method = crate::planner::apply_select_index_hint_ctx(
                access_method,
                hinted_index,
                stmt.where_clause.as_ref(),
                &resolved.indexes,
                &resolved.columns,
                resolved.def.id,
                &table_stats,
                &mut ctx.stats,
                &select_col_idxs,
                effective_coll,
            )?;
        }
        let access_method =
            normalize_clustered_access_method(access_method, resolved.def.is_clustered());

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
                crate::expr::Expr::Collate { expr, .. } => {
                    collect_expr_columns(expr, mask);
                }
                crate::expr::Expr::Function { args, .. } => {
                    for arg in args {
                        collect_expr_columns(arg, mask);
                    }
                }
                crate::expr::Expr::Window { spec, .. } => {
                    for expr in &spec.partition_by {
                        collect_expr_columns(expr, mask);
                    }
                    for item in &spec.order_by {
                        collect_expr_columns(&item.expr, mask);
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
                // ARRAY_AGG also has expression + ORDER BY expressions inside it.
                crate::expr::Expr::ArrayAgg { expr, order_by, .. } => {
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
                // GROUPING() args may reference columns — recurse.
                crate::expr::Expr::Grouping { args, .. } => {
                    for a in args {
                        collect_expr_columns(a, mask);
                    }
                }
                // Phase 20.4 — ARRAY[expr, ...]: recurse into elements.
                crate::expr::Expr::ArrayConstructor { elements } => {
                    for a in elements {
                        collect_expr_columns(a, mask);
                    }
                }
                // Phase 20.4, Step 5 — array subscript: recurse into array and index.
                crate::expr::Expr::Subscript {
                    array,
                    index,
                    slice,
                } => {
                    collect_expr_columns(array, mask);
                    collect_expr_columns(index, mask);
                    if let Some(s) = slice {
                        collect_expr_columns(s, mask);
                    }
                }
                // Subquery internals are not scanned — they run as separate queries.
                crate::expr::Expr::Literal(_)
                | crate::expr::Expr::Default
                | crate::expr::Expr::Param { .. }
                | crate::expr::Expr::OuterColumn { .. }
                | crate::expr::Expr::InsertValue { .. }
                | crate::expr::Expr::ExcludedValue { .. }
                | crate::expr::Expr::SqlJsonQuery { .. }
                | crate::expr::Expr::Subquery(_)
                | crate::expr::Expr::InSubquery { .. }
                | crate::expr::Expr::Exists { .. } => {}
                // Phase 20.4 — ANY/ALL: recurse into expr (comparison target) and array.
                crate::expr::Expr::AnyOf { expr, array, .. }
                | crate::expr::Expr::AllOf { expr, array, .. } => {
                    collect_expr_columns(expr, mask);
                    collect_expr_columns(array, mask);
                }
                crate::expr::Expr::Row(elems) => {
                    for e in elems {
                        collect_expr_columns(e, mask);
                    }
                }
                crate::expr::Expr::FieldAccess { col_idx, .. } => {
                    if *col_idx < mask.len() {
                        mask[*col_idx] = true;
                    }
                }
                // Phase 20.20 — XML constructor forms: recurse into sub-expressions.
                crate::expr::Expr::XmlElement { attrs, content, .. } => {
                    for (e, _) in attrs {
                        collect_expr_columns(e, mask);
                    }
                    for e in content {
                        collect_expr_columns(e, mask);
                    }
                }
                crate::expr::Expr::XmlForest { items } => {
                    for (e, _) in items {
                        collect_expr_columns(e, mask);
                    }
                }
                crate::expr::Expr::XmlRoot { doc, .. } => collect_expr_columns(doc, mask),
                crate::expr::Expr::XmlConcat { args } => {
                    for e in args {
                        collect_expr_columns(e, mask);
                    }
                }
                crate::expr::Expr::XmlQuery { doc, .. } => collect_expr_columns(doc, mask),
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
                    for ob in &stmt.order_by {
                        collect_expr_columns(&ob.expr, &mut mask);
                    }
                    for gb in stmt.group_by.exprs() {
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
                if let Some(ref wc) = stmt.where_clause {
                    // Phase 39.20: compile a raw-byte BatchPredicate so rows that
                    // fail the WHERE are filtered BEFORE decode — never paying the
                    // Vec<Value> + per-TEXT String allocations for rejected rows.
                    // Mirrors the heap scan fast path (scan_table_filtered_parallel).
                    let col_types_dt: Vec<axiomdb_types::DataType> = resolved
                        .columns
                        .iter()
                        .map(|c| crate::table::column_type_to_data_type(c.col_type))
                        .collect();
                    if let Some(bp) = crate::eval::batch::try_compile(wc, &col_types_dt) {
                        // try_compile only accepts predicates whose eval_on_raw is
                        // exactly equivalent to eval() — the predicate is fully
                        // applied here, so skip the later re-filter.
                        where_already_applied = true;
                        crate::table::scan_clustered_table_filtered(
                            storage,
                            &resolved.def,
                            &resolved.columns,
                            snap,
                            clustered_decode_mask.as_deref(),
                            &bp,
                        )?
                    } else {
                        // Fallback (OR / LIKE / IN / Text / subquery, etc.):
                        // decode all rows, then filter via scalar eval.
                        if !expr_has_subquery(wc) {
                            where_already_applied = true;
                        }
                        let mut rows = crate::table::scan_clustered_table_masked(
                            storage,
                            &resolved.def,
                            &resolved.columns,
                            snap,
                            clustered_decode_mask.as_deref(),
                        )?;
                        rows.retain(|(_, values)| match eval(wc, values) {
                            Ok(v) => is_truthy(&v),
                            Err(_) => true,
                        });
                        rows
                    }
                } else {
                    crate::table::scan_clustered_table_masked(
                        storage,
                        &resolved.def,
                        &resolved.columns,
                        snap,
                        clustered_decode_mask.as_deref(),
                    )?
                }
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
                        for gb in stmt.group_by.exprs() {
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
                        for gb in stmt.group_by.exprs() {
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
            crate::planner::AccessMethod::IndexLookup {
                index_def,
                key,
                covers_predicate,
            } if resolved.def.is_clustered() && index_def.is_primary => {
                // ── Clustered PK point lookup (Phase 39.15) ──────────────────
                // Direct B-tree search returns full row inline — no heap fetch.
                // The exact-PK key reproduces the entire WHERE, so skip the
                // per-row recheck below (SQLite TERM_CODED analog), exactly as
                // the covering IndexRange arm does.
                if *covers_predicate {
                    where_already_applied = true;
                }
                // Attack 5: pass the session's leaf hint so consecutive PK
                // lookups in the same leaf skip the descent.
                let looked_up = crate::time_select_phase!(
                    lookup_ns,
                    crate::table::lookup_clustered_row_with_hint(
                        storage,
                        &resolved.def,
                        &resolved.columns,
                        key,
                        snap,
                        Some(ctx.clustered_leaf_hint_slot()),
                    )?
                );
                match looked_up {
                    Some(pair) => vec![pair],
                    None => vec![],
                }
            }
            crate::planner::AccessMethod::IndexLookup { index_def, key, .. }
                if resolved.def.is_clustered() =>
            {
                clustered_secondary_rows_for_lookup(storage, &resolved, index_def, key, snap)?
            }
            crate::planner::AccessMethod::IndexLookup { index_def, key, .. } => {
                // Bloom filter: skip B-Tree read if key is definitely absent.
                // Only applied for UNIQUE indexes — non-unique indexes store key||RID in
                // the bloom (one entry per row), but the lookup key here is the bare value.
                // Checking a bare value key against a bloom populated with key||RID entries
                // produces false negatives, so we skip the bloom check for non-unique indexes.
                // Skip bloom for primary key: deferred deletion model guarantees the
                // key is in the B-Tree after INSERT, so the bloom check is wasted cycles.
                if index_def.is_unique
                    && !index_def.is_primary
                    && index_def.include_columns.is_empty()
                    && !bloom.might_exist(index_def.index_id, key)
                {
                    vec![]
                } else {
                    let lo = rid_lo(key);
                    let hi = rid_hi(key);
                    let pairs =
                        BTree::range_in(storage, index_def.root_page_id, Some(&lo), Some(&hi))?;
                    let mut result = Vec::with_capacity(pairs.len());
                    for (rid, _k) in pairs {
                        if !HeapChain::is_slot_visible(
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
            crate::planner::AccessMethod::IndexRange {
                index_def,
                lo,
                hi,
                lo_inclusive,
                hi_inclusive,
                covers_predicate,
            } if resolved.def.is_clustered() && index_def.is_primary => {
                // ── Clustered PK range scan (Phase 39.15) ────────────────────
                // Single pass through clustered leaves. No heap indirection.
                // Bounds are honored exactly (exclusive `<`/`>` excludes the
                // boundary key); when the range reproduces the whole WHERE the
                // per-row recheck below is skipped (SQLite TERM_CODED analog).
                if *covers_predicate {
                    where_already_applied = true;
                }
                crate::table::range_clustered_table(
                    storage,
                    &resolved.def,
                    &resolved.columns,
                    lo.as_deref(),
                    hi.as_deref(),
                    *lo_inclusive,
                    *hi_inclusive,
                    snap,
                )?
            }
            crate::planner::AccessMethod::IndexRange {
                index_def, lo, hi, ..
            } if resolved.def.is_clustered() => clustered_secondary_rows_for_range(
                storage,
                &resolved,
                index_def,
                lo.as_deref(),
                hi.as_deref(),
                snap,
            )?,
            crate::planner::AccessMethod::IndexRange {
                index_def, lo, hi, ..
            } => {
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
                n_key_cols: _,
                n_include_cols,
                needed_key_positions: _,
            } => {
                // Index-only scan (Phase 6.13): values decoded from B-Tree key bytes.
                // Only the 24-byte heap slot header is read for MVCC visibility.
                // Non-unique: lo/hi need RID suffix for correct range bounds.
                let lo_adj = rid_lo(lo);
                let hi_adj = hi.as_deref().map(rid_hi);
                let (lo_ref, hi_ref) = (Some(lo_adj.as_slice()), hi_adj.as_deref());
                let pairs = BTree::range_in(storage, index_def.root_page_id, lo_ref, hi_ref)?;
                let n_table_cols = resolved.columns.len();
                let mut result = Vec::with_capacity(pairs.len());
                for (rid, key_bytes) in pairs {
                    if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap.clone())?
                    {
                        continue;
                    }
                    let decoded = crate::index_maintenance::decode_secondary_entry_values(
                        index_def, &key_bytes,
                    );
                    let mut row_values = vec![Value::Null; n_table_cols];
                    match decoded {
                        Ok((all_key_vals, include_vals)) => {
                            for (key_pos, idx_col) in index_def.columns.iter().enumerate() {
                                let table_idx = idx_col.col_idx as usize;
                                if let (true, Some(val)) =
                                    (table_idx < n_table_cols, all_key_vals.get(key_pos))
                                {
                                    row_values[table_idx] = val.clone();
                                }
                            }
                            if *n_include_cols == index_def.include_columns.len() {
                                for (include_pos, col_idx) in
                                    index_def.include_columns.iter().enumerate()
                                {
                                    let table_idx = *col_idx as usize;
                                    if let (true, Some(val)) =
                                        (table_idx < n_table_cols, include_vals.get(include_pos))
                                    {
                                        row_values[table_idx] = val.clone();
                                    }
                                }
                                result.push((rid, row_values));
                            } else if let Some(values) =
                                TableEngine::read_row(storage, &resolved.columns, rid)?
                            {
                                result.push((rid, values));
                            }
                        }
                        Err(_) => {
                            if let Some(values) =
                                TableEngine::read_row(storage, &resolved.columns, rid)?
                            {
                                result.push((rid, values));
                            }
                        }
                    }
                }
                result
            }
            crate::planner::AccessMethod::GinScan {
                index_def,
                query_terms,
            } => gin_scan_rows(storage, &resolved, index_def, query_terms, snap.clone())?,
        };

        // ── Phase 20.11: TABLESAMPLE — sample the scanned base rows ────────────
        // The ctx executor (wire path) previously ignored TABLESAMPLE entirely,
        // so SYSTEM(0) returned every row. Sample here: 0% → none, 100% → all,
        // otherwise keep each row with probability `percent/100`. SYSTEM is
        // applied at row granularity (the same approximation select_core uses for
        // clustered tables). COUNT(*) reaches this because its fast path is now
        // skipped when a TABLESAMPLE clause is present.
        let mut raw_rows = raw_rows;
        if let Some(s) = &from_table_ref.tablesample {
            if s.percent <= 0.0 {
                raw_rows.clear();
            } else if s.percent < 100.0 {
                use rand::Rng;
                let threshold = s.percent / 100.0;
                let mut rng = rand::thread_rng();
                raw_rows.retain(|_| rng.gen::<f64>() < threshold);
            }
        }

        // ── Phase 13.7: SELECT … FOR UPDATE / FOR SHARE row-level locking ──────
        // Handled BEFORE the normal combined_rows pipeline so we can keep RecordIds
        // through WHERE → ORDER BY → LIMIT and lock exactly the returned rows.
        // Only active when lock_clause is Some AND no GROUP BY/aggregates
        // (GROUP BY + FOR UPDATE is semantically undefined; fall through to non-lock path).
        if let Some(ref lc) = stmt.lock_clause {
            if resolved.def.id >= FOREIGN_TABLE_ID_BASE {
                return Err(DbError::NotImplemented {
                    feature: "FOR UPDATE is not supported on foreign tables".into(),
                });
            }
            if resolved.def.is_clustered() {
                return Err(DbError::NotImplemented {
                    feature: "FOR UPDATE on clustered tables not yet supported".into(),
                });
            }

            // Only take the locking fast path for non-aggregate queries.
            if stmt.group_by.is_empty() && !has_aggregates(&stmt.columns, &stmt.having) {
                // Step 1: WHERE filter, keeping RecordIds.
                let mut rid_pairs: Vec<(RecordId, Row)> = if !where_already_applied {
                    if let Some(ref wc) = stmt.where_clause {
                        let mut filtered = Vec::new();
                        let mut sq_cache_lk: SubqueryCache = HashMap::new();
                        let mut in_set_lk: InSetCache = HashMap::new();
                        let mut corr_lk: CorrelatedCache = HashMap::new();
                        let mut mat_lk: MaterializedCache = HashMap::new();
                        for (rid, values) in raw_rows {
                            let mut runner = ExecSubqueryRunner {
                                storage: exec_ctx.storage(),
                                txn: exec_ctx.coord(),
                                bloom: exec_ctx.bloom(),
                                ctx,
                                outer_row: &values,
                                cache: Some(&mut sq_cache_lk),
                                in_set_cache: Some(&mut in_set_lk),
                                correlated_cache: Some(&mut corr_lk),
                                materialized: Some(&mut mat_lk),
                            };
                            if is_truthy(&eval_with(wc, &values, &mut runner)?) {
                                filtered.push((rid, values));
                            }
                        }
                        filtered
                    } else {
                        raw_rows
                    }
                } else {
                    raw_rows
                };

                // Step 2: ORDER BY — sort pairs by the value component.
                let resolved_ob_lk = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
                if !resolved_ob_lk.is_empty() {
                    let mut sort_err: Option<DbError> = None;
                    rid_pairs.sort_by(|(_, a), (_, b)| {
                        if sort_err.is_some() {
                            return std::cmp::Ordering::Equal;
                        }
                        match compare_rows_for_sort(a, b, &resolved_ob_lk) {
                            Ok(ord) => ord,
                            Err(e) => {
                                sort_err = Some(e);
                                std::cmp::Ordering::Equal
                            }
                        }
                    });
                    if let Some(e) = sort_err {
                        return Err(e);
                    }
                }

                // Determine lock modes.
                let (table_mode, row_mode) = match lc.strength {
                    LockStrength::ForKeyShare | LockStrength::ForShare => (
                        axiomdb_lock::LockMode::IntentionShared,
                        axiomdb_lock::LockMode::Shared,
                    ),
                    LockStrength::ForNoKeyUpdate | LockStrength::ForUpdate => (
                        axiomdb_lock::LockMode::IntentionExclusive,
                        axiomdb_lock::LockMode::Exclusive,
                    ),
                };

                if lc.wait_policy == LockWaitPolicy::SkipLocked {
                    // SkipLocked pipeline: ORDER BY already done; no LIMIT yet.
                    // Try-lock each candidate row, keep only granted ones, then apply LIMIT.
                    if let (Some(lm), Some(ct)) = (exec_ctx.lock_manager(), conn_txn) {
                        lm.acquire_table_lock_sync(ct.txn_id, resolved.def.id, table_mode)?;
                        let mut locked_pairs: Vec<(RecordId, Row)> =
                            Vec::with_capacity(rid_pairs.len());
                        for (rid, row) in rid_pairs {
                            if lm.try_acquire_record_lock_sync(
                                ct.txn_id,
                                rid.page_id,
                                rid.slot_id,
                                row_mode,
                            )? {
                                locked_pairs.push((rid, row));
                            }
                            // Ok(false) → silently skip this row
                        }
                        // Apply LIMIT/OFFSET on the filtered (locked) set.
                        let (limit_n, offset_n) =
                            eval_limit_offset_usize(&stmt.limit, &stmt.offset)?;
                        if offset_n > 0 {
                            let skip = offset_n.min(locked_pairs.len());
                            locked_pairs = locked_pairs[skip..].to_vec();
                        }
                        if let Some(n) = limit_n {
                            locked_pairs.truncate(n);
                        }
                        rid_pairs = locked_pairs;
                    }
                    // No lock_manager or no conn_txn → fall through with full rid_pairs.
                    // Apply LIMIT normally in that case.
                    if exec_ctx.lock_manager().is_none() || conn_txn.is_none() {
                        let (limit_n, offset_n) =
                            eval_limit_offset_usize(&stmt.limit, &stmt.offset)?;
                        if offset_n > 0 {
                            let skip = offset_n.min(rid_pairs.len());
                            rid_pairs = rid_pairs[skip..].to_vec();
                        }
                        if let Some(n) = limit_n {
                            rid_pairs.truncate(n);
                        }
                    }
                } else {
                    // Block / NoWait pipeline: LIMIT first, then acquire all.
                    // Step 3: LIMIT/OFFSET — lock only the rows that will be returned.
                    let (limit_n, offset_n) = eval_limit_offset_usize(&stmt.limit, &stmt.offset)?;
                    if offset_n > 0 {
                        let skip = offset_n.min(rid_pairs.len());
                        rid_pairs = rid_pairs[skip..].to_vec();
                    }
                    if let Some(n) = limit_n {
                        rid_pairs.truncate(n);
                    }

                    // Step 4: Acquire table-intention + row-level locks.
                    if let (Some(lm), Some(ct)) = (exec_ctx.lock_manager(), conn_txn) {
                        let mut row_flags = axiomdb_lock::LockFlags::REC_NOT_GAP;
                        if lc.wait_policy == LockWaitPolicy::NoWait {
                            row_flags = row_flags.union(axiomdb_lock::LockFlags::NOWAIT);
                        }
                        lm.acquire_table_lock_sync(ct.txn_id, resolved.def.id, table_mode)?;
                        for (rid, _) in &rid_pairs {
                            lm.acquire_record_lock_sync(
                                ct.txn_id,
                                rid.page_id,
                                rid.slot_id,
                                row_mode,
                                row_flags,
                            )?;
                        }
                    }
                    // No lock_manager or no active txn → silently skip (autocommit path).
                }

                // Step 5: Project and return (ORDER BY + LIMIT already applied above).
                let locked_rows: Vec<Row> = rid_pairs.into_iter().map(|(_, r)| r).collect();
                let out_cols =
                    build_select_column_meta(&stmt.columns, &resolved.columns, &resolved.def)?;
                let mut proj_cache_lk: SubqueryCache = HashMap::new();
                let mut proj_in_set_lk: InSetCache = HashMap::new();
                let mut proj_corr_lk: CorrelatedCache = HashMap::new();
                let mut proj_mat_lk: MaterializedCache = HashMap::new();
                let rows =
                    project_rows_with_window_support(&stmt.columns, &locked_rows, |expr, v| {
                        let mut runner = ExecSubqueryRunner {
                            storage: exec_ctx.storage(),
                            txn: exec_ctx.coord(),
                            bloom: exec_ctx.bloom(),
                            ctx,
                            outer_row: v,
                            cache: Some(&mut proj_cache_lk),
                            in_set_cache: Some(&mut proj_in_set_lk),
                            correlated_cache: Some(&mut proj_corr_lk),
                            materialized: Some(&mut proj_mat_lk),
                        };
                        eval_with(expr, v, &mut runner)
                    })?;
                let rows = expand_unnest_rows(&stmt.columns, rows);
                let rows = if stmt.distinct {
                    apply_distinct_with_session(rows)
                } else {
                    rows
                };
                return Ok(QueryResult::Rows {
                    columns: out_cols,
                    rows,
                });
            }
            // GROUP BY or aggregate + FOR UPDATE: fall through to normal pipeline
            // (locking is skipped — semantically undefined for aggregates).
        }

        // ── EXISTS decorrelation fast-path ────────────────────────────────────
        let mut combined_rows: Vec<Row> = crate::time_select_phase!(
            where_ns,
            if !where_already_applied {
                if let Some(ref wc) = stmt.where_clause {
                    if let Some(decorr) = try_extract_exists_decorrelation(wc) {
                        apply_exists_semijoin(
                            raw_rows,
                            &decorr,
                            exec_ctx.storage(),
                            exec_ctx.coord(),
                        )?
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
            }
        );

        if !stmt.group_by.is_empty() || has_aggregates(&stmt.columns, &stmt.having) {
            // Single-table path: choose sorted strategy when the access method
            // already delivers rows in group-key order (Phase 4.9b).
            let strategy = choose_group_by_strategy_ctx_with_collation(
                stmt.group_by.exprs(),
                &access_method,
                effective_coll,
                &resolved.columns,
            );
            return execute_select_grouped(stmt, combined_rows, strategy);
        }

        let resolved_ob = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
        if !stmt.distinct_on.is_empty() {
            // Phase 21.12 — DISTINCT ON: sort by (distinct_on ASC, then ORDER BY),
            // keep first pre-projection row per DISTINCT ON key group.
            combined_rows = apply_distinct_on(
                combined_rows,
                &stmt.distinct_on,
                &resolved_ob,
                &stmt.columns,
            )?;
        } else {
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
        }

        let out_cols = crate::time_select_phase!(
            colmeta_ns,
            build_select_column_meta(&stmt.columns, &resolved.columns, &resolved.def)?
        );
        // Fast path: a bare `SELECT *` is the identity projection over the
        // already-decoded rows. `project_rows_with_window_support` would clone
        // every row — and every heap value in it (Strings, arrays) — a SECOND
        // time via `extend_from_slice` (see shared.rs). Skip it entirely and
        // move `combined_rows` straight through. Saves ~1 Vec + N heap-value
        // clones per row on the hot full-scan / range-scan path.
        let mut rows = if stmt.columns.len() == 1 && matches!(stmt.columns[0], SelectItem::Wildcard)
        {
            combined_rows
        } else {
            let mut proj_cache_ctx: SubqueryCache = HashMap::new();
            let mut proj_in_set_ctx: InSetCache = HashMap::new();
            let mut proj_corr_ctx: CorrelatedCache = HashMap::new();
            let mut proj_mat_ctx: MaterializedCache = HashMap::new();
            project_rows_with_window_support(&stmt.columns, &combined_rows, |expr, v| {
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
                eval_with(expr, v, &mut runner)
            })?
        };
        // No-op for a bare wildcard (no UNNEST items); kept for the else branch.
        rows = expand_unnest_rows(&stmt.columns, rows);

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

fn execute_select_no_from_ctx(
    stmt: SelectStmt,
    exec_ctx: &ExecutionContext,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let mut runner = ExecSubqueryRunner {
        storage: exec_ctx.storage(),
        txn: exec_ctx.coord(),
        bloom: exec_ctx.bloom(),
        ctx,
        outer_row: &[],
        cache: None,
        in_set_cache: None,
        correlated_cache: None,
        materialized: None,
    };
    let mut out_row: Row = Vec::new();
    let mut out_cols: Vec<ColumnMeta> = Vec::new();
    let has_windows = select_items_have_window_functions(&stmt.columns);
    for item in &stmt.columns {
        match item {
            SelectItem::Expr { expr, alias } => {
                let v = if has_windows {
                    Value::Null
                } else {
                    eval_with(expr, &[], &mut runner)?
                };
                let name = alias
                    .clone()
                    .unwrap_or_else(|| expr_column_name(expr, None));
                let dt = if has_windows {
                    infer_expr_type(expr, &[]).0
                } else {
                    datatype_of_value(&v)
                };
                out_cols.push(ColumnMeta::computed(name, dt));
                if !has_windows {
                    out_row.push(v);
                }
            }
            SelectItem::Wildcard | SelectItem::QualifiedWildcard(_) => {
                return Ok(QueryResult::Rows {
                    columns: vec![],
                    rows: vec![],
                });
            }
        }
    }
    let rows = if has_windows {
        project_rows_with_window_support(&stmt.columns, &[Vec::new()], |expr, row| {
            eval_with(expr, row, &mut runner)
        })?
    } else {
        vec![out_row]
    };
    let rows = expand_unnest_rows(&stmt.columns, rows);
    let rows = if stmt.distinct {
        apply_distinct_with_session(rows)
    } else {
        rows
    };
    Ok(QueryResult::Rows {
        columns: out_cols,
        rows,
    })
}
