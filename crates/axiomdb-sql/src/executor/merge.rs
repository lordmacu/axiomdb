// ── SQL-standard MERGE heap executor ─────────────────────────────────────────

fn execute_merge_ctx(
    stmt: MergeStmt,
    exec_ctx: &ExecutionContext,
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let storage = exec_ctx.storage();
    let txn = exec_ctx.coord();
    let bloom = exec_ctx.bloom();

    let resolved = resolve_table_cached(storage, txn, ctx, Some(conn_txn), &stmt.target)?;
    if resolved.def.is_clustered() {
        return Err(DbError::NotImplemented {
            feature: "MERGE on clustered tables (Phase follow-up)".into(),
        });
    }
    if resolved.def.immutable
        && stmt.actions.iter().any(|action| {
            matches!(
                action.kind,
                MergeActionKind::Update(_) | MergeActionKind::Delete
            )
        })
    {
        return Err(DbError::ImmutableTable {
            table: resolved.def.table_name.clone(),
            operation: "MERGE".into(),
        });
    }

    if let Some(lm) = exec_ctx.lock_manager() {
        lm.acquire_table_lock_sync(
            conn_txn.txn_id,
            resolved.def.id,
            axiomdb_lock::LockMode::IntentionExclusive,
        )?;
    }

    let mut secondary_indexes: Vec<IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();
    let compiled_preds =
        crate::partial_index::compile_index_predicates(&secondary_indexes, &resolved.columns)?;
    let compiled_index_exprs =
        crate::partial_index::compile_index_exprs(&secondary_indexes, &resolved.columns)?;

    let source_rows = materialize_merge_source(stmt.source.clone(), exec_ctx, conn_txn, ctx)?;
    if let Some(access) = merge_unique_lookup_access(&stmt.on, target_width_for(&resolved), &resolved)
    {
        return execute_merge_with_unique_lookup(
            stmt,
            resolved,
            secondary_indexes,
            compiled_preds,
            compiled_index_exprs,
            access,
            source_rows,
            storage,
            txn,
            conn_txn,
            bloom,
            ctx,
        );
    }

    let snap = txn.active_snapshot(conn_txn);
    let target_rows = TableEngine::scan_table(storage, &resolved.def, &resolved.columns, snap, None)?;
    let target_width = target_width_for(&resolved);

    let mut affected = 0u64;
    let mut first_generated: Option<u64> = None;
    let mut touched_targets = std::collections::HashSet::<RecordId>::new();

    for source_row in source_rows {
        let mut matched_any = false;
        for (target_rid, target_row) in &target_rows {
            let combined = merge_combined_row(target_row, &source_row);
            if !is_truthy(&eval(&stmt.on, &combined)?) {
                continue;
            }
            matched_any = true;
            if let Some(delta) = execute_merge_matched_action(
                &stmt,
                &resolved,
                &mut secondary_indexes,
                &compiled_preds,
                target_width,
                *target_rid,
                target_row,
                &source_row,
                storage,
                txn,
                conn_txn,
                bloom,
                ctx,
                &mut touched_targets,
            )? {
                affected += delta;
            }
        }

        if !matched_any {
            let null_target = vec![Value::Null; target_width];
            let combined = merge_combined_row(&null_target, &source_row);
            if let Some(delta) = execute_merge_not_matched_action(
                &stmt,
                &resolved,
                &mut secondary_indexes,
                &compiled_preds,
                &compiled_index_exprs,
                &combined,
                storage,
                txn,
                conn_txn,
                bloom,
                ctx,
                &mut first_generated,
            )? {
                affected += delta;
            }
        }
    }

    if affected > 0 {
        ctx.stats.on_rows_changed(resolved.def.id, affected);
    }
    if let Some(id) = first_generated {
        THREAD_LAST_INSERT_ID.with(|v| v.set(id));
        return Ok(QueryResult::affected_with_id(affected, id));
    }
    Ok(QueryResult::Affected {
        count: affected,
        last_insert_id: None,
    })
}

fn target_width_for(resolved: &ResolvedTable) -> usize {
    resolved.columns.len()
}

struct MergeUniqueLookupAccess {
    target_col_idx: usize,
    source_col_idx: usize,
    index: IndexDef,
}

