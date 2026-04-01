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
    //
    // No-op partition (Phase 6.20): rows whose evaluated new_values == old_values
    // skip heap and index mutation. `count` remains the matched-row count (MySQL
    // semantics) — physical no-ops still count as "affected".
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

    // ── Field-level patch fast path (InnoDB-inspired) ────────────────────
    // If ALL changed columns are fixed-size (Int, BigInt, Real, Bool, Date,
    // Timestamp) and no variable-length columns precede them, patch the
    // encoded bytes directly in the heap page — skip full decode + encode.
    // Reduces per-row work from ~469 bytes to ~28 bytes (16.75× less).
    let col_types: Vec<axiomdb_types::DataType> = schema_cols
        .iter()
        .map(|c| crate::table::column_type_to_data_type(c.col_type))
        .collect();
    let field_patch_eligible = !to_update.is_empty()
        && ctx.strict_mode
        && assignments.iter().all(|(col_pos, _)| {
            // Check target column is fixed-size (patchable).
            axiomdb_types::field_patch::fixed_encoded_size(col_types[*col_pos]).is_some()
        });

    if field_patch_eligible && resolved.foreign_keys.is_empty() {
        // Fast path: patch fields in-place on heap pages.
        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        let mut patched = 0u64;

        // Group updates by page_id for batch page processing.
        let mut page_groups: std::collections::BTreeMap<u64, Vec<(u16, &Vec<Value>)>> =
            std::collections::BTreeMap::new();
        for (rid, _old, new_vals) in &to_update {
            page_groups
                .entry(rid.page_id)
                .or_default()
                .push((rid.slot_id, new_vals));
        }

        for (page_id, slots) in &page_groups {
            let mut page = storage.read_page(*page_id)?.into_page();
            let mut page_dirty = false;

            for &(slot_id, new_vals) in slots {
                let entry = axiomdb_storage::heap::read_slot(&page, slot_id);
                if entry.is_dead() {
                    continue;
                }
                let off = entry.offset as usize;
                let len = entry.length as usize;
                let hdr_size = std::mem::size_of::<axiomdb_storage::RowHeader>();

                // Read null bitmap from row data.
                let row_start = off + hdr_size;
                let bitmap_len = col_types.len().div_ceil(8);
                let bitmap = page.as_bytes()[row_start..row_start + bitmap_len].to_vec();

                // Update RowHeader (new txn_id, increment version).
                {
                    let hdr_bytes = &page.as_bytes()[off..off + hdr_size];
                    let old_hdr: &axiomdb_storage::RowHeader =
                        bytemuck::from_bytes(hdr_bytes);
                    let new_hdr = axiomdb_storage::RowHeader {
                        txn_id_created: txn_id,
                        txn_id_deleted: 0,
                        row_version: old_hdr.row_version.wrapping_add(1),
                        _flags: old_hdr._flags,
                    };
                    let raw = page.as_bytes_mut();
                    raw[off..off + hdr_size]
                        .copy_from_slice(bytemuck::bytes_of(&new_hdr));
                }

                // Patch each changed field directly.
                // Use runtime scanning to handle variable-length cols before target.
                let row_len = len - hdr_size;
                for &(col_pos, _) in &assignments {
                    let row_data_slice =
                        &page.as_bytes()[row_start..row_start + row_len];
                    if let Some(loc) =
                        axiomdb_types::field_patch::compute_field_location_runtime(
                            &col_types,
                            col_pos,
                            &bitmap,
                            Some(row_data_slice),
                        )
                    {
                        let row_data_mut =
                            &mut page.as_bytes_mut()[row_start..row_start + row_len];
                        let _ = axiomdb_types::field_patch::write_field(
                            row_data_mut, &loc, &new_vals[col_pos],
                        );
                    }
                }

                // Clear all-visible flag.
                page.clear_all_visible();
                page_dirty = true;
                patched += 1;
            }

            if page_dirty {
                page.update_checksum();
                storage.write_page(*page_id, &page)?;
            }
        }

        // WAL: record as batch UpdateInPlace with old+new tuples.
        // For simplicity, use the existing batch path (reads pages again
        // for old images — acceptable overhead vs the patching savings).
        // Future: field-delta WAL entries would be even more compact.
        // For now, just record the undo ops.
        for (rid, _old, _new) in &to_update {
            let entry = axiomdb_storage::heap::read_slot(
                &storage.read_page(rid.page_id)?.into_page(),
                rid.slot_id,
            );
            if !entry.is_dead() {
                txn.record_delete(
                    resolved.def.id,
                    &[],
                    &[],
                    rid.page_id,
                    rid.slot_id,
                )?;
            }
        }

        // Index maintenance (same as normal path).
        if !secondary_indexes.is_empty() {
            let update_pairs: Vec<(RecordId, Vec<Value>, RecordId, Vec<Value>)> = to_update
                .iter()
                .map(|(rid, old, new)| (*rid, old.clone(), *rid, new.clone()))
                .collect();
            apply_update_index_maintenance(
                &mut secondary_indexes.to_vec(),
                &compiled_preds,
                &update_pairs,
                storage,
                txn,
                bloom,
                snap,
            )?;
        }

        if patched > 0 {
            ctx.stats.on_rows_changed(resolved.def.id, patched);
        }
        ctx.invalidate_all();

        return Ok(QueryResult::Affected {
            count: matched_count,
            last_insert_id: None,
        });
    }

    // ── Normal UPDATE path (full decode + encode) ────────────────────────
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

    if !secondary_indexes.is_empty() && !to_update.is_empty() {
        let all_rids_stable = to_update
            .iter()
            .zip(new_rids.iter())
            .all(|((old_rid, _, _), new_rid)| old_rid == new_rid);
        let any_index_affected =
            statement_might_affect_indexes(&secondary_indexes, &compiled_preds, &assignments);

        if any_index_affected || !all_rids_stable {
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
                snap,
            )?;
            ctx.invalidate_all();
        }
    }

    Ok(QueryResult::Affected {
        count: matched_count,
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
            let snap = txn.active_snapshot()?;
            apply_update_index_maintenance(
                &mut secondary_indexes,
                &compiled_preds,
                &update_pairs,
                storage,
                txn,
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
                        let delete_keys_for_idx =
                            vec![crate::index_maintenance::encode_index_entry_key(
                                idx, &key_vals, *old_rid,
                            )?];
                        if let Some(new_root) =
                            crate::index_maintenance::delete_many_from_single_index(
                                idx,
                                &delete_keys_for_idx,
                                storage,
                                bloom,
                            )?
                        {
                            CatalogWriter::new(storage, txn)?
                                .update_index_root(idx.index_id, new_root)?;
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
                    let _ = txn.record_index_insert(idx.index_id, idx.root_page_id, key);
                }
            }

            if let Some(new_root) = crate::index_maintenance::insert_many_into_single_index(
                idx,
                pred,
                &batch_refs,
                storage,
                bloom,
                snap,
            )? {
                CatalogWriter::new(storage, txn)?.update_index_root(idx.index_id, new_root)?;
            }
        }
    }
    Ok(())
}
