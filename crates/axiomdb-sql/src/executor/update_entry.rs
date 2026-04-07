fn execute_update(
    stmt: UpdateStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut ConnectionTxn,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let mut resolver =
            make_resolver_with_database(storage, txn, Some(conn_txn), DEFAULT_DATABASE_NAME)?;
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

    let snap = txn.active_snapshot(conn_txn);
    if resolved.def.is_clustered() {
        let mut temp_ctx = SessionContext::new();
        return execute_clustered_update(
            stmt.where_clause,
            assignments,
            &schema_cols,
            &secondary_indexes,
            storage,
            txn,
            conn_txn,
            snap,
            &resolved,
            &mut noop_bloom,
            &mut temp_ctx,
        );
    }

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
    let mut matched_count: u64 = 0;
    for (rid, current_values) in candidate_rows {
        // Evaluate assigned columns without cloning the full row.
        // Build new_values by iterating columns once, cloning only non-assigned
        // values and evaluating expressions for assigned ones.
        let mut changed = false;
        let mut new_values = Vec::with_capacity(current_values.len());
        for (ci, cv) in current_values.iter().enumerate() {
            if let Some((_, val_expr)) = assignments.iter().find(|(pos, _)| *pos == ci) {
                let nv = eval(val_expr, &current_values)?;
                if nv != *cv {
                    changed = true;
                }
                new_values.push(nv);
            } else {
                new_values.push(cv.clone());
            }
        }
        matched_count += 1;
        if !changed {
            continue; // no-op: skip heap/index work
        }
        to_update.push((rid, current_values, new_values));
    }

    let heap_updates: Vec<(RecordId, Vec<Value>)> = to_update
        .iter()
        .map(|(rid, _old, new)| (*rid, new.clone()))
        .collect();
    let new_rids = TableEngine::update_rows_preserve_rid(
        storage,
        txn,
        conn_txn,
        &resolved.def,
        &schema_cols,
        heap_updates,
    )?;

    if !secondary_indexes.is_empty() && !to_update.is_empty() {
        let all_rids_stable = to_update
            .iter()
            .zip(new_rids.iter())
            .all(|((old_rid, _, _), new_rid)| old_rid == new_rid);
        let any_index_affected =
            statement_might_affect_indexes(&secondary_indexes, &compiled_preds, &assignments);

        if any_index_affected || !all_rids_stable {
            let update_pairs: Vec<(RecordId, Vec<Value>, RecordId, Vec<Value>)> = to_update
                .into_iter()
                .zip(new_rids)
                .map(|((old_rid, old_values, new_values), new_rid)| {
                    (old_rid, old_values, new_rid, new_values)
                })
                .collect();
            let snap = txn.active_snapshot(conn_txn);
            apply_update_index_maintenance(
                &mut secondary_indexes,
                &compiled_preds,
                &update_pairs,
                storage,
                txn,
                conn_txn,
                &mut noop_bloom,
                snap,
            )?;
        }
    }

    Ok(QueryResult::Affected {
        count: matched_count,
        last_insert_id: None,
    })
}

fn statement_might_affect_indexes(
    indexes: &[IndexDef],
    compiled_preds: &[Option<Expr>],
    assignments: &[(usize, Expr)],
) -> bool {
    let assigned_cols: std::collections::HashSet<usize> =
        assignments.iter().map(|(pos, _)| *pos).collect();
    indexes.iter().enumerate().any(|(idx_pos, idx)| {
        let pred = compiled_preds.get(idx_pos).and_then(|p| p.as_ref());
        let key_overlap = idx
            .columns
            .iter()
            .any(|c| assigned_cols.contains(&(c.col_idx as usize)));
        if key_overlap {
            return true;
        }
        if let Some(pred_expr) = pred {
            let pred_cols = crate::partial_index::collect_column_indices(pred_expr);
            if pred_cols.iter().any(|c| assigned_cols.contains(c)) {
                return true;
            }
        }
        false
    })
}

fn apply_update_index_maintenance(
    current_indexes: &mut [IndexDef],
    compiled_preds: &[Option<Expr>],
    update_pairs: &[(RecordId, Vec<Value>, RecordId, Vec<Value>)],
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &mut crate::bloom::BloomRegistry,
    snap: TransactionSnapshot,
) -> Result<(), DbError> {
    for (idx_pos, idx) in current_indexes.iter_mut().enumerate() {
        if idx.columns.is_empty() {
            continue;
        }
        let pred = compiled_preds.get(idx_pos).and_then(|p| p.as_ref());

        // Phase 7.3b — HOT optimization: per-index check.
        // If no key column changed for any row in this batch, skip this index entirely.
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
                // For UPDATE: delete old key for unique/PK indexes to avoid
                // stale entries that would confuse index lookups returning
                // the wrong row values. Non-unique indexes use lazy deletion.
                if idx.is_unique || idx.is_fk_index {
                    if let Some(key_vals) =
                        crate::index_maintenance::index_key_values_if_indexed(
                            idx, old_values, pred,
                        )?
                    {
                        let old_key = crate::index_maintenance::encode_index_entry_key(
                            idx, &key_vals, *old_rid,
                        )?;
                        // Record undo BEFORE deleting so ROLLBACK can re-insert the old key.
                        txn.record_index_delete(
                            conn_txn,
                            idx.index_id,
                            idx.root_page_id,
                            old_key.clone(),
                            *old_rid,
                            idx.fillfactor,
                        );
                        let delete_keys_for_idx = vec![old_key];
                        if let Some(new_root) =
                            crate::index_maintenance::delete_many_from_single_index(
                                idx,
                                &delete_keys_for_idx,
                                storage,
                                bloom,
                            )?
                        {
                            CatalogWriter::new(storage, txn, conn_txn)?
                                .update_index_root(idx.index_id, new_root)?;
                            idx.root_page_id = new_root;
                        }
                    }
                }
                insert_rows.push((*new_rid, new_values));
            }
        }

        if !insert_rows.is_empty() {
            let batch_refs: Vec<(&[Value], RecordId)> = insert_rows
                .iter()
                .map(|(rid, vals)| (vals.as_slice(), *rid))
                .collect();

            // Record index undo BEFORE inserting so ROLLBACK can reverse them.
            // We encode each new key and record it in the undo log.
            for &(vals, rid) in &batch_refs {
                if let Some(key_vals) =
                    crate::index_maintenance::index_key_values_if_indexed(idx, vals, pred)?
                {
                    let key =
                        crate::index_maintenance::encode_index_entry_key(idx, &key_vals, rid)?;
                    txn.record_index_insert(conn_txn, idx.index_id, idx.root_page_id, key);
                }
            }

            if let Some(new_root) = crate::index_maintenance::insert_many_into_single_index(
                idx,
                pred,
                &batch_refs,
                storage,
                bloom,
                snap.clone(),
            )? {
                CatalogWriter::new(storage, txn, conn_txn)?.update_index_root(idx.index_id, new_root)?;
                idx.root_page_id = new_root;
            }
        }
    }
    Ok(())
}
