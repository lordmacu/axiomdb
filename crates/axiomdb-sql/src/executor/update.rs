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
#[allow(clippy::too_many_arguments)]
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
        type WalImageRef<'a> = (&'a [u8], &'a [u8], &'a [u8], u64, u16);
        let batch_refs: Vec<WalImageRef<'_>> = wal_images
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
    _secondary_indexes: &[axiomdb_catalog::IndexDef],
    access_method: &crate::planner::AccessMethod,
    storage: &dyn StorageEngine,
    snap: axiomdb_core::TransactionSnapshot,
    resolved: &axiomdb_catalog::ResolvedTable,
    root_pid: u64,
) -> Result<Vec<ClusteredUpdateCandidate>, DbError> {
    use std::ops::Bound;

    let col_types: Vec<axiomdb_types::DataType> = schema_cols
        .iter()
        .map(|c| crate::table::column_type_to_data_type(c.col_type))
        .collect();

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
                axiomdb_storage::clustered_tree::lookup(storage, Some(root_pid), &key[..], &snap)?
            {
                raw_rows.push(row);
            }
        }
        crate::planner::AccessMethod::IndexLookup { index_def, key } => {
            let hi = clustered_secondary_high_bound(&key[..]);
            raw_rows.extend(clustered_rows_for_secondary_access(
                storage,
                root_pid,
                resolved,
                index_def,
                Some(key.as_slice()),
                Some(hi.as_slice()),
                snap,
            )?);
        }
        crate::planner::AccessMethod::IndexRange { index_def, lo, hi } if index_def.is_primary => {
            let iter = axiomdb_storage::clustered_tree::range(
                storage,
                Some(root_pid),
                lo.clone().map_or(Bound::Unbounded, Bound::Included),
                hi.clone().map_or(Bound::Unbounded, Bound::Included),
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
                index_def,
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
/// Fused clustered scan-patch: walks the leaf chain once, evaluates WHERE per
/// cell, patches fixed-size fields in-place. One page read+write per leaf.
/// Eliminates: candidate Vec, per-row tree descent, per-row page I/O.
#[allow(clippy::too_many_arguments)]
fn fused_clustered_scan_patch(
    where_clause: Option<&Expr>,
    assignments: &[(usize, Expr)],
    col_types: &[axiomdb_types::DataType],
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    snap: axiomdb_core::TransactionSnapshot,
    resolved: &axiomdb_catalog::ResolvedTable,
    root_pid: u64,
    ctx: &mut SessionContext,
    from: std::ops::Bound<Vec<u8>>,
    to: std::ops::Bound<Vec<u8>>,
) -> Result<QueryResult, DbError> {
    use std::ops::Bound;

    use axiomdb_storage::{clustered_internal, clustered_leaf, page::PageType};

    let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
    let n_cols = col_types.len();

    let (mut current, mut slot_idx) = match &from {
        Bound::Unbounded => {
            let mut pid = root_pid;
            loop {
                let page = storage.read_page(pid)?;
                let pt = PageType::try_from(page.header().page_type)
                    .map_err(|e| DbError::Other(format!("{e}")))?;
                match pt {
                    PageType::ClusteredLeaf => break (pid, 0usize),
                    PageType::ClusteredInternal => {
                        pid = clustered_internal::child_at(&page, 0)?;
                    }
                    _ => {
                        return Err(DbError::BTreeCorrupted {
                            msg: format!(
                                "fused_clustered_scan_patch: unexpected page type {pt:?}"
                            ),
                        });
                    }
                }
            }
        }
        Bound::Included(key) => {
            let leaf =
                axiomdb_storage::clustered_tree::descend_to_leaf_pub(storage, root_pid, key)?;
            let slot = match clustered_leaf::search(&leaf, key) {
                Ok(pos) | Err(pos) => pos,
            };
            (leaf.header().page_id, slot)
        }
        Bound::Excluded(key) => {
            let leaf =
                axiomdb_storage::clustered_tree::descend_to_leaf_pub(storage, root_pid, key)?;
            let slot = match clustered_leaf::search(&leaf, key) {
                Ok(pos) => pos + 1,
                Err(pos) => pos,
            };
            (leaf.header().page_id, slot)
        }
    };

    // Compile BatchPredicate for zero-alloc raw-byte WHERE evaluation.
    // Falls back to eval() for unsupported patterns (OR, LIKE, Text comparisons).
    let batch_pred = where_clause
        .and_then(|wc| crate::eval::batch::try_compile(wc, col_types));

    let mut matched = 0u64;
    let mut patched = 0u64;
    let mut sparse_row: Vec<Value> = vec![Value::Null; n_cols];

    while current != clustered_leaf::NULL_PAGE {
        let mut page = storage.read_page(current)?.into_page();
        let next = clustered_leaf::next_leaf(&page);
        let page_id = page.header().page_id;
        let num = clustered_leaf::num_cells(&page) as usize;
        let mut page_dirty = false;

        // Phase 1: Collect cells to patch on this page (to avoid borrow conflicts).
        //
        // `local_row_data` is intentionally NOT stored here (was the root cause of the
        // 2× full-row clone per matched row). Phase 2 reads row bytes directly from
        // the page buffer via `cell_row_data_abs_off` — zero allocation for inline cells.
        // Overflow cells re-read the cell on demand (rare, < 1% of typical workloads).
        struct PatchInfo {
            idx: usize,
            old_header: axiomdb_storage::heap::RowHeader,
            total_row_len: usize,
            overflow_first_page: Option<u64>,
            key: Vec<u8>,
            changed_fields: Vec<(usize, Value)>, // (col_pos, new_value)
        }
        let mut patches: Vec<PatchInfo> = Vec::new();

        // BatchPredicate fast-reject: evaluate WHERE on raw cell bytes
        // BEFORE decoding. Cells that fail the predicate are skipped entirely
        // (no decode, no eval, no allocation). ~20ns/row vs ~130ns/row.
        let mut bp_passed = vec![true; num];
        if let Some(ref bp) = batch_pred {
            for (idx, slot) in bp_passed.iter_mut().enumerate() {
                if let Ok(cell) = clustered_leaf::read_cell(&page, idx as u16) {
                    *slot = cell.row_header.is_visible(&snap) && bp.eval_on_raw(cell.row_data);
                } else {
                    *slot = false;
                }
            }
        }

        while slot_idx < num {
            let idx = slot_idx;
            slot_idx += 1;

            // BatchPredicate pre-filter: skip cells that failed raw-byte eval.
            if !bp_passed[idx] {
                continue;
            }

            let cell = clustered_leaf::read_cell(&page, idx as u16)?;
            let above_lower = match &from {
                Bound::Unbounded => true,
                Bound::Included(lo) => cell.key >= lo.as_slice(),
                Bound::Excluded(lo) => cell.key > lo.as_slice(),
            };
            if !above_lower {
                continue;
            }
            let below_upper = match &to {
                Bound::Unbounded => true,
                Bound::Included(hi) => cell.key <= hi.as_slice(),
                Bound::Excluded(hi) => cell.key < hi.as_slice(),
            };
            if !below_upper {
                current = clustered_leaf::NULL_PAGE;
                break;
            }
            if !cell.row_header.is_visible(&snap) {
                continue;
            }

            // Decode row_data for assignment evaluation (field-patch only needs
            // the assigned columns, but eval() needs full row context).
            let values = axiomdb_types::codec::decode_row(cell.row_data, col_types)?;

            if let Some(wc) = where_clause {
                if !is_truthy(&eval(wc, &values)?) {
                    continue;
                }
            }
            matched += 1;

            // Evaluate assignments — collect only changed fields.
            let mut changed_fields = Vec::new();
            for &(col_pos, ref val_expr) in assignments {
                // Use sparse_row for eval context.
                sparse_row[col_pos] = values[col_pos].clone();
                let new_val = eval(val_expr, &values)?;
                sparse_row[col_pos] = Value::Null;
                if new_val != values[col_pos] {
                    changed_fields.push((col_pos, new_val));
                }
            }

            if changed_fields.is_empty() {
                continue;
            }

            patches.push(PatchInfo {
                idx,
                old_header: cell.row_header,
                total_row_len: cell.total_row_len,
                overflow_first_page: cell.overflow_first_page,
                key: cell.key.to_vec(),
                changed_fields,
            });
        }

        // Phase 2: Apply patches + collect compact field deltas for WAL.
        //
        // Two sub-paths based on whether the cell is inline or overflow-backed:
        //
        //   • Inline cells (fast path, InnoDB btr_cur_upd_rec_in_place model):
        //     1. Read phase  — immutable page borrow: compute field locations from
        //        the page bytes directly (no row_data clone), capture old bytes into
        //        [u8;8] stack buffers, encode new values.
        //     2. Write phase — mutable page borrow: call patch_field_in_place() and
        //        update_row_header_in_place() for direct page-buffer mutation.
        //     Result: zero heap allocations per row for the fixed-size hot path.
        //
        //   • Overflow cells (fallback, unchanged): re-read the cell on demand and
        //     call rewrite_cell_same_key_with_overflow as before. These are rare
        //     (<1% of typical workloads).
        let mut wal_patches: Vec<axiomdb_wal::ClusteredFieldPatchEntry> = Vec::new();
        let bitmap_len = col_types.len().div_ceil(8);

        for patch in &patches {
            if patch.overflow_first_page.is_some() {
                // ── Overflow fallback: re-read row_data, apply via full cell rewrite ──
                let cell = clustered_leaf::read_cell(&page, patch.idx as u16)?;
                let mut patched_data = cell.row_data.to_vec();
                let bitmap = patched_data[..bitmap_len.min(patched_data.len())].to_vec();

                let mut field_deltas: Vec<axiomdb_wal::FieldDelta> = Vec::new();
                for (col_pos, new_val) in &patch.changed_fields {
                    if let Some(loc) = axiomdb_types::field_patch::compute_field_location_runtime(
                        col_types,
                        *col_pos,
                        &bitmap,
                        Some(&patched_data),
                    ) {
                        let mut old_buf = [0u8; 8];
                        old_buf[..loc.size]
                            .copy_from_slice(&patched_data[loc.offset..loc.offset + loc.size]);
                        let new_encoded =
                            axiomdb_types::field_patch::encode_value_fixed(new_val, loc.data_type)?;
                        patched_data[loc.offset..loc.offset + loc.size]
                            .copy_from_slice(&new_encoded[..loc.size]);
                        field_deltas.push(axiomdb_wal::FieldDelta {
                            offset: loc.offset as u16,
                            size: loc.size as u8,
                            old_bytes: old_buf,
                            new_bytes: new_encoded,
                        });
                    }
                }

                let new_header = axiomdb_storage::heap::RowHeader {
                    txn_id_created: txn_id,
                    txn_id_deleted: 0,
                    row_version: patch.old_header.row_version.wrapping_add(1),
                    _flags: patch.old_header._flags,
                };

                if clustered_leaf::rewrite_cell_same_key_with_overflow(
                    &mut page,
                    patch.idx,
                    &patch.key,
                    &new_header,
                    patch.total_row_len,
                    &patched_data,
                    patch.overflow_first_page,
                )?
                .is_some()
                {
                    page_dirty = true;
                    patched += 1;
                    wal_patches.push(axiomdb_wal::ClusteredFieldPatchEntry {
                        key: patch.key.clone(),
                        old_header: patch.old_header,
                        new_header,
                        old_row_data: Vec::new(),
                        field_deltas,
                    });
                }
                continue;
            }

            // ── Inline fast path: direct page-buffer mutation ─────────────────
            //
            // Read phase: hold an immutable borrow on the page to compute field
            // locations and capture old bytes — no clone of row_data.
            let (row_data_abs_off, _key_len_in_page) =
                clustered_leaf::cell_row_data_abs_off(&page, patch.idx)?;

            // field_writes: (field_abs_off, size, old_bytes:[u8;8], new_bytes:[u8;8])
            // Built entirely from stack-allocated data — zero heap per entry.
            let (field_writes, any_real_change) = {
                let b = page.as_bytes();
                let row_slice = &b[row_data_abs_off..];
                let bitmap = &row_slice[..bitmap_len.min(row_slice.len())];

                let mut fw: Vec<(usize, usize, [u8; 8], [u8; 8])> =
                    Vec::with_capacity(patch.changed_fields.len());
                let mut changed = false;

                for (col_pos, new_val) in &patch.changed_fields {
                    let Some(loc) = axiomdb_types::field_patch::compute_field_location_runtime(
                        col_types,
                        *col_pos,
                        bitmap,
                        Some(row_slice),
                    ) else {
                        continue;
                    };

                    let new_encoded = axiomdb_types::field_patch::encode_value_fixed(
                        new_val, loc.data_type,
                    )?;
                    let field_abs = row_data_abs_off + loc.offset;

                    // Capture old bytes from the page (no clone of full row).
                    let mut old_buf = [0u8; 8];
                    old_buf[..loc.size].copy_from_slice(&b[field_abs..field_abs + loc.size]);

                    // MAYBE_NOP: if the new bytes are byte-identical to the old
                    // (e.g. SET score = score + 0.0), skip this field entirely.
                    if old_buf[..loc.size] == new_encoded[..loc.size] {
                        continue;
                    }

                    fw.push((field_abs, loc.size, old_buf, new_encoded));
                    changed = true;
                }
                (fw, changed)
            }; // immutable borrow on page dropped here

            if !any_real_change {
                continue;
            }

            let new_header = axiomdb_storage::heap::RowHeader {
                txn_id_created: txn_id,
                txn_id_deleted: 0,
                row_version: patch.old_header.row_version.wrapping_add(1),
                _flags: patch.old_header._flags,
            };

            // Write phase: mutable borrow — patch changed bytes directly in the
            // page buffer (InnoDB btr_cur_upd_rec_in_place equivalent).
            for (field_abs, size, _, new_buf) in &field_writes {
                clustered_leaf::patch_field_in_place(&mut page, *field_abs, &new_buf[..*size])?;
            }
            clustered_leaf::update_row_header_in_place(&mut page, patch.idx, &new_header)?;

            page_dirty = true;
            patched += 1;

            // Build WAL delta. FieldDelta.old_bytes/new_bytes are [u8;8] inline —
            // no Vec<u8> heap allocation per field.
            let field_deltas: Vec<axiomdb_wal::FieldDelta> = field_writes
                .iter()
                .map(|(field_abs, size, old_buf, new_buf)| axiomdb_wal::FieldDelta {
                    offset: (field_abs - row_data_abs_off) as u16,
                    size: *size as u8,
                    old_bytes: *old_buf,
                    new_bytes: *new_buf,
                })
                .collect();

            wal_patches.push(axiomdb_wal::ClusteredFieldPatchEntry {
                key: patch.key.clone(),
                old_header: patch.old_header,
                new_header,
                old_row_data: Vec::new(),
                field_deltas,
            });
        }

        if page_dirty {
            page.update_checksum();
            storage.write_page(page_id, &page)?;
        }

        // Batch WAL with compact field deltas (not full row images).
        if !wal_patches.is_empty() {
            txn.record_clustered_field_patch_batch(
                resolved.def.id,
                root_pid,
                &wal_patches,
            )?;
        }

        if current == clustered_leaf::NULL_PAGE {
            break;
        }
        if next != clustered_leaf::NULL_PAGE {
            storage.prefetch_hint(next, 4);
        }
        current = next;
        slot_idx = 0;
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

/// Three update paths:
/// 1. Non-key in-place: `update_in_place()` when PK and index keys unchanged
/// 2. Non-key relocation: `update_with_relocation()` when row grows beyond leaf
/// 3. Key change: `delete_mark()` + `insert()` when PK changes
#[allow(clippy::too_many_arguments)]
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
    use std::ops::Bound;

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

    // ── Fused clustered scan-patch fast path ─────────────────────────────
    // When ALL of these hold, skip candidate collection entirely and patch
    // fields directly on clustered leaf pages in a single pass:
    //   1. All SET cols fixed-size (field_patch eligible)
    //   2. No FK constraints
    //   3. No secondary indexes with changed key columns
    //   4. PK columns not changed
    let field_patch_ok = resolved.foreign_keys.is_empty()
        && assignments.iter().all(|(col_pos, _)| {
            axiomdb_types::field_patch::fixed_encoded_size(col_types[*col_pos]).is_some()
        });
    let pk_might_change = assignments.iter().any(|(col_pos, _)| pk_col_positions.contains(col_pos));
    let has_affected_secondary = secondary_indexes.iter().any(|idx| {
        !idx.is_primary
            && idx.columns.iter().any(|c| {
                assignments.iter().any(|(a_pos, _)| *a_pos == c.col_idx as usize)
            })
    });

    let access_method = where_clause
        .as_ref()
        .map(|wc| {
            normalize_clustered_update_access_method(crate::planner::plan_update_candidates_ctx(
                wc,
                secondary_indexes,
                schema_cols,
                ctx.effective_collation(),
            ))
        })
        .unwrap_or(crate::planner::AccessMethod::Scan);

    if field_patch_ok && !pk_might_change && !has_affected_secondary {
        let bounds = match &access_method {
            crate::planner::AccessMethod::Scan => {
                Some((Bound::Unbounded, Bound::Unbounded))
            }
            crate::planner::AccessMethod::IndexLookup { index_def, key } if index_def.is_primary => {
                Some((Bound::Included(key.clone()), Bound::Included(key.clone())))
            }
            crate::planner::AccessMethod::IndexRange { index_def, lo, hi } if index_def.is_primary => {
                Some((
                    lo.clone().map_or(Bound::Unbounded, Bound::Included),
                    hi.clone().map_or(Bound::Unbounded, Bound::Included),
                ))
            }
            _ => None,
        };

        if let Some((from, to)) = bounds {
            return fused_clustered_scan_patch(
                where_clause.as_ref(),
                &assignments,
                &col_types,
                storage,
                txn,
                snap,
                resolved,
                root_pid,
                ctx,
                from,
                to,
            );
        }
    }

    let candidates = collect_clustered_update_candidates(
        where_clause.as_ref(),
        schema_cols,
        secondary_indexes,
        &access_method,
        storage,
        snap,
        resolved,
        root_pid,
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

        // Field-patch optimization: when all changed columns are fixed-size
        // and PK is unchanged, patch bytes directly in the existing row_data
        // instead of full encode_row(). Saves ~16× per row.
        let field_patch_ok = !pk_changed
            && assignments.iter().all(|(col_pos, _)| {
                axiomdb_types::field_patch::fixed_encoded_size(col_types[*col_pos]).is_some()
            });

        let new_row_data = if field_patch_ok {
            // Patch only changed fields in a copy of the existing row_data.
            let mut patched = candidate.row_data.clone();
            let bitmap_len = col_types.len().div_ceil(8);
            let bitmap = patched[..bitmap_len].to_vec();
            for &(col_pos, _) in &assignments {
                if candidate.values[col_pos] == new_values[col_pos] {
                    continue;
                }
                if let Some(loc) =
                    axiomdb_types::field_patch::compute_field_location_runtime(
                        &col_types,
                        col_pos,
                        &bitmap,
                        Some(&patched),
                    )
                {
                    let _ = axiomdb_types::field_patch::write_field(
                        &mut patched,
                        &loc,
                        &new_values[col_pos],
                    );
                }
            }
            patched
        } else {
            axiomdb_types::codec::encode_row(&new_values, &col_types)?
        };

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
