// ── Staged INSERT flush (Phase 5.21) ──────────────────────────────────────────
//
// `flush_pending_inserts_ctx` drains the `SessionContext::pending_inserts`
// buffer and writes all staged rows to the heap + WAL + indexes in one batch.
//
// Called before any barrier statement: SELECT, UPDATE, DELETE, DDL, COMMIT,
// table switch inside consecutive INSERTs, and any ineligible INSERT shape.
// On ROLLBACK the buffer is discarded without calling this function.

fn detect_committed_empty_unique_indexes(
    storage: &mut dyn StorageEngine,
    indexes: &[IndexDef],
) -> Result<std::collections::HashSet<u32>, DbError> {
    let mut committed_empty = std::collections::HashSet::new();
    for idx in indexes {
        if idx.is_unique && !idx.is_fk_index {
            let page = storage.read_page(idx.root_page_id)?;
            let body = page.body();
            let num_keys = u16::from_le_bytes([body[2], body[3]]);
            if num_keys == 0 {
                committed_empty.insert(idx.index_id);
            }
        }
    }
    Ok(committed_empty)
}

pub(super) struct InsertBatchApply<'a> {
    pub table_def: &'a TableDef,
    pub columns: &'a [CatalogColumnDef],
    pub indexes: &'a mut [IndexDef],
    pub rows: &'a [Vec<Value>],
    pub compiled_preds: &'a [Option<Expr>],
    pub skip_unique_check: bool,
    pub committed_empty: &'a std::collections::HashSet<u32>,
}

fn persist_batch_insert_indexes(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    plan: &mut InsertBatchApply<'_>,
    rids: &[RecordId],
) -> Result<bool, DbError> {
    if plan.indexes.is_empty() {
        return Ok(false);
    }

    let snap = txn.active_snapshot()?;
    let updated = crate::index_maintenance::batch_insert_into_indexes(
        plan.indexes,
        plan.rows,
        rids,
        storage,
        bloom,
        plan.compiled_preds,
        plan.skip_unique_check,
        plan.committed_empty,
        snap,
    )?;
    for (index_id, new_root) in &updated {
        CatalogWriter::new(storage, txn)?.update_index_root(*index_id, *new_root)?;
    }

    // Record UndoIndexInsert for each (row, index) so ROLLBACK can clean B-Trees.
    for idx in plan.indexes.iter() {
        if idx.columns.is_empty() {
            continue;
        }
        for (row, rid) in plan.rows.iter().zip(rids.iter()) {
            let key_vals: Vec<axiomdb_types::Value> = idx
                .columns
                .iter()
                .map(|c| row.get(c.col_idx as usize).cloned().unwrap_or(axiomdb_types::Value::Null))
                .collect();
            if key_vals.iter().any(|v| matches!(v, axiomdb_types::Value::Null)) {
                continue;
            }
            let key = if idx.is_fk_index || !idx.is_unique {
                let mut k = crate::key_encoding::encode_index_key(&key_vals)?;
                k.extend_from_slice(&axiomdb_index::page_layout::encode_rid(*rid));
                k
            } else {
                crate::key_encoding::encode_index_key(&key_vals)?
            };
            let _ = txn.record_index_insert(idx.index_id, idx.root_page_id, key);
        }
    }

    Ok(!updated.is_empty())
}

pub(super) fn apply_insert_batch(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    mut plan: InsertBatchApply<'_>,
) -> Result<Vec<RecordId>, DbError> {
    let rids = TableEngine::insert_rows_batch(storage, txn, plan.table_def, plan.columns, plan.rows)?;
    let _ = persist_batch_insert_indexes(storage, txn, bloom, &mut plan, &rids)?;
    Ok(rids)
}

pub(super) fn apply_insert_batch_with_ctx(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
    mut plan: InsertBatchApply<'_>,
) -> Result<Vec<RecordId>, DbError> {
    let rids =
        TableEngine::insert_rows_batch_with_ctx(storage, txn, plan.table_def, plan.columns, ctx, plan.rows)?;
    let roots_changed = persist_batch_insert_indexes(storage, txn, bloom, &mut plan, &rids)?;
    if roots_changed {
        ctx.invalidate_all();
    }
    Ok(rids)
}

