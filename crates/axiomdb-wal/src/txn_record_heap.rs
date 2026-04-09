#[cfg(test)]
pub(crate) struct HeapUpdateRecord<'a> {
    pub(crate) table_id: u32,
    pub(crate) key: &'a [u8],
    pub(crate) old_value: &'a [u8],
    pub(crate) new_value: &'a [u8],
    pub(crate) page_id: u64,
    pub(crate) old_slot: u16,
    pub(crate) new_slot: u16,
}

impl TxnManager {
    // ── DML recording ────────────────────────────────────────────────────────

    // NOTE: record_* methods prepend PHYSICAL_LOC_LEN bytes to new_value (Insert/Update)
    // and old_value (Delete) so crash recovery can locate the heap slot without an
    // in-memory undo log.  See `PHYSICAL_LOC_LEN` and `decode_physical_loc`.

    /// Records an INSERT into the WAL and enqueues an undo operation.
    ///
    /// Must be called **after** the heap + index changes have been applied to storage.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if called outside a transaction.
    pub fn record_insert(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        key: &[u8],
        value: &[u8],
        page_id: u64,
        slot_id: u16,
    ) -> Result<(), DbError> {
        let txn_id = conn_txn.txn_id;

        let mut new_value = Vec::with_capacity(PHYSICAL_LOC_LEN + value.len());
        new_value.extend_from_slice(&encode_physical_loc(page_id, slot_id));
        new_value.extend_from_slice(value);

        let mut entry = WalEntry::new(
            0,
            txn_id,
            EntryType::Insert,
            table_id,
            key.to_vec(),
            vec![],
            new_value,
        );
        self.wal
            .append_with_buf(&mut entry, &mut conn_txn.wal_scratch)?;
        conn_txn
            .undo_ops
            .push(UndoOp::UndoInsert { page_id, slot_id });
        Ok(())
    }

    /// Records N INSERTs into the WAL in a **single `write_all` call**.
    ///
    /// Equivalent to calling [`record_insert`] N times but uses
    /// [`WalWriter::reserve_lsns`] + [`WalWriter::write_batch`] to write all
    /// entries in one shot, reducing BufWriter call overhead from O(N) to O(1).
    ///
    /// `phys_locs[i]` and `values[i]` must correspond to the same row.
    /// Both slices must have the same length; a length mismatch is an internal
    /// error (caller invariant — never caused by user SQL).
    ///
    /// The entries written to disk are byte-for-byte identical to those produced
    /// by N calls to `record_insert` — crash recovery is unchanged.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if called outside a transaction.
    pub fn record_insert_batch(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        phys_locs: &[(u64, u16)],
        values: &[Vec<u8>],
    ) -> Result<(), DbError> {
        let n = phys_locs.len();
        debug_assert_eq!(
            n,
            values.len(),
            "record_insert_batch: phys_locs and values must have the same length"
        );
        if n == 0 {
            return Ok(());
        }

        let txn_id = conn_txn.txn_id;
        let lsn_base = self.wal.reserve_lsns(n);
        conn_txn.wal_scratch.clear();

        for (i, ((page_id, slot_id), value)) in phys_locs.iter().zip(values.iter()).enumerate() {
            let key = encode_physical_loc(*page_id, *slot_id);
            let mut new_value = Vec::with_capacity(PHYSICAL_LOC_LEN + value.len());
            new_value.extend_from_slice(&key);
            new_value.extend_from_slice(value);
            let entry = WalEntry::new(
                lsn_base + i as u64,
                txn_id,
                EntryType::Insert,
                table_id,
                key.to_vec(),
                vec![],
                new_value,
            );
            entry.serialize_into(&mut conn_txn.wal_scratch);
        }

        self.wal.write_batch(lsn_base, &conn_txn.wal_scratch)?;

        for (page_id, slot_id) in phys_locs {
            conn_txn.undo_ops.push(UndoOp::UndoInsert {
                page_id: *page_id,
                slot_id: *slot_id,
            });
        }
        Ok(())
    }

