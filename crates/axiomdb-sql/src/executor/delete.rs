fn execute_delete_ctx(
    stmt: DeleteStmt,
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
    let secondary_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();

    let snap = txn.active_snapshot()?;

    // ── Clustered table DELETE dispatch (Phase 39.17) ────────────────────
    if resolved.def.is_clustered() {
        return execute_clustered_delete(
            stmt.where_clause,
            &resolved.columns,
            &secondary_indexes,
            storage,
            txn,
            snap,
            &resolved,
            bloom,
            ctx,
        );
    }

    // Check if any FK constraint references THIS table as the parent.
    // If so, we must scan rows (to get parent key values) and cannot use the fast path.
    let has_fk_references = {
        let mut reader = CatalogReader::new(storage, snap)?;
        !reader
            .list_fk_constraints_referencing(resolved.def.id)?
            .is_empty()
    };

    // No-WHERE + no FK parent references → bulk-empty fast path (Phase 5.16).
    // This replaces the old "no secondary indexes" gate: PK + UNIQUE + composite
    // indexes are all handled by root rotation, not per-row B-Tree deletes.
    if stmt.where_clause.is_none() && !has_fk_references {
        // Collect all indexes with columns (PK, UNIQUE, non-unique, FK auto-indexes).
        let all_indexes: Vec<axiomdb_catalog::IndexDef> = resolved
            .indexes
            .iter()
            .filter(|i| !i.columns.is_empty())
            .cloned()
            .collect();

        let plan = plan_bulk_empty_table(storage, &resolved.def, &all_indexes, snap)?;
        let count = plan.visible_row_count;

        apply_bulk_empty_table(storage, txn, bloom, &resolved.def, plan)?;

        // Invalidate session schema cache so the next query reloads the new roots.
        ctx.invalidate_all();

        // Release deferred pages now if we're in immediate-commit mode.
        // In group-commit mode this is handled by the CommitCoordinator.
        // We use a best-effort release here; group-commit path does not hold
        // an active txn at this point, so active_txn_id() == None.
        if let Some(committed_txn_id) = txn.active_txn_id() {
            // Still inside an explicit transaction — pages freed at outer COMMIT.
            let _ = committed_txn_id; // suppress unused warning
        }

        return Ok(QueryResult::Affected {
            count,
            last_insert_id: None,
        });
    }

    // Candidate discovery (Phase 6.3b): use index when predicate is sargable.
    let schema_cols = resolved.columns.clone();
    let to_delete: Vec<(RecordId, Vec<Value>)> = if let Some(ref wc) = stmt.where_clause {
        let effective_coll = ctx.effective_collation();
        let delete_access = crate::planner::plan_delete_candidates_ctx(
            wc,
            &secondary_indexes,
            &schema_cols,
            effective_coll,
        );
        collect_delete_candidates(
            wc,
            &secondary_indexes,
            &schema_cols,
            &delete_access,
            storage,
            snap,
            &resolved.def,
            bloom,
        )?
    } else {
        // No WHERE and has_fk_references=true (bulk-empty path already returned
        // for the no-WHERE + no-FK case). Full scan: all rows qualify.
        TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?
    };

    // FK parent enforcement: must run BEFORE heap delete so RESTRICT can abort
    // cleanly and CASCADE/SET NULL can still read/update child rows.
    if has_fk_references && !to_delete.is_empty() {
        crate::fk_enforcement::enforce_fk_on_parent_delete(
            &to_delete,
            resolved.def.id,
            storage,
            txn,
            bloom,
            0,
        )?;
    }

    // Batch-delete from heap: each page read+written once instead of 3× per row.
    let rids_only: Vec<RecordId> = to_delete.iter().map(|(rid, _)| *rid).collect();
    let count = TableEngine::delete_rows_batch(storage, txn, &resolved.def, &rids_only)?;

    // MVCC deferred index deletion (PostgreSQL model): ALL index entries are left
    // in place during DELETE — PK, UNIQUE, FK auto-indexes, and non-unique alike.
    // Dead entries are filtered by heap visibility checks on read (7.3b).
    // VACUUM (7.11) cleans dead entries from all index types.
    //
    // Why safe for PK/UNIQUE: has_visible_duplicate() checks heap visibility
    // before raising UniqueViolation, so INSERT after DELETE of same key works.
    //
    // Why safe for FK auto-indexes: FK enforcement now checks heap visibility
    // (is_slot_visible) before raising ForeignKeyParentViolation.

    // Track row changes for stats staleness (Phase 6.11).
    if count > 0 {
        ctx.stats.on_rows_changed(resolved.def.id, count);
    }

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

/// Clustered table DELETE: collects candidates from the clustered B-tree,
/// then applies MVCC delete-mark to each via `clustered_tree::delete_mark()`.
/// Physical cell removal deferred to VACUUM (39.18).
#[allow(clippy::too_many_arguments)]
fn execute_clustered_delete(
    where_clause: Option<Expr>,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    secondary_indexes: &[axiomdb_catalog::IndexDef],
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    snap: axiomdb_core::TransactionSnapshot,
    resolved: &axiomdb_catalog::ResolvedTable,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let root_pid = txn
        .clustered_root(resolved.def.id)
        .unwrap_or(resolved.def.root_page_id);

    let has_fk_references = {
        let mut reader = CatalogReader::new(storage, snap)?;
        !reader
            .list_fk_constraints_referencing(resolved.def.id)?
            .is_empty()
    };

    // ── Fast path: DELETE without WHERE on clustered table ────────────────
    // Walk leaf chain directly, mark ALL visible cells as deleted.
    // Zero row decode — only reads RowHeader (24 bytes per cell).
    if where_clause.is_none() && !has_fk_references {
        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        let count = delete_mark_all_clustered_leaves(storage, txn, resolved.def.id, root_pid, txn_id, snap)?;
        if count > 0 {
            ctx.stats.on_rows_changed(resolved.def.id, count);
        }
        ctx.invalidate_all();
        return Ok(QueryResult::Affected {
            count,
            last_insert_id: None,
        });
    }

    let candidates = collect_clustered_delete_candidates(
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

    if has_fk_references {
        let parent_rows: Vec<(RecordId, Vec<Value>)> = candidates
            .iter()
            .map(|candidate| {
                (
                    RecordId {
                        page_id: 0,
                        slot_id: 0,
                    },
                    candidate.values.clone(),
                )
            })
            .collect();
        crate::fk_enforcement::enforce_fk_on_parent_delete(
            &parent_rows,
            resolved.def.id,
            storage,
            txn,
            bloom,
            0,
        )?;
    }

    let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
    let mut count = 0u64;

    // InnoDB-inspired batch delete: candidates arrive in PK order from the
    // clustered scan. Group consecutive candidates on the same leaf page.
    // Uses patch_txn_id_deleted (8-byte direct write) instead of full cell rewrite,
    // and lightweight WAL (no row_data images) + UndoClusteredUndelete.
    let mut i = 0;
    while i < candidates.len() {
        let candidate = &candidates[i];

        // Descend to the leaf page for this candidate's key.
        let leaf_ref =
            axiomdb_storage::clustered_tree::descend_to_leaf_pub(storage, root_pid, &candidate.pk_key)?;
        let page_id = leaf_ref.header().page_id;
        let mut page = leaf_ref.into_page();
        let mut page_dirty = false;
        let mut patched_keys: Vec<Vec<u8>> = Vec::new();

        // Process all consecutive candidates that live on this same leaf page.
        while i < candidates.len() {
            let c = &candidates[i];
            let pos = match axiomdb_storage::clustered_leaf::search(&page, &c.pk_key) {
                Ok(pos) => pos,
                Err(_) => {
                    // Key not on this page — candidate is on a different leaf.
                    break;
                }
            };

            // Verify the cell is still visible (not already deleted by another op).
            // Only read header — no row_data allocation needed for delete-mark.
            let cell = axiomdb_storage::clustered_leaf::read_cell(&page, pos as u16)?;
            if !cell.row_header.is_visible(&snap) {
                i += 1;
                continue;
            }

            // Direct 8-byte patch: modify ONLY txn_id_deleted in the cell header.
            // InnoDB uses a single-byte delete flag; we patch 8 bytes (txn_id).
            // No full cell rewrite, no row_data copy.
            let _old_deleted = axiomdb_storage::clustered_leaf::patch_txn_id_deleted(
                &mut page, pos, txn_id,
            )?;
            page_dirty = true;
            count += 1;
            patched_keys.push(c.pk_key.clone());
            i += 1;
        }

        // Single page write for all marks on this leaf.
        if page_dirty {
            page.update_checksum();
            storage.write_page(page_id, &page)?;
        }

        // Lightweight batch WAL + undo for all patched cells on this leaf.
        if !patched_keys.is_empty() {
            txn.record_clustered_delete_mark_lightweight(resolved.def.id, root_pid, &patched_keys)?;
        }
    }

    // Secondary index entries left in place — MVCC deferred cleanup.
    // Dead entries filtered by visibility checks on read.
    // VACUUM (39.18) will physically clean them.

    if count > 0 {
        ctx.stats.on_rows_changed(resolved.def.id, count);
    }
    ctx.invalidate_all();

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

#[derive(Debug, Clone)]
struct ClusteredDeleteCandidate {
    pk_key: Vec<u8>,
    /// Decoded row values — needed only for FK enforcement (parent-key checks).
    /// Not used for the actual delete-mark path (which patches 8 bytes directly).
    values: Vec<Value>,
}

fn normalize_clustered_delete_access_method(
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

fn clustered_delete_primary_index(
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

fn clustered_delete_secondary_high_bound(logical_key: &[u8]) -> Vec<u8> {
    let mut hi = logical_key.to_vec();
    if hi.len() < crate::key_encoding::MAX_INDEX_KEY {
        hi.resize(crate::key_encoding::MAX_INDEX_KEY, 0xFF);
    }
    hi
}

fn clustered_rows_for_secondary_delete_access(
    storage: &dyn StorageEngine,
    root_pid: u64,
    resolved: &axiomdb_catalog::ResolvedTable,
    index_def: &axiomdb_catalog::IndexDef,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    snap: axiomdb_core::TransactionSnapshot,
) -> Result<Vec<axiomdb_storage::clustered_tree::ClusteredRow>, DbError> {
    let primary_idx = clustered_delete_primary_index(resolved)?;
    let layout =
        crate::clustered_secondary::ClusteredSecondaryLayout::derive(index_def, primary_idx)?;
    let hi_owned = hi.map(clustered_delete_secondary_high_bound);
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

/// Fast-path DELETE without WHERE: walks the clustered leaf chain and marks
/// ALL visible cells as deleted. Zero row decode — only modifies RowHeader.
/// One page read+write per leaf page (batch).
fn delete_mark_all_clustered_leaves(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_id: u32,
    root_pid: u64,
    txn_id: u64,
    snap: axiomdb_core::TransactionSnapshot,
) -> Result<u64, DbError> {
    use axiomdb_storage::{clustered_internal, clustered_leaf, page::PageType};

    // Descend to leftmost leaf.
    let mut pid = root_pid;
    loop {
        let page = storage.read_page(pid)?;
        let pt = PageType::try_from(page.header().page_type)
            .map_err(|e| DbError::Other(format!("{e}")))?;
        match pt {
            PageType::ClusteredLeaf => break,
            PageType::ClusteredInternal => {
                pid = clustered_internal::child_at(&page, 0)?;
            }
            _ => {
                return Err(DbError::BTreeCorrupted {
                    msg: format!("delete_mark_all: unexpected page type {pt:?} at {pid}"),
                });
            }
        }
    }

    let mut count = 0u64;
    let mut current = pid;

    while current != clustered_leaf::NULL_PAGE {
        let mut page = storage.read_page(current)?.into_page();
        let next = clustered_leaf::next_leaf(&page);
        let n = clustered_leaf::num_cells(&page) as usize;
        let mut page_dirty = false;

        // Phase 1: Identify cells to mark (indices only — minimal allocation).
        let mut mark_indices: Vec<usize> = Vec::new();
        for idx in 0..n {
            let cell = clustered_leaf::read_cell(&page, idx as u16)?;
            if !cell.row_header.is_visible(&snap) {
                continue;
            }
            if cell.row_header.txn_id_deleted != 0 {
                continue;
            }
            mark_indices.push(idx);
        }

        // Phase 2: Direct header patch — InnoDB-inspired: modify ONLY 8 bytes
        // (txn_id_deleted) per cell. No cell copy, no full row image allocation.
        // Undo uses lightweight UndoClusteredUndelete (only PK key, no row_data).
        let mut patched_keys: Vec<Vec<u8>> = Vec::new();

        for &idx in &mark_indices {
            // Read key BEFORE patch (need it for WAL + undo).
            let cell = clustered_leaf::read_cell(&page, idx as u16)?;
            let pk_key = cell.key.to_vec(); // Only allocation: PK key (~8 bytes)

            // Patch txn_id_deleted directly on page buffer (8 bytes).
            let _old_deleted = clustered_leaf::patch_txn_id_deleted(
                &mut page, idx, txn_id,
            )?;
            page_dirty = true;
            count += 1;
            patched_keys.push(pk_key);
        }

        if page_dirty {
            page.update_checksum();
            storage.write_page(current, &page)?;
        }

        // Lightweight WAL + undo: minimal images (empty row_data) + UndoClusteredUndelete.
        // This reduces per-row WAL from ~150 bytes to ~70 bytes (header-only images)
        // and undo from ~100 bytes (full row clone) to ~16 bytes (PK key only).
        if !patched_keys.is_empty() {
            txn.record_clustered_delete_mark_lightweight(table_id, root_pid, &patched_keys)?;
        }
        current = next;
    }

    Ok(count)
}

fn collect_clustered_delete_candidates(
    where_clause: Option<&Expr>,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    secondary_indexes: &[axiomdb_catalog::IndexDef],
    storage: &dyn StorageEngine,
    snap: axiomdb_core::TransactionSnapshot,
    resolved: &axiomdb_catalog::ResolvedTable,
    root_pid: u64,
    effective_collation: crate::session::SessionCollation,
) -> Result<Vec<ClusteredDeleteCandidate>, DbError> {
    use std::ops::Bound;

    let col_types: Vec<axiomdb_types::DataType> = schema_cols
        .iter()
        .map(|c| crate::table::column_type_to_data_type(c.col_type))
        .collect();

    let access_method = where_clause
        .map(|wc| {
            normalize_clustered_delete_access_method(crate::planner::plan_delete_candidates_ctx(
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
            let hi = clustered_delete_secondary_high_bound(&key);
            raw_rows.extend(clustered_rows_for_secondary_delete_access(
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
            raw_rows.extend(clustered_rows_for_secondary_delete_access(
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
        candidates.push(ClusteredDeleteCandidate {
            pk_key: row.key,
            values,
        });
    }

    Ok(candidates)
}

// ── Column mask for lazy decode ───────────────────────────────────────────────

/// Builds a boolean mask over `n_cols` columns. `mask[i]` is `true` if column
/// `i` is referenced by any expression in `exprs`. Used by `execute_select_ctx`
/// and `execute_delete_ctx` to tell `scan_table` which columns to decode.
///
/// Conservative: any [`SelectItem::Wildcard`] or [`SelectItem::QualifiedWildcard`]
/// in the query's SELECT list will cause the caller to pass `None` instead (full
/// decode), so this function is only called when the select list is fully
/// resolved to column expressions.
/// Collect column indices referenced in an expression into a boolean mask.
/// Used to build the two-phase decode mask: only decode WHERE columns first.
fn collect_where_columns(e: &Expr, mask: &mut [bool]) {
    match e {
        Expr::Column { col_idx, .. } => {
            if *col_idx < mask.len() {
                mask[*col_idx] = true;
            }
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_where_columns(left, mask);
            collect_where_columns(right, mask);
        }
        Expr::UnaryOp { operand, .. } => {
            collect_where_columns(operand, mask);
        }
        Expr::Function { args, .. } => {
            for arg in args {
                collect_where_columns(arg, mask);
            }
        }
        _ => {}
    }
}

fn collect_delete_candidates(
    where_clause: &Expr,
    _indexes: &[axiomdb_catalog::IndexDef],
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    access: &crate::planner::AccessMethod,
    storage: &mut dyn StorageEngine,
    snap: axiomdb_core::TransactionSnapshot,
    table_def: &axiomdb_catalog::TableDef,
    bloom: &crate::bloom::BloomRegistry,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    use crate::planner::AccessMethod;

    match access {
        AccessMethod::Scan | AccessMethod::IndexOnlyScan { .. } => {
            // Optimized scan: compile a BatchPredicate for zero-alloc raw-byte
            // predicate evaluation, zone map page skipping, and two-phase decode
            // (only decode WHERE columns first, full decode only for matching rows).
            // Mirrors the SELECT path in select.rs for parity.
            let col_types: Vec<axiomdb_types::DataType> = schema_cols
                .iter()
                .map(|c| crate::table::column_type_to_data_type(c.col_type))
                .collect();
            let batch_pred = crate::eval::batch::try_compile(where_clause, &col_types);
            let zm_pred =
                crate::planner::extract_zone_map_predicate(where_clause, schema_cols);

            // Build WHERE column mask for two-phase decode: only decode columns
            // referenced in the WHERE clause first, skip full decode for non-matching rows.
            let n_cols = schema_cols.len();
            let where_col_mask = {
                let mut mask = vec![false; n_cols];
                collect_where_columns(where_clause, &mut mask);
                if mask.iter().filter(|&&b| b).count() < n_cols {
                    Some(mask)
                } else {
                    None
                }
            };

            TableEngine::scan_table_filtered(
                storage,
                table_def,
                schema_cols,
                snap,
                |values| match eval(where_clause, values) {
                    Ok(v) => is_truthy(&v),
                    Err(_) => true, // include on error, let caller handle
                },
                zm_pred.as_ref().map(|(ci, p)| (*ci, p)),
                where_col_mask.as_deref(),
                batch_pred.as_ref(),
            )
        }

        AccessMethod::IndexLookup { index_def, key } => {
            // Point lookup via B-Tree → batch heap read → WHERE recheck.
            let candidate_rids: Vec<RecordId> = if index_def.is_unique {
                if index_def.is_unique && !bloom.might_exist(index_def.index_id, key) {
                    vec![]
                } else {
                    match BTree::lookup_in(storage, index_def.root_page_id, key)? {
                        None => vec![],
                        Some(rid) => vec![rid],
                    }
                }
            } else {
                // Non-unique: key||RID format — range [key||0..0, key||FF..FF].
                let lo = rid_lo(key);
                let hi = rid_hi(key);
                BTree::range_in(storage, index_def.root_page_id, Some(&lo), Some(&hi))?
                    .into_iter()
                    .map(|(rid, _)| rid)
                    .collect()
            };

            let batch_rows =
                TableEngine::read_rows_batch(storage, schema_cols, &candidate_rids)?;
            let mut result = Vec::with_capacity(candidate_rids.len());
            for (rid, maybe_values) in candidate_rids.into_iter().zip(batch_rows) {
                if let Some(values) = maybe_values {
                    if is_truthy(&eval(where_clause, &values)?) {
                        result.push((rid, values));
                    }
                }
            }
            Ok(result)
        }

        AccessMethod::IndexRange { index_def, lo, hi } => {
            // Range scan via B-Tree → batch heap read → WHERE recheck.
            let (lo_adj, hi_adj);
            let (lo_ref, hi_ref) = if index_def.is_unique {
                (lo.as_deref(), hi.as_deref())
            } else {
                lo_adj = lo.as_deref().map(rid_lo);
                hi_adj = hi.as_deref().map(rid_hi);
                (lo_adj.as_deref(), hi_adj.as_deref())
            };
            let pairs = BTree::range_in(storage, index_def.root_page_id, lo_ref, hi_ref)?;

            let candidate_rids: Vec<RecordId> = pairs.into_iter().map(|(rid, _)| rid).collect();
            let batch_rows =
                TableEngine::read_rows_batch(storage, schema_cols, &candidate_rids)?;
            let mut result = Vec::with_capacity(candidate_rids.len());
            for (rid, maybe_values) in candidate_rids.into_iter().zip(batch_rows) {
                if let Some(values) = maybe_values {
                    if is_truthy(&eval(where_clause, &values)?) {
                        result.push((rid, values));
                    }
                }
            }
            Ok(result)
        }
    }
}

fn execute_delete(
    stmt: DeleteStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let resolved = {
        let mut resolver = make_resolver(storage, txn)?;
        resolver.resolve_table(stmt.table.schema.as_deref(), &stmt.table.name)?
    };
    let schema_cols = resolved.columns.clone();
    // Use the already-loaded indexes from the resolved table (cached by SchemaCache).
    // Must be `mut` so we can keep root_page_id in sync as rows are deleted.
    let secondary_indexes: Vec<IndexDef> = resolved
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
        return execute_clustered_delete(
            stmt.where_clause,
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

    // Check if any FK constraint references THIS table as the parent.
    // If so, fall through to the row-by-row path so RESTRICT/CASCADE still fires.
    let has_fk_references = {
        let mut reader = CatalogReader::new(storage, snap)?;
        !reader
            .list_fk_constraints_referencing(resolved.def.id)?
            .is_empty()
    };

    // No-WHERE + no parent-FK references → bulk-empty fast path (Phase 5.16).
    if stmt.where_clause.is_none() && !has_fk_references {
        let plan = plan_bulk_empty_table(storage, &resolved.def, &secondary_indexes, snap)?;
        let count = plan.visible_row_count;
        apply_bulk_empty_table(storage, txn, &mut noop_bloom, &resolved.def, plan)?;
        return Ok(QueryResult::Affected {
            count,
            last_insert_id: None,
        });
    }

    // Candidate discovery (Phase 6.3b): index path when predicate is sargable.
    let to_delete: Vec<(RecordId, Vec<Value>)> = if let Some(ref wc) = stmt.where_clause {
        let delete_access =
            crate::planner::plan_delete_candidates(wc, &secondary_indexes, &schema_cols);
        collect_delete_candidates(
            wc,
            &secondary_indexes,
            &schema_cols,
            &delete_access,
            storage,
            snap,
            &resolved.def,
            &noop_bloom,
        )?
    } else {
        // No WHERE + has_fk_references=true — full scan, all rows qualify.
        TableEngine::scan_table(storage, &resolved.def, &schema_cols, snap, None)?
    };

    // Batch-delete from heap: each page read+written once instead of 3× per row.
    let rids_only: Vec<RecordId> = to_delete.iter().map(|(rid, _)| *rid).collect();
    let count = TableEngine::delete_rows_batch(storage, txn, &resolved.def, &rids_only)?;

    // MVCC deferred index deletion (PostgreSQL model): all index entries left in
    // place. Dead entries filtered by heap visibility. VACUUM cleans later.

    Ok(QueryResult::Affected {
        count,
        last_insert_id: None,
    })
}

// ── CREATE TABLE ─────────────────────────────────────────────────────────────