/// Flushes the staged INSERT buffer to heap, WAL, and indexes in one batch.
///
/// No-op if there is no pending batch. On return `ctx.pending_inserts` is `None`.
///
/// ## Flush sequence
/// 1. Batch-insert all rows into the heap via `insert_rows_batch_with_ctx`.
/// 2. Insert all (row, rid) pairs into every secondary index via
///    `batch_insert_into_indexes`, tracking root changes across splits.
/// 3. Persist changed index roots once per index (not once per row).
/// 4. Update the stats tracker.
///
/// ## Error handling
/// Any error from step 1–3 is returned to the caller as-is. The pending
/// batch is cleared **only on success**; on error the batch is also cleared
/// since the caller (COMMIT/barrier) will propagate the error and the
/// transaction state is now inconsistent.
pub(super) fn flush_pending_inserts_ctx(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<(), DbError> {
    let batch = match ctx.pending_inserts.take() {
        Some(b) => b,
        None => return Ok(()),
    };

    if batch.rows.is_empty() {
        return Ok(());
    }

    let mut indexes = batch.indexes;
    let rids = apply_insert_batch_with_ctx(
        storage,
        txn,
        bloom,
        ctx,
        InsertBatchApply {
            table_def: &batch.table_def,
            columns: &batch.columns,
            indexes: &mut indexes,
            rows: &batch.rows,
            compiled_preds: &batch.compiled_preds,
            skip_unique_check: true, // pre-verified at enqueue time
            committed_empty: &batch.committed_empty,
        },
    )?;

    // ── Phase 4: stats ────────────────────────────────────────────────────────
    ctx.stats
        .on_rows_changed(batch.table_def.id, rids.len() as u64);

    Ok(())
}

// ── Clustered INSERT batch flush (Phase 40.1) ─────────────────────────────────

/// Flushes the staged clustered INSERT buffer to the B-tree, WAL, and secondary
/// indexes in one sorted batch call.
///
/// No-op if there is no pending clustered batch. On return
/// `ctx.clustered_insert_batch` is `None`.
///
/// ## Flush sequence
/// 1. Sort staged rows ascending by primary-key bytes (enables rightmost-leaf
///    fast path inside `apply_clustered_insert_rows`).
/// 2. Convert `StagedClusteredRow` → `PreparedClusteredInsertRow` (zero-copy
///    field move; the two types are structurally identical).
/// 3. Delegate to `apply_clustered_insert_rows` which handles the
///    `try_insert_rightmost_leaf_batch` fast path, fallback single inserts, WAL
///    recording, secondary-index maintenance, and root persistence.
/// 4. Update the stats tracker and invalidate the schema cache.
pub(super) fn flush_clustered_insert_batch(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<(), DbError> {
    let batch = match ctx.clustered_insert_batch.take() {
        Some(b) => b,
        None => return Ok(()),
    };

    if batch.rows.is_empty() {
        return Ok(());
    }

    // Sort ascending by PK so `apply_clustered_insert_rows` detects the
    // append-biased pattern and uses `try_insert_rightmost_leaf_batch`.
    let mut rows = batch.rows;
    rows.sort_unstable_by(|a, b| a.primary_key_bytes.cmp(&b.primary_key_bytes));

    // Convert StagedClusteredRow → PreparedClusteredInsertRow (same fields).
    let prepared: Vec<crate::clustered_table::PreparedClusteredInsertRow> = rows
        .into_iter()
        .map(|r| crate::clustered_table::PreparedClusteredInsertRow {
            values: r.values,
            encoded_row: r.encoded_row,
            primary_key_values: r.primary_key_values,
            primary_key_bytes: r.primary_key_bytes,
        })
        .collect();

    let mut secondary_indexes = batch.secondary_indexes;
    let count = apply_clustered_insert_rows(
        storage,
        txn,
        bloom,
        &batch.table_def,
        &batch.primary_idx,
        &mut secondary_indexes,
        &batch.secondary_layouts,
        &batch.compiled_preds,
        &prepared,
    )?;

    ctx.stats.on_rows_changed(batch.table_id, count);
    ctx.invalidate_all();

    Ok(())
}
