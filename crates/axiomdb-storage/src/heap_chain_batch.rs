impl HeapChain {
    /// Inserts multiple pre-encoded row payloads into the chain rooted at
    /// `root_page_id`, loading each heap page exactly **once** regardless of
    /// how many rows are written to it.
    ///
    /// ## Performance contract
    ///
    /// For N rows that span P pages, this method does P `read_page` + P `write_page`
    /// calls (plus one extra write per page transition for the chain pointer).
    /// The individual `insert()` method does N reads + N writes — i.e., this is
    /// `N/rows_per_page` times cheaper for large batches.
    ///
    /// ## Crash safety
    ///
    /// Each page is written before `record_insert()` is called for the rows it
    /// contains (that happens in `TableEngine::insert_rows_batch()`). The WAL
    /// BufWriter is not flushed here; durability comes from `TxnManager::commit()`.
    ///
    /// Chain growth follows the same two-write ordering as `insert()`:
    /// 1. Write the new page (with its rows) first.
    /// 2. Then update `next_page_id` in the previous page.
    ///
    /// ## Returns
    ///
    /// One `(page_id, slot_id)` per input row, in the same order as `rows`.
    /// Empty `rows` returns `Ok(vec![])` immediately.
    /// Batch insert with optional zone map updates (Phase 8.3b).
    ///
    /// `zm_values` is parallel to `rows`: `zm_values[i]` is `Some((col_idx, val))`
    /// if the i-th row has a zone-mappable numeric value. The zone map is updated
    /// in the page header BEFORE the checksum is computed.
    pub fn insert_batch(
        storage: &mut dyn StorageEngine,
        root_page_id: u64,
        rows: &[Vec<u8>],
        txn_id: TxnId,
    ) -> Result<Vec<(u64, u16)>, DbError> {
        Self::insert_batch_with_zm(storage, root_page_id, rows, txn_id, &[])
    }

    /// Core batch insert implementation with zone map support.
    pub fn insert_batch_with_zm(
        storage: &mut dyn StorageEngine,
        root_page_id: u64,
        rows: &[Vec<u8>],
        txn_id: TxnId,
        zm_values: &[Option<(u8, i64)>],
    ) -> Result<Vec<(u64, u16)>, DbError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        // Walk to the last page once, before the hot loop.
        let mut last_id = Self::last_page_id(storage, root_page_id)?;

        // Load the last page into a local copy.
        // Subsequent rows on the same page reuse this copy — no further reads.
        let mut page = Page::from_bytes(*storage.read_page(last_id)?.as_bytes())?;
        let mut dirty = false;
        let mut result = Vec::with_capacity(rows.len());

        for (row_idx, data) in rows.iter().enumerate() {
            match insert_tuple(&mut page, data, txn_id) {
                // ── Row fits on current page ───────────────────────────────────
                Ok(slot_id) => {
                    result.push((last_id, slot_id));
                    // Phase 8.3b: update zone map with this row's value.
                    if let Some(Some((col_idx, val))) = zm_values.get(row_idx) {
                        crate::zone_map::update_zone_map(&mut page, *col_idx, *val);
                    }
                    dirty = true;
                }

                // ── Current page is full → flush, allocate new, retry ─────────
                Err(DbError::HeapPageFull { .. }) => {
                    // Step 1: flush current page with its accumulated rows.
                    page.update_checksum();
                    storage.write_page(last_id, &page)?;

                    // Step 2: allocate an empty new page.
                    let new_id = storage.alloc_page(PageType::Data)?;
                    let new_page = Page::new(PageType::Data, new_id);

                    // Step 3: link — re-read the page we just wrote, set the
                    // chain pointer, and write it again.
                    // (Same two-write ordering as HeapChain::insert().)
                    let raw2 = *storage.read_page(last_id)?.as_bytes();
                    let mut prev = Page::from_bytes(raw2)?;
                    chain_set_next_page(&mut prev, new_id);
                    prev.update_checksum();
                    storage.write_page(last_id, &prev)?;

                    // Step 4: switch to the new page.
                    last_id = new_id;
                    page = new_page;

                    // Step 5: retry insert on the empty new page (guaranteed fit).
                    let slot_id = insert_tuple(&mut page, data, txn_id)?;
                    result.push((last_id, slot_id));
                    if let Some(Some((col_idx, val))) = zm_values.get(row_idx) {
                        crate::zone_map::update_zone_map(&mut page, *col_idx, *val);
                    }
                    dirty = true;
                }

                Err(other) => return Err(other),
            }
        }

        // Flush the last page if it has any unsaved rows.
        // (If the last row triggered a page transition, the new page is dirty.)
        if dirty {
            page.update_checksum();
            storage.write_page(last_id, &page)?;
        }

        Ok(result)
    }

    /// Scans every page in the chain rooted at `root_page_id` and clears
    /// `txn_id_deleted` on every slot that was deleted by `txn_id`.
    ///
    /// Used by ROLLBACK and crash recovery to undo a `WalEntry::Truncate`:
    /// each affected slot has its deletion stamp cleared, making the row
    /// visible again to future snapshots.
    ///
    /// Each page is read once, modified in-place for all matching slots,
    /// and written back once — O(P) page I/O for P pages in the chain.
    ///
    /// # Errors
    /// - I/O errors from storage reads/writes.
    pub fn clear_deletions_by_txn(
        storage: &mut dyn StorageEngine,
        root_page_id: u64,
        txn_id: TxnId,
    ) -> Result<(), DbError> {
        let mut current = root_page_id;

        while current != 0 {
            let raw = *storage.read_page(current)?.as_bytes();
            let mut page = Page::from_bytes(raw)?;
            let next = chain_next_page(&page);
            let n = num_slots(&page);
            let mut modified = false;

            for slot_id in 0..n {
                if let Some(deleted_by) = read_tuple_header(&page, slot_id)? {
                    if deleted_by == txn_id {
                        // clear_deletion is idempotent: safe to call even if
                        // already cleared (e.g., second recovery run).
                        match clear_deletion(&mut page, slot_id) {
                            Ok(()) => modified = true,
                            // AlreadyDeleted means the slot is physically dead
                            // (not just logically deleted) — skip it.
                            Err(axiomdb_core::DbError::AlreadyDeleted { .. }) => {}
                            Err(e) => return Err(e),
                        }
                    }
                }
            }

            if modified {
                // Checksum was already updated by each clear_deletion() call.
                storage.write_page(current, &page)?;
            }

            current = next;
        }

        Ok(())
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Walks the chain from `root_page_id` and returns the ID of the last page
    /// (the page whose `next_page_id == 0`).
    fn last_page_id(storage: &dyn StorageEngine, root_page_id: u64) -> Result<u64, DbError> {
        let mut current = root_page_id;
        loop {
            let page = storage.read_page(current)?;
            let next = chain_next_page(&page);
            if next == 0 {
                return Ok(current);
            }
            current = next;
        }
    }
}
