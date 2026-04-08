/// Standard UPDATE path: processes pre-collected candidate rows through
/// expression evaluation, FK validation, and field-patch or full-encode paths.
#[allow(clippy::too_many_arguments)]
fn execute_update_with_candidates(
    candidate_rows: Vec<(RecordId, Vec<Value>)>,
    assignments: Vec<(usize, Expr)>,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    secondary_indexes: &[axiomdb_catalog::IndexDef],
    col_types: &[axiomdb_types::DataType],
    field_patch_eligible: bool,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    conn_txn: &mut ConnectionTxn,
    snap: axiomdb_core::TransactionSnapshot,
    resolved: &axiomdb_catalog::ResolvedTable,
    ctx: &mut SessionContext,
    bloom: &mut crate::bloom::BloomRegistry,
) -> Result<QueryResult, DbError> {

    // Collect all matching (rid, old_values, new_values) triples before touching
    // the heap. Old values are kept for secondary index maintenance (delete old
    // key before inserting new key into each B-Tree).
    //
    // No-op partition (Phase 6.20): rows whose evaluated new_values == old_values
    // skip heap and index mutation. `count` remains the matched-row count (MySQL
    // semantics) — physical no-ops still count as "affected".
    //
    // When the field-patch fast path is eligible AND no secondary indexes need
    // maintenance, we store only the sparse assigned-column values instead of
    // cloning the full row — eliminating String clones for unchanged columns.
    let needs_full_row = !field_patch_eligible || !secondary_indexes.is_empty();

    let mut to_update: Vec<(RecordId, Vec<Value>, Vec<Value>)> = Vec::new();
    // Sparse path: (rid, [(col_pos, new_value)])
    let mut to_update_sparse: Vec<(RecordId, Vec<(usize, Value)>)> = Vec::new();
    let mut matched_count: u64 = 0;

    for (rid, current_values) in candidate_rows {
        matched_count += 1;
        if needs_full_row {
            // Full-row path: build complete new_values (normal path / FK / index maintenance).
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
            if !changed {
                continue;
            }
            to_update.push((rid, current_values, new_values));
        } else {
            // Sparse path: only evaluate and store assigned columns.
            // Avoids cloning unchanged columns (e.g., String fields like name/email).
            let mut sparse = Vec::with_capacity(assignments.len());
            let mut changed = false;
            for &(col_pos, ref val_expr) in &assignments {
                let nv = eval(val_expr, &current_values)?;
                if nv != current_values[col_pos] {
                    changed = true;
                }
                sparse.push((col_pos, nv));
            }
            if !changed {
                continue;
            }
            to_update_sparse.push((rid, sparse));
        }
    }

    // CHAR(N) padding + VARCHAR(N) length + CHECK constraint validation (mirrors INSERT path).
    for (_, _, new_values) in &mut to_update {
        enforce_text_constraints(&resolved.columns, new_values)?;
    }
    if !resolved.constraints.is_empty() {
        for (_, _, new_values) in &to_update {
            check_row_constraints(
                &resolved.constraints,
                new_values,
                &resolved.def.table_name,
            )?;
        }
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
                conn_txn,
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
            conn_txn,
        )?;
    }

    let compiled_preds =
        crate::partial_index::compile_index_predicates(secondary_indexes, schema_cols)?;

    // ── Field-level patch fast path (InnoDB-inspired) ────────────────────
    // If ALL changed columns are fixed-size (Int, BigInt, Real, Bool, Date,
    // Timestamp) and no variable-length columns precede them, patch the
    // encoded bytes directly in the heap page — skip full decode + encode.
    // Reduces per-row work from ~469 bytes to ~28 bytes (16.75× less).
    //
    // The sparse path (to_update_sparse) avoids cloning unchanged columns,
    // eliminating String allocations for non-assigned fields.

    let use_sparse_fast_path = field_patch_eligible
        && !to_update_sparse.is_empty()
        && resolved.foreign_keys.is_empty();
    let use_full_fast_path = !use_sparse_fast_path
        && !to_update.is_empty()
        && field_patch_eligible
        && resolved.foreign_keys.is_empty();

    if use_sparse_fast_path || use_full_fast_path {
        // Fast path: patch fields in-place on heap pages.
        let mut patched = 0u64;

        // Build page groups from either sparse or full data.
        // Sparse: (slot_id, &[(col_pos, Value)])
        // Full:   (slot_id, &Vec<Value>)
        enum SlotData<'a> {
            Sparse(&'a [(usize, Value)]),
            Full(&'a Vec<Value>),
        }

        let mut page_groups: std::collections::BTreeMap<u64, Vec<(u16, SlotData<'_>)>> =
            std::collections::BTreeMap::new();
        if use_sparse_fast_path {
            for (rid, sparse_vals) in &to_update_sparse {
                page_groups
                    .entry(rid.page_id)
                    .or_default()
                    .push((rid.slot_id, SlotData::Sparse(sparse_vals)));
            }
        } else {
            for (rid, _old, new_vals) in &to_update {
                page_groups
                    .entry(rid.page_id)
                    .or_default()
                    .push((rid.slot_id, SlotData::Full(new_vals)));
            }
        }

        // Collect WAL images: (key, old_tuple_image, new_tuple_image, page_id, slot_id).
        #[allow(clippy::type_complexity)]
        let mut wal_images: Vec<(Vec<u8>, Vec<u8>, Vec<u8>, u64, u16)> = Vec::new();
        let hdr_size = std::mem::size_of::<axiomdb_storage::RowHeader>();

        for (page_id, slots) in &page_groups {
            let mut page = storage.read_page(*page_id)?.into_page();
            let mut page_dirty = false;

            for &(slot_id, ref slot_data) in slots {
                let entry = axiomdb_storage::heap::read_slot(&page, slot_id);
                if entry.is_dead() {
                    continue;
                }
                let off = entry.offset as usize;
                let len = entry.length as usize;

                // Capture old tuple image BEFORE patching (RowHeader + row data).
                let old_image = page.as_bytes()[off..off + len].to_vec();

                // Read null bitmap from row data.
                let row_start = off + hdr_size;
                let bitmap_len = col_types.len().div_ceil(8);
                let bitmap = page.as_bytes()[row_start..row_start + bitmap_len].to_vec();

                // Update RowHeader: keep original txn_id_created (MVCC visibility),
                // only increment version. Do NOT change txn_id_created — the row
                // was created by another transaction and must remain visible to
                // snapshots that already see it.
                {
                    let hdr_bytes = &page.as_bytes()[off..off + hdr_size];
                    let old_hdr: &axiomdb_storage::RowHeader =
                        bytemuck::from_bytes(hdr_bytes);
                    let new_hdr = axiomdb_storage::RowHeader {
                        txn_id_created: old_hdr.txn_id_created,
                        txn_id_deleted: 0,
                        row_version: old_hdr.row_version.wrapping_add(1),
                        _flags: old_hdr._flags,
                    };
                    let raw = page.as_bytes_mut();
                    raw[off..off + hdr_size]
                        .copy_from_slice(bytemuck::bytes_of(&new_hdr));
                }

                // Patch each changed field directly.
                let row_len = len - hdr_size;
                match slot_data {
                    SlotData::Sparse(sparse_vals) => {
                        for (col_pos, new_val) in *sparse_vals {
                            let row_data_slice =
                                &page.as_bytes()[row_start..row_start + row_len];
                            if let Some(loc) =
                                axiomdb_types::field_patch::compute_field_location_runtime(
                                    col_types,
                                    *col_pos,
                                    &bitmap,
                                    Some(row_data_slice),
                                )
                            {
                                let row_data_mut =
                                    &mut page.as_bytes_mut()[row_start..row_start + row_len];
                                let _ = axiomdb_types::field_patch::write_field(
                                    row_data_mut, &loc, new_val,
                                );
                            }
                        }
                    }
                    SlotData::Full(new_vals) => {
                        for &(col_pos, _) in &assignments {
                            let row_data_slice =
                                &page.as_bytes()[row_start..row_start + row_len];
                            if let Some(loc) =
                                axiomdb_types::field_patch::compute_field_location_runtime(
                                    col_types,
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
                    }
                }

                // Capture new tuple image AFTER patching.
                let new_image = page.as_bytes()[off..off + len].to_vec();

                wal_images.push((vec![], old_image, new_image, *page_id, slot_id));

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

        // WAL: record as batch UpdateInPlace with old+new tuple images.
        // On ROLLBACK, UndoUpdateInPlace restores the old tuple image byte-for-byte.
        {
            #[allow(clippy::type_complexity)]
            let batch_refs: Vec<(&[u8], &[u8], &[u8], u64, u16)> = wal_images
                .iter()
                .map(|(k, old, new, pid, sid)| {
                    (k.as_slice(), old.as_slice(), new.as_slice(), *pid, *sid)
                })
                .collect();
            let _ = txn.record_update_in_place_batch(conn_txn, resolved.def.id, &batch_refs);
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
                conn_txn,
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
        conn_txn,
        &resolved.def,
        schema_cols,
        ctx,
        heap_updates,
    )?;

    if !secondary_indexes.is_empty() && !to_update.is_empty() {
        let all_rids_stable = to_update
            .iter()
            .zip(new_rids.iter())
            .all(|((old_rid, _, _), new_rid)| old_rid == new_rid);
        let any_index_affected =
            statement_might_affect_indexes(secondary_indexes, &compiled_preds, &assignments);

        if any_index_affected || !all_rids_stable {
            let mut current_indexes = secondary_indexes.to_vec();
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
                conn_txn,
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


