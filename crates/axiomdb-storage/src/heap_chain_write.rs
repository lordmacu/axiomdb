impl HeapChain {
    /// Inserts `data` with `txn_id` into the chain rooted at `root_page_id`.
    ///
    /// Walks to the last page in the chain. If that page is full, allocates a
    /// new `Data` page, links it to the chain, and inserts there.
    ///
    /// When `batch` is `Some`, page allocation is satisfied from the local
    /// per-transaction batch (Phase 40.9 Tier-1), falling back to the global
    /// bitmap only on refill.
    ///
    /// Returns `(page_id, slot_id)` of the newly inserted tuple.
    ///
    /// # Errors
    /// - I/O errors from storage reads/writes.
    /// - [`DbError::HeapPageFull`] is never returned to the caller; it triggers
    ///   automatic chain growth instead.
    pub fn insert(
        storage: &mut dyn StorageEngine,
        root_page_id: u64,
        data: &[u8],
        txn_id: TxnId,
        batch: Option<&mut LocalPageBatch>,
    ) -> Result<(u64, u16), DbError> {
        let locks = storage.page_lock_table();

        // Walk to the last page.
        let last_page_id = Self::last_page_id(storage, root_page_id)?;

        // Acquire X-latch on the target page before modification.
        // PostgreSQL pattern: LockBuffer(BUFFER_LOCK_EXCLUSIVE) before heap insert.
        let _latch = locks.write(last_page_id);

        let raw = *storage.read_page(last_page_id)?.as_bytes();
        let mut page = Page::from_bytes(raw)?;

        match insert_tuple(&mut page, data, txn_id) {
            Ok(slot_id) => {
                page.update_checksum();
                storage.write_page_under_page_lock(last_page_id, &page)?;
                // _latch dropped here (RAII)
                Ok((last_page_id, slot_id))
            }
            Err(DbError::HeapPageFull { .. }) => {
                // Drop latch on full page before allocating (avoid holding
                // latch during potentially slow alloc).
                drop(_latch);

                // Chain is full on last page. Allocate a new page.
                let new_page_id = batch_alloc_page(storage, batch, PageType::Data)?;
                let mut new_page = Page::new(PageType::Data, new_page_id);

                // Insert into the empty new page — guaranteed to fit.
                let slot_id = insert_tuple(&mut new_page, data, txn_id)?;
                new_page.update_checksum();

                // Latch ordering: new_page before last_page (prevents deadlock).
                let _latch_new = locks.write(new_page_id);
                storage.write_page_under_page_lock(new_page_id, &new_page)?;

                // Re-latch the old last page to update the chain pointer.
                let _latch_prev = locks.write(last_page_id);
                let raw2 = *storage.read_page(last_page_id)?.as_bytes();
                let mut prev_page = Page::from_bytes(raw2)?;
                chain_set_next_page(&mut prev_page, new_page_id);
                prev_page.update_checksum();
                storage.write_page_under_page_lock(last_page_id, &prev_page)?;

                Ok((new_page_id, slot_id))
            }
            Err(e) => Err(e),
        }
    }

    /// Inserts `data` using a validated [`HeapAppendHint`] for O(1) tail lookup.
    ///
    /// If `hint` is `None`, `root_page_id` mismatches, or the hinted page is no
    /// longer the tail (`chain_next_page != 0`), falls back to a full chain walk
    /// via `last_page_id` and heals the hint with the actual tail.
    ///
    /// Chain growth (page full) always updates the hint to the new tail page.
    pub fn insert_with_hint(
        storage: &mut dyn StorageEngine,
        root_page_id: u64,
        data: &[u8],
        txn_id: TxnId,
        mut hint: Option<&mut HeapAppendHint>,
        batch: Option<&mut LocalPageBatch>,
    ) -> Result<(u64, u16), DbError> {
        // Resolve tail — use hint when valid, full walk otherwise.
        let last_page_id =
            Self::resolve_tail_with_hint(storage, root_page_id, hint.as_deref_mut())?;

        let locks = storage.page_lock_table();
        let _latch = locks.write(last_page_id);
        let raw = *storage.read_page(last_page_id)?.as_bytes();
        let mut page = Page::from_bytes(raw)?;

        match insert_tuple(&mut page, data, txn_id) {
            Ok(slot_id) => {
                page.update_checksum();
                storage.write_page_under_page_lock(last_page_id, &page)?;
                // Hint stays valid — same tail page, nothing changed.
                Ok((last_page_id, slot_id))
            }
            Err(DbError::HeapPageFull { .. }) => {
                drop(_latch);
                // Allocate new tail page and link it.
                let new_page_id = batch_alloc_page(storage, batch, PageType::Data)?;
                let mut new_page = Page::new(PageType::Data, new_page_id);
                let slot_id = insert_tuple(&mut new_page, data, txn_id)?;
                new_page.update_checksum();
                // Step 1: write new page first (crash safety).
                let _latch_new = locks.write(new_page_id);
                storage.write_page_under_page_lock(new_page_id, &new_page)?;
                // Step 2: link from previous tail.
                let _latch_prev = locks.write(last_page_id);
                let raw2 = *storage.read_page(last_page_id)?.as_bytes();
                let mut prev_page = Page::from_bytes(raw2)?;
                chain_set_next_page(&mut prev_page, new_page_id);
                prev_page.update_checksum();
                storage.write_page_under_page_lock(last_page_id, &prev_page)?;
                // Update hint to new tail.
                if let Some(h) = hint {
                    h.root_page_id = root_page_id;
                    h.tail_page_id = new_page_id;
                }
                Ok((new_page_id, slot_id))
            }
            Err(e) => Err(e),
        }
    }

    /// Resolves the tail page for a chain, using a hint for O(1) fast path.
    ///
    /// Validates the hint by checking `chain_next_page == 0`. On mismatch or
    /// absence, falls back to a full `last_page_id` walk and heals the hint.
    fn resolve_tail_with_hint(
        storage: &dyn StorageEngine,
        root_page_id: u64,
        hint: Option<&mut HeapAppendHint>,
    ) -> Result<u64, DbError> {
        if let Some(h) = hint {
            if h.root_page_id == root_page_id {
                // Validate: hinted page must exist and have next_page_id == 0.
                // PageNotFound or a non-zero next_page_id both mean the hint is stale.
                let valid = storage
                    .read_page(h.tail_page_id)
                    .map(|p| chain_next_page(&p) == 0)
                    .unwrap_or(false);
                if valid {
                    return Ok(h.tail_page_id);
                }
                // Hint is stale — fall through to full walk + self-heal.
            }
            // Hint root mismatch or stale — full walk + self-heal.
            let tail = Self::last_page_id(storage, root_page_id)?;
            h.root_page_id = root_page_id;
            h.tail_page_id = tail;
            return Ok(tail);
        }
        Self::last_page_id(storage, root_page_id)
    }

    /// Stamps `txn_id_deleted` on the tuple at `(page_id, slot_id)`.
    ///
    /// This is an MVCC deletion: the tuple remains on disk and is still visible
    /// to snapshots older than `txn_id`. It disappears from scans once all
    /// snapshots that predate the deletion have been released (VACUUM).
    pub fn delete(
        storage: &mut dyn StorageEngine,
        page_id: u64,
        slot_id: u16,
        txn_id: TxnId,
    ) -> Result<(), DbError> {
        let _latch = storage.page_lock_table().write(page_id);
        let raw = *storage.read_page(page_id)?.as_bytes();
        let mut page = Page::from_bytes(raw)?;
        crate::heap::delete_tuple(&mut page, slot_id, txn_id)?;
        page.update_checksum();
        storage.write_page_under_page_lock(page_id, &page)?;
        Ok(())
    }

    /// Deletes multiple tuples in a single pass — each heap page is read and
    /// written **exactly once**, regardless of how many slots are deleted on it.
    ///
    /// ## Algorithm
    ///
    /// 1. Sort `rids` by `page_id` so that all slots on the same page are
    ///    processed in one contiguous run.
    /// 2. For each page: read once, call [`mark_deleted`] for every slot on
    ///    that page, compute the checksum **once**, and write back.
    /// 3. Return `(page_id, slot_id, old_bytes)` for each input slot — the
    ///    `old_bytes` are the application payload extracted before marking dead,
    ///    required for WAL `record_delete` entries.
    ///
    /// ## Performance
    ///
    /// For N rows across P pages this is **O(P)** page I/O instead of the
    /// **O(3N)** of N individual [`delete`] calls (read + read + write per row).
    /// At ~200 rows/page, a 10 K-row DELETE goes from ~30 K page ops to ~100.
    ///
    /// ## WAL ordering invariant
    ///
    /// `write_page()` happens **before** the caller records WAL entries — the
    /// same ordering as [`insert_batch`] and the single-row [`delete`] path.
    ///
    /// ## Errors
    ///
    /// - [`DbError::AlreadyDeleted`] if any slot is already dead (fails fast,
    ///   prior pages may already have been written).
    /// - I/O errors from `storage`.
    pub fn delete_batch(
        storage: &mut dyn StorageEngine,
        root_page_id: u64,
        rids: &[(u64, u16)],
        txn_id: TxnId,
    ) -> Result<Vec<(u64, u16, Vec<u8>)>, DbError> {
        if rids.is_empty() {
            return Ok(vec![]);
        }
        storage.prefetch_hint(root_page_id, 0);

        // Sort by (page_id, slot_id) so all slots on the same page are adjacent.
        let mut sorted: Vec<(u64, u16)> = rids.to_vec();
        sorted.sort_unstable_by_key(|&(page_id, slot_id)| (page_id, slot_id));

        let mut result = Vec::with_capacity(rids.len());
        let mut i = 0;

        let locks = storage.page_lock_table();
        while i < sorted.len() {
            let page_id = sorted[i].0;

            // ── X-latch + Read page ONCE ─────────────────────────────────────
            let _latch = locks.write(page_id);
            let raw = *storage.read_page(page_id)?.as_bytes();
            let mut page = Page::from_bytes(raw)?;

            // ── Mark all slots on this page dead in-memory ────────────────────
            // `mark_deleted` stamps txn_id_deleted without recomputing the
            // checksum — we do that once below after all slots are processed.
            while i < sorted.len() && sorted[i].0 == page_id {
                let slot_id = sorted[i].1;

                // Extract old_bytes BEFORE marking dead.
                let old_bytes = match crate::heap::read_tuple(&page, slot_id)? {
                    None => {
                        return Err(DbError::AlreadyDeleted { page_id, slot_id });
                    }
                    Some((_header, data)) => data.to_vec(),
                };

                crate::heap::mark_deleted(&mut page, slot_id, txn_id)?;
                result.push((page_id, slot_id, old_bytes));
                i += 1;
            }

            // ── One checksum + one write for all slots on this page ───────────
            page.update_checksum();
            storage.write_page_under_page_lock(page_id, &page)?;
        }

        Ok(result)
    }

    /// Tries to rewrite multiple tuples in place, preserving their `(page_id, slot_id)`.
    ///
    /// Each input row provides the target `RecordId` and the new application payload
    /// bytes (already row-codec encoded, excluding the 24-byte `RowHeader`).
    ///
    /// Returns a vector parallel to `updates`:
    /// - `Some(old_tuple_image)` if the rewrite succeeded in place
    /// - `None` if the new tuple did not fit in the slot capacity and the caller
    ///   must fall back to delete+insert
    ///
    /// Pages are grouped by `page_id`, read once, mutated in memory for every
    /// successful same-slot rewrite on that page, and written once at the end.
    pub fn rewrite_batch_same_slot(
        storage: &mut dyn StorageEngine,
        root_page_id: u64,
        updates: &[(axiomdb_core::RecordId, Vec<u8>)],
        txn_id: TxnId,
    ) -> Result<Vec<Option<Vec<u8>>>, DbError> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        storage.prefetch_hint(root_page_id, 0);

        let mut indexed: Vec<(usize, u64, u16, &[u8])> = updates
            .iter()
            .enumerate()
            .map(|(i, (rid, new_row))| (i, rid.page_id, rid.slot_id, new_row.as_slice()))
            .collect();
        indexed.sort_unstable_by_key(|(_, page_id, slot_id, _)| (*page_id, *slot_id));

        let mut result: Vec<Option<Vec<u8>>> = vec![None; updates.len()];
        let mut i = 0;
        while i < indexed.len() {
            let page_id = indexed[i].1;
            let _latch = storage.page_lock_table().write(page_id);
            // Use into_page() to avoid 16KB copy from PageRef.
            let mut page = storage.read_page(page_id)?.into_page();
            let mut dirty = false;

            while i < indexed.len() && indexed[i].1 == page_id {
                let (orig_idx, _, slot_id, new_row) = indexed[i];
                if let Some(old_image) =
                    try_rewrite_tuple_same_slot_no_checksum(&mut page, slot_id, new_row, txn_id)?
                {
                    result[orig_idx] = Some(old_image);
                    dirty = true;
                }
                i += 1;
            }

            if dirty {
                page.update_checksum();
                storage.write_page_under_page_lock(page_id, &page)?;
            }
        }

        Ok(result)
    }

}
