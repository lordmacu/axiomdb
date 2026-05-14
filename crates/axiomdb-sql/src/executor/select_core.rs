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
                    // SELECT * with no FROM → empty result (0 columns, 0 rows), MySQL-compatible.
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

    // FROM is present — handle derived table (subquery in FROM) or real table.
    if matches!(stmt.from, Some(FromClause::Subquery { .. })) {
        return execute_select_derived(stmt, storage, txn, conn_txn);
    }

    // JSON_TABLE in FROM (Phase 11.20a).
    if matches!(stmt.from, Some(FromClause::JsonTable(_))) {
        return execute_select_json_table_source(stmt, storage, txn, conn_txn);
    }

    // JSONB SRF in FROM (Phase 11.25a).
    if matches!(stmt.from, Some(FromClause::JsonbSrf(_))) {
        return execute_select_jsonb_srf_source(stmt, storage, txn, conn_txn);
    }

    // VALUES inline table in FROM (Phase 21.22).
    if matches!(stmt.from, Some(FromClause::Values(_))) {
        return execute_select_values_source(stmt, storage, txn, conn_txn);
    }

    // Phase 20.4, Step 7 — UNNEST array expansion in FROM.
    if matches!(stmt.from, Some(FromClause::Unnest(_))) {
        return execute_select_unnest_source(stmt, storage, txn, conn_txn);
    }

    // Extract the FROM table reference.
    let from_table_ref = match stmt.from.take() {
        Some(FromClause::Table(tref)) => tref,
        _ => unreachable!("already handled None, Subquery, JsonTable, JsonbSrf, Values, Unnest above"),
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
            None,
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
        // ── Single-table path (no JOIN) ───────────────────────────────────────
        let resolved = {
            let mut resolver =
                make_resolver_with_database(storage, txn, conn_txn, DEFAULT_DATABASE_NAME)?;
            resolver.resolve_table(from_table_ref.schema.as_deref(), &from_table_ref.name)?
        };
        let snap = conn_txn
            .map(|c| txn.active_snapshot(c))
            .unwrap_or_else(|| txn.snapshot());

        // ── Query planner: pick the best access method (non-ctx path) ────
        // No session context available — use conservative defaults (no stats).
        let mut stale_tracker = crate::session::StaleStatsTracker::default();
        let mut access_method = crate::planner::plan_select(
            stmt.where_clause.as_ref(),
            &resolved.indexes,
            &resolved.columns,
            resolved.def.id,
            &[], // no stats in non-ctx path — always use index (conservative)
            &mut stale_tracker,
            &[], // no select_col_idxs in non-ctx path — no index-only scan
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
                &[],
                &mut stale_tracker,
                &[],
                crate::session::SessionCollation::Binary,
            )?;
        }
        let access_method =
            normalize_clustered_access_method(access_method, resolved.def.is_clustered());

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
                if index_def.is_unique {
                    let lo = rid_lo(key);
                    let hi = rid_hi(key);
                    let pairs =
                        BTree::range_in(storage, index_def.root_page_id, Some(&lo), Some(&hi))?;
                    let mut result = Vec::with_capacity(pairs.len());
                    for (rid, _k) in pairs {
                        if !axiomdb_storage::heap_chain::HeapChain::is_slot_visible(
                            storage,
                            rid.page_id,
                            rid.slot_id,
                            snap.clone(),
                        )? {
                            continue;
                        }
                        if let Some(values) = TableEngine::read_row(storage, &resolved.columns, rid)? {
                            result.push((rid, values));
                        }
                    }
                    result
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
            crate::planner::AccessMethod::GinScan {
                index_def,
                query_terms,
            } => gin_scan_rows(storage, &resolved, index_def, query_terms, snap.clone())?,
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
                        materialized: None,
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
        if !stmt.distinct_on.is_empty() {
            // Phase 21.12 — DISTINCT ON: sort by (distinct_on ASC, then ORDER BY),
            // keep first pre-projection row per DISTINCT ON key group.
            combined_rows = apply_distinct_on(combined_rows, &stmt.distinct_on, &resolved_ob, &stmt.columns)?;
        } else {
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
        }

        let out_cols = build_select_column_meta(&stmt.columns, &resolved.columns, &resolved.def)?;
        let mut proj_cache: SubqueryCache = HashMap::new();
        let mut proj_in_set_cache: InSetCache = HashMap::new();
        let mut proj_corr_cache: CorrelatedCache = HashMap::new();
        let mut rows = project_rows_with_window_support(&stmt.columns, &combined_rows, |expr, v| {
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
                materialized: None,
            };
            eval_with(expr, v, &mut runner)
        })?;

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

