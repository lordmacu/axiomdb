fn execute_select_with_joins_ctx(
    stmt: SelectStmt,
    from_ref: crate::ast::TableRef,
    exec_ctx: &ExecutionContext,
    conn_txn: Option<&axiomdb_wal::ConnectionTxn>,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    // Session collation for eval()-based comparisons in join ON, WHERE, ORDER BY, etc.
    // Guard propagates from execute_select_ctx when called via the join path, but
    // we set it here too so this function can also be called independently.
    let _coll_guard = CollationGuard::new(ctx.effective_collation());

    let mut all_sources: Vec<JoinSourceSchema> = Vec::new();
    let mut scanned: Vec<Vec<Row>> = Vec::new();
    let mut col_offsets: Vec<usize> = Vec::new();
    let mut running_offset = 0usize;
    let snap = conn_txn
        .map(|c| txn.active_snapshot(c))
        .unwrap_or_else(|| txn.snapshot());

    {
        let from_t = resolve_table_cached(storage, txn, ctx, conn_txn, &from_ref)?;
        let from_rows =
            crate::table::scan_table_any_layout(storage, &from_t.def, &from_t.columns, snap.clone())?;
        col_offsets.push(running_offset);
        running_offset += from_t.columns.len();
        all_sources.push(join_source_schema_from_resolved(&from_ref, &from_t));
        scanned.push(from_rows.into_iter().map(|(_, r)| r).collect());

        for join in &stmt.joins {
            match &join.table {
                FromClause::Table(tref) => {
                    let jt = resolve_table_cached(storage, txn, ctx, conn_txn, tref)?;
                    let rows =
                        crate::table::scan_table_any_layout(storage, &jt.def, &jt.columns, snap.clone())?;
                    col_offsets.push(running_offset);
                    running_offset += jt.columns.len();
                    all_sources.push(join_source_schema_from_resolved(tref, &jt));
                    scanned.push(rows.into_iter().map(|(_, r)| r).collect());
                }
                FromClause::Subquery { query, alias } => {
                    let inner_result =
                        execute_select_ctx((**query).clone(), exec_ctx, conn_txn, ctx)?;
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
                // Phase 11.20a — JSON_TABLE on the right side of a JOIN.
                // Non-correlated doc only: evaluate once with an empty row,
                // materialize, then combine via the normal nested-loop path.
                // Correlated doc (LATERAL semantics) → 11.20d.
                FromClause::JsonTable(jt) => {
                    if crate::json_table::doc_has_column_refs(&jt.doc) {
                        return Err(DbError::NotImplemented {
                            feature: "correlated JSON_TABLE in a JOIN (LATERAL semantics) — \
                                      deferred to 11.20d"
                                .into(),
                        });
                    }
                    let spec = crate::json_table::compile_json_table(jt)?;
                    let column_metas = crate::json_table::column_metas_for_spec(&spec);
                    let doc_val = crate::eval::eval(&jt.doc, &[])?;
                    let rows = match crate::json_table::doc_to_serde(&doc_val)? {
                        None => Vec::new(),
                        Some(sj) => {
                            let mut runner = crate::eval::NoSubquery;
                            crate::json_table::materialize_json_table(&spec, &sj, &[], &mut runner)?
                        }
                    };
                    col_offsets.push(running_offset);
                    running_offset += column_metas.len();
                    all_sources.push(join_source_schema_from_derived(&spec.alias, column_metas));
                    scanned.push(rows);
                }
            }
        }
    }

    let mut combined_rows: Vec<Row> = scanned[0].clone();
    let mut left_col_count = all_sources[0].columns.len();

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

        for (j, col) in all_sources[right_idx].columns.iter().enumerate() {
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

    let resolved_ob = resolve_positional_order_by(&stmt.order_by, &stmt.columns);
    if !resolved_ob.is_empty() && stmt.limit.is_some() && !stmt.distinct && !stmt.calc_found_rows
    {
        let (limit_n, offset_n) = eval_limit_offset_usize(&stmt.limit, &stmt.offset)?;
        let top_n = offset_n + limit_n.unwrap_or(usize::MAX).min(usize::MAX - offset_n);
        combined_rows = apply_order_by_top_n(combined_rows, &resolved_ob, top_n)?;
    } else {
        combined_rows = apply_order_by(combined_rows, &resolved_ob)?;
    }

    let out_cols = build_join_column_meta(&stmt.columns, &all_sources, &stmt.joins)?;

    let mut rows = combined_rows
        .iter()
        .map(|r| project_join_row(&stmt.columns, r, &all_sources))
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
