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

    // ── Clustered table UPDATE dispatch (Phase 39.16) ────────────────────
    if resolved.def.is_clustered() {
        return execute_clustered_update(
            stmt.where_clause,
            assignments,
            &schema_cols,
            &secondary_indexes,
            storage,
            txn,
            snap,
            &resolved,
            bloom,
            ctx,
        );
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
                return fused_index_range_patch(
                    index_def,
                    lo.as_deref(),
                    hi.as_deref(),
                    &assignments,
                    &col_types,
                    storage,
                    txn,
                    snap,
                    &resolved,
                    ctx,
                );
            }
        }

        // Fall through to standard candidate collection.
        let candidate_rows: Vec<(RecordId, Vec<Value>)> = collect_delete_candidates(
            wc,
            &secondary_indexes,
            &schema_cols,
            &update_access,
            storage,
            snap,
            &resolved.def,
            bloom,
        )?;

        return execute_update_with_candidates(
            candidate_rows,
            assignments,
            &schema_cols,
            &secondary_indexes,
            &col_types,
            field_patch_eligible,
            storage,
            txn,
            snap,
            &resolved,
            ctx,
            bloom,
        );
    }

    // No WHERE clause — full table scan.
    let candidate_rows: Vec<(RecordId, Vec<Value>)> =
        TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?;

    execute_update_with_candidates(
        candidate_rows,
        assignments,
        &schema_cols,
        &secondary_indexes,
        &col_types,
        field_patch_eligible,
        storage,
        txn,
        snap,
        &resolved,
        ctx,
        bloom,
    )
}

