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
        stmt.table.schema.as_deref(),
        &stmt.table.name,
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

    let snap = txn.active_snapshot()?;
    let candidate_rows: Vec<(RecordId, Vec<Value>)> = if let Some(ref wc) = stmt.where_clause {
        let effective_coll = ctx.effective_collation();
        let update_access = crate::planner::plan_update_candidates_ctx(
            wc,
            &secondary_indexes,
            &schema_cols,
            effective_coll,
        );
        collect_delete_candidates(
            wc,
            &secondary_indexes,
            &schema_cols,
            &update_access,
            storage,
            snap,
            &resolved.def,
            bloom,
        )?
    } else {
        // UPDATE always needs all columns: unchanged columns carry over as-is to
        // the new row. Lazy decode (column_mask) does not help here.
        TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?
    };

    // Collect all matching (rid, old_values, new_values) triples before touching
    // the heap. Old values are kept for secondary index maintenance (delete old
    // key before inserting new key into each B-Tree).
    let mut to_update: Vec<(RecordId, Vec<Value>, Vec<Value>)> = Vec::new();
    for (rid, current_values) in candidate_rows {
        let mut new_values = current_values.clone();
        for (col_pos, val_expr) in &assignments {
            new_values[*col_pos] = eval(val_expr, &current_values)?;
        }
        to_update.push((rid, current_values, new_values));
    }

    let count = to_update.len() as u64;

    // FK child validation: check new FK values before applying any updates.
    if !resolved.foreign_keys.is_empty() {
        for (_, old_values, new_values) in &to_update {
            crate::fk_enforcement::check_fk_child_update(
                old_values,
                new_values,
                &resolved.foreign_keys,
                storage,
                txn,
                bloom,
            )?;
        }
    }

    // FK parent enforcement: check if this table is referenced by any FK and
    // the referenced column value is changing (RESTRICT/NO ACTION).
    if !to_update.is_empty() {
        let old_rows: Vec<(RecordId, Vec<Value>)> = to_update
            .iter()
            .map(|(rid, old, _)| (*rid, old.clone()))
            .collect();
        let new_rows: Vec<Vec<Value>> = to_update.iter().map(|(_, _, new)| new.clone()).collect();
        crate::fk_enforcement::enforce_fk_on_parent_update(
            &old_rows,
            &new_rows,
            resolved.def.id,
            storage,
            txn,
        )?;
    }

    let compiled_preds =
        crate::partial_index::compile_index_predicates(&secondary_indexes, &schema_cols)?;

    let heap_updates: Vec<(RecordId, Vec<Value>)> = to_update
        .iter()
        .map(|(rid, _old, new)| (*rid, new.clone()))
        .collect();
    let new_rids = TableEngine::update_rows_preserve_rid_with_ctx(
        storage,
        txn,
        &resolved.def,
        &schema_cols,
        ctx,
        heap_updates,
    )?;

    if !secondary_indexes.is_empty() {
        let mut current_indexes = secondary_indexes;
        let update_pairs: Vec<(RecordId, Vec<Value>, RecordId, Vec<Value>)> = to_update
            .into_iter()
            .zip(new_rids)
            .map(|((old_rid, old_values, new_values), new_rid)| {
                (old_rid, old_values, new_rid, new_values)
            })
            .collect();
        apply_update_index_maintenance(
            &mut current_indexes,
            &compiled_preds,
            &update_pairs,
            storage,
            txn,
            bloom,
        )?;
        ctx.invalidate_all();
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}


