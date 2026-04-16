impl TableEngine {
    /// Encodes and inserts a row into the table heap, WAL-logging the insert.
    ///
    /// Applies implicit coercion (strict mode) from each value to the declared
    /// column type before encoding. For example, `Text("42")` into an `INT`
    /// column becomes `Int(42)`.
    ///
    /// Must be called inside an active transaction (`txn.begin()` already called).
    ///
    /// # Errors
    /// - [`DbError::TypeMismatch`] — `values.len() != columns.len()`.
    /// - [`DbError::InvalidCoercion`] — a value cannot be coerced to the column type.
    /// - [`DbError::NoActiveTransaction`] — no transaction is active.
    /// - I/O errors from storage or WAL writes.
    pub fn insert_row(
        storage: &dyn StorageEngine,
        txn: &TxnManager,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        table_def: &TableDef,
        columns: &[ColumnDef],
        values: Vec<Value>,
    ) -> Result<RecordId, DbError> {
        ensure_heap_table(table_def, "INSERT into clustered table — Phase 39.14")?;
        if values.len() != columns.len() {
            return Err(DbError::TypeMismatch {
                expected: format!("{} columns", columns.len()),
                got: format!("{} values", values.len()),
            });
        }

        let col_types = column_data_types(columns);
        let coerced = coerce_values(values, columns)?;
        let encoded = encode_row(&coerced, &col_types)?;
        // Phase 11.2: TOAST oversized rows to overflow chains.
        let encoded = toast_row_if_needed(
            encoded,
            &coerced,
            &col_types,
            storage,
            &mut conn_txn.local_page_batch,
        )?;

        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        let (page_id, slot_id) = {
            let batch = &mut conn_txn.local_page_batch;
            HeapChain::insert(storage, table_def.root_page_id, &encoded, txn_id, Some(batch))?
        };

        let key = encode_rid(page_id, slot_id);
        txn.record_insert(conn_txn, table_def.id, &key, &encoded, page_id, slot_id)?;

        Ok(RecordId { page_id, slot_id })
    }

    /// Inserts one row using an optional heap-tail hint for O(1) tail lookup.
    ///
    /// If `hint` is `Some(...)`, the tail page is resolved via
    /// [`HeapChain::insert_with_hint`] instead of walking from the root.
    /// The hint is updated in place after the insert so the caller can pass the
    /// same reference to subsequent calls and accumulate tail state.
    ///
    /// Use this in hot loops (ctx per-row insert paths) to avoid O(N²) behavior.
    pub fn insert_row_with_hint(
        storage: &dyn StorageEngine,
        txn: &TxnManager,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        table_def: &TableDef,
        columns: &[ColumnDef],
        values: Vec<Value>,
        hint: Option<&mut HeapAppendHint>,
    ) -> Result<RecordId, DbError> {
        ensure_heap_table(table_def, "INSERT into clustered table — Phase 39.14")?;
        if values.len() != columns.len() {
            return Err(DbError::TypeMismatch {
                expected: format!("{} columns", columns.len()),
                got: format!("{} values", values.len()),
            });
        }
        let col_types = column_data_types(columns);
        let coerced = coerce_values(values, columns)?;
        let encoded = encode_row(&coerced, &col_types)?;
        let encoded = toast_row_if_needed(
            encoded, &coerced, &col_types, storage, &mut conn_txn.local_page_batch,
        )?;
        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        let (page_id, slot_id) = {
            let batch = &mut conn_txn.local_page_batch;
            HeapChain::insert_with_hint(storage, table_def.root_page_id, &encoded, txn_id, hint, Some(batch))?
        };
        let key = encode_rid(page_id, slot_id);
        txn.record_insert(conn_txn, table_def.id, &key, &encoded, page_id, slot_id)?;
        Ok(RecordId { page_id, slot_id })
    }

    /// Encodes and inserts **multiple rows** into the table heap in one pass,
    /// WAL-logging each insert.
    ///
    /// This is the batch counterpart of [`insert_row`]. It calls
    /// [`HeapChain::insert_batch`] which loads each heap page exactly once
    /// regardless of how many rows are written to it — reducing per-row
    /// `read_page` + `write_page` calls from O(N) to O(pages).
    ///
    /// ## Encoding phase (fail-fast)
    ///
    /// All rows are coerced and encoded before any heap or WAL write. If any
    /// row fails type coercion, the function returns an error and the heap is
    /// untouched.
    ///
    /// ## WAL ordering
    ///
    /// `HeapChain::insert_batch()` writes pages before returning the
    /// `(page_id, slot_id)` pairs. `record_insert()` is then called for each
    /// row. Both heap and WAL writes are in the BufWriter / mmap (not yet
    /// durable). Durability comes from `TxnManager::commit()`.
    ///
    /// Must be called inside an active transaction.
    ///
    /// # Errors
    /// - [`DbError::TypeMismatch`] — any row has wrong column count.
    /// - [`DbError::InvalidCoercion`] — any value cannot be coerced.
    /// - [`DbError::NoActiveTransaction`] — no transaction is active.
    /// - I/O errors from storage or WAL writes.
    pub fn insert_rows_batch(
        storage: &dyn StorageEngine,
        txn: &TxnManager,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        table_def: &TableDef,
        columns: &[ColumnDef],
        batch: &[Vec<Value>],
    ) -> Result<Vec<RecordId>, DbError> {
        ensure_heap_table(table_def, "INSERT into clustered table — Phase 39.14")?;
        if batch.is_empty() {
            return Ok(Vec::new());
        }

        let col_types = column_data_types(columns);

        // ── Encode all rows first (fail-fast, no heap writes yet) ─────────────
        let encoded_rows: Vec<Vec<u8>> = batch
            .iter()
            .map(|values| {
                let values = values.clone();
                if values.len() != columns.len() {
                    return Err(DbError::TypeMismatch {
                        expected: format!("{} columns", columns.len()),
                        got: format!("{} values", values.len()),
                    });
                }
                let coerced = coerce_values(values, columns)?;
                encode_row(&coerced, &col_types)
            })
            .collect::<Result<_, _>>()?;

        // ── Insert all rows into the heap in one batch pass ───────────────────
        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        let phys_locs =
            HeapChain::insert_batch(storage, table_def.root_page_id, &encoded_rows, txn_id)?;

        // ── WAL: one compact PageWrite entry per affected page ───────────────
        // Group slot_ids by page_id. Each PageWrite entry carries only the
        // slot_ids (not the 16 KB page image), reducing WAL from ~820 KB to
        // ~20 KB per 10 K-row batch — a 40× reduction.
        // Crash recovery only needs slot_ids to mark inserted slots dead on undo.
        let mut page_slot_map: std::collections::HashMap<u64, Vec<u16>> =
            std::collections::HashMap::new();
        for &(page_id, slot_id) in &phys_locs {
            page_slot_map.entry(page_id).or_default().push(slot_id);
        }

        // Sort by page_id for deterministic WAL ordering.
        let mut sorted_pages: Vec<(u64, Vec<u16>)> = page_slot_map.into_iter().collect();
        sorted_pages.sort_unstable_by_key(|(page_id, _)| *page_id);

        // Emit one PageWrite WAL entry per affected page.
        let pw_refs: Vec<(u64, &[u16])> = sorted_pages
            .iter()
            .map(|(pid, slots)| (*pid, slots.as_slice()))
            .collect();
        txn.record_page_writes(conn_txn, table_def.id, &pw_refs)?;

        let result = phys_locs
            .iter()
            .map(|(page_id, slot_id)| RecordId {
                page_id: *page_id,
                slot_id: *slot_id,
            })
            .collect();

        Ok(result)
    }

    /// Stamps an MVCC deletion on the row at `record_id`, WAL-logging the delete.
    ///
    /// The old row bytes are read before deletion to include as `old_value` in
    /// the WAL entry for crash recovery.
    ///
    /// Must be called inside an active transaction.
    ///
    /// # Errors
    /// - [`DbError::AlreadyDeleted`] — the slot is already dead.
    /// - [`DbError::InvalidSlot`] — `record_id` points to a non-existent slot.
    /// - [`DbError::NoActiveTransaction`] — no transaction is active.
    /// - I/O errors from storage or WAL writes.
    pub fn delete_row(
        storage: &dyn StorageEngine,
        txn: &TxnManager,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        table_def: &TableDef,
        record_id: RecordId,
    ) -> Result<(), DbError> {
        ensure_heap_table(table_def, "DELETE from clustered table — Phase 39.17")?;
        let old_bytes = HeapChain::read_row(storage, record_id.page_id, record_id.slot_id)?.ok_or(
            DbError::AlreadyDeleted {
                page_id: record_id.page_id,
                slot_id: record_id.slot_id,
            },
        )?;

        // Phase 11.2: free TOAST overflow chains before deleting the row.
        free_toast_chains_in_encoded(&old_bytes, storage);

        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        HeapChain::delete(storage, record_id.page_id, record_id.slot_id, txn_id)?;

        let key = encode_rid(record_id.page_id, record_id.slot_id);
        txn.record_delete(
            conn_txn,
            table_def.id,
            &key,
            &old_bytes,
            record_id.page_id,
            record_id.slot_id,
        )?;

        Ok(())
    }

    /// Deletes multiple rows in a single pass over the heap.
    ///
    /// Each heap page is read and written **exactly once** regardless of how
    /// many rows are deleted from it — compared to N × `delete_row()` calls
    /// which do 3 page operations per row (read + read + write).
    ///
    /// WAL entries are emitted after the page writes, preserving the invariant
    /// that `write_page()` always precedes `record_delete()`.
    ///
    /// Returns the number of rows deleted.
    pub fn delete_rows_batch(
        storage: &dyn StorageEngine,
        txn: &TxnManager,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        table_def: &TableDef,
        rids: &[RecordId],
    ) -> Result<u64, DbError> {
        ensure_heap_table(table_def, "DELETE from clustered table — Phase 39.17")?;
        if rids.is_empty() {
            return Ok(0);
        }

        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        let raw_rids: Vec<(u64, u16)> = rids.iter().map(|r| (r.page_id, r.slot_id)).collect();

        // Batch-delete on the heap: each page read+written once.
        let deleted = HeapChain::delete_batch(storage, table_def.root_page_id, &raw_rids, txn_id)?;

        // Batch WAL: one PageDelete entry per affected page (instead of one
        // Delete entry per row). Reduces WAL from O(N × 150 bytes) to O(P × 50 bytes).
        let mut page_deletes: Vec<(u64, Vec<u16>)> = Vec::new();
        for (page_id, slot_id, _old_bytes) in &deleted {
            match page_deletes.last_mut() {
                Some((last_pid, slots)) if *last_pid == *page_id => {
                    slots.push(*slot_id);
                }
                _ => {
                    page_deletes.push((*page_id, vec![*slot_id]));
                }
            }
        }
        txn.record_delete_batch(conn_txn, table_def.id, &page_deletes)?;

        Ok(deleted.len() as u64)
    }

    /// Updates multiple rows in two batch passes: delete all old slots, then
    /// insert all new rows.
    ///
    /// Inspired by OceanBase's dual-row buffer (`ObDASUpdIterator`) and
    /// MariaDB's `ha_bulk_update_row()`: accumulate all (old, new) pairs first,
    /// then flush as a single delete_batch + insert_batch operation.
    ///
    /// ## Performance
    ///
    /// Per-row `update_row()` does ~3 page ops per row (read + read+write for
    /// delete + read+write for insert). This function does O(P) ops for P pages:
    /// - `delete_rows_batch`: 1 read + 1 write per page holding old rows
    /// - `insert_rows_batch`: 1 read + 1 write per page receiving new rows
    ///
    /// For 5,000 rows across 50 pages: ~200 page ops vs ~15,000.
    ///
    /// ## WAL ordering
    ///
    /// All deletes (heap write + WAL) happen before all inserts, ensuring that
    /// crash recovery can undo the update by undoing inserts (killing new slots)
    /// then undoing deletes (resurrecting old slots) in reverse WAL order.
    ///
    /// Must be called inside an active transaction.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] — no transaction is active.
    /// - [`DbError::TypeMismatch`] — any new row has wrong column count.
    /// - I/O errors from storage or WAL writes.
    pub fn update_rows_batch(
        storage: &dyn StorageEngine,
        txn: &TxnManager,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        table_def: &TableDef,
        columns: &[ColumnDef],
        updates: Vec<(RecordId, Vec<Value>)>,
    ) -> Result<u64, DbError> {
        ensure_heap_table(table_def, "UPDATE on clustered table — Phase 39.16")?;
        if updates.is_empty() {
            return Ok(0);
        }

        let (rids, new_values): (Vec<RecordId>, Vec<Vec<Value>>) = updates.into_iter().unzip();

        // Phase 1: batch-delete all old rows (O(P) page I/O for P pages).
        // Reads each page once, marks all targeted slots dead, writes once.
        Self::delete_rows_batch(storage, txn, conn_txn, table_def, &rids)?;

        // Phase 2: batch-insert all new rows (O(P') page I/O for P' pages).
        // Encodes all rows first (fail-fast), then appends in one heap pass.
        Self::insert_rows_batch(storage, txn, conn_txn, table_def, columns, &new_values)?;

        Ok(rids.len() as u64)
    }

    /// Replaces the row at `record_id` with `new_values`, WAL-logging both the
    /// delete and the insert.
    ///
    /// Implemented as `delete_row` + `insert_row` to avoid the same-page
    /// assumption of `TxnManager::record_update`. The returned `RecordId` is
    /// the physical location of the new row, which may differ from `record_id`
    /// if the old page was full and the chain grew.
    ///
    /// Must be called inside an active transaction.
    ///
    /// # Errors
    /// - [`DbError::TypeMismatch`] — `new_values.len() != columns.len()`.
    /// - [`DbError::InvalidCoercion`] — a new value cannot be coerced to the column type.
    /// - [`DbError::AlreadyDeleted`] — the old row slot is already dead.
    /// - [`DbError::NoActiveTransaction`] — no transaction is active.
    /// - I/O errors from storage or WAL writes.
    pub fn update_row(
        storage: &dyn StorageEngine,
        txn: &TxnManager,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        table_def: &TableDef,
        columns: &[ColumnDef],
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
        let coerced = coerce_values(new_values, columns)?;
        let new_encoded = encode_row(&coerced, &col_types)?;
        update_encoded_row_with_hint(
            storage,
            txn,
            conn_txn,
            table_def,
            record_id,
            &new_encoded,
            None,
        )
    }

    /// Updates one row using a heap-tail hint for the insert half.
    ///
    /// The delete half is unchanged; the insert half calls
    /// [`HeapChain::insert_with_hint`] to avoid re-walking the chain from root
    /// on each iteration of a bulk UPDATE loop.
    pub fn update_row_with_hint(
        storage: &dyn StorageEngine,
        txn: &TxnManager,
        conn_txn: &mut axiomdb_wal::ConnectionTxn,
        table_def: &TableDef,
        columns: &[ColumnDef],
        record_id: RecordId,
        new_values: Vec<Value>,
        hint: Option<&mut HeapAppendHint>,
    ) -> Result<RecordId, DbError> {
        ensure_heap_table(table_def, "UPDATE on clustered table — Phase 39.16")?;
        if new_values.len() != columns.len() {
            return Err(DbError::TypeMismatch {
                expected: format!("{} columns", columns.len()),
                got: format!("{} values", new_values.len()),
            });
        }
        let col_types = column_data_types(columns);
        let coerced = coerce_values(new_values, columns)?;
        let new_encoded = encode_row(&coerced, &col_types)?;
        update_encoded_row_with_hint(
            storage,
            txn,
            conn_txn,
            table_def,
            record_id,
            &new_encoded,
            hint,
        )
    }

}