    /// Records N bulk-insert pages into the WAL as compact `PageWrite` entries.
    ///
    /// Each element of `page_writes` is `(page_id, slot_ids)` where `slot_ids`
    /// lists the slots inserted by this transaction on that page.
    ///
    /// ## Compact WAL format
    ///
    /// `new_value = [num_slots: u16 LE][slot_id × num_slots: u16 LE each]`
    ///
    /// No page bytes are stored — crash recovery only needs the slot IDs to mark
    /// inserted slots dead on undo. Eliminating the 16 KB page image reduces WAL
    /// size from ~820 KB to ~20 KB per 10K-row batch (40× reduction).
    ///
    /// Inspired by MariaDB's InnoDB redo log (logical delta) and OceanBase's
    /// `ObDASWriteBuffer` (row-level buffering, not page-level snapshot).
    ///
    /// ## WAL ordering
    ///
    /// Uses `reserve_lsns + write_batch` for O(1) BufWriter calls — same
    /// pattern as `record_insert_batch`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if called outside a transaction.
    pub fn record_page_writes(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        page_writes: &[(u64, &[u16])],
    ) -> Result<(), DbError> {
        let n = page_writes.len();
        if n == 0 {
            return Ok(());
        }

        let txn_id = conn_txn.txn_id;
        let lsn_base = self.wal.reserve_lsns(n);
        conn_txn.wal_scratch.clear();

        for (i, (page_id, slot_ids)) in page_writes.iter().enumerate() {
            let key = page_id.to_le_bytes();
            let mut new_value = Vec::with_capacity(2 + slot_ids.len() * 2);
            new_value.extend_from_slice(&(slot_ids.len() as u16).to_le_bytes());
            for &slot_id in slot_ids.iter() {
                new_value.extend_from_slice(&slot_id.to_le_bytes());
            }
            let entry = WalEntry::new(
                lsn_base + i as u64,
                txn_id,
                EntryType::PageWrite,
                table_id,
                key.to_vec(),
                vec![],
                new_value,
            );
            entry.serialize_into(&mut conn_txn.wal_scratch);
        }

        self.wal.write_batch(lsn_base, &conn_txn.wal_scratch)?;

        for (page_id, slot_ids) in page_writes {
            for &slot_id in slot_ids.iter() {
                conn_txn.undo_ops.push(UndoOp::UndoInsert {
                    page_id: *page_id,
                    slot_id,
                });
            }
        }
        Ok(())
    }

    /// Batch-records DELETEs into the WAL: one `PageDelete` entry per affected
    /// page listing the slot_ids deleted on that page.
    ///
    /// Mirrors [`record_page_writes`] for INSERT. Reduces WAL size from
    /// O(N × 150 bytes) to O(P × 50 bytes) where P = pages touched.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if called outside a transaction.
    pub fn record_delete_batch(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        page_deletes: &[(u64, Vec<u16>)],
    ) -> Result<(), DbError> {
        if page_deletes.is_empty() {
            return Ok(());
        }

        let txn_id = conn_txn.txn_id;
        let n = page_deletes.len();
        let lsn_base = self.wal.reserve_lsns(n);
        conn_txn.wal_scratch.clear();

        for (i, (page_id, slot_ids)) in page_deletes.iter().enumerate() {
            let key = page_id.to_le_bytes();
            let mut new_value = Vec::with_capacity(2 + slot_ids.len() * 2);
            new_value.extend_from_slice(&(slot_ids.len() as u16).to_le_bytes());
            for &slot_id in slot_ids {
                new_value.extend_from_slice(&slot_id.to_le_bytes());
            }
            let entry = WalEntry::new(
                lsn_base + i as u64,
                txn_id,
                EntryType::PageDelete,
                table_id,
                key.to_vec(),
                vec![],
                new_value,
            );
            entry.serialize_into(&mut conn_txn.wal_scratch);
        }

        self.wal.write_batch(lsn_base, &conn_txn.wal_scratch)?;

        for (page_id, slot_ids) in page_deletes {
            for &slot_id in slot_ids {
                conn_txn.undo_ops.push(UndoOp::UndoDelete {
                    page_id: *page_id,
                    slot_id,
                });
            }
        }
        Ok(())
    }