fn merge_unique_lookup_access(
    on: &Expr,
    target_width: usize,
    resolved: &ResolvedTable,
) -> Option<MergeUniqueLookupAccess> {
    let Expr::BinaryOp {
        op: BinaryOp::Eq,
        left,
        right,
    } = on
    else {
        return None;
    };

    let pair = match (&**left, &**right) {
        (
            Expr::Column {
                col_idx: left_idx, ..
            },
            Expr::Column {
                col_idx: right_idx, ..
            },
        ) if *left_idx < target_width && *right_idx >= target_width => {
            Some((*left_idx, *right_idx - target_width))
        }
        (
            Expr::Column {
                col_idx: left_idx, ..
            },
            Expr::Column {
                col_idx: right_idx, ..
            },
        ) if *right_idx < target_width && *left_idx >= target_width => {
            Some((*right_idx, *left_idx - target_width))
        }
        _ => None,
    }?;

    let (target_col_idx, source_col_idx) = pair;
    let index = resolved.indexes.iter().find(|idx| {
        (idx.is_primary || idx.is_unique)
            && !idx.is_fk_index
            && idx.columns.len() == 1
            && idx
                .predicate
                .as_deref()
                .is_none_or(|predicate| predicate.trim().is_empty())
            && idx.columns[0].expr.is_none()
            && idx.columns[0].col_idx as usize == target_col_idx
    })?;

    Some(MergeUniqueLookupAccess {
        target_col_idx,
        source_col_idx,
        index: index.clone(),
    })
}

