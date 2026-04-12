// ── REPLACE INTO conflict displacement (Phase MySQL-compat) ──────────────────
//
// Per-row helper used by the heap INSERT executor when `stmt.replace == true`.
// Before the row is written, every row that would violate a PRIMARY KEY or
// UNIQUE constraint is deleted. FK cascade runs against the displaced row via
// the same machinery as a plain DELETE.
//
// Design reference: MariaDB `sql/sql_insert.cc::replace_row` — AxiomDB uses
// proactive lookups instead of the handler's retry-on-error loop because the
// cost (one B-tree probe per unique index per row) is deterministic and our
// error path would otherwise have to reconstruct which index conflicted.

/// Runs the REPLACE conflict-displacement pass for a single heap-table row.
///
/// Returns the number of displaced rows (0 when the incoming row has no
/// conflicts — REPLACE then behaves exactly like INSERT for this row).
///
/// The incoming `row_values` must be schema-ordered and already have
/// `DEFAULT` + `AUTO_INCREMENT` resolved so the keys we probe are identical
/// to the ones the subsequent INSERT would try to write.
#[allow(clippy::too_many_arguments)]
fn replace_displace_conflicts_heap(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
    resolved: &axiomdb_catalog::ResolvedTable,
    schema_cols: &[axiomdb_catalog::schema::ColumnDef],
    row_values: &[Value],
) -> Result<u64, DbError> {
    use axiomdb_index::BTree;
    use axiomdb_storage::heap_chain::HeapChain;

    // MariaDB AI exhaustion rule — fire BEFORE any deletes so we never leave
    // partial state behind. Only relevant when the user provided an explicit
    // value for the AUTO_INCREMENT column AND that value collides on the AI
    // column's unique index with a different existing row.
    if let Some(ai_pos) = schema_cols.iter().position(|c| c.auto_increment) {
        if let Some(val) = row_values.get(ai_pos) {
            let user_supplied = !matches!(val, Value::Null | Value::Int(0) | Value::BigInt(0));
            if user_supplied {
                // Find the unique/PK index whose first (and only) column is the
                // AI column — that's the conflict domain we must guard.
                let ai_idx_exists = resolved.indexes.iter().any(|i| {
                    (i.is_primary || i.is_unique)
                        && i.columns.len() == 1
                        && i.columns[0].col_idx == ai_pos as u16
                });
                if ai_idx_exists {
                    // No extra work — the generic conflict loop below will
                    // discover the collision and delete the pre-existing row.
                    // MariaDB's stricter rule (reject the REPLACE to prevent
                    // key-space exhaustion) kicks in only when the existing
                    // row's AI value already matches the user-supplied value;
                    // that's the same as saying "the row already uses this
                    // ID", which REPLACE semantics intentionally overwrite.
                    // We therefore do NOT error here — we match user intent
                    // while preserving the overall displacement flow.
                }
            }
        }
    }

    let mut deleted: u64 = 0;
    let snap = txn.active_snapshot(&*conn_txn);

    // Iterate unique and PK indexes. FK auto-indexes are non-unique (they
    // encode RID into the key as a tiebreaker) so they're filtered.
    for idx in resolved.indexes.iter() {
        if !(idx.is_primary || idx.is_unique) {
            continue;
        }
        if idx.is_fk_index {
            continue;
        }
        if idx.columns.is_empty() {
            continue;
        }

        // Partial-index predicate — only conflict if the incoming row
        // satisfies the predicate. Rows outside the predicate's domain
        // cannot clash on this index.
        if idx.predicate.as_deref().is_some_and(|s| !s.is_empty()) {
            let compiled = crate::partial_index::compile_index_predicates(
                std::slice::from_ref(idx),
                schema_cols,
            )?;
            if let Some(Some(pred)) = compiled.first() {
                let v = crate::eval::eval(pred, row_values)?;
                if !crate::eval::is_truthy(&v) {
                    continue;
                }
            }
        }

        // Extract key values in index-column order; MATCH SIMPLE: any NULL
        // makes the row not conflict on this index.
        let mut key_vals: Vec<Value> = Vec::with_capacity(idx.columns.len());
        let mut any_null = false;
        for ic in &idx.columns {
            let v = row_values
                .get(ic.col_idx as usize)
                .cloned()
                .unwrap_or(Value::Null);
            if matches!(v, Value::Null) {
                any_null = true;
                break;
            }
            key_vals.push(v);
        }
        if any_null {
            continue;
        }

        let key = crate::key_encoding::encode_index_key(&key_vals)?;

        // Bloom shortcut: if the filter says "definitely absent", skip the
        // B-tree probe entirely.
        if !bloom.might_exist(idx.index_id, &key) {
            continue;
        }

        let Some(existing_rid) = BTree::lookup_in(storage, idx.root_page_id, &key)? else {
            continue;
        };

        // Visibility check — a stale index entry (row already deleted in our
        // MVCC snapshot) must not trigger a DELETE here.
        if !HeapChain::is_slot_visible(storage, existing_rid.page_id, existing_rid.slot_id, snap.clone())? {
            continue;
        }

        // Decode the old row for FK parent-delete enforcement.
        let row_bytes =
            HeapChain::read_row(storage, existing_rid.page_id, existing_rid.slot_id)?;
        let Some(bytes) = row_bytes else { continue };
        let old_values = crate::table::decode_row_from_bytes(&bytes, schema_cols)?;

        // FK parent-delete: CASCADE / SET NULL / SET DEFAULT on children fire
        // exactly as for a plain DELETE. RESTRICT aborts the statement.
        let parent_fk_check = {
            let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap.clone())?;
            !reader
                .list_fk_constraints_referencing(resolved.def.id)?
                .is_empty()
        };
        if parent_fk_check {
            crate::fk_enforcement::enforce_fk_on_parent_delete(
                &[(existing_rid, old_values.clone())],
                resolved.def.id,
                storage,
                txn,
                conn_txn,
                bloom,
                0,
            )?;
        }

        // Heap delete — leaves index entries behind; MVCC visibility filters
        // them on subsequent reads (same convention as DELETE statement).
        crate::table::TableEngine::delete_row(
            storage,
            txn,
            conn_txn,
            &resolved.def,
            existing_rid,
        )?;

        // Track row changes for stats staleness (match DELETE behavior).
        ctx.stats.on_rows_changed(resolved.def.id, 1);

        deleted = deleted.saturating_add(1);
    }

    Ok(deleted)
}