// ── TOAST cleanup on DELETE (Phase 11.2) ─────────────────────────────────────

/// Scans raw encoded row bytes for TOAST sentinel u24 values and frees the
/// corresponding overflow chains. Best-effort: errors are silently ignored
/// (the row is being deleted anyway).
fn free_toast_chains_in_encoded(encoded: &[u8], storage: &dyn StorageEngine) {
    use axiomdb_types::codec::{TOAST_SENTINEL_LZ4, TOAST_SENTINEL_RAW};

    // TOAST sentinels appear as u24 LE bytes: [0xFE, 0xFF, 0xFF] (RAW) or [0xFD, 0xFF, 0xFF] (LZ4).
    // Scan for these 3-byte patterns and extract the following u64 page_id.
    let raw_bytes = [
        (TOAST_SENTINEL_RAW & 0xFF) as u8,
        ((TOAST_SENTINEL_RAW >> 8) & 0xFF) as u8,
        ((TOAST_SENTINEL_RAW >> 16) & 0xFF) as u8,
    ];
    let lz4_bytes = [
        (TOAST_SENTINEL_LZ4 & 0xFF) as u8,
        ((TOAST_SENTINEL_LZ4 >> 8) & 0xFF) as u8,
        ((TOAST_SENTINEL_LZ4 >> 16) & 0xFF) as u8,
    ];

    let mut i = 0;
    while i + 15 <= encoded.len() {
        let is_toast = (encoded[i] == raw_bytes[0]
            && encoded[i + 1] == raw_bytes[1]
            && encoded[i + 2] == raw_bytes[2])
            || (encoded[i] == lz4_bytes[0]
                && encoded[i + 1] == lz4_bytes[1]
                && encoded[i + 2] == lz4_bytes[2]);
        if is_toast {
            let page_id =
                u64::from_le_bytes(encoded[i + 3..i + 11].try_into().unwrap_or([0; 8]));
            if page_id > 0 {
                let _ = axiomdb_storage::clustered_overflow::free_blob(storage, page_id);
            }
            i += 15; // skip the TOAST pointer
        } else {
            i += 1;
        }
    }
}

