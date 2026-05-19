impl TxnManager {
    pub fn record_clustered_insert(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        key: &[u8],
        new_row: &ClusteredRowImage,
    ) -> Result<(), DbError> {
        let mut entry = WalEntry::new(
            0,
            conn_txn.txn_id,
            EntryType::ClusteredInsert,
            table_id,
            key.to_vec(),
            vec![],
            new_row.to_bytes()?,
        );
        self.wal
            .append_with_buf(&mut entry, &mut conn_txn.wal_scratch)?;
        conn_txn.clustered_roots.insert(table_id, new_row.root_pid);
        conn_txn.undo_ops.push(UndoOp::UndoClusteredInsert {
            table_id,
            key: key.to_vec(),
        });
        Ok(())
    }

    /// Batch version of `record_clustered_insert` — accumulates N entries
    /// into a single `write_batch()` call. WAL entries are byte-identical
    /// to N individual calls; crash recovery is unchanged.
    ///
    /// Each element: `(key, image)`. All entries share `table_id`.
    ///
    /// Attack 15: collapses per-row WAL append overhead in the Appender
    /// and bulk INSERT paths.
    pub fn record_clustered_insert_batch(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        inserts: &[(&[u8], &ClusteredRowImage)],
    ) -> Result<(), DbError> {
        let n = inserts.len();
        if n == 0 {
            return Ok(());
        }

        let txn_id = conn_txn.txn_id;
        let lsn_base = self.wal.reserve_lsns(n);
        conn_txn.wal_scratch.clear();

        for (i, (key, new_row)) in inserts.iter().enumerate() {
            let entry = WalEntry::new(
                lsn_base + i as u64,
                txn_id,
                EntryType::ClusteredInsert,
                table_id,
                key.to_vec(),
                vec![],
                new_row.to_bytes()?,
            );
            entry.serialize_into(&mut conn_txn.wal_scratch);
        }

        self.wal.write_batch(lsn_base, &conn_txn.wal_scratch)?;

        for (key, new_row) in inserts {
            conn_txn.clustered_roots.insert(table_id, new_row.root_pid);
            conn_txn.undo_ops.push(UndoOp::UndoClusteredInsert {
                table_id,
                key: key.to_vec(),
            });
        }
        Ok(())
    }

    pub fn record_clustered_delete_mark(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        key: &[u8],
        old_row: &ClusteredRowImage,
        new_row: &ClusteredRowImage,
    ) -> Result<(), DbError> {
        let mut entry = WalEntry::new(
            0,
            conn_txn.txn_id,
            EntryType::ClusteredDeleteMark,
            table_id,
            key.to_vec(),
            old_row.to_bytes()?,
            new_row.to_bytes()?,
        );
        self.wal
            .append_with_buf(&mut entry, &mut conn_txn.wal_scratch)?;
        conn_txn.clustered_roots.insert(table_id, new_row.root_pid);
        conn_txn.undo_ops.push(UndoOp::UndoClusteredRestore {
            table_id,
            key: key.to_vec(),
            old_row: old_row.clone(),
        });
        Ok(())
    }

    pub fn record_clustered_field_patch_batch(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        root_pid: u64,
        patches: &[ClusteredFieldPatchEntry],
    ) -> Result<(), DbError> {
        let n = patches.len();
        if n == 0 {
            return Ok(());
        }

        let txn_id = conn_txn.txn_id;
        let lsn_base = self.wal.reserve_lsns(n);
        conn_txn.wal_scratch.clear();

        for (i, patch) in patches.iter().enumerate() {
            let entry = WalEntry::new(
                lsn_base + i as u64,
                txn_id,
                EntryType::ClusteredFieldPatch,
                table_id,
                patch.key.clone(),
                patch.encode_old_value(),
                patch.encode_new_value(),
            );
            entry.serialize_into(&mut conn_txn.wal_scratch);
        }

        self.wal.write_batch(lsn_base, &conn_txn.wal_scratch)?;

        for patch in patches {
            conn_txn.clustered_roots.insert(table_id, root_pid);
            conn_txn.undo_ops.push(UndoOp::UndoClusteredFieldPatch {
                table_id,
                key: patch.key.clone(),
                old_header: patch.old_header,
                field_deltas: patch.field_deltas.clone(),
            });
        }
        Ok(())
    }

    /// Batch version of `record_clustered_update` — accumulates N entries into
    /// a single `write_batch()` call. WAL entries are byte-identical to N
    /// individual calls; crash recovery is unchanged.
    ///
    /// Each element: `(key, old_row, new_row)`.
    pub fn record_clustered_update_batch(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        updates: &[(&[u8], &ClusteredRowImage, &ClusteredRowImage)],
    ) -> Result<(), DbError> {
        let n = updates.len();
        if n == 0 {
            return Ok(());
        }

        let txn_id = conn_txn.txn_id;
        let lsn_base = self.wal.reserve_lsns(n);
        conn_txn.wal_scratch.clear();

        for (i, (key, old_row, new_row)) in updates.iter().enumerate() {
            let entry = WalEntry::new(
                lsn_base + i as u64,
                txn_id,
                EntryType::ClusteredUpdate,
                table_id,
                key.to_vec(),
                old_row.to_bytes()?,
                new_row.to_bytes()?,
            );
            entry.serialize_into(&mut conn_txn.wal_scratch);
        }

        self.wal.write_batch(lsn_base, &conn_txn.wal_scratch)?;

        for (key, old_row, new_row) in updates {
            conn_txn.clustered_roots.insert(table_id, new_row.root_pid);
            conn_txn.undo_ops.push(UndoOp::UndoClusteredRestore {
                table_id,
                key: key.to_vec(),
                old_row: (*old_row).clone(),
            });
        }
        Ok(())
    }

    pub fn record_clustered_delete_mark_lightweight(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        root_pid: u64,
        pk_keys: &[Vec<u8>],
    ) -> Result<(), DbError> {
        let n = pk_keys.len();
        if n == 0 {
            return Ok(());
        }

        let txn_id = conn_txn.txn_id;
        let lsn_base = self.wal.reserve_lsns(n);
        conn_txn.wal_scratch.clear();

        let empty_hdr = axiomdb_storage::heap::RowHeader {
            txn_id_created: 0,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        };
        let del_hdr = axiomdb_storage::heap::RowHeader {
            txn_id_created: 0,
            txn_id_deleted: txn_id,
            row_version: 0,
            _flags: 0,
        };
        let old_image = ClusteredRowImage::new(root_pid, empty_hdr, &[]);
        let new_image = ClusteredRowImage::new(root_pid, del_hdr, &[]);
        let old_bytes = old_image.to_bytes()?;
        let new_bytes = new_image.to_bytes()?;

        for (i, pk_key) in pk_keys.iter().enumerate() {
            let entry = WalEntry::new(
                lsn_base + i as u64,
                txn_id,
                EntryType::ClusteredDeleteMark,
                table_id,
                pk_key.clone(),
                old_bytes.clone(),
                new_bytes.clone(),
            );
            entry.serialize_into(&mut conn_txn.wal_scratch);
        }

        self.wal.write_batch(lsn_base, &conn_txn.wal_scratch)?;

        for pk_key in pk_keys {
            conn_txn.clustered_roots.insert(table_id, root_pid);
            conn_txn.undo_ops.push(UndoOp::UndoClusteredUndelete {
                table_id,
                key: pk_key.clone(),
            });
        }
        Ok(())
    }

    pub fn record_clustered_delete_mark_batch(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        deletes: &[(&[u8], &ClusteredRowImage, &ClusteredRowImage)],
    ) -> Result<(), DbError> {
        let n = deletes.len();
        if n == 0 {
            return Ok(());
        }

        let txn_id = conn_txn.txn_id;
        let lsn_base = self.wal.reserve_lsns(n);
        conn_txn.wal_scratch.clear();

        for (i, (key, old_row, new_row)) in deletes.iter().enumerate() {
            let entry = WalEntry::new(
                lsn_base + i as u64,
                txn_id,
                EntryType::ClusteredDeleteMark,
                table_id,
                key.to_vec(),
                old_row.to_bytes()?,
                new_row.to_bytes()?,
            );
            entry.serialize_into(&mut conn_txn.wal_scratch);
        }

        self.wal.write_batch(lsn_base, &conn_txn.wal_scratch)?;

        for (key, old_row, _new_row) in deletes {
            conn_txn.clustered_roots.insert(table_id, old_row.root_pid);
            conn_txn.undo_ops.push(UndoOp::UndoClusteredRestore {
                table_id,
                key: key.to_vec(),
                old_row: (*old_row).clone(),
            });
        }
        Ok(())
    }

    pub fn record_clustered_update(
        &self,
        conn_txn: &mut ConnectionTxn,
        table_id: u32,
        key: &[u8],
        old_row: &ClusteredRowImage,
        new_row: &ClusteredRowImage,
    ) -> Result<(), DbError> {
        let mut entry = WalEntry::new(
            0,
            conn_txn.txn_id,
            EntryType::ClusteredUpdate,
            table_id,
            key.to_vec(),
            old_row.to_bytes()?,
            new_row.to_bytes()?,
        );
        self.wal
            .append_with_buf(&mut entry, &mut conn_txn.wal_scratch)?;
        conn_txn.clustered_roots.insert(table_id, new_row.root_pid);
        conn_txn.undo_ops.push(UndoOp::UndoClusteredRestore {
            table_id,
            key: key.to_vec(),
            old_row: old_row.clone(),
        });
        Ok(())
    }

}
