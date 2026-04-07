fn execute_update_ctx(
    stmt: UpdateStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let resolved = resolve_table_cached(
        storage,
        txn,
        ctx,
        &stmt.table,
    )?;
    let schema_cols = resolved.columns.clone();
    let secondary_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    let assignments: Vec<(usize, Expr)> = stmt
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

    let snap = txn.active_snapshot(ctx.conn_txn.as_ref().expect("active txn"));

    // ── Clustered table UPDATE dispatch (Phase 39.16) ────────────────────
    if resolved.def.is_clustered() {
        let mut conn = ctx.conn_txn.take().expect("active txn for clustered update");
        let result = execute_clustered_update(
            stmt.where_clause,
            assignments,
            &schema_cols,
            &secondary_indexes,
            storage,
            txn,
            &mut conn,
            snap,
            &resolved,
            bloom,
            ctx,
        );
        ctx.conn_txn = Some(conn);
        return result;
    }

    // Pre-compute field-patch eligibility early — needed for both the fused
    // index-range path and the standard candidate loop optimization.
    let col_types: Vec<axiomdb_types::DataType> = schema_cols
        .iter()
        .map(|c| crate::table::column_type_to_data_type(c.col_type))
        .collect();
    let field_patch_eligible = ctx.strict_mode
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
            if index_def.is_primary && field_patch_eligible && !has_affected_secondary {
                let mut conn = ctx.conn_txn.take().expect("active txn for fused_index_range_patch");
                let result = fused_index_range_patch(
                    index_def,
                    lo.as_deref(),
                    hi.as_deref(),
                    &assignments,
                    &col_types,
                    storage,
                    txn,
                    &mut conn,
                    snap,
                    &resolved,
                    ctx,
                );
                ctx.conn_txn = Some(conn);
                return result;
            }
        }

        // Fall through to standard candidate collection.
        let candidate_rows: Vec<(RecordId, Vec<Value>)> = collect_delete_candidates(
            wc,
            &secondary_indexes,
            &schema_cols,
            &update_access,
            storage,
            snap.clone(),
            &resolved.def,
            bloom,
        )?;

        let mut conn = ctx.conn_txn.take().expect("active txn for execute_update_with_candidates");
        let result = execute_update_with_candidates(
            candidate_rows,
            assignments,
            &schema_cols,
            &secondary_indexes,
            &col_types,
            field_patch_eligible,
            storage,
            txn,
            &mut conn,
            snap,
            &resolved,
            ctx,
            bloom,
        );
        ctx.conn_txn = Some(conn);
        return result;
    }

    // No WHERE clause — full table scan.
    let candidate_rows: Vec<(RecordId, Vec<Value>)> =
        TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap.clone(), None)?;

    let mut conn = ctx.conn_txn.take().expect("active txn for execute_update_with_candidates fullscan");
    let result = execute_update_with_candidates(
        candidate_rows,
        assignments,
        &schema_cols,
        &secondary_indexes,
        &col_types,
        field_patch_eligible,
        storage,
        txn,
        &mut conn,
        snap,
        &resolved,
        ctx,
        bloom,
    );
    ctx.conn_txn = Some(conn);
    result
}