/// Fused index-range patch: B-tree range scan → group by page → patch fields
/// directly on heap pages without decoding rows. Eliminates:
/// - Full row decode (5000× for typical range)
/// - WHERE recheck (redundant, index already filtered)
/// - Double heap page read (candidate collection + patch phase)
fn fused_index_range_patch(
    index_def: &axiomdb_catalog::IndexDef,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    assignments: &[(usize, Expr)],
    col_types: &[axiomdb_types::DataType],
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    snap: axiomdb_core::TransactionSnapshot,
    resolved: &axiomdb_catalog::ResolvedTable,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let _txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;

    // 1. B-tree range scan → collect RecordIds only.
    let pairs = BTree::range_in(storage, index_def.root_page_id, lo, hi)?;
    let rids: Vec<RecordId> = pairs.into_iter().map(|(rid, _)| rid).collect();

    if rids.is_empty() {
        return Ok(QueryResult::Affected {
            count: 0,
            last_insert_id: None,
        });
    }

    // 2. Group RIDs by page_id for sequential page access.
    let mut page_groups: std::collections::BTreeMap<u64, Vec<u16>> =
        std::collections::BTreeMap::new();
    for rid in &rids {
        page_groups
            .entry(rid.page_id)
            .or_default()
            .push(rid.slot_id);
    }

    // 3. For each page: read → patch visible matching slots → write.
    let hdr_size = std::mem::size_of::<axiomdb_storage::RowHeader>();
    let bitmap_len = col_types.len().div_ceil(8);
    let n_cols = col_types.len();

    #[allow(clippy::type_complexity)]
    let mut wal_images: Vec<(Vec<u8>, Vec<u8>, Vec<u8>, u64, u16)> = Vec::new();
    let mut matched = 0u64;
    let mut patched = 0u64;

    // Reusable sparse row for eval(): only populated at assigned column positions.
    let mut sparse_row: Vec<Value> = vec![Value::Null; n_cols];

    for (page_id, slot_ids) in &page_groups {
        let mut page = storage.read_page(*page_id)?.into_page();
        let mut page_dirty = false;

        for &slot_id in slot_ids {
            let entry = axiomdb_storage::heap::read_slot(&page, slot_id);
            if entry.is_dead() {
                continue;
            }
            let off = entry.offset as usize;
            let len = entry.length as usize;

            // MVCC visibility check (O(1): just txn_id comparisons).
            // Copy header fields to avoid holding an immutable borrow across mutations.
            let (hdr_visible, hdr_txn_created, hdr_version, hdr_flags) = {
                let hdr: &axiomdb_storage::RowHeader =
                    bytemuck::from_bytes(&page.as_bytes()[off..off + hdr_size]);
                (
                    hdr.is_visible(&snap),
                    hdr.txn_id_created,
                    hdr.row_version,
                    hdr._flags,
                )
            };
            if !hdr_visible {
                continue;
            }
            matched += 1;

            // Capture old tuple image BEFORE patching.
            let old_image = page.as_bytes()[off..off + len].to_vec();

            let row_start = off + hdr_size;
            let row_len = len - hdr_size;
            let bitmap = page.as_bytes()[row_start..row_start + bitmap_len].to_vec();

            // For each assignment: read field → eval → write field.
            let mut changed = false;
            for &(col_pos, ref val_expr) in assignments {
                let row_data = &page.as_bytes()[row_start..row_start + row_len];
                if let Some(loc) =
                    axiomdb_types::field_patch::compute_field_location_runtime(
                        col_types,
                        col_pos,
                        &bitmap,
                        Some(row_data),
                    )
                {
                    let current_val =
                        axiomdb_types::field_patch::read_field(row_data, &loc)?;
                    // Populate sparse row for eval, then reset after.
                    sparse_row[col_pos] = current_val.clone();
                    let new_val = eval(val_expr, &sparse_row)?;
                    sparse_row[col_pos] = Value::Null;

                    if new_val != current_val {
                        changed = true;
                        let row_mut =
                            &mut page.as_bytes_mut()[row_start..row_start + row_len];
                        let _ = axiomdb_types::field_patch::write_field(
                            row_mut, &loc, &new_val,
                        );
                    }
                }
            }

            if changed {
                // Increment row version, preserve txn_id_created.
                let new_hdr = axiomdb_storage::RowHeader {
                    txn_id_created: hdr_txn_created,
                    txn_id_deleted: 0,
                    row_version: hdr_version.wrapping_add(1),
                    _flags: hdr_flags,
                };
                page.as_bytes_mut()[off..off + hdr_size]
                    .copy_from_slice(bytemuck::bytes_of(&new_hdr));

                let new_image = page.as_bytes()[off..off + len].to_vec();
                wal_images.push((vec![], old_image, new_image, *page_id, slot_id));
                page.clear_all_visible();
                page_dirty = true;
                patched += 1;
            }
        }

        if page_dirty {
            page.update_checksum();
            storage.write_page(*page_id, &page)?;
        }
    }

    // WAL: batch UpdateInPlace with old+new tuple images.
    if !wal_images.is_empty() {
        let batch_refs: Vec<(&[u8], &[u8], &[u8], u64, u16)> = wal_images
            .iter()
            .map(|(k, old, new, pid, sid)| {
                (k.as_slice(), old.as_slice(), new.as_slice(), *pid, *sid)
            })
            .collect();
        txn.record_update_in_place_batch(resolved.def.id, &batch_refs)?;
    }

    if patched > 0 {
        ctx.stats.on_rows_changed(resolved.def.id, patched);
    }
    ctx.invalidate_all();

    Ok(QueryResult::Affected {
        count: matched,
        last_insert_id: None,
    })
}

#[derive(Debug, Clone)]
struct ClusteredUpdateCandidate {
    pk_key: Vec<u8>,
    row_header: axiomdb_storage::heap::RowHeader,
    row_data: Vec<u8>,
    values: Vec<Value>,
}

fn normalize_clustered_update_access_method(
    access_method: crate::planner::AccessMethod,
) -> crate::planner::AccessMethod {
    match access_method {
        crate::planner::AccessMethod::IndexOnlyScan {
            index_def, lo, hi, ..
        } => {
            let is_single_key_point = index_def.columns.len() == 1
                && hi
                    .as_ref()
                    .map(|bound| bound.as_slice() == lo.as_slice())
                    .unwrap_or(false);

            if is_single_key_point {
                crate::planner::AccessMethod::IndexLookup { index_def, key: lo }
            } else {
                crate::planner::AccessMethod::IndexRange {
                    index_def,
                    lo: Some(lo),
                    hi,
                }
            }
        }
        other => other,
    }
}