    /// Records a DELETE into the WAL and enqueues an undo operation.
    ///
    /// Must be called **after** `txn_id_deleted` has been stamped in the RowHeader.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if called outside a transaction.
    pub fn record_delete(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        key: &[u8],
        old_value: &[u8],
        page_id: u64,
        slot_id: u16,
    ) -> Result<(), DbError> {
        let txn_id = conn_txn.txn_id;

        let mut ov = Vec::with_capacity(PHYSICAL_LOC_LEN + old_value.len());
        ov.extend_from_slice(&encode_physical_loc(page_id, slot_id));
        ov.extend_from_slice(old_value);

        let mut entry = WalEntry::new(
            0,
            txn_id,
            EntryType::Delete,
            table_id,
            key.to_vec(),
            ov,
            vec![],
        );
        self.wal
            .append_with_buf(&mut entry, &mut conn_txn.wal_scratch)?;
        conn_txn
            .undo_ops
            .push(UndoOp::UndoDelete { page_id, slot_id });
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn record_update(
        &self,
        conn_txn: &mut ConnectionTxn,
        update: HeapUpdateRecord<'_>,
    ) -> Result<(), DbError> {
        let txn_id = conn_txn.txn_id;

        let mut ov = Vec::with_capacity(PHYSICAL_LOC_LEN + update.old_value.len());
        ov.extend_from_slice(&encode_physical_loc(update.page_id, update.old_slot));
        ov.extend_from_slice(update.old_value);

        let mut nv = Vec::with_capacity(PHYSICAL_LOC_LEN + update.new_value.len());
        nv.extend_from_slice(&encode_physical_loc(update.page_id, update.new_slot));
        nv.extend_from_slice(update.new_value);

        let mut entry = WalEntry::new(
            0,
            txn_id,
            EntryType::Update,
            update.table_id,
            update.key.to_vec(),
            ov,
            nv,
        );
        self.wal
            .append_with_buf(&mut entry, &mut conn_txn.wal_scratch)?;
        conn_txn.undo_ops.push(UndoOp::UndoInsert {
            page_id: update.page_id,
            slot_id: update.new_slot,
        });
        conn_txn.undo_ops.push(UndoOp::UndoDelete {
            page_id: update.page_id,
            slot_id: update.old_slot,
        });
        Ok(())
    }

    pub fn record_update_in_place(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        key: &[u8],
        old_tuple_image: &[u8],
        new_tuple_image: &[u8],
        page_id: u64,
        slot_id: u16,
    ) -> Result<(), DbError> {
        let txn_id = conn_txn.txn_id;

        let mut ov = Vec::with_capacity(PHYSICAL_LOC_LEN + old_tuple_image.len());
        ov.extend_from_slice(&encode_physical_loc(page_id, slot_id));
        ov.extend_from_slice(old_tuple_image);

        let mut nv = Vec::with_capacity(PHYSICAL_LOC_LEN + new_tuple_image.len());
        nv.extend_from_slice(&encode_physical_loc(page_id, slot_id));
        nv.extend_from_slice(new_tuple_image);

        let mut entry = WalEntry::new(
            0,
            txn_id,
            EntryType::UpdateInPlace,
            table_id,
            key.to_vec(),
            ov,
            nv,
        );
        self.wal
            .append_with_buf(&mut entry, &mut conn_txn.wal_scratch)?;
        conn_txn.undo_ops.push(UndoOp::UndoUpdateInPlace {
            page_id,
            slot_id,
            old_image: old_tuple_image.to_vec(),
        });
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    pub fn record_update_in_place_batch(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        images: &[(&[u8], &[u8], &[u8], u64, u16)],
    ) -> Result<(), DbError> {
        let n = images.len();
        if n == 0 {
            return Ok(());
        }

        let txn_id = conn_txn.txn_id;
        let lsn_base = self.wal.reserve_lsns(n);
        conn_txn.wal_scratch.clear();

        for (i, (key, old_tuple_image, new_tuple_image, page_id, slot_id)) in
            images.iter().enumerate()
        {
            let mut ov = Vec::with_capacity(PHYSICAL_LOC_LEN + old_tuple_image.len());
            ov.extend_from_slice(&encode_physical_loc(*page_id, *slot_id));
            ov.extend_from_slice(old_tuple_image);

            let mut nv = Vec::with_capacity(PHYSICAL_LOC_LEN + new_tuple_image.len());
            nv.extend_from_slice(&encode_physical_loc(*page_id, *slot_id));
            nv.extend_from_slice(new_tuple_image);

            let entry = WalEntry::new(
                lsn_base + i as u64,
                txn_id,
                EntryType::UpdateInPlace,
                table_id,
                key.to_vec(),
                ov,
                nv,
            );
            entry.serialize_into(&mut conn_txn.wal_scratch);
        }

        self.wal.write_batch(lsn_base, &conn_txn.wal_scratch)?;

        for (_, old_tuple_image, _, page_id, slot_id) in images {
            conn_txn.undo_ops.push(UndoOp::UndoUpdateInPlace {
                page_id: *page_id,
                slot_id: *slot_id,
                old_image: old_tuple_image.to_vec(),
            });
        }
        Ok(())
    }

    /// Records a full-table delete (DELETE without WHERE / TRUNCATE) as a
    /// single WAL entry instead of N per-row entries.
    ///
    /// The physical heap pages must already have been updated by `delete_batch()`
    /// before calling this. ONE WAL entry replaces N `record_delete()` calls.
    ///
    /// The `key` field of the WAL entry holds `root_page_id` as 8 bytes LE —
    /// sufficient for crash recovery to locate the heap chain and undo the deletion.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is open.
    pub fn record_truncate(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        root_page_id: u64,
    ) -> Result<(), DbError> {
        let mut entry = WalEntry::new(
            0,
            conn_txn.txn_id,
            EntryType::Truncate,
            table_id,
            root_page_id.to_le_bytes().to_vec(),
            vec![],
            vec![],
        );
        self.wal
            .append_with_buf(&mut entry, &mut conn_txn.wal_scratch)?;
        conn_txn
            .undo_ops
            .push(UndoOp::UndoTruncate { root_page_id });
        Ok(())
    }
}
