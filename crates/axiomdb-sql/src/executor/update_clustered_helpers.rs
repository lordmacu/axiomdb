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
    let layout =
        crate::clustered_secondary::ClusteredSecondaryLayout::derive(index_def, primary_idx)?;
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
        crate::planner::AccessMethod::IndexLookup { index_def, key, .. } if index_def.is_primary => {
            if let Some(row) =
                axiomdb_storage::clustered_tree::lookup(storage, Some(root_pid), &key[..], &snap)?
            {
                raw_rows.push(row);
            }
        }
        crate::planner::AccessMethod::IndexLookup { index_def, key, .. } => {
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
        crate::planner::AccessMethod::IndexRange { index_def, lo, hi, .. } if index_def.is_primary => {
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
        crate::planner::AccessMethod::IndexRange { index_def, lo, hi, .. } => {
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
        crate::planner::AccessMethod::IndexOnlyScan { .. }
        | crate::planner::AccessMethod::GinScan { .. } => unreachable!(),
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

#[expect(
    clippy::too_many_arguments,
    reason = "clustered secondary update needs both old/new rows and executor state"
)]
fn apply_clustered_secondary_update(
    idx: &axiomdb_catalog::IndexDef,
    compiled_pred: Option<&crate::expr::Expr>,
    layout: &crate::clustered_secondary::ClusteredSecondaryLayout,
    sec_root: &std::sync::atomic::AtomicU64,
    table_root_page_id: u64,
    old_values: &[Value],
    new_values: &[Value],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot(conn_txn);

    if idx.index_type == 4 {
        let old_terms =
            crate::index_maintenance::gin_terms_if_indexed(idx, old_values, compiled_pred)?;
        let new_terms =
            crate::index_maintenance::gin_terms_if_indexed(idx, new_values, compiled_pred)?;

        if old_terms.is_none() && new_terms.is_none() {
            return Ok(());
        }

        let old_pk_key = old_terms
            .as_ref()
            .map(|_| {
                crate::index_maintenance::encode_clustered_pk_key_from_row(
                    &idx.name,
                    &layout.primary_cols,
                    old_values,
                )
            })
            .transpose()?;
        let new_pk_key = new_terms
            .as_ref()
            .map(|_| {
                crate::index_maintenance::encode_clustered_pk_key_from_row(
                    &idx.name,
                    &layout.primary_cols,
                    new_values,
                )
            })
            .transpose()?;

        if old_terms == new_terms && old_pk_key == new_pk_key {
            return Ok(());
        }

        if let (Some(terms), Some(pk_key)) = (old_terms.as_ref(), old_pk_key.as_ref()) {
            for term in terms {
                let key = crate::index_maintenance::gin_clustered_key(term, pk_key);
                let _ = BTree::delete_in(storage, sec_root, &key)?;
                txn.record_index_delete(
                    conn_txn,
                    idx.index_id,
                    sec_root.load(std::sync::atomic::Ordering::Acquire),
                    key,
                    crate::index_maintenance::GIN_CLUSTERED_DUMMY_RID,
                    idx.fillfactor,
                );
            }
        }

        if let (Some(terms), Some(pk_key)) = (new_terms.as_ref(), new_pk_key.as_ref()) {
            for term in terms {
                let key = crate::index_maintenance::gin_clustered_key(term, pk_key);
                // Clear any stale entry left by a previous MVCC-deferred delete at
                // this same PK before re-inserting. A new-only term whose key was
                // left behind by an earlier DELETE (entries are kept for VACUUM)
                // would otherwise collide and return DuplicateKey, since the
                // clustered GIN key [term][0x00][pk_key] carries no row version.
                let _ = BTree::delete_in(storage, sec_root, &key)?;
                BTree::insert_in(
                    storage,
                    sec_root,
                    &key,
                    crate::index_maintenance::GIN_CLUSTERED_DUMMY_RID,
                    idx.fillfactor,
                )?;
                txn.record_index_insert(
                    conn_txn,
                    idx.index_id,
                    sec_root.load(std::sync::atomic::Ordering::Acquire),
                    key,
                );
            }
        }

        return Ok(());
    }

    let old_indexed =
        crate::index_maintenance::index_key_values_if_indexed(idx, old_values, compiled_pred)?
            .is_some();
    let new_indexed =
        crate::index_maintenance::index_key_values_if_indexed(idx, new_values, compiled_pred)?
            .is_some();
    let old_entry = old_indexed
        .then(|| layout.entry_from_row(old_values))
        .transpose()?
        .flatten();
    let new_entry = new_indexed
        .then(|| layout.entry_from_row(new_values))
        .transpose()?
        .flatten();
    let outcome = match (old_indexed, new_indexed) {
        (false, false) => crate::clustered_secondary::ClusteredSecondaryUpdateOutcome::Unchanged,
        (false, true) => {
            layout.insert_row_visible(storage, sec_root, table_root_page_id, &snap, new_values)?;
            crate::clustered_secondary::ClusteredSecondaryUpdateOutcome::Inserted
        }
        (true, false) => {
            let _ = layout.delete_row(storage, sec_root, old_values)?;
            crate::clustered_secondary::ClusteredSecondaryUpdateOutcome::Deleted
        }
        (true, true) => layout.update_row_visible(
            storage,
            sec_root,
            table_root_page_id,
            &snap,
            old_values,
            new_values,
        )?,
    };
    let current_root = sec_root.load(std::sync::atomic::Ordering::Acquire);

    match outcome {
        crate::clustered_secondary::ClusteredSecondaryUpdateOutcome::Unchanged => {}
        crate::clustered_secondary::ClusteredSecondaryUpdateOutcome::Inserted => {
            if let Some(new_entry) = new_entry {
                bloom.add(idx.index_id, &new_entry.physical_key);
                txn.record_index_insert(
                    conn_txn,
                    idx.index_id,
                    current_root,
                    new_entry.physical_key,
                );
            }
        }
        crate::clustered_secondary::ClusteredSecondaryUpdateOutcome::Deleted => {
            if let Some(old_entry) = old_entry {
                txn.record_index_delete(
                    conn_txn,
                    idx.index_id,
                    current_root,
                    old_entry.physical_key,
                    RecordId {
                        page_id: 0,
                        slot_id: 0,
                    },
                    idx.fillfactor,
                );
            }
        }
        crate::clustered_secondary::ClusteredSecondaryUpdateOutcome::Replaced => {
            if let Some(old_entry) = old_entry {
                txn.record_index_delete(
                    conn_txn,
                    idx.index_id,
                    current_root,
                    old_entry.physical_key,
                    RecordId {
                        page_id: 0,
                        slot_id: 0,
                    },
                    idx.fillfactor,
                );
            }
            if let Some(new_entry) = new_entry {
                bloom.add(idx.index_id, &new_entry.physical_key);
                txn.record_index_insert(
                    conn_txn,
                    idx.index_id,
                    current_root,
                    new_entry.physical_key,
                );
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
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    snap: axiomdb_core::TransactionSnapshot,
    resolved: &axiomdb_catalog::ResolvedTable,
    root_pid: u64,
    ctx: &mut SessionContext,
    from: std::ops::Bound<Vec<u8>>,
    to: std::ops::Bound<Vec<u8>>,
) -> Result<QueryResult, DbError> {
    use std::ops::Bound;

    use axiomdb_storage::{clustered_internal, clustered_leaf, page::PageType};

    let txn_id = conn_txn.txn_id;
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
                            msg: format!("fused_clustered_scan_patch: unexpected page type {pt:?}"),
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
    let batch_pred = where_clause.and_then(|wc| crate::eval::batch::try_compile(wc, col_types));

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

                    let new_encoded =
                        axiomdb_types::field_patch::encode_value_fixed(new_val, loc.data_type)?;
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
                .map(
                    |(field_abs, size, old_buf, new_buf)| axiomdb_wal::FieldDelta {
                        offset: (field_abs - row_data_abs_off) as u16,
                        size: *size as u8,
                        old_bytes: *old_buf,
                        new_bytes: *new_buf,
                    },
                )
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
                conn_txn,
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
