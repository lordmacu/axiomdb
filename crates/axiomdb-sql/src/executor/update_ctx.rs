fn execute_update_ctx(
    stmt: UpdateStmt,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // Phase 21.4b — UPDATE RETURNING is supported on the heap candidate
    // path. Clustered tables and the fused index-range patch path are
    // deferred to 21.4c (each writes through a path that does not retain
    // the post-update row in a single buffer).
    let want_returning = !stmt.returning.is_empty();
    // SAFETY: see ExecutionContext::storage_mut / coord_mut / bloom_mut.
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let bloom = exec_ctx.bloom();
    let resolved = resolve_table_cached(
        storage,
        txn,
        ctx,
        Some(conn_txn),
        &stmt.table,
    )?;

    // Phase 13.9: immutable tables reject UPDATE at the executor layer.
    if resolved.def.immutable {
        return Err(DbError::ImmutableTable {
            table: resolved.def.table_name.clone(),
            operation: "UPDATE".into(),
        });
    }

    // Phase 40.11: IX(table) — once per UPDATE statement, before candidate scan.
    if let Some(lm) = exec_ctx.lock_manager() {
        lm.acquire_table_lock_sync(
            conn_txn.txn_id,
            resolved.def.id,
            axiomdb_lock::LockMode::IntentionExclusive,
        )?;
    }

    let schema_cols = resolved.columns.clone();
    let secondary_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    let mut assignments: Vec<(usize, Expr)> = stmt
        .assignments
        .into_iter()
        .map(|a| {
            let pos = schema_cols
                .iter()
                .position(|c| c.name == a.column)
                .ok_or_else(|| DbError::ColumnNotFound {
                    name: a.column.clone(),
                    table: resolved.def.table_name.clone(),
                })?;
            Ok((pos, a.value))
        })
        .collect::<Result<_, DbError>>()?;
    validate_generated_update_assignments(
        &assignments,
        &schema_cols,
        &resolved.def.table_name,
    )?;

    // MySQL `ON UPDATE` auto-refresh: every column with a persisted
    // `on_update_expr` that the statement did NOT explicitly assign is
    // appended to `assignments` with its expression reparsed into the AST.
    // Common case: `updated_at TIMESTAMP ON UPDATE CURRENT_TIMESTAMP`.
    for (pos, col) in schema_cols.iter().enumerate() {
        let Some(expr_sql) = col.on_update_expr.as_ref() else {
            continue;
        };
        if assignments.iter().any(|(p, _)| *p == pos) {
            continue;
        }
        match crate::parser::parse_expr_only(expr_sql) {
            Ok(expr) => assignments.push((pos, expr)),
            Err(_) => {
                // Malformed persisted expression — skip the auto-update to
                // avoid failing the whole UPDATE. The column keeps its old
                // value, matching MySQL's tolerant behavior.
            }
        }
    }

    // Extract ORDER BY / LIMIT before stmt.where_clause borrow (G5.3).
    let stmt_order_by = stmt.order_by;
    let stmt_limit = stmt.limit;

    let snap = txn.active_snapshot(conn_txn);

    // Pre-compute field-patch eligibility early — needed for both the fused
    // index-range path and the standard candidate loop optimization.
    let col_types: Vec<axiomdb_types::DataType> = schema_cols
        .iter()
        .map(|c| crate::table::column_type_to_data_type(c.col_type))
        .collect();

    if !stmt.joins.is_empty() {
        let join_stmt = UpdateStmt {
            table: stmt.table,
            joins: stmt.joins,
            assignments: vec![],
            where_clause: stmt.where_clause,
            order_by: stmt_order_by,
            limit: stmt_limit,
            returning: Vec::new(),
        };
        return execute_update_join_ctx(
            join_stmt,
            assignments,
            &schema_cols,
            &secondary_indexes,
            &col_types,
            exec_ctx,
            conn_txn,
            snap,
            &resolved,
            ctx,
        );
    }

    // ── Clustered table UPDATE dispatch (Phase 39.16) ────────────────────
    if resolved.def.is_clustered() {
        if want_returning {
            return Err(DbError::NotImplemented {
                feature: "UPDATE ... RETURNING on clustered tables — deferred to 21.4c"
                    .into(),
            });
        }
        return execute_clustered_update(
            stmt.where_clause,
            &stmt_order_by,
            stmt_limit.as_ref(),
            assignments,
            &schema_cols,
            &secondary_indexes,
            storage,
            txn,
            conn_txn,
            snap,
            &resolved,
            bloom,
            ctx,
        );
    }

    let field_patch_eligible = ctx.strict_mode
        && !schema_cols.iter().any(|c| c.generated_expr.is_some())
        && resolved.foreign_keys.is_empty()
        && assignments.iter().all(|(col_pos, _)| {
            axiomdb_types::field_patch::fixed_encoded_size(col_types[*col_pos]).is_some()
        });

    // ── Fused index-range patch (InnoDB-inspired) ────────────────────────
    // When ALL of these hold, skip candidate collection entirely and patch
    // fields directly on heap pages from B-tree RIDs in a single pass:
    //   1. WHERE uses IndexRange on PRIMARY KEY
    //   2. field_patch eligible (all SET cols fixed-size, no FKs)
    //   3. No secondary indexes affected
    if let Some(ref wc) = stmt.where_clause {
        let effective_coll = ctx.effective_collation();
        let update_access = crate::planner::plan_update_candidates_ctx(
            wc,
            &secondary_indexes,
            &schema_cols,
            effective_coll,
        );

        if let crate::planner::AccessMethod::IndexRange { ref index_def, ref lo, ref hi } = update_access {
            let has_affected_secondary = secondary_indexes.iter().any(|i| !i.is_primary);
            if !want_returning && index_def.is_primary && field_patch_eligible && !has_affected_secondary {
                return fused_index_range_patch(
                    index_def,
                    lo.as_deref(),
                    hi.as_deref(),
                    &assignments,
                    &col_types,
                    storage,
                    txn,
                    conn_txn,
                    snap,
                    &resolved,
                    ctx,
                );
            }
        }

        // Fall through to standard candidate collection.
        let candidate_rows_raw: Vec<(RecordId, Vec<Value>)> = collect_delete_candidates(
            wc,
            &secondary_indexes,
            &schema_cols,
            &update_access,
            storage,
            snap.clone(),
            &resolved.def,
            bloom,
        )?;
        let candidate_rows = apply_order_by_limit_to_candidates(
            candidate_rows_raw,
            &stmt_order_by,
            stmt_limit.as_ref(),
        )?;

        return execute_update_with_candidates(
            candidate_rows,
            assignments,
            &schema_cols,
            &secondary_indexes,
            &col_types,
            field_patch_eligible,
            exec_ctx,
            conn_txn,
            snap,
            &resolved,
            ctx,
            &stmt.returning,
        );
    }

    // No WHERE clause — full table scan.
    let candidate_rows_raw: Vec<(RecordId, Vec<Value>)> =
        TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap.clone(), None)?;
    let candidate_rows = apply_order_by_limit_to_candidates(
        candidate_rows_raw,
        &stmt_order_by,
        stmt_limit.as_ref(),
    )?;

    execute_update_with_candidates(
        candidate_rows,
        assignments,
        &schema_cols,
        &secondary_indexes,
        &col_types,
        field_patch_eligible,
        exec_ctx,
        conn_txn,
        snap,
        &resolved,
        ctx,
        &stmt.returning,
    )
}