fn clustered_update_primary_index(
    resolved: &axiomdb_catalog::ResolvedTable,
) -> Result<&axiomdb_catalog::IndexDef, DbError> {
    resolved
        .indexes
        .iter()
        .find(|idx| idx.is_primary && !idx.columns.is_empty())
        .ok_or_else(|| DbError::Internal {
            message: format!(
                "clustered table {}.{} is missing primary-index metadata",
                resolved.def.schema_name, resolved.def.table_name
            ),
        })
}

fn clustered_secondary_high_bound(logical_key: &[u8]) -> Vec<u8> {
    let mut hi = logical_key.to_vec();
    if hi.len() < crate::key_encoding::MAX_INDEX_KEY {
        hi.resize(crate::key_encoding::MAX_INDEX_KEY, 0xFF);
    }
    hi
}

fn clustered_rows_for_secondary_access(
    storage: &dyn StorageEngine,
    root_pid: u64,
    resolved: &axiomdb_catalog::ResolvedTable,
    index_def: &axiomdb_catalog::IndexDef,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    snap: axiomdb_core::TransactionSnapshot,
) -> Result<Vec<axiomdb_storage::clustered_tree::ClusteredRow>, DbError> {
    let primary_idx = clustered_update_primary_index(resolved)?;
    let layout = crate::clustered_secondary::ClusteredSecondaryLayout::derive(index_def, primary_idx)?;
    let hi_owned = hi.map(clustered_secondary_high_bound);
    let pairs = BTree::range_in(storage, index_def.root_page_id, lo, hi_owned.as_deref())?;
    let mut rows = Vec::with_capacity(pairs.len());

    for (_rid, key_bytes) in pairs {
        let entry = layout.decode_entry_key(&key_bytes)?;
        let pk_key = crate::key_encoding::encode_index_key(&entry.primary_key)?;
        if let Some(row) =
            axiomdb_storage::clustered_tree::lookup(storage, Some(root_pid), &pk_key, &snap)?
        {
            rows.push(row);
        }
    }

    Ok(rows)
}

fn collect_clustered_update_candidates(
    where_clause: Option<&Expr>,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    secondary_indexes: &[axiomdb_catalog::IndexDef],
    storage: &dyn StorageEngine,
    snap: axiomdb_core::TransactionSnapshot,
    resolved: &axiomdb_catalog::ResolvedTable,
    root_pid: u64,
    effective_collation: crate::session::SessionCollation,
) -> Result<Vec<ClusteredUpdateCandidate>, DbError> {
    use std::ops::Bound;

    let col_types: Vec<axiomdb_types::DataType> = schema_cols
        .iter()
        .map(|c| crate::table::column_type_to_data_type(c.col_type))
        .collect();

    let access_method = where_clause
        .map(|wc| {
            normalize_clustered_update_access_method(crate::planner::plan_update_candidates_ctx(
                wc,
                secondary_indexes,
                schema_cols,
                effective_collation,
            ))
        })
        .unwrap_or(crate::planner::AccessMethod::Scan);

    let mut raw_rows = Vec::new();
    match access_method {
        crate::planner::AccessMethod::Scan => {
            let iter = axiomdb_storage::clustered_tree::range(
                storage,
                Some(root_pid),
                Bound::Unbounded,
                Bound::Unbounded,
                &snap,
            )?;
            for row in iter {
                raw_rows.push(row?);
            }
        }
        crate::planner::AccessMethod::IndexLookup { index_def, key } if index_def.is_primary => {
            if let Some(row) =
                axiomdb_storage::clustered_tree::lookup(storage, Some(root_pid), &key, &snap)?
            {
                raw_rows.push(row);
            }
        }
        crate::planner::AccessMethod::IndexLookup { index_def, key } => {
            let hi = clustered_secondary_high_bound(&key);
            raw_rows.extend(clustered_rows_for_secondary_access(
                storage,
                root_pid,
                resolved,
                &index_def,
                Some(&key),
                Some(&hi),
                snap,
            )?);
        }
        crate::planner::AccessMethod::IndexRange { index_def, lo, hi } if index_def.is_primary => {
            let iter = axiomdb_storage::clustered_tree::range(
                storage,
                Some(root_pid),
                lo.map_or(Bound::Unbounded, Bound::Included),
                hi.map_or(Bound::Unbounded, Bound::Included),
                &snap,
            )?;
            for row in iter {
                raw_rows.push(row?);
            }
        }
        crate::planner::AccessMethod::IndexRange { index_def, lo, hi } => {
            raw_rows.extend(clustered_rows_for_secondary_access(
                storage,
                root_pid,
                resolved,
                &index_def,
                lo.as_deref(),
                hi.as_deref(),
                snap,
            )?);
        }
        crate::planner::AccessMethod::IndexOnlyScan { .. } => unreachable!(),
    }

    let mut seen = std::collections::HashSet::new();
    let mut candidates = Vec::with_capacity(raw_rows.len());
    for row in raw_rows {
        if !seen.insert(row.key.clone()) {
            continue;
        }
        let values = axiomdb_types::codec::decode_row(&row.row_data, &col_types)?;
        if let Some(wc) = where_clause {
            if !is_truthy(&eval(wc, &values)?) {
                continue;
            }
        }
        candidates.push(ClusteredUpdateCandidate {
            pk_key: row.key,
            row_header: row.row_header,
            row_data: row.row_data,
            values,
        });
    }

    Ok(candidates)
}

