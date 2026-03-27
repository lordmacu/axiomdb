// ── Staged INSERT flush (Phase 5.21) ──────────────────────────────────────────
//
// `flush_pending_inserts_ctx` drains the `SessionContext::pending_inserts`
// buffer and writes all staged rows to the heap + WAL + indexes in one batch.
//
// Called before any barrier statement: SELECT, UPDATE, DELETE, DDL, COMMIT,
// table switch inside consecutive INSERTs, and any ineligible INSERT shape.
// On ROLLBACK the buffer is discarded without calling this function.

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

    // ── Phase 1: batch heap insert ────────────────────────────────────────────
    let rids = TableEngine::insert_rows_batch_with_ctx(
        storage,
        txn,
        &batch.table_def,
        &batch.columns,
        ctx,
        &batch.rows,
    )?;

    // ── Phase 2 + 3: grouped index maintenance, one root persist per index ───
    let mut indexes = batch.indexes;
    if !indexes.is_empty() {
        let updated = crate::index_maintenance::batch_insert_into_indexes(
            &mut indexes,
            &batch.rows,
            &rids,
            storage,
            bloom,
            &batch.compiled_preds,
        )?;
        for (index_id, new_root) in updated {
            CatalogWriter::new(storage, txn)?.update_index_root(index_id, new_root)?;
        }
        // Invalidate schema cache so next resolve re-reads updated roots.
        ctx.invalidate_all();
    }

    // ── Phase 4: stats ────────────────────────────────────────────────────────
    ctx.stats
        .on_rows_changed(batch.table_def.id, rids.len() as u64);

    Ok(())
}