#[allow(clippy::too_many_arguments)]
fn execute_merge_with_unique_lookup(
    stmt: MergeStmt,
    resolved: ResolvedTable,
    mut secondary_indexes: Vec<IndexDef>,
    compiled_preds: Vec<Option<Expr>>,
    compiled_index_exprs: Vec<Vec<Option<Expr>>>,
    access: MergeUniqueLookupAccess,
    source_rows: Vec<Vec<Value>>,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let target_width = target_width_for(&resolved);
    let original_matches = source_rows
        .iter()
        .map(|source_row| {
            merge_lookup_target_by_unique(
                &access,
                source_row,
                &resolved,
                storage,
                txn,
                conn_txn,
                bloom,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut affected = 0u64;
    let mut first_generated: Option<u64> = None;
    let mut touched_targets = std::collections::HashSet::<RecordId>::new();

    for (source_row, matched) in source_rows.into_iter().zip(original_matches) {
        if let Some((target_rid, target_row)) = matched {
            if let Some(delta) = execute_merge_matched_action(
                &stmt,
                &resolved,
                &mut secondary_indexes,
                &compiled_preds,
                target_width,
                target_rid,
                &target_row,
                &source_row,
                storage,
                txn,
                conn_txn,
                bloom,
                ctx,
                &mut touched_targets,
            )? {
                affected += delta;
            }
        } else {
            let null_target = vec![Value::Null; target_width];
            let combined = merge_combined_row(&null_target, &source_row);
            if let Some(delta) = execute_merge_not_matched_action(
                &stmt,
                &resolved,
                &mut secondary_indexes,
                &compiled_preds,
                &compiled_index_exprs,
                &combined,
                storage,
                txn,
                conn_txn,
                bloom,
                ctx,
                &mut first_generated,
            )? {
                affected += delta;
            }
        }
    }

    if affected > 0 {
        ctx.stats.on_rows_changed(resolved.def.id, affected);
    }
    if let Some(id) = first_generated {
        THREAD_LAST_INSERT_ID.with(|v| v.set(id));
        return Ok(QueryResult::affected_with_id(affected, id));
    }
    Ok(QueryResult::Affected {
        count: affected,
        last_insert_id: None,
    })
}

fn merge_lookup_target_by_unique(
    access: &MergeUniqueLookupAccess,
    source_row: &[Value],
    resolved: &ResolvedTable,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
) -> Result<Option<(RecordId, Vec<Value>)>, DbError> {
    let key_value = source_row
        .get(access.source_col_idx)
        .cloned()
        .unwrap_or(Value::Null);
    if matches!(key_value, Value::Null) {
        return Ok(None);
    }

    let key = crate::key_encoding::encode_index_key(&[key_value])?;
    if access.index.include_columns.is_empty() && !bloom.might_exist(access.index.index_id, &key) {
        return Ok(None);
    }
    let Some(rid) = crate::index_maintenance::lookup_secondary_rids_by_logical_key(
        storage,
        &access.index,
        &key,
    )?
    .into_iter()
    .next() else {
        return Ok(None);
    };
    let snap = txn.active_snapshot(conn_txn);
    if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap)? {
        return Ok(None);
    }
    let Some(bytes) = HeapChain::read_row(storage, rid.page_id, rid.slot_id)? else {
        return Ok(None);
    };
    let row = crate::table::decode_row_from_bytes(&bytes, &resolved.columns)?;
    if row.get(access.target_col_idx).is_none() {
        return Ok(None);
    }
    Ok(Some((rid, row)))
}

fn materialize_merge_source(
    source: FromClause,
    exec_ctx: &ExecutionContext,
    conn_txn: &ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<Vec<Vec<Value>>, DbError> {
    let select = SelectStmt {
        with_ctes: Vec::new(),
        distinct: false,
        distinct_on: vec![],
        hints: vec![],
        calc_found_rows: false,
        columns: vec![SelectItem::Wildcard],
        from: Some(source),
        joins: vec![],
        where_clause: None,
        group_by: GroupByClause::None,
        having: None,
        order_by: vec![],
        limit: None,
        offset: None,
        lock_mode: None::<LockMode>,
        set_op_rest: vec![],
    };
    match execute_select_ctx(select, exec_ctx, Some(conn_txn), ctx)? {
        QueryResult::Rows { rows, .. } => Ok(rows),
        other => Err(DbError::Other(format!(
            "MERGE USING: expected Rows from source, got {other:?}"
        ))),
    }
}

fn merge_combined_row(target_row: &[Value], source_row: &[Value]) -> Vec<Value> {
    let mut combined = Vec::with_capacity(target_row.len() + source_row.len());
    combined.extend_from_slice(target_row);
    combined.extend_from_slice(source_row);
    combined
}

#[allow(clippy::too_many_arguments)]
fn execute_merge_matched_action(
    stmt: &MergeStmt,
    resolved: &ResolvedTable,
    secondary_indexes: &mut [IndexDef],
    compiled_preds: &[Option<Expr>],
    target_width: usize,
    target_rid: RecordId,
    target_row: &[Value],
    source_row: &[Value],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
    touched_targets: &mut std::collections::HashSet<RecordId>,
) -> Result<Option<u64>, DbError> {
    let combined = merge_combined_row(target_row, source_row);
    for action in &stmt.actions {
        if action.condition != MergeActionCondition::Matched {
            continue;
        }
        if let Some(guard) = &action.guard {
            if !is_truthy(&eval(guard, &combined)?) {
                continue;
            }
        }
        match &action.kind {
            MergeActionKind::Update(assignments) => {
                merge_mark_touched(touched_targets, target_rid)?;
                apply_merge_update_heap(
                    resolved,
                    secondary_indexes,
                    compiled_preds,
                    target_width,
                    target_rid,
                    target_row,
                    &combined,
                    assignments,
                    storage,
                    txn,
                    conn_txn,
                    bloom,
                    ctx,
                )?;
                return Ok(Some(1));
            }
            MergeActionKind::Delete => {
                merge_mark_touched(touched_targets, target_rid)?;
                apply_merge_delete_heap(
                    resolved, target_rid, target_row, storage, txn, conn_txn, bloom, ctx,
                )?;
                return Ok(Some(1));
            }
            MergeActionKind::DoNothing => return Ok(Some(0)),
            MergeActionKind::Insert { .. } => {
                return Err(DbError::Internal {
                    message: "MERGE INSERT action reached MATCHED branch".into(),
                })
            }
        }
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn execute_merge_not_matched_action(
    stmt: &MergeStmt,
    resolved: &ResolvedTable,
    secondary_indexes: &mut [IndexDef],
    compiled_preds: &[Option<Expr>],
    compiled_index_exprs: &[Vec<Option<Expr>>],
    combined: &[Value],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
    first_generated: &mut Option<u64>,
) -> Result<Option<u64>, DbError> {
    for action in &stmt.actions {
        if action.condition != MergeActionCondition::NotMatched {
            continue;
        }
        if let Some(guard) = &action.guard {
            if !is_truthy(&eval(guard, combined)?) {
                continue;
            }
        }
        match &action.kind {
            MergeActionKind::Insert { columns, values } => {
                apply_merge_insert_heap(
                    resolved,
                    secondary_indexes,
                    compiled_preds,
                    compiled_index_exprs,
                    columns,
                    values,
                    combined,
                    storage,
                    txn,
                    conn_txn,
                    bloom,
                    ctx,
                    first_generated,
                )?;
                return Ok(Some(1));
            }
            MergeActionKind::DoNothing => return Ok(Some(0)),
            MergeActionKind::Update(_) | MergeActionKind::Delete => {
                return Err(DbError::Internal {
                    message: "MERGE matched-only action reached NOT MATCHED branch".into(),
                })
            }
        }
    }
    Ok(None)
}

fn merge_mark_touched(
    touched_targets: &mut std::collections::HashSet<RecordId>,
    rid: RecordId,
) -> Result<(), DbError> {
    if !touched_targets.insert(rid) {
        return Err(DbError::CardinalityViolation { count: 2 });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_merge_update_heap(
    resolved: &ResolvedTable,
    secondary_indexes: &mut [IndexDef],
    compiled_preds: &[Option<Expr>],
    target_width: usize,
    old_rid: RecordId,
    old_row: &[Value],
    combined: &[Value],
    assignments: &[Assignment],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<(), DbError> {
    let mut new_row = old_row.to_vec();
    for assignment in assignments {
        let target_idx = resolved
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(&assignment.column))
            .ok_or_else(|| DbError::ColumnNotFound {
                name: assignment.column.clone(),
                table: resolved.def.table_name.clone(),
            })?;
        if target_idx >= target_width {
            return Err(DbError::Internal {
                message: "MERGE target column index outside target row".into(),
            });
        }
        if resolved.columns[target_idx].generated_expr.is_some()
            && !matches!(assignment.value, Expr::Default)
        {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "generated column '{}.{}' cannot be assigned explicitly",
                    resolved.def.table_name, resolved.columns[target_idx].name
                ),
            });
        }
        new_row[target_idx] = eval(&assignment.value, combined)?;
    }
    materialize_generated_columns(&resolved.columns, &mut new_row)?;

    if new_row == old_row {
        return Ok(());
    }

    enforce_text_constraints(&resolved.columns, &mut new_row)?;
    check_row_constraints_with_cols(
        &resolved.constraints,
        &new_row,
        &resolved.def.table_name,
        &resolved.columns,
    )?;
    if !resolved.foreign_keys.is_empty() {
        let (immediate_fks, deferred_fk_ids) =
            crate::fk_enforcement::split_child_update_foreign_keys(
                old_row,
                &new_row,
                &resolved.foreign_keys,
            );
        ctx.mark_deferred_fk_constraints(deferred_fk_ids);
        crate::fk_enforcement::check_fk_child_update(
            old_row,
            &new_row,
            &immediate_fks,
            storage,
            txn,
            conn_txn,
            bloom,
        )?;
    }
    let parent_key_changed = {
        let snap = txn.active_snapshot(&*conn_txn);
        let mut reader = CatalogReader::new(storage, snap)?;
        let parent_fks = reader.list_fk_constraints_referencing(resolved.def.id)?;
        parent_fks.iter().any(|fk| {
            old_row.get(fk.parent_col_idx as usize) != new_row.get(fk.parent_col_idx as usize)
        })
    };
    if parent_key_changed {
        crate::fk_enforcement::enforce_fk_on_parent_update(
            &[(old_rid, old_row.to_vec())],
            &[new_row.clone()],
            resolved.def.id,
            storage,
            txn,
            conn_txn,
            bloom,
            Some(&mut ctx.deferred_fk_constraint_ids),
        )?;
    }

    let coerced = crate::table::coerce_values_with_ctx(new_row, &resolved.columns, ctx, 1)?;
    validate_enum_row_values(&coerced, &resolved.columns, storage, txn, conn_txn)?;
    let new_rid = TableEngine::update_row(
        storage,
        txn,
        conn_txn,
        &resolved.def,
        &resolved.columns,
        old_rid,
        coerced.clone(),
    )?;

    if !secondary_indexes.is_empty() {
        let compiled_index_exprs =
            crate::partial_index::compile_index_exprs(secondary_indexes, &resolved.columns)?;
        let update_pairs = vec![(old_rid, old_row.to_vec(), new_rid, coerced)];
        let snap = txn.active_snapshot(&*conn_txn);
        apply_update_index_maintenance(
            secondary_indexes,
            compiled_preds,
            &compiled_index_exprs,
            &update_pairs,
            storage,
            txn,
            conn_txn,
            bloom,
            snap,
        )?;
    }
    Ok(())
}

fn apply_merge_delete_heap(
    resolved: &ResolvedTable,
    rid: RecordId,
    row: &[Value],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot(&*conn_txn);
    let has_fk_references = {
        let mut reader = CatalogReader::new(storage, snap)?;
        !reader
            .list_fk_constraints_referencing(resolved.def.id)?
            .is_empty()
    };
    if has_fk_references {
        crate::fk_enforcement::enforce_fk_on_parent_delete(
            &[(rid, row.to_vec())],
            resolved.def.id,
            storage,
            txn,
            conn_txn,
            bloom,
            0,
            Some(&mut ctx.deferred_fk_constraint_ids),
        )?;
    }
    TableEngine::delete_rows_batch(storage, txn, conn_txn, &resolved.def, &[rid])?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_merge_insert_heap(
    resolved: &ResolvedTable,
    secondary_indexes: &mut [IndexDef],
    compiled_preds: &[Option<Expr>],
    compiled_index_exprs: &[Vec<Option<Expr>>],
    columns: &Option<Vec<String>>,
    values: &[Expr],
    combined: &[Value],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
    first_generated: &mut Option<u64>,
) -> Result<(), DbError> {
    let mut provided = values
        .iter()
        .map(|expr| eval(expr, combined))
        .collect::<Result<Vec<_>, _>>()?;
    let col_positions =
        build_insert_column_positions(&resolved.columns, columns, &resolved.def.table_name)?;
    validate_generated_insert_exprs(
        &col_positions,
        values,
        &resolved.columns,
        &resolved.def.table_name,
    )?;
    resolve_expr_defaults(&col_positions, values, &mut provided, &resolved.columns);
    let mut full_values = materialize_insert_row(&col_positions, &provided, &resolved.columns);

    assign_auto_increment(
        storage,
        txn,
        conn_txn,
        &resolved.def,
        &resolved.columns,
        &mut full_values,
        first_generated,
    )?;
    materialize_generated_columns(&resolved.columns, &mut full_values)?;

    enforce_text_constraints(&resolved.columns, &mut full_values)?;
    check_row_constraints_with_cols(
        &resolved.constraints,
        &full_values,
        &resolved.def.table_name,
        &resolved.columns,
    )?;
    if !resolved.foreign_keys.is_empty() {
        let (immediate_fks, deferred_fk_ids) =
            crate::fk_enforcement::split_child_insert_foreign_keys(
                &full_values,
                &resolved.foreign_keys,
            );
        ctx.mark_deferred_fk_constraints(deferred_fk_ids);
        crate::fk_enforcement::check_fk_child_insert(
            &full_values,
            &immediate_fks,
            storage,
            txn,
            &*conn_txn,
            bloom,
        )?;
    }

    let full_values = crate::table::coerce_values_with_ctx(full_values, &resolved.columns, ctx, 1)?;
    validate_enum_row_values(&full_values, &resolved.columns, storage, txn, conn_txn)?;
    let rid = TableEngine::insert_row_with_ctx(
        storage,
        txn,
        &resolved.def,
        &resolved.columns,
        ctx,
        conn_txn,
        full_values.clone(),
        1,
    )?;
    if !secondary_indexes.is_empty() {
        let snap = txn.active_snapshot(&*conn_txn);
        let updated = crate::index_maintenance::insert_into_indexes_with_undo(
            secondary_indexes,
            &full_values,
            rid,
            storage,
            bloom,
            compiled_preds,
            compiled_index_exprs,
            snap,
            Some(txn),
            Some(conn_txn),
        )?;
        for (index_id, new_root) in updated {
            CatalogWriter::new(storage, txn, conn_txn)?.update_index_root(index_id, new_root)?;
            if let Some(idx) = secondary_indexes
                .iter_mut()
                .find(|i| i.index_id == index_id)
            {
                idx.root_page_id = new_root;
            }
        }
        ctx.invalidate_all();
    }
    Ok(())
}