// ── TOAST: overflow large values to chain pages (Phase 11.2) ─────────────────

/// Per-column TOAST state after externalization pass.
enum ToastCol {
    Inline,
    External { page_id: u64, raw_len: u32, lz4: bool },
}

/// If `encoded` exceeds [`TOAST_THRESHOLD`], externalizes the largest Text/Bytes
/// values to overflow chains and returns a re-encoded row with TOAST pointers.
/// Leaves unchanged if the row already fits.
fn toast_row_if_needed(
    encoded: Vec<u8>,
    values: &[Value],
    col_types: &[axiomdb_types::DataType],
    storage: &dyn StorageEngine,
    batch: &mut axiomdb_storage::LocalPageBatch,
) -> Result<Vec<u8>, DbError> {
    use axiomdb_types::codec::{TOAST_COMPRESS_MIN, TOAST_THRESHOLD};
    use axiomdb_types::DataType;

    if encoded.len() <= TOAST_THRESHOLD {
        return Ok(encoded);
    }

    let mut state: Vec<ToastCol> = (0..values.len()).map(|_| ToastCol::Inline).collect();
    let mut saved = 0usize;

    // Candidates sorted largest-first.
    let mut cands: Vec<(usize, usize)> = values
        .iter()
        .enumerate()
        .filter_map(|(i, v)| match (col_types.get(i), v) {
            (Some(DataType::Text), Value::Text(s)) => Some((i, s.len())),
            (Some(DataType::Bytes), Value::Bytes(b)) => Some((i, b.len())),
            _ => None,
        })
        .collect();
    cands.sort_by_key(|&(_, sz)| std::cmp::Reverse(sz));

    for &(ci, sz) in &cands {
        if encoded.len().saturating_sub(saved) <= TOAST_THRESHOLD {
            break;
        }
        let raw: Vec<u8> = match &values[ci] {
            Value::Text(s) => s.as_bytes().to_vec(),
            Value::Bytes(b) => b.clone(),
            _ => continue,
        };
        let (data, lz4) = if raw.len() >= TOAST_COMPRESS_MIN {
            let c = lz4_flex::compress_prepend_size(&raw);
            if c.len() < raw.len() { (c, true) } else { (raw.clone(), false) }
        } else {
            (raw.clone(), false)
        };
        let pid =
            axiomdb_storage::clustered_overflow::write_refcounted_chain(storage, Some(batch), &data)?;
        let pid = match pid { Some(p) => p, None => continue };
        state[ci] = ToastCol::External { page_id: pid, raw_len: raw.len() as u32, lz4 };
        // u24+payload replaced by u24+12 bytes pointer → save (sz - 12).
        saved += sz.saturating_sub(12);
    }

    // Re-encode with TOAST pointers for externalized columns.
    toast_encode(values, col_types, &state)
}