fn execute_update(
    stmt: UpdateStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let mut resolver = make_resolver(storage, txn)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };

    let schema_cols = resolved.columns.clone();

    // Resolve assignment column positions once, before the scan.
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

    // Use the already-loaded indexes from the resolved table (cached by SchemaCache).
    let mut secondary_indexes: Vec<IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    // No-op bloom for the non-ctx path (bloom is managed by execute_with_ctx callers).
    let mut noop_bloom = crate::bloom::BloomRegistry::new();

    let snap = txn.active_snapshot()?;
    let candidate_rows: Vec<(RecordId, Vec<Value>)> = if let Some(ref wc) = stmt.where_clause {
        let update_access = crate::planner::plan_update_candidates(
            wc,
            &secondary_indexes,
            &schema_cols,
        );
        collect_delete_candidates(
            wc,
            &secondary_indexes,
            &schema_cols,
            &update_access,
            storage,
            snap,
            &resolved.def,
            &noop_bloom,
        )?
    } else {
        TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?
    };

    let compiled_preds =
        crate::partial_index::compile_index_predicates(&secondary_indexes, &schema_cols)?;

    let mut to_update: Vec<(RecordId, Vec<Value>, Vec<Value>)> = Vec::new();
    for (rid, current_values) in candidate_rows {
        let mut new_values = current_values.clone();
        for (col_pos, val_expr) in &assignments {
            new_values[*col_pos] = eval(val_expr, &current_values)?;
        }
        to_update.push((rid, current_values, new_values));
    }

    let count = to_update.len() as u64;
    let heap_updates: Vec<(RecordId, Vec<Value>)> = to_update
        .iter()
        .map(|(rid, _old, new)| (*rid, new.clone()))
        .collect();
    let new_rids = TableEngine::update_rows_preserve_rid(
        storage,
        txn,
        &resolved.def,
        &schema_cols,
        heap_updates,
    )?;

    if !secondary_indexes.is_empty() {
        let update_pairs: Vec<(RecordId, Vec<Value>, RecordId, Vec<Value>)> = to_update
            .into_iter()
            .zip(new_rids)
            .map(|((old_rid, old_values, new_values), new_rid)| {
                (old_rid, old_values, new_rid, new_values)
            })
            .collect();
        apply_update_index_maintenance(
            &mut secondary_indexes,
            &compiled_preds,
            &update_pairs,
            storage,
            txn,
            &mut noop_bloom,
        )?;
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

fn apply_update_index_maintenance(
    current_indexes: &mut [IndexDef],
    compiled_preds: &[Option<Expr>],
    update_pairs: &[(RecordId, Vec<Value>, RecordId, Vec<Value>)],
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
) -> Result<(), DbError> {
    for (idx_pos, idx) in current_indexes.iter_mut().enumerate() {
        if idx.columns.is_empty() {
            continue;
        }
        let pred = compiled_preds.get(idx_pos).and_then(|p| p.as_ref());

        let mut delete_keys: Vec<Vec<u8>> = Vec::new();
        let mut insert_rows: Vec<(RecordId, &Vec<Value>)> = Vec::new();
        for (old_rid, old_values, new_rid, new_values) in update_pairs {
            if crate::index_maintenance::update_affects_index(
                idx,
                pred,
                old_values,
                *old_rid,
                new_values,
                *new_rid,
            )? {
                if let Some(key_vals) =
                    crate::index_maintenance::index_key_values_if_indexed(idx, old_values, pred)?
                {
                    delete_keys.push(crate::index_maintenance::encode_index_entry_key(
                        idx, &key_vals, *old_rid,
                    )?);
                }
                insert_rows.push((*new_rid, new_values));
            }
        }

        if !delete_keys.is_empty() {
            delete_keys.sort_unstable();
            if let Some(new_root) = crate::index_maintenance::delete_many_from_single_index(
                idx,
                &delete_keys,
                storage,
                bloom,
            )? {
                CatalogWriter::new(storage, txn)?.update_index_root(idx.index_id, new_root)?;
            }
        }

        for (new_rid, new_values) in insert_rows {
            if let Some(new_root) = crate::index_maintenance::insert_into_single_index(
                idx,
                pred,
                new_values,
                new_rid,
                storage,
                bloom,
            )? {
                CatalogWriter::new(storage, txn)?.update_index_root(idx.index_id, new_root)?;
            }
        }
    }
    Ok(())
}
