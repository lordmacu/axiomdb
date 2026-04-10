impl TxnManager {
    /// Rolls back the active transaction: undoes heap changes and writes a
    /// Rollback WAL entry (not fsynced — rolled-back data is intentionally ephemeral).
    ///
    /// Captures the current undo log position as a statement-level savepoint.
    ///
    /// The returned `Savepoint` can be passed to [`rollback_to_savepoint`] to undo
    /// only the operations recorded *after* this call, leaving the transaction active.
    ///
    /// Call this **before** executing each statement inside an explicit transaction
    /// to implement MySQL-style statement-level rollback on error.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if called outside an active transaction.
    ///
    /// [`rollback_to_savepoint`]: TxnManager::rollback_to_savepoint
    pub fn savepoint(&self, conn_txn: &ConnectionTxn) -> Savepoint {
        Savepoint {
            undo_len: conn_txn.undo_ops.len(),
            deferred_free_len: conn_txn.deferred_free_pages.len(),
            batch_freed_len: conn_txn.local_page_batch.freed_len(),
        }
    }

    /// Undoes all operations recorded **after** `sp`, leaving the transaction active.
    ///
    /// This implements MySQL's statement-level rollback semantics: when a statement
    /// errors inside an explicit transaction, only that statement's writes are
    /// undone. The transaction remains open; subsequent statements can execute.
    ///
    /// Undo ops are applied in reverse order (last write first), identical to the
    /// full `rollback()` path but scoped to `sp.0..undo_ops.len()`.
    ///
    /// # Errors
    ///
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - I/O errors from undo writes.
    pub fn rollback_to_savepoint(
        &mut self,
        conn_txn: &mut ConnectionTxn,
        sp: Savepoint,
        storage: &mut dyn StorageEngine,
    ) -> Result<(), DbError> {
        let txn_id = conn_txn.txn_id;

        // Discard deferred-free pages recorded after this savepoint.
        conn_txn.deferred_free_pages.truncate(sp.deferred_free_len);

        // Discard freed-after-savepoint entries in the local page batch.
        // The undo restores those rows → the pages are still in use.
        conn_txn
            .local_page_batch
            .truncate_freed(sp.batch_freed_len);

        // Drain only the undo ops recorded after the savepoint.
        let ops_to_undo: Vec<UndoOp> = conn_txn.undo_ops.drain(sp.undo_len..).rev().collect();
        for op in ops_to_undo {
            match op {
                UndoOp::UndoInsert { page_id, slot_id } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    mark_slot_dead(&mut page, slot_id)?;
                    storage.write_page(page_id, &page)?;
                }
                UndoOp::UndoDelete { page_id, slot_id } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    clear_deletion(&mut page, slot_id)?;
                    storage.write_page(page_id, &page)?;
                }
                UndoOp::UndoUpdateInPlace {
                    page_id,
                    slot_id,
                    old_image,
                } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    restore_tuple_image(&mut page, slot_id, &old_image)?;
                    storage.write_page(page_id, &page)?;
                }
                UndoOp::UndoTruncate { root_page_id } => {
                    HeapChain::clear_deletions_by_txn(storage, root_page_id, txn_id)?;
                }
                UndoOp::UndoIndexInsert { .. } | UndoOp::UndoIndexDelete { .. } => {
                    // Handled by caller via collect_index_undos_since().
                }
                UndoOp::UndoClusteredInsert { table_id, key } => {
                    let root_pid =
                        clustered_root_for_undo(&conn_txn.clustered_roots, table_id, "savepoint")?;
                    let new_root = clustered_tree::delete_physical_by_key(storage, root_pid, &key)?
                        .ok_or_else(|| DbError::BTreeCorrupted {
                            msg: format!(
                                "clustered savepoint rollback could not find inserted key {:?} in table {table_id}",
                                String::from_utf8_lossy(&key)
                            ),
                        })?;
                    conn_txn.clustered_roots.insert(table_id, new_root);
                }
                UndoOp::UndoClusteredRestore {
                    table_id,
                    key,
                    old_row,
                } => {
                    let root_pid =
                        clustered_root_for_undo(&conn_txn.clustered_roots, table_id, "savepoint")?;
                    let new_root = clustered_tree::restore_exact_row_image(
                        storage,
                        root_pid,
                        &key,
                        &old_row.row_header,
                        &old_row.row_data,
                    )?;
                    conn_txn.clustered_roots.insert(table_id, new_root);
                }
                UndoOp::UndoClusteredUndelete { table_id, key } => {
                    let root_pid =
                        clustered_root_for_undo(&conn_txn.clustered_roots, table_id, "savepoint")?;
                    let leaf = clustered_tree::descend_to_leaf_pub(storage, root_pid, &key)?;
                    let pos = match axiomdb_storage::clustered_leaf::search(&leaf, &key) {
                        Ok(pos) => pos,
                        Err(_) => continue,
                    };
                    let mut page = leaf.into_page();
                    let _ =
                        axiomdb_storage::clustered_leaf::patch_txn_id_deleted(&mut page, pos, 0);
                    page.update_checksum();
                    storage.write_page(page.header().page_id, &page)?;
                }
                UndoOp::UndoClusteredFieldPatch {
                    table_id,
                    key,
                    old_header,
                    field_deltas,
                } => {
                    let root_pid =
                        clustered_root_for_undo(&conn_txn.clustered_roots, table_id, "savepoint")?;
                    let leaf = clustered_tree::descend_to_leaf_pub(storage, root_pid, &key)?;
                    let pos = match axiomdb_storage::clustered_leaf::search(&leaf, &key) {
                        Ok(pos) => pos,
                        Err(_) => continue,
                    };
                    let (row_data_abs_off, _) =
                        axiomdb_storage::clustered_leaf::cell_row_data_abs_off(&leaf, pos)?;
                    let mut page = leaf.into_page();
                    for delta in &field_deltas {
                        let field_abs_off = row_data_abs_off + delta.offset as usize;
                        axiomdb_storage::clustered_leaf::patch_field_in_place(
                            &mut page,
                            field_abs_off,
                            &delta.old_bytes[..delta.size as usize],
                        )?;
                    }
                    axiomdb_storage::clustered_leaf::update_row_header_in_place(
                        &mut page,
                        pos,
                        &old_header,
                    )?;
                    page.update_checksum();
                    storage.write_page(page.header().page_id, &page)?;
                }
            }
        }

        // Propagate the post-undo clustered roots back to TxnManager so that
        // subsequent calls to `clustered_root()` return the correct (restored) root.
        for (table_id, root_pid) in &conn_txn.clustered_roots {
            self.last_clustered_roots.insert(*table_id, *root_pid);
        }

        Ok(())
    }

    /// Applies undo operations in **reverse chronological order**:
    /// - `UndoInsert`: marks the slot dead (row hidden from all future snapshots).
    /// - `UndoDelete`: clears `txn_id_deleted` (row is live again).
    ///
    /// Does **not** advance `max_committed`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is open.
    /// - I/O errors from undo writes or WAL append.
    pub fn rollback(
        &mut self,
        mut conn_txn: ConnectionTxn,
        storage: &mut dyn StorageEngine,
    ) -> Result<(), DbError> {
        let txn_id = conn_txn.txn_id;

        // Write Rollback entry — informational for crash recovery. No fsync.
        let mut entry = WalEntry::new(0, txn_id, EntryType::Rollback, 0, vec![], vec![], vec![]);
        self.wal.append(&mut entry)?;

        // Phase 40.9: return pre-allocated-but-unused pages to the bitmap.
        // The freed list is dropped: the undo restores the rows → pages in use.
        let avail = conn_txn.local_page_batch.take_for_rollback();
        if !avail.is_empty() {
            storage.free_page_batch(&avail)?;
        }

        // Deferred-free pages are discarded with conn_txn (catalog undo restores old roots).

        // Apply undo ops in reverse (last DML first).
        for op in conn_txn.undo_ops.into_iter().rev() {
            match op {
                UndoOp::UndoInsert { page_id, slot_id } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    mark_slot_dead(&mut page, slot_id)?;
                    storage.write_page(page_id, &page)?;
                }
                UndoOp::UndoDelete { page_id, slot_id } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    clear_deletion(&mut page, slot_id)?;
                    storage.write_page(page_id, &page)?;
                }
                UndoOp::UndoUpdateInPlace {
                    page_id,
                    slot_id,
                    old_image,
                } => {
                    let bytes = *storage.read_page(page_id)?.as_bytes();
                    let mut page = Page::from_bytes(bytes)?;
                    restore_tuple_image(&mut page, slot_id, &old_image)?;
                    storage.write_page(page_id, &page)?;
                }
                UndoOp::UndoTruncate { root_page_id } => {
                    HeapChain::clear_deletions_by_txn(storage, root_page_id, txn_id)?;
                }
                UndoOp::UndoIndexInsert { .. } | UndoOp::UndoIndexDelete { .. } => {
                    // Handled by caller via collect_index_undos().
                }
                UndoOp::UndoClusteredInsert { table_id, key } => {
                    let root_pid =
                        clustered_root_for_undo(&conn_txn.clustered_roots, table_id, "rollback")?;
                    let new_root = clustered_tree::delete_physical_by_key(storage, root_pid, &key)?
                        .ok_or_else(|| DbError::BTreeCorrupted {
                            msg: format!(
                                "clustered rollback could not find inserted key {:?} in table {table_id}",
                                String::from_utf8_lossy(&key)
                            ),
                        })?;
                    conn_txn.clustered_roots.insert(table_id, new_root);
                }
                UndoOp::UndoClusteredRestore {
                    table_id,
                    key,
                    old_row,
                } => {
                    let root_pid =
                        clustered_root_for_undo(&conn_txn.clustered_roots, table_id, "rollback")?;
                    let new_root = clustered_tree::restore_exact_row_image(
                        storage,
                        root_pid,
                        &key,
                        &old_row.row_header,
                        &old_row.row_data,
                    )?;
                    conn_txn.clustered_roots.insert(table_id, new_root);
                }
                UndoOp::UndoClusteredUndelete { table_id, key } => {
                    let root_pid =
                        clustered_root_for_undo(&conn_txn.clustered_roots, table_id, "rollback")?;
                    let leaf = clustered_tree::descend_to_leaf_pub(storage, root_pid, &key)?;
                    if let Ok(pos) = axiomdb_storage::clustered_leaf::search(&leaf, &key) {
                        let mut page = leaf.into_page();
                        let _ = axiomdb_storage::clustered_leaf::patch_txn_id_deleted(
                            &mut page, pos, 0,
                        );
                        page.update_checksum();
                        storage.write_page(page.header().page_id, &page)?;
                    }
                }
                UndoOp::UndoClusteredFieldPatch {
                    table_id,
                    key,
                    old_header,
                    field_deltas,
                } => {
                    let root_pid =
                        clustered_root_for_undo(&conn_txn.clustered_roots, table_id, "rollback")?;
                    let leaf = clustered_tree::descend_to_leaf_pub(storage, root_pid, &key)?;
                    if let Ok(pos) = axiomdb_storage::clustered_leaf::search(&leaf, &key) {
                        let (row_data_abs_off, _) =
                            axiomdb_storage::clustered_leaf::cell_row_data_abs_off(&leaf, pos)?;
                        let mut page = leaf.into_page();
                        for delta in &field_deltas {
                            let field_abs_off = row_data_abs_off + delta.offset as usize;
                            axiomdb_storage::clustered_leaf::patch_field_in_place(
                                &mut page,
                                field_abs_off,
                                &delta.old_bytes[..delta.size as usize],
                            )?;
                        }
                        axiomdb_storage::clustered_leaf::update_row_header_in_place(
                            &mut page,
                            pos,
                            &old_header,
                        )?;
                        page.update_checksum();
                        storage.write_page(page.header().page_id, &page)?;
                    }
                }
            }
        }

        // Remove from active_set (max_committed is unchanged).
        {
            let mut set = self.active_set.write().unwrap();
            set.remove(&txn_id);
            let new_lowest = set.iter().copied().min().unwrap_or(0);
            self.lowest_active_id.store(new_lowest, Ordering::Relaxed);
        }

        // Propagate the post-undo clustered roots back to TxnManager so that
        // subsequent calls to `clustered_root()` return the correct (restored) root.
        for (table_id, root_pid) in conn_txn.clustered_roots {
            self.last_clustered_roots.insert(table_id, root_pid);
        }

        Ok(())
    }

}
