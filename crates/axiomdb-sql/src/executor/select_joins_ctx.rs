fn execute_select_with_joins_ctx(
    stmt: SelectStmt,
    from_ref: crate::ast::TableRef,
    exec_ctx: &ExecutionContext,
    conn_txn: Option<&axiomdb_wal::ConnectionTxn>,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let snap = conn_txn
        .map(|c| txn.active_snapshot(c))
        .unwrap_or_else(|| txn.snapshot());
    let from_t = resolve_table_cached(storage, txn, ctx, conn_txn, &from_ref)?;
    let from_rows =
        crate::table::scan_table_any_layout(storage, &from_t.def, &from_t.columns, snap)?;
    let first_source = join_source_schema_from_resolved(&from_ref, &from_t);
    let first_rows: Vec<Row> = from_rows.into_iter().map(|(_, r)| r).collect();
    execute_select_with_joins_first_materialized(
        stmt,
        first_source,
        first_rows,
        exec_ctx,
        conn_txn,
        ctx,
    )
}

/// Phase 11.20d2 — shared join-loop entry point that takes an already-
/// materialized first source. Used both by the regular FROM-Table path
/// (`execute_select_with_joins_ctx`) and by the JSON_TABLE-first path
/// (`execute_select_json_table_source` when `!stmt.joins.is_empty()`).
fn execute_select_with_joins_first_materialized(
    stmt: SelectStmt,
    first_source: JoinSourceSchema,
    first_rows: Vec<Row>,
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
    // Phase 11.20d3: parallel tracker. None → right side is already
    // materialized in `scanned[idx]`. Some(spec) → LATERAL-correlated
    // JSON_TABLE that must be re-evaluated per outer row during the
    // combine loop.
    let mut correlated_jt: Vec<Option<crate::json_table::JsonTableSpec>> = vec![None];
    // Phase 11.25a: parallel tracker for correlated JSONB SRFs.
    let mut correlated_srf: Vec<Option<crate::ast::JsonbSrfKind>> = vec![None];
    // Phase 21.9: parallel tracker for correlated LATERAL subqueries.
    // Some(subquery) → LATERAL subquery correlated to left side, must be
    // re-evaluated per outer row. None → non-LATERAL or non-correlated.
    let mut correlated_sub: Vec<Option<SelectStmt>> = vec![None];
    // Phase 21.9: Track accumulated columns from correlated LATERAL subqueries.
    // Even when deferred (empty columns in all_sources), subsequent LATERALs
    // need to reference these output columns for correlation detection.
    let mut lateral_accum_cols = 0usize;
    let mut running_offset = 0usize;
    let snap = conn_txn
        .map(|c| txn.active_snapshot(c))
        .unwrap_or_else(|| txn.snapshot());

    {
        col_offsets.push(running_offset);
        running_offset += first_source.columns.len();
        all_sources.push(first_source);
        scanned.push(first_rows);
        // NOTE: do NOT push to correlated_sub here. The correlated_sub/jt/srf
        // vectors are indexed by join index (1..=joins.len()), not by source index.
        // They are initialized with vec![None] covering index 0 (unused sentinel),
        // then each join pushes exactly one entry. Pushing here would cause an
        // off-by-one where correlated_sub[right_idx] always points one slot too early.

        for join in stmt.joins.iter() {
            match &join.table {
                FromClause::Table(tref) => {
                    let jt = resolve_table_cached(storage, txn, ctx, conn_txn, tref)?;
                    let rows = crate::table::scan_table_any_layout(
                        storage,
                        &jt.def,
                        &jt.columns,
                        snap.clone(),
                    )?;
                    col_offsets.push(running_offset);
                    running_offset += jt.columns.len();
                    all_sources.push(join_source_schema_from_resolved(tref, &jt));
                    scanned.push(rows.into_iter().map(|(_, r)| r).collect());
                    correlated_jt.push(None);
                    correlated_srf.push(None);
                    correlated_sub.push(None);
                }
                FromClause::Subquery {
                    query,
                    alias,
                    lateral,
                } => {
                    // Phase 21.9: RIGHT/FULL JOIN on LATERAL subquery is not supported
                    // (LATERAL semantics imply left-to-right evaluation; RIGHT/FULL
                    // would require outer-scan which is ill-defined for LATERAL).
                    if *lateral && matches!(join.join_type, JoinType::Right | JoinType::Full) {
                        return Err(DbError::NotImplemented {
                            feature:
                                "RIGHT/FULL JOIN on LATERAL subquery — PG-compatible rejection"
                                    .into(),
                        });
                    }
                    // Phase 21.9: check if this LATERAL subquery is correlated
                    // (references outer columns from the left side of the join).
                    // For correlated subqueries, we defer materialization to the
                    // per-outer-row combine loop. Non-LATERAL or non-correlated
                    // subqueries are materialized once upfront.
                    let base_left_cols = all_sources.iter().map(|s| s.columns.len()).sum::<usize>();
                    let effective_left_cols = base_left_cols + lateral_accum_cols;
                    let is_correlated = *lateral
                        && crate::json_table::subquery_is_correlated(query, effective_left_cols);

                    let (columns, rows) = if is_correlated {
                        // Deferred: store the subquery for the combine loop.
                        // Build placeholder ColumnMeta from the SELECT list so that:
                        //   1. running_offset advances by the correct column count
                        //   2. right_col_count is correct for LEFT JOIN null padding
                        //   3. project_join_row can look up columns by name
                        // Actual typed values come from execute_select_ctx per outer row.
                        lateral_accum_cols += query.columns.len();
                        let placeholder_cols: Vec<crate::result::ColumnMeta> = query
                            .columns
                            .iter()
                            .enumerate()
                            .map(|(i, item)| {
                                let name = match item {
                                    crate::ast::SelectItem::Expr { alias: Some(a), .. } => {
                                        a.clone()
                                    }
                                    crate::ast::SelectItem::Expr { expr, alias: None } => {
                                        format!("col{i}_{expr:?}")
                                            .chars()
                                            .take(64)
                                            .collect()
                                    }
                                    _ => format!("col{i}"),
                                };
                                crate::result::ColumnMeta::computed(
                                    name,
                                    axiomdb_types::DataType::Text,
                                )
                            })
                            .collect();
                        (placeholder_cols, Vec::new())
                    } else {
                        let inner_result =
                            execute_select_ctx((**query).clone(), exec_ctx, conn_txn, ctx)?;
                        match inner_result {
                            QueryResult::Rows { columns, rows } => (columns, rows),
                            _ => {
                                return Err(DbError::Internal {
                                    message: "join-side subquery did not return rows".into(),
                                });
                            }
                        }
                    };
                    col_offsets.push(running_offset);
                    running_offset += columns.len();
                    all_sources.push(join_source_schema_from_derived(alias, columns));
                    scanned.push(rows);
                    correlated_sub.push(if is_correlated {
                        Some((**query).clone())
                    } else {
                        None
                    });
                    correlated_jt.push(None);
                    correlated_srf.push(None);
                }
                // Phase 11.20a/d3 — JSON_TABLE on the right side of a JOIN.
                // Non-correlated doc: evaluate once with an empty row, materialize,
                // feed to the batch nested-loop path. LATERAL-correlated doc (or
                // PASSING): defer materialization to the per-outer-row combine loop.
                FromClause::JsonTable(jt) => {
                    let spec = crate::json_table::compile_json_table(jt)?;
                    let column_metas = crate::json_table::column_metas_for_spec(&spec);
                    col_offsets.push(running_offset);
                    running_offset += column_metas.len();
                    all_sources.push(join_source_schema_from_derived(&spec.alias, column_metas));
                    if crate::json_table::jsontable_is_correlated(jt) {
                        scanned.push(Vec::new());
                        correlated_jt.push(Some(spec));
                        correlated_srf.push(None);
                        correlated_sub.push(None);
                    } else {
                        let doc_val = crate::eval::eval(&jt.doc, &[])?;
                        let rows = match crate::json_table::doc_to_serde(&doc_val)? {
                            None => Vec::new(),
                            Some(sj) => {
                                let mut runner = crate::eval::NoSubquery;
                                crate::json_table::materialize_json_table(
                                    &spec,
                                    &sj,
                                    &[],
                                    &mut runner,
                                )?
                            }
                        };
                        scanned.push(rows);
                        correlated_jt.push(None);
                        correlated_srf.push(None);
                        correlated_sub.push(None);
                    }
                }
                // Phase 11.25a — JSONB SRF (jsonb_each, etc.) on the right side.
                FromClause::JsonbSrf(srf) => {
                    let alias = crate::jsonb_srf::srf_alias(srf);
                    let column_metas = crate::jsonb_srf::column_metas_for_srf(srf.kind, &alias);
                    col_offsets.push(running_offset);
                    running_offset += column_metas.len();
                    all_sources.push(join_source_schema_from_derived(&alias, column_metas));
                    if crate::jsonb_srf::srf_is_correlated(srf) {
                        scanned.push(Vec::new());
                        correlated_jt.push(None);
                        correlated_srf.push(Some(srf.kind));
                        correlated_sub.push(None);
                    } else {
                        let doc_val = crate::eval::eval(&srf.doc, &[])?;
                        let rows = crate::jsonb_srf::materialize_jsonb_srf(srf.kind, &doc_val)?;
                        scanned.push(rows);
                        correlated_jt.push(None);
                        correlated_srf.push(None);
                        correlated_sub.push(None);
                    }
                }
                // Phase 21.22 — inline VALUES on the right side.
                FromClause::Values(vc) => {
                    let column_metas = crate::values_clause::column_metas_for_values(vc);
                    let rows = crate::values_clause::materialize_values(vc)?;
                    col_offsets.push(running_offset);
                    running_offset += column_metas.len();
                    all_sources.push(join_source_schema_from_derived(&vc.alias, column_metas));
                    scanned.push(rows);
                    correlated_jt.push(None);
                    correlated_srf.push(None);
                    correlated_sub.push(None);
                }
                // Phase 21.3 — recursive CTE as join right side is deferred;
                // MVP only supports recursive CTE as first-FROM source.
                FromClause::RecursiveCte(_) => {
                    return Err(DbError::NotImplemented {
                        feature: "recursive CTE on the right side of a JOIN — use as \
                                  first-FROM for now (21.3 MVP)"
                            .into(),
                    });
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

        combined_rows = if let Some(spec) = correlated_jt[right_idx].as_ref() {
            // Phase 11.20d3 — correlated JSON_TABLE right side.
            let jt_ast = match &stmt.joins[i].table {
                FromClause::JsonTable(j) => j.as_ref(),
                _ => unreachable!("correlated_jt set but AST is not JsonTable"),
            };
            apply_correlated_jt_join(
                combined_rows,
                jt_ast,
                spec,
                &all_sources[right_idx].columns,
                right_col_count,
                join.join_type,
                &join.condition,
                &left_schema,
                right_col_offset,
            )?
        } else if let Some(kind) = correlated_srf[right_idx] {
            // Phase 11.25a — correlated JSONB SRF right side.
            let srf_ast = match &stmt.joins[i].table {
                FromClause::JsonbSrf(s) => s.as_ref(),
                _ => unreachable!("correlated_srf set but AST is not JsonbSrf"),
            };
            apply_correlated_srf_join(
                combined_rows,
                kind,
                &srf_ast.doc,
                &all_sources[right_idx].columns,
                right_col_count,
                join.join_type,
                &join.condition,
                &left_schema,
                right_col_offset,
            )?
        } else if let Some(subquery) = correlated_sub[right_idx].as_ref() {
            // Phase 21.9 — correlated LATERAL subquery right side.
            apply_correlated_subquery_join(
                combined_rows,
                subquery,
                &all_sources[right_idx].columns,
                right_col_count,
                join.join_type,
                &join.condition,
                &left_schema,
                right_col_offset,
                exec_ctx,
                conn_txn,
                ctx,
            )?
        } else {
            apply_join(
                combined_rows,
                &scanned[right_idx],
                left_col_count,
                right_col_count,
                join.join_type,
                &join.condition,
                &left_schema,
                right_col_offset,
                &all_sources[right_idx].columns,
            )?
        };

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
    if !resolved_ob.is_empty() && stmt.limit.is_some() && !stmt.distinct && !stmt.calc_found_rows {
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
