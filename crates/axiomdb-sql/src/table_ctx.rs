impl TableEngine {
    // ── ctx-aware write variants (session strict_mode + warning emission) ─────

    /// Session-aware insert: applies strict or permissive coercion depending on
    /// `ctx.strict_mode`, emitting warning 1265 on permissive fallback.
    ///
    /// `row_num` is 1-based and statement-local — used in the warning message so
    /// multi-row `INSERT VALUES` callers can pass the loop counter.
    pub fn insert_row_with_ctx(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        ctx: &mut SessionContext,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        values: Vec<Value>,
        row_num: usize,
    ) -> Result<RecordId, DbError> {
        ensure_heap_table(table_def, "INSERT into clustered table — Phase 39.14")?;
        if values.len() != columns.len() {
            return Err(DbError::TypeMismatch {
                expected: format!("{} columns", columns.len()),
                got: format!("{} values", values.len()),
            });
        }
        let col_types = column_data_types(columns);
        let coerced = coerce_values_with_ctx(values, columns, ctx, row_num)?;
        let encoded = encode_row(&coerced, &col_types)?;
        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;

        // Phase 5.18: pull heap-tail hint from the session cache, use it for O(1)
        // tail lookup, and write the updated hint back after the insert.
        let mut hint_opt = ctx.get_heap_tail_hint(table_def.id, table_def.root_page_id);
        let (page_id, slot_id) = HeapChain::insert_with_hint(
            storage,
            table_def.root_page_id,
            &encoded,
            txn_id,
            hint_opt.as_mut(),
        )?;
        if let Some(h) = hint_opt {
            ctx.set_heap_tail_hint(table_def.id, h.root_page_id, h.tail_page_id);
        } else {
            // No existing hint — seed one for the next call.
            ctx.set_heap_tail_hint(table_def.id, table_def.root_page_id, page_id);
        }

        let key = encode_rid(page_id, slot_id);
        txn.record_insert(
            conn_txn,
            table_def.id,
            &key,
            &encoded,
            page_id,
            slot_id,
        )?;
        Ok(RecordId { page_id, slot_id })
    }

    /// Session-aware batch insert: applies strict or permissive coercion per row,
    /// emitting warning 1265 (with 1-based row numbers) on permissive fallback.
    pub fn insert_rows_batch_with_ctx(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        ctx: &mut SessionContext,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        batch: &[Vec<Value>],
    ) -> Result<Vec<RecordId>, DbError> {
        ensure_heap_table(table_def, "INSERT into clustered table — Phase 39.14")?;
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        let col_types = column_data_types(columns);

        // Phase 8.3b: find first non-PK integer column for zone map tracking.
        let zm_col_idx = columns.iter().position(|c| {
            matches!(
                c.col_type,
                ColumnType::Int | ColumnType::BigInt | ColumnType::Bool
            )
        });

        let mut encoded_rows: Vec<Vec<u8>> = Vec::with_capacity(batch.len());
        let mut zm_values: Vec<Option<(u8, i64)>> = Vec::with_capacity(batch.len());

        for (i, values) in batch.iter().enumerate() {
            let values = values.clone();
            if values.len() != columns.len() {
                return Err(DbError::TypeMismatch {
                    expected: format!("{} columns", columns.len()),
                    got: format!("{} values", values.len()),
                });
            }
            let coerced = coerce_values_with_ctx(values, columns, ctx, i + 1)?;

            // Extract zone map value from the tracked column.
            let zm_val = zm_col_idx.and_then(|ci| {
                let val = match &coerced[ci] {
                    Value::Int(n) => Some(*n as i64),
                    Value::BigInt(n) => Some(*n),
                    Value::Bool(b) => Some(if *b { 1i64 } else { 0 }),
                    _ => None,
                };
                val.map(|v| (ci as u8, v))
            });
            zm_values.push(zm_val);

            encoded_rows.push(encode_row(&coerced, &col_types)?);
        }

        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        let phys_locs = HeapChain::insert_batch_with_zm(
            storage,
            table_def.root_page_id,
            &encoded_rows,
            txn_id,
            &zm_values,
        )?;

        let mut page_slot_map: std::collections::HashMap<u64, Vec<u16>> =
            std::collections::HashMap::new();
        for &(page_id, slot_id) in &phys_locs {
            page_slot_map.entry(page_id).or_default().push(slot_id);
        }
        let mut sorted_pages: Vec<(u64, Vec<u16>)> = page_slot_map.into_iter().collect();
        sorted_pages.sort_unstable_by_key(|(page_id, _)| *page_id);
        let pw_refs: Vec<(u64, &[u16])> = sorted_pages
            .iter()
            .map(|(pid, slots)| (*pid, slots.as_slice()))
            .collect();
        txn.record_page_writes(
            conn_txn,
            table_def.id,
            &pw_refs,
        )?;

        Ok(phys_locs
            .iter()
            .map(|(page_id, slot_id)| RecordId {
                page_id: *page_id,
                slot_id: *slot_id,
            })
            .collect())
    }

    /// Session-aware single-row update: applies strict or permissive coercion,
    /// emitting warning 1265 on permissive fallback.
    pub fn update_row_with_ctx(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        ctx: &mut SessionContext,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        record_id: RecordId,
        new_values: Vec<Value>,
    ) -> Result<RecordId, DbError> {
        ensure_heap_table(table_def, "UPDATE on clustered table — Phase 39.16")?;
        if new_values.len() != columns.len() {
            return Err(DbError::TypeMismatch {
                expected: format!("{} columns", columns.len()),
                got: format!("{} values", new_values.len()),
            });
        }
        let col_types = column_data_types(columns);
        let coerced = coerce_values_with_ctx(new_values, columns, ctx, 1)?;
        let new_encoded = encode_row(&coerced, &col_types)?;
        // Phase 5.18: use session heap-tail hint for the insert half of UPDATE.
        let mut hint_opt = ctx.get_heap_tail_hint(table_def.id, table_def.root_page_id);
        let new_rid = update_encoded_row_with_hint(
            storage,
            txn,
            conn_txn,
            table_def,
            record_id,
            &new_encoded,
            hint_opt.as_mut(),
        )?;
        if let Some(h) = hint_opt {
            ctx.set_heap_tail_hint(table_def.id, h.root_page_id, h.tail_page_id);
        } else {
            ctx.set_heap_tail_hint(table_def.id, table_def.root_page_id, new_rid.page_id);
        }
        Ok(new_rid)
    }

    /// Session-aware batch update: applies strict or permissive coercion per row
    /// (1-based row numbers for warning messages).
    pub fn update_rows_batch_with_ctx(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        ctx: &mut SessionContext,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        updates: Vec<(RecordId, Vec<Value>)>,
    ) -> Result<u64, DbError> {
        ensure_heap_table(table_def, "UPDATE on clustered table — Phase 39.16")?;
        if updates.is_empty() {
            return Ok(0);
        }
        let (rids, new_values_vec): (Vec<RecordId>, Vec<Vec<Value>>) = updates.into_iter().unzip();

        // Delete all old rows first.
        Self::delete_rows_batch(
            storage,
            txn,
            conn_txn,
            table_def,
            &rids,
        )?;

        // Encode all new rows with ctx-aware coercion, then batch-insert.
        let col_types = column_data_types(columns);
        let encoded_rows: Vec<Vec<u8>> = new_values_vec
            .into_iter()
            .enumerate()
            .map(|(i, values)| {
                if values.len() != columns.len() {
                    return Err(DbError::TypeMismatch {
                        expected: format!("{} columns", columns.len()),
                        got: format!("{} values", values.len()),
                    });
                }
                let coerced = coerce_values_with_ctx(values, columns, ctx, i + 1)?;
                encode_row(&coerced, &col_types)
            })
            .collect::<Result<_, _>>()?;

        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        let phys_locs =
            HeapChain::insert_batch(storage, table_def.root_page_id, &encoded_rows, txn_id)?;

        let mut page_slot_map: std::collections::HashMap<u64, Vec<u16>> =
            std::collections::HashMap::new();
        for &(page_id, slot_id) in &phys_locs {
            page_slot_map.entry(page_id).or_default().push(slot_id);
        }
        let mut sorted_pages: Vec<(u64, Vec<u16>)> = page_slot_map.into_iter().collect();
        sorted_pages.sort_unstable_by_key(|(page_id, _)| *page_id);
        let pw_refs: Vec<(u64, &[u16])> = sorted_pages
            .iter()
            .map(|(pid, slots)| (*pid, slots.as_slice()))
            .collect();
        txn.record_page_writes(
            conn_txn,
            table_def.id,
            &pw_refs,
        )?;

        Ok(rids.len() as u64)
    }

    /// Updates rows while attempting to preserve each row's `RecordId`.
    ///
    /// For every row, AxiomDB first tries a same-slot rewrite in the heap. If
    /// the new encoded row fits in the existing slot capacity, the row keeps the
    /// same `(page_id, slot_id)` and the WAL records an `UpdateInPlace`. If the
    /// row does not fit, this falls back to the existing delete+insert path and
    /// returns a new `RecordId`.
    ///
    /// The returned vector is parallel to `updates`.
    pub fn update_rows_preserve_rid(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        table_def: &TableDef,
        columns: &[ColumnDef],
        updates: Vec<(RecordId, Vec<Value>)>,
    ) -> Result<Vec<RecordId>, DbError> {
        ensure_heap_table(table_def, "UPDATE on clustered table — Phase 39.16")?;
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        let col_types = column_data_types(columns);
        let prepared: Vec<(RecordId, Vec<u8>)> = updates
            .into_iter()
            .map(|(rid, values)| {
                if values.len() != columns.len() {
                    return Err(DbError::TypeMismatch {
                        expected: format!("{} columns", columns.len()),
                        got: format!("{} values", values.len()),
                    });
                }
                let coerced = coerce_values(values, columns)?;
                let encoded = encode_row(&coerced, &col_types)?;
                Ok((rid, encoded))
            })
            .collect::<Result<_, _>>()?;

        apply_prepared_updates_preserve_rid(storage, txn, conn_txn, table_def, prepared, None)
    }

    /// Session-aware stable-RID batch update.
    ///
    /// Uses the same preserve-RID fast path as [`update_rows_preserve_rid`], but
    /// applies strict/permissive coercion with warning emission through `ctx`.
    pub fn update_rows_preserve_rid_with_ctx(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        table_def: &TableDef,
        columns: &[ColumnDef],
        ctx: &mut SessionContext,
        updates: Vec<(RecordId, Vec<Value>)>,
    ) -> Result<Vec<RecordId>, DbError> {
        ensure_heap_table(table_def, "UPDATE on clustered table — Phase 39.16")?;
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        let col_types = column_data_types(columns);
        let prepared: Vec<(RecordId, Vec<u8>)> = updates
            .into_iter()
            .enumerate()
            .map(|(i, (rid, values))| {
                if values.len() != columns.len() {
                    return Err(DbError::TypeMismatch {
                        expected: format!("{} columns", columns.len()),
                        got: format!("{} values", values.len()),
                    });
                }
                let coerced = coerce_values_with_ctx(values, columns, ctx, i + 1)?;
                let encoded = encode_row(&coerced, &col_types)?;
                Ok((rid, encoded))
            })
            .collect::<Result<_, _>>()?;

        let mut hint_opt = ctx.get_heap_tail_hint(table_def.id, table_def.root_page_id);
        let original_rids: Vec<RecordId> = prepared.iter().map(|(rid, _)| *rid).collect();
        let new_rids = apply_prepared_updates_preserve_rid(
            storage,
            txn,
            conn_txn,
            table_def,
            prepared,
            hint_opt.as_mut(),
        )?;
        if let Some(h) = hint_opt {
            ctx.set_heap_tail_hint(table_def.id, h.root_page_id, h.tail_page_id);
        } else if let Some(last_fallback) = original_rids
            .iter()
            .zip(new_rids.iter())
            .rev()
            .find_map(|(old_rid, new_rid)| (old_rid != new_rid).then_some(*new_rid))
        {
            ctx.set_heap_tail_hint(table_def.id, table_def.root_page_id, last_fallback.page_id);
        }
        Ok(new_rids)
    }
}