/// Phase 20.4, Step 7 — SELECT whose FROM is an `UNNEST(...)` clause.
/// Materializes the array expansion rows, then applies WHERE / GROUP BY /
/// ORDER BY / LIMIT / joins. UNNEST is always LATERAL: when it is the first
/// FROM source (no outer correlation) we pass an empty outer row.
fn execute_select_unnest_source(
    mut stmt: SelectStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    _conn_txn: Option<&ConnectionTxn>,
) -> Result<QueryResult, DbError> {
    let un = match stmt.from.take() {
        Some(FromClause::Unnest(un)) => *un,
        _ => unreachable!("execute_select_unnest_source called with non-Unnest FROM"),
    };

    let alias = un.alias.clone().unwrap_or_else(|| "unnest".to_string());
    // UNNEST as the first FROM source has no outer correlation.
    let derived_rows = crate::unnest::materialize_unnest(&un, None)?;
    let derived_cols = crate::unnest::column_metas_for_unnest(&un);

    if !stmt.joins.is_empty() {
        let mut temp_ctx = SessionContext::new();
        let temp_bloom = crate::bloom::BloomRegistry::new();
        let temp_exec_ctx = ExecutionContext::new(storage, txn, &temp_bloom, None);
        let first_source = join_source_schema_from_derived(&alias, derived_cols);
        return execute_select_with_joins_first_materialized(
            stmt,
            first_source,
            derived_rows,
            &temp_exec_ctx,
            _conn_txn,
            &mut temp_ctx,
        );
    }

    let mut combined_rows: Vec<Row> = Vec::new();
    let mut sq_cache: SubqueryCache = HashMap::new();
    let mut in_set_cache: InSetCache = HashMap::new();
    let mut corr_cache: CorrelatedCache = HashMap::new();
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
                cache: Some(&mut sq_cache),
                in_set_cache: Some(&mut in_set_cache),
                correlated_cache: Some(&mut corr_cache),
                materialized: None,
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

    let resolved_ob = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
    if !stmt.distinct_on.is_empty() {
        combined_rows = apply_distinct_on(combined_rows, &stmt.distinct_on, &resolved_ob, &stmt.columns)?;
    } else if !resolved_ob.is_empty() && stmt.limit.is_some() && !stmt.distinct && !stmt.calc_found_rows {
        let (limit_n, offset_n) = eval_limit_offset_usize(&stmt.limit, &stmt.offset)?;
        let top_n = offset_n + limit_n.unwrap_or(usize::MAX).min(usize::MAX - offset_n);
        combined_rows = apply_order_by_top_n(combined_rows, &resolved_ob, top_n)?;
    } else {
        combined_rows = apply_order_by(combined_rows, &resolved_ob)?;
    }

    let out_cols = build_derived_output_columns(&stmt.columns, &derived_cols)?;
    let mut rows = project_rows_with_window_support(&stmt.columns, &combined_rows, |expr, row| {
        eval(expr, row)
    })?;

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

/// Executes a SELECT whose FROM clause is a derived table: `FROM (SELECT ...) AS alias`.
///
/// The inner query is executed to produce a materialized set of rows, which are
/// then treated as a virtual table for the outer query's WHERE / GROUP BY / ORDER BY.
fn execute_select_derived(
    mut stmt: SelectStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: Option<&ConnectionTxn>,
) -> Result<QueryResult, DbError> {
    let (inner_query, alias) = match stmt.from.take() {
        Some(FromClause::Subquery { query, alias, .. }) => (*query, alias),
        _ => unreachable!("execute_select_derived called with non-subquery FROM"),
    };

    // Execute the inner query to materialize the derived table.
    let mut temp_ctx = SessionContext::new();
    let temp_bloom = crate::bloom::BloomRegistry::new();
    let temp_exec_ctx = ExecutionContext::new(storage, txn, &temp_bloom, None);
    let inner_result = execute_select_ctx(inner_query, &temp_exec_ctx, None, &mut temp_ctx)?;
    let (derived_cols, derived_rows) = match inner_result {
        QueryResult::Rows { columns, rows } => (columns, rows),
        _ => {
            return Err(DbError::Internal {
                message: "derived table inner query did not return rows".into(),
            })
        }
    };

    // Subquery FROM + JOINs: route through the shared join-loop with the
    // materialized subquery rows as first source (mirrors JSON_TABLE join path).
    if !stmt.joins.is_empty() {
        let mut join_ctx = SessionContext::new();
        let join_bloom = crate::bloom::BloomRegistry::new();
        let join_exec_ctx = ExecutionContext::new(storage, txn, &join_bloom, None);
        let first_source = join_source_schema_from_derived(&alias, derived_cols);
        return execute_select_with_joins_first_materialized(
            stmt,
            first_source,
            derived_rows,
            &join_exec_ctx,
            conn_txn,
            &mut join_ctx,
        );
    }

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
                materialized: None,
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
    if !stmt.distinct_on.is_empty() {
        // Phase 21.12 — DISTINCT ON: sort by (distinct_on ASC, then ORDER BY),
        // keep first pre-projection row per DISTINCT ON key group.
        combined_rows = apply_distinct_on(combined_rows, &stmt.distinct_on, &resolved_ob, &stmt.columns)?;
    } else if !resolved_ob.is_empty() && stmt.limit.is_some() && !stmt.distinct && !stmt.calc_found_rows {
        let (limit_n, offset_n) = eval_limit_offset_usize(&stmt.limit, &stmt.offset)?;
        let top_n = offset_n + limit_n.unwrap_or(usize::MAX).min(usize::MAX - offset_n);
        combined_rows = apply_order_by_top_n(combined_rows, &resolved_ob, top_n)?;
    } else {
        combined_rows = apply_order_by(combined_rows, &resolved_ob)?;
    }

    // Build output columns from SELECT list against derived column metadata.
    let out_cols = build_derived_output_columns(&stmt.columns, &derived_cols)?;
    let mut rows = project_rows_with_window_support(&stmt.columns, &combined_rows, |expr, row| {
        eval(expr, row)
    })?;

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

/// Phase 11.20a — executes a SELECT whose FROM clause is a `JSON_TABLE(...)`
/// row source. Mirrors `execute_select_derived`: materialize the rows first,
/// then apply WHERE / GROUP BY / ORDER BY / LIMIT on top.
///
/// Joins with JSON_TABLE on the first source are not supported in 11.20a —
/// the JSON_TABLE → JOIN wiring lives in `select_joins_ctx` which requires a
/// base TableRef FROM. Deferred to 11.20d.
fn execute_select_json_table_source(
    mut stmt: SelectStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    _conn_txn: Option<&ConnectionTxn>,
) -> Result<QueryResult, DbError> {
    let jt_ast = match stmt.from.take() {
        Some(FromClause::JsonTable(jt)) => *jt,
        _ => unreachable!("execute_select_json_table_source called with non-JsonTable FROM"),
    };

    // First-FROM JSON_TABLE has no outer source, so correlated `doc` or PASSING
    // expressions cannot resolve. Reject with a clear semantic error (Phase
    // 11.20d3 — supersedes the earlier "deferred" placeholder).
    if crate::json_table::jsontable_is_correlated(&jt_ast) {
        return Err(DbError::ParseError {
            message: "correlated JSON_TABLE requires an outer FROM source — \
                      doc / PASSING expressions cannot reference outer columns \
                      when JSON_TABLE is the first FROM entry"
                .into(),
            position: None,
        });
    }

    // Compile + evaluate.
    let spec = crate::json_table::compile_json_table(&jt_ast)?;
    let doc_val = crate::eval::eval(&jt_ast.doc, &[])?;
    let derived_rows: Vec<Row> = match crate::json_table::doc_to_serde(&doc_val)? {
        None => Vec::new(),
        Some(sj) => {
            let mut runner = crate::eval::NoSubquery;
            crate::json_table::materialize_json_table(&spec, &sj, &[], &mut runner)?
        }
    };
    let derived_cols = crate::json_table::column_metas_for_spec(&spec);

    // Phase 11.20d2: JSON_TABLE as first FROM combined with JOINs — route to
    // the shared join-loop entry point with the materialized rows as source 0.
    if !stmt.joins.is_empty() {
        let mut temp_ctx = SessionContext::new();
        let temp_bloom = crate::bloom::BloomRegistry::new();
        let temp_exec_ctx = ExecutionContext::new(storage, txn, &temp_bloom, None);
        let first_source = join_source_schema_from_derived(&spec.alias, derived_cols);
        return execute_select_with_joins_first_materialized(
            stmt,
            first_source,
            derived_rows,
            &temp_exec_ctx,
            _conn_txn,
            &mut temp_ctx,
        );
    }

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
                materialized: None,
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

    // ORDER BY + top-N optimization (or DISTINCT ON sort-then-first).
    let resolved_ob = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
    if !stmt.distinct_on.is_empty() {
        // Phase 21.12 — DISTINCT ON: sort by (distinct_on ASC, then ORDER BY),
        // keep first pre-projection row per DISTINCT ON key group.
        combined_rows = apply_distinct_on(combined_rows, &stmt.distinct_on, &resolved_ob, &stmt.columns)?;
    } else if !resolved_ob.is_empty() && stmt.limit.is_some() && !stmt.distinct && !stmt.calc_found_rows {
        let (limit_n, offset_n) = eval_limit_offset_usize(&stmt.limit, &stmt.offset)?;
        let top_n = offset_n + limit_n.unwrap_or(usize::MAX).min(usize::MAX - offset_n);
        combined_rows = apply_order_by_top_n(combined_rows, &resolved_ob, top_n)?;
    } else {
        combined_rows = apply_order_by(combined_rows, &resolved_ob)?;
    }

    // Projection + distinct + limit/offset.
    let out_cols = build_derived_output_columns(&stmt.columns, &derived_cols)?;
    let mut rows = project_rows_with_window_support(&stmt.columns, &combined_rows, |expr, row| {
        eval(expr, row)
    })?;

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

/// Phase 11.25a — SELECT whose FROM clause is a JSONB set-returning
/// function (`jsonb_each`, `jsonb_object_keys`, etc.).
fn execute_select_jsonb_srf_source(
    mut stmt: SelectStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    _conn_txn: Option<&ConnectionTxn>,
) -> Result<QueryResult, DbError> {
    let srf = match stmt.from.take() {
        Some(FromClause::JsonbSrf(s)) => *s,
        _ => unreachable!("execute_select_jsonb_srf_source called with non-JsonbSrf FROM"),
    };

    if crate::jsonb_srf::srf_is_correlated(&srf) {
        return Err(DbError::ParseError {
            message: format!(
                "correlated {} requires an outer FROM source — doc cannot \
                 reference outer columns when {} is the first FROM entry",
                srf.kind.fn_name(),
                srf.kind.fn_name(),
            ),
            position: None,
        });
    }

    let alias = crate::jsonb_srf::srf_alias(&srf);
    let doc_val = crate::eval::eval(&srf.doc, &[])?;
    let derived_rows = crate::jsonb_srf::materialize_jsonb_srf(srf.kind, &doc_val)?;
    let derived_cols = crate::jsonb_srf::column_metas_for_srf(srf.kind, &alias);

    if !stmt.joins.is_empty() {
        let mut temp_ctx = SessionContext::new();
        let temp_bloom = crate::bloom::BloomRegistry::new();
        let temp_exec_ctx = ExecutionContext::new(storage, txn, &temp_bloom, None);
        let first_source = join_source_schema_from_derived(&alias, derived_cols);
        return execute_select_with_joins_first_materialized(
            stmt,
            first_source,
            derived_rows,
            &temp_exec_ctx,
            _conn_txn,
            &mut temp_ctx,
        );
    }

    let mut combined_rows: Vec<Row> = Vec::new();
    let mut sq_cache: SubqueryCache = HashMap::new();
    let mut in_set_cache: InSetCache = HashMap::new();
    let mut corr_cache: CorrelatedCache = HashMap::new();
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
                cache: Some(&mut sq_cache),
                in_set_cache: Some(&mut in_set_cache),
                correlated_cache: Some(&mut corr_cache),
                materialized: None,
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

    let resolved_ob = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
    if !stmt.distinct_on.is_empty() {
        // Phase 21.12 — DISTINCT ON: sort by (distinct_on ASC, then ORDER BY),
        // keep first pre-projection row per DISTINCT ON key group.
        combined_rows = apply_distinct_on(combined_rows, &stmt.distinct_on, &resolved_ob, &stmt.columns)?;
    } else if !resolved_ob.is_empty() && stmt.limit.is_some() && !stmt.distinct && !stmt.calc_found_rows {
        let (limit_n, offset_n) = eval_limit_offset_usize(&stmt.limit, &stmt.offset)?;
        let top_n = offset_n + limit_n.unwrap_or(usize::MAX).min(usize::MAX - offset_n);
        combined_rows = apply_order_by_top_n(combined_rows, &resolved_ob, top_n)?;
    } else {
        combined_rows = apply_order_by(combined_rows, &resolved_ob)?;
    }

    let out_cols = build_derived_output_columns(&stmt.columns, &derived_cols)?;
    let mut rows = project_rows_with_window_support(&stmt.columns, &combined_rows, |expr, row| {
        eval(expr, row)
    })?;

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

/// Phase 21.22 — SELECT whose FROM is an inline `VALUES (...)` clause.
/// Mirrors `execute_select_jsonb_srf_source`: materialize rows, then
/// apply WHERE / GROUP BY / ORDER BY / LIMIT / joins.
fn execute_select_values_source(
    mut stmt: SelectStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    _conn_txn: Option<&ConnectionTxn>,
) -> Result<QueryResult, DbError> {
    let vc = match stmt.from.take() {
        Some(FromClause::Values(v)) => *v,
        _ => unreachable!("execute_select_values_source called with non-Values FROM"),
    };

    let alias = vc.alias.clone();
    let derived_rows = crate::values_clause::materialize_values(&vc)?;
    let derived_cols = crate::values_clause::column_metas_for_values(&vc);

    if !stmt.joins.is_empty() {
        let mut temp_ctx = SessionContext::new();
        let temp_bloom = crate::bloom::BloomRegistry::new();
        let temp_exec_ctx = ExecutionContext::new(storage, txn, &temp_bloom, None);
        let first_source = join_source_schema_from_derived(&alias, derived_cols);
        return execute_select_with_joins_first_materialized(
            stmt,
            first_source,
            derived_rows,
            &temp_exec_ctx,
            _conn_txn,
            &mut temp_ctx,
        );
    }

    let mut combined_rows: Vec<Row> = Vec::new();
    let mut sq_cache: SubqueryCache = HashMap::new();
    let mut in_set_cache: InSetCache = HashMap::new();
    let mut corr_cache: CorrelatedCache = HashMap::new();
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
                cache: Some(&mut sq_cache),
                in_set_cache: Some(&mut in_set_cache),
                correlated_cache: Some(&mut corr_cache),
                materialized: None,
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

    let resolved_ob = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
    if !stmt.distinct_on.is_empty() {
        // Phase 21.12 — DISTINCT ON: sort by (distinct_on ASC, then ORDER BY),
        // keep first pre-projection row per DISTINCT ON key group.
        combined_rows = apply_distinct_on(combined_rows, &stmt.distinct_on, &resolved_ob, &stmt.columns)?;
    } else if !resolved_ob.is_empty() && stmt.limit.is_some() && !stmt.distinct && !stmt.calc_found_rows {
        let (limit_n, offset_n) = eval_limit_offset_usize(&stmt.limit, &stmt.offset)?;
        let top_n = offset_n + limit_n.unwrap_or(usize::MAX).min(usize::MAX - offset_n);
        combined_rows = apply_order_by_top_n(combined_rows, &resolved_ob, top_n)?;
    } else {
        combined_rows = apply_order_by(combined_rows, &resolved_ob)?;
    }

    let out_cols = build_derived_output_columns(&stmt.columns, &derived_cols)?;
    let mut rows = project_rows_with_window_support(&stmt.columns, &combined_rows, |expr, row| {
        eval(expr, row)
    })?;

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