fn apply_clustered_secondary_update(
    idx: &axiomdb_catalog::IndexDef,
    layout: &crate::clustered_secondary::ClusteredSecondaryLayout,
    sec_root: &std::sync::atomic::AtomicU64,
    old_values: &[Value],
    new_values: &[Value],
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
) -> Result<(), DbError> {
    let old_entry = layout.entry_from_row(old_values)?;
    let new_entry = layout.entry_from_row(new_values)?;
    let outcome = layout.update_row(storage, sec_root, old_values, new_values)?;
    let current_root = sec_root.load(std::sync::atomic::Ordering::Acquire);

    match outcome {
        crate::clustered_secondary::ClusteredSecondaryUpdateOutcome::Unchanged => {}
        crate::clustered_secondary::ClusteredSecondaryUpdateOutcome::Inserted => {
            if let Some(new_entry) = new_entry {
                bloom.add(idx.index_id, &new_entry.physical_key);
                txn.record_index_insert(idx.index_id, current_root, new_entry.physical_key)?;
            }
        }
        crate::clustered_secondary::ClusteredSecondaryUpdateOutcome::Deleted => {
            if let Some(old_entry) = old_entry {
                txn.record_index_delete(
                    idx.index_id,
                    current_root,
                    old_entry.physical_key,
                    RecordId {
                        page_id: 0,
                        slot_id: 0,
                    },
                    idx.fillfactor,
                )?;
            }
        }
        crate::clustered_secondary::ClusteredSecondaryUpdateOutcome::Replaced => {
            if let Some(old_entry) = old_entry {
                txn.record_index_delete(
                    idx.index_id,
                    current_root,
                    old_entry.physical_key,
                    RecordId {
                        page_id: 0,
                        slot_id: 0,
                    },
                    idx.fillfactor,
                )?;
            }
            if let Some(new_entry) = new_entry {
                bloom.add(idx.index_id, &new_entry.physical_key);
                txn.record_index_insert(idx.index_id, current_root, new_entry.physical_key)?;
            }
        }
    }

    Ok(())
}

