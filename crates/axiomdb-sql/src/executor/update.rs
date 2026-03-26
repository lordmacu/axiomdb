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
    // UPDATE always needs all columns: unchanged columns carry over as-is to
    // the new row. Lazy decode (column_mask) does not help here.
    let rows = TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?;

    // Collect all matching (rid, old_values, new_values) triples before touching
    // the heap. Old values are kept for secondary index maintenance (delete old
    // key before inserting new key into each B-Tree).
    let mut to_update: Vec<(RecordId, Vec<Value>, Vec<Value>)> = Vec::new();
    for (rid, current_values) in rows {
        if let Some(ref wc) = stmt.where_clause {
            if !is_truthy(&eval(wc, &current_values)?) {
                continue;
            }
        }
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

    if secondary_indexes.is_empty() {
        // Fast path: no secondary indexes — use batch heap update (O(P) page I/O).
        let heap_updates: Vec<(RecordId, Vec<Value>)> = to_update
            .into_iter()
            .map(|(rid, _old, new)| (rid, new))
            .collect();
        match heap_updates.len() {
            0 => {}
            1 => {
                let (rid, new_values) = heap_updates.into_iter().next().unwrap();
                TableEngine::update_row_with_ctx(
                    storage,
                    txn,
                    &resolved.def,
                    &schema_cols,
                    ctx,
                    rid,
                    new_values,
                )?;
            }
            _ => {
                // Multi-row batch: delete_batch + insert_batch — O(P) page I/O.
                TableEngine::update_rows_batch_with_ctx(
                    storage,
                    txn,
                    &resolved.def,
                    &schema_cols,
                    ctx,
                    heap_updates,
                )?;
            }
        }
    } else {
        // Secondary indexes present — Phase 5.19 batch approach:
        // 1. Apply all heap updates, collecting (old_rid, old_vals, new_rid, new_vals).
        // 2. Batch-delete all old index keys in one pass per index.
        // 3. Insert new index keys per row (existing per-row path).
        let mut current_indexes = secondary_indexes;

        // Step 1: heap updates → collect old/new row pairs with their rids.
        let mut update_pairs: Vec<(RecordId, Vec<Value>, RecordId, Vec<Value>)> =
            Vec::with_capacity(to_update.len());
        for (old_rid, old_values, new_values) in to_update {
            let new_rid = TableEngine::update_row_with_ctx(
                storage,
                txn,
                &resolved.def,
                &schema_cols,
                ctx,
                old_rid,
                new_values.clone(),
            )?;
            update_pairs.push((old_rid, old_values, new_rid, new_values));
        }

        // Step 2: batch-delete all old index keys.
        // Build delete rows as (old_rid, old_values) for the collector.
        let delete_rows: Vec<(RecordId, Vec<Value>)> = update_pairs
            .iter()
            .map(|(old_rid, old_vals, _, _)| (*old_rid, old_vals.clone()))
            .collect();
        let del_key_buckets = crate::index_maintenance::collect_delete_keys_by_index(
            &current_indexes,
            &delete_rows,
            &compiled_preds,
        )?;
        let del_updated = crate::index_maintenance::delete_many_from_indexes(
            &mut current_indexes,
            del_key_buckets,
            storage,
            bloom,
        )?;
        for (index_id, new_root) in del_updated {
            CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
        }

        // Step 3: insert new keys per row (indexes already have updated roots).
        for (_, _, new_rid, new_values) in &update_pairs {
            let ins_updated = crate::index_maintenance::insert_into_indexes(
                &current_indexes,
                new_values,
                *new_rid,
                storage,
                bloom,
                &compiled_preds,
            )?;
            for (index_id, new_root) in ins_updated {
                CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
                if let Some(idx) = current_indexes.iter_mut().find(|i| i.index_id == index_id) {
                    idx.root_page_id = new_root;
                }
                ctx.invalidate_all();
            }
        }
        // Invalidate once after all updates to drop any stale cached roots.
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

    let snap = txn.active_snapshot()?;
    let rows = TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?;

    // Use the already-loaded indexes from the resolved table (cached by SchemaCache).
    let mut secondary_indexes: Vec<IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    // No-op bloom for the non-ctx path (bloom is managed by execute_with_ctx callers).
    let mut noop_bloom = crate::bloom::BloomRegistry::new();

    let compiled_preds =
        crate::partial_index::compile_index_predicates(&secondary_indexes, &schema_cols)?;

    let mut count = 0u64;
    for (rid, current_values) in rows {
        // WHERE filter.
        if let Some(ref wc) = stmt.where_clause {
            if !is_truthy(&eval(wc, &current_values)?) {
                continue;
            }
        }
        // Apply SET assignments.
        let mut new_values = current_values.clone();
        for (col_pos, val_expr) in &assignments {
            new_values[*col_pos] = eval(val_expr, &current_values)?;
        }
        let new_rid = TableEngine::update_row(
            storage,
            txn,
            &resolved.def,
            &schema_cols,
            rid,
            new_values.clone(),
        )?;
        // Index maintenance: delete old key, insert new key.
        if !secondary_indexes.is_empty() {
            let del_updated = crate::index_maintenance::delete_from_indexes(
                &secondary_indexes,
                &current_values,
                rid,
                storage,
                &mut noop_bloom,
                &compiled_preds,
            )?;
            for (index_id, new_root) in &del_updated {
                CatalogWriter::new(storage, txn)?.update_index_root(*index_id, *new_root)?;
            }
            // Update in-memory root_page_ids before insert so insert uses the
            // correct (post-delete) root page.
            for (index_id, new_root) in del_updated {
                if let Some(idx) = secondary_indexes
                    .iter_mut()
                    .find(|i| i.index_id == index_id)
                {
                    idx.root_page_id = new_root;
                }
            }
            let ins_updated = crate::index_maintenance::insert_into_indexes(
                &secondary_indexes,
                &new_values,
                new_rid,
                storage,
                &mut noop_bloom,
                &compiled_preds,
            )?;
            for (index_id, new_root) in ins_updated {
                CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
            }
        }
        count += 1;
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}
