impl TxnManager {
    // ── Index undo (Phase 7.3b) ────────────────────────────────────────────

    pub fn record_index_insert(
        &self,
        conn_txn: &mut ConnectionTxn,
        index_id: u32,
        root_page_id: u64,
        key: Vec<u8>,
    ) {
        conn_txn.undo_ops.push(UndoOp::UndoIndexInsert {
            index_id,
            root_page_id,
            key,
        });
    }

    pub fn record_index_delete(
        &self,
        conn_txn: &mut ConnectionTxn,
        index_id: u32,
        root_page_id: u64,
        key: Vec<u8>,
        rid: RecordId,
        fillfactor: u8,
    ) {
        conn_txn.undo_ops.push(UndoOp::UndoIndexDelete {
            index_id,
            root_page_id,
            key,
            rid,
            fillfactor,
        });
    }

    /// Returns all `UndoIndexInsert` operations from the active transaction's
    /// undo log, in reverse chronological order (last insert first).
    ///
    /// Called by the executor before `rollback()` or `rollback_to_savepoint()`
    /// to handle B-Tree deletes at the executor layer (TxnManager cannot depend
    /// on `axiomdb-index`).
    ///
    /// Returns a reverse-chronological list of index undo records.
    pub fn collect_index_undos(&self, conn_txn: &ConnectionTxn) -> Vec<IndexUndoRecord> {
        conn_txn
            .undo_ops
            .iter()
            .rev()
            .filter_map(|op| match op {
                UndoOp::UndoIndexInsert {
                    index_id,
                    root_page_id,
                    key,
                } => Some(IndexUndoRecord::DeleteInserted {
                    index_id: *index_id,
                    root_page_id: *root_page_id,
                    key: key.clone(),
                }),
                UndoOp::UndoIndexDelete {
                    index_id,
                    root_page_id,
                    key,
                    rid,
                    fillfactor,
                } => Some(IndexUndoRecord::RestoreDeleted {
                    index_id: *index_id,
                    root_page_id: *root_page_id,
                    key: key.clone(),
                    rid: *rid,
                    fillfactor: *fillfactor,
                }),
                _ => None,
            })
            .collect()
    }

    /// Like [`collect_index_undos`] but only returns ops recorded after the
    /// given savepoint.
    pub fn collect_index_undos_since(
        &self,
        conn_txn: &ConnectionTxn,
        sp: &Savepoint,
    ) -> Vec<IndexUndoRecord> {
        conn_txn
            .undo_ops
            .iter()
            .skip(sp.undo_len)
            .rev()
            .filter_map(|op| match op {
                UndoOp::UndoIndexInsert {
                    index_id,
                    root_page_id,
                    key,
                } => Some(IndexUndoRecord::DeleteInserted {
                    index_id: *index_id,
                    root_page_id: *root_page_id,
                    key: key.clone(),
                }),
                UndoOp::UndoIndexDelete {
                    index_id,
                    root_page_id,
                    key,
                    rid,
                    fillfactor,
                } => Some(IndexUndoRecord::RestoreDeleted {
                    index_id: *index_id,
                    root_page_id: *root_page_id,
                    key: key.clone(),
                    rid: *rid,
                    fillfactor: *fillfactor,
                }),
                _ => None,
            })
            .collect()
    }

}