/// Clustered table UPDATE: collects candidates from the clustered B-tree,
/// evaluates assignments, then applies updates via the clustered storage layer.
///
/// Three update paths:
/// 1. Non-key in-place: `update_in_place()` when PK and index keys unchanged
/// 2. Non-key relocation: `update_with_relocation()` when row grows beyond leaf
/// 3. Key change: `delete_mark()` + `insert()` when PK changes
fn execute_clustered_update(
    where_clause: Option<Expr>,
    assignments: Vec<(usize, Expr)>,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    secondary_indexes: &[axiomdb_catalog::IndexDef],
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    snap: axiomdb_core::TransactionSnapshot,
    resolved: &axiomdb_catalog::ResolvedTable,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let col_types: Vec<axiomdb_types::DataType> = schema_cols
        .iter()
        .map(|c| crate::table::column_type_to_data_type(c.col_type))
        .collect();

    let pk_col_positions: Vec<usize> = clustered_update_primary_index(resolved)?
        .columns
        .iter()
        .map(|c| c.col_idx as usize)
        .collect();

    let mut root_pid = txn
        .clustered_root(resolved.def.id)
        .unwrap_or(resolved.def.root_page_id);
    let candidates = collect_clustered_update_candidates(
        where_clause.as_ref(),
        schema_cols,
        secondary_indexes,
        storage,
        snap,
        resolved,
        root_pid,
        ctx.effective_collation(),
    )?;

    if candidates.is_empty() {
        return Ok(QueryResult::Affected {
            count: 0,
            last_insert_id: None,
        });
    }

    let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
    let mut matched_count = 0u64;
    let mut changed_count = 0u64;

    let primary_idx = clustered_update_primary_index(resolved)?;
    let secondary_layouts: Vec<(
        &axiomdb_catalog::IndexDef,
        crate::clustered_secondary::ClusteredSecondaryLayout,
        std::sync::atomic::AtomicU64,
    )> = secondary_indexes
        .iter()
        .filter(|idx| !idx.is_primary && !idx.columns.is_empty())
        .filter_map(|idx| {
            crate::clustered_secondary::ClusteredSecondaryLayout::derive(idx, primary_idx)
                .ok()
                .map(|layout| {
                    (
                        idx,
                        layout,
                        std::sync::atomic::AtomicU64::new(idx.root_page_id),
                    )
                })
        })
        .collect();

    for candidate in &candidates {
        matched_count += 1;

        let mut new_values = candidate.values.clone();
        let mut changed = false;
        for &(col_pos, ref val_expr) in &assignments {
            let nv = eval(val_expr, &candidate.values)?;
            if nv != candidate.values[col_pos] {
                changed = true;
            }
            new_values[col_pos] = nv;
        }
        if !changed {
            continue;
        }

        let pk_changed = pk_col_positions
            .iter()
            .any(|&pos| candidate.values[pos] != new_values[pos]);
        let new_row_data = axiomdb_types::codec::encode_row(&new_values, &col_types)?;
        let old_image =
            axiomdb_wal::ClusteredRowImage::new(root_pid, candidate.row_header, &candidate.row_data);

        if pk_changed {
            let new_pk_key = crate::key_encoding::encode_index_key(
                &pk_col_positions
                    .iter()
                    .map(|&pos| new_values[pos].clone())
                    .collect::<Vec<_>>(),
            )?;

            if !axiomdb_storage::clustered_tree::delete_mark(
                storage,
                Some(root_pid),
                &candidate.pk_key,
                txn_id,
                &snap,
            )? {
                continue;
            }

            let delete_marked_header = axiomdb_storage::heap::RowHeader {
                txn_id_created: candidate.row_header.txn_id_created,
                txn_id_deleted: txn_id,
                row_version: candidate.row_header.row_version,
                _flags: candidate.row_header._flags,
            };
            let delete_mark_image = axiomdb_wal::ClusteredRowImage::new(
                root_pid,
                delete_marked_header,
                &candidate.row_data,
            );
            txn.record_clustered_delete_mark(
                resolved.def.id,
                &candidate.pk_key,
                &old_image,
                &delete_mark_image,
            )?;

            let new_header = axiomdb_storage::heap::RowHeader {
                txn_id_created: txn_id,
                txn_id_deleted: 0,
                row_version: 0,
                _flags: candidate.row_header._flags,
            };
            root_pid = axiomdb_storage::clustered_tree::insert(
                storage,
                Some(root_pid),
                &new_pk_key,
                &new_header,
                &new_row_data,
            )?;
            let inserted_image =
                axiomdb_wal::ClusteredRowImage::new(root_pid, new_header, &new_row_data);
            txn.record_clustered_insert(resolved.def.id, &new_pk_key, &inserted_image)?;

            for (idx, layout, sec_root) in &secondary_layouts {
                apply_clustered_secondary_update(
                    idx,
                    layout,
                    sec_root,
                    &candidate.values,
                    &new_values,
                    storage,
                    txn,
                    bloom,
                )?;
            }
        } else {
            let new_header = axiomdb_storage::heap::RowHeader {
                txn_id_created: txn_id,
                txn_id_deleted: 0,
                row_version: candidate.row_header.row_version.saturating_add(1),
                _flags: candidate.row_header._flags,
            };

            match axiomdb_storage::clustered_tree::update_in_place(
                storage,
                Some(root_pid),
                &candidate.pk_key,
                &new_row_data,
                txn_id,
                &snap,
            ) {
                Ok(true) => {
                    let new_image =
                        axiomdb_wal::ClusteredRowImage::new(root_pid, new_header, &new_row_data);
                    txn.record_clustered_update(
                        resolved.def.id,
                        &candidate.pk_key,
                        &old_image,
                        &new_image,
                    )?;
                }
                Ok(false) => continue,
                Err(DbError::HeapPageFull { .. }) => {
                    if let Some(new_root) = axiomdb_storage::clustered_tree::update_with_relocation(
                        storage,
                        Some(root_pid),
                        &candidate.pk_key,
                        &new_row_data,
                        txn_id,
                        &snap,
                    )? {
                        let new_image = axiomdb_wal::ClusteredRowImage::new(
                            new_root,
                            new_header,
                            &new_row_data,
                        );
                        txn.record_clustered_update(
                            resolved.def.id,
                            &candidate.pk_key,
                            &old_image,
                            &new_image,
                        )?;
                        root_pid = new_root;
                    } else {
                        continue;
                    }
                }
                Err(err) => return Err(err),
            }

            let any_sec_col_changed = secondary_layouts.iter().any(|(idx, _, _)| {
                idx.columns.iter().any(|c| {
                    let pos = c.col_idx as usize;
                    candidate.values.get(pos) != new_values.get(pos)
                })
            });
            if any_sec_col_changed {
                for (idx, layout, sec_root) in &secondary_layouts {
                    apply_clustered_secondary_update(
                        idx,
                        layout,
                        sec_root,
                        &candidate.values,
                        &new_values,
                        storage,
                        txn,
                        bloom,
                    )?;
                }
            }
        }

        changed_count += 1;
    }

    if root_pid != resolved.def.root_page_id {
        axiomdb_catalog::CatalogWriter::new(storage, txn)?
            .update_table_root(resolved.def.id, root_pid)?;
    }

    for (idx, _, sec_root) in &secondary_layouts {
        let current = sec_root.load(std::sync::atomic::Ordering::Acquire);
        if current != idx.root_page_id {
            axiomdb_catalog::CatalogWriter::new(storage, txn)?
                .update_index_root(idx.index_id, current)?;
        }
    }

    if changed_count > 0 {
        ctx.stats.on_rows_changed(resolved.def.id, changed_count);
    }
    ctx.invalidate_all();

    Ok(QueryResult::Affected {
        count: matched_count,
        last_insert_id: None,
    })
}

/// Standard UPDATE path: processes pre-collected candidate rows through
/// expression evaluation, FK validation, and field-patch or full-encode paths.
fn execute_update_with_candidates(
    candidate_rows: Vec<(RecordId, Vec<Value>)>,
    assignments: Vec<(usize, Expr)>,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    secondary_indexes: &[axiomdb_catalog::IndexDef],
    col_types: &[axiomdb_types::DataType],
    field_patch_eligible: bool,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
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
        let _txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
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
                                    &col_types,
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
            txn.record_update_in_place_batch(resolved.def.id, &batch_refs)?;
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
    if resolved.def.is_clustered() {
        let mut temp_ctx = SessionContext::new();
        return execute_clustered_update(
            stmt.where_clause,
            assignments,
            &schema_cols,
            &secondary_indexes,
            storage,
            txn,
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
                idx.root_page_id = new_root;
            }
        }
    }
    Ok(())
}