/// Encodes a row where some columns are externalized to overflow chains.
fn toast_encode(
    values: &[Value],
    col_types: &[axiomdb_types::DataType],
    state: &[ToastCol],
) -> Result<Vec<u8>, DbError> {
    use axiomdb_types::codec::{TOAST_SENTINEL_LZ4, TOAST_SENTINEL_RAW};
    use axiomdb_types::DataType;

    let n = col_types.len();
    let blen = n.div_ceil(8);
    let mut buf = Vec::with_capacity(blen + 128);

    // Null bitmap.
    let mut bm = vec![0u8; blen];
    for (i, v) in values.iter().enumerate() {
        if matches!(v, Value::Null) {
            bm[i / 8] |= 1 << (i % 8);
        }
    }
    buf.extend_from_slice(&bm);

    for (i, v) in values.iter().enumerate() {
        if matches!(v, Value::Null) { continue; }
        match &state[i] {
            ToastCol::External { page_id, raw_len, lz4 } => {
                let s = if *lz4 { TOAST_SENTINEL_LZ4 } else { TOAST_SENTINEL_RAW };
                axiomdb_types::codec::encode_toast_pointer(&mut buf, s, *page_id, *raw_len);
            }
            ToastCol::Inline => match (col_types[i], v) {
                (DataType::Bool, Value::Bool(b)) => buf.push(u8::from(*b)),
                (DataType::Int, Value::Int(n)) => buf.extend_from_slice(&n.to_le_bytes()),
                (DataType::BigInt, Value::BigInt(n)) => buf.extend_from_slice(&n.to_le_bytes()),
                (DataType::Real, Value::Real(f)) => buf.extend_from_slice(&f.to_le_bytes()),
                (DataType::Decimal, Value::Decimal(m, s)) => {
                    buf.extend_from_slice(&m.to_le_bytes());
                    buf.push(*s);
                }
                (DataType::Text, Value::Text(s)) => {
                    let b = s.as_bytes();
                    let len = b.len();
                    buf.push(len as u8);
                    buf.push((len >> 8) as u8);
                    buf.push((len >> 16) as u8);
                    buf.extend_from_slice(b);
                }
                (DataType::Bytes, Value::Bytes(b)) => {
                    let len = b.len();
                    buf.push(len as u8);
                    buf.push((len >> 8) as u8);
                    buf.push((len >> 16) as u8);
                    buf.extend_from_slice(b);
                }
                (DataType::Date, Value::Date(d)) => buf.extend_from_slice(&d.to_le_bytes()),
                (DataType::Timestamp, Value::Timestamp(t)) => buf.extend_from_slice(&t.to_le_bytes()),
                (DataType::Uuid, Value::Uuid(u)) => buf.extend_from_slice(u),
                _ => return Err(DbError::Internal { message: format!("toast_encode: type mismatch col {i}") }),
            },
        }
    }
    Ok(buf)
}
