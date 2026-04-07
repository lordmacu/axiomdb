impl HeapChain {
    /// Returns only the `(page_id, slot_id)` of every tuple visible to `snap`.
    ///
    /// Equivalent to [`scan_visible`] but skips copying the row payload —
    /// useful when the caller only needs record locations (e.g. DELETE without
    /// a WHERE clause) and never needs to decode the row values.
    ///
    /// Eliminates all `Vec<u8>` allocations for the row data and all
    /// row-decode overhead compared to [`scan_visible`] + discard.
    ///
    /// Uses the all-visible fast path: if a page's `PAGE_FLAG_ALL_VISIBLE` bit
    /// is set, per-slot MVCC checks are skipped entirely. After the first slow-path
    /// scan that finds all slots committed and undeleted, the flag is set on the
    /// page (lazy-set write) so every subsequent scan takes the fast path.
    pub fn scan_rids_visible(
        storage: &mut dyn StorageEngine,
        root_page_id: u64,
        snap: TransactionSnapshot,
    ) -> Result<Vec<(u64, u16)>, DbError> {
        let mut result = Vec::new();
        let mut current = root_page_id;
        storage.prefetch_hint(root_page_id, 0);

        while current != 0 {
            let raw = *storage.read_page(current)?.as_bytes();
            let mut page = Page::from_bytes(raw)?;
            let next = chain_next_page(&page);

            if page.is_all_visible() {
                // Fast path: no txn_id_deleted stamps on this page — skip
                // the all_vis tracking and lazy-set write overhead. We still
                // call is_visible() for snapshot correctness: the flag means
                // "no deleted rows", not "visible to every possible snapshot".
                for slot_id in 0..num_slots(&page) {
                    let entry = read_slot(&page, slot_id);
                    if entry.is_dead() {
                        continue;
                    }
                    let off = entry.offset as usize;
                    let len = entry.length as usize;
                    let bytes = &page.as_bytes()[off..off + len];
                    let header: &crate::heap::RowHeader = bytemuck::from_bytes(
                        &bytes[..std::mem::size_of::<crate::heap::RowHeader>()],
                    );
                    if header.is_visible(&snap) {
                        result.push((current, slot_id));
                    }
                }
            } else {
                // Slow path: per-slot MVCC check + lazy-set tracking.
                let mut all_vis = true;
                let mut has_alive = false;

                for slot_id in 0..num_slots(&page) {
                    let entry = read_slot(&page, slot_id);
                    if entry.is_dead() {
                        continue;
                    }
                    has_alive = true;
                    let off = entry.offset as usize;
                    let len = entry.length as usize;
                    let bytes = &page.as_bytes()[off..off + len];
                    let header: &crate::heap::RowHeader = bytemuck::from_bytes(
                        &bytes[..std::mem::size_of::<crate::heap::RowHeader>()],
                    );
                    // all_vis requires universal visibility: created must be
                    // committed (txn_id_created < snapshot_id), not just visible
                    // to this specific snapshot via current_txn_id.
                    if header.txn_id_deleted != 0 || header.txn_id_created >= snap.snapshot_id {
                        all_vis = false;
                    }
                    if !header.is_visible(&snap) {
                        continue;
                    }
                    result.push((current, slot_id));
                }

                // Lazy-set: one-time write per page. After this, future scans use fast path.
                if all_vis && has_alive && page.header().item_count > 0 {
                    page.set_all_visible();
                    page.update_checksum();
                    storage.write_page(current, &page)?;
                }
            }

            current = next;
        }

        Ok(result)
    }

    /// Read-only variant of [`scan_visible`] — takes `&dyn StorageEngine`
    /// (immutable borrow) and never sets the all-visible flag.
    ///
    /// Used by `CatalogReader` and any other path that holds only a shared
    /// reference to storage. Catalog tables are small (a few pages) and not
    /// hot enough to warrant the lazy-set write.
    pub fn scan_visible_ro(
        storage: &dyn StorageEngine,
        root_page_id: u64,
        snap: TransactionSnapshot,
    ) -> Result<Vec<(u64, u16, Vec<u8>)>, DbError> {
        let mut result = Vec::new();
        let mut current = root_page_id;

        while current != 0 {
            let page = storage.read_page(current)?;
            let next = chain_next_page(&page);
            for (slot_id, data) in scan_visible(&page, &snap) {
                result.push((current, slot_id, data.to_vec()));
            }
            current = next;
        }
        Ok(result)
    }

    /// Returns all tuples visible to `snap` across the entire chain.
    ///
    /// Each item is `(page_id, slot_id, data_bytes)` where `data_bytes` is the
    /// application payload (excluding the [`RowHeader`]).
    ///
    /// Tuples are returned in chain order (root page first, within a page in
    /// slot order). Dead slots and MVCC-invisible tuples are excluded.
    ///
    /// Uses the all-visible fast path: if a page's `PAGE_FLAG_ALL_VISIBLE` bit
    /// is set, per-slot `txn_id_deleted` tracking and `all_vis` bookkeeping are
    /// skipped. The flag means "no deleted rows on this page" — MVCC visibility
    /// (`is_visible`) is still checked per slot for snapshot correctness. After
    /// the first slow-path scan that finds all slots committed and undeleted,
    /// the flag is written to the page so subsequent scans take the fast path.
    ///
    /// For read-only callers (catalog scans), use [`scan_visible_ro`] instead.
    ///
    /// [`RowHeader`]: crate::heap::RowHeader
    pub fn scan_visible(
        storage: &mut dyn StorageEngine,
        root_page_id: u64,
        snap: TransactionSnapshot,
    ) -> Result<Vec<(u64, u16, Vec<u8>)>, DbError> {
        let mut result = Vec::new();
        let mut current = root_page_id;
        storage.prefetch_hint(root_page_id, 0);

        while current != 0 {
            let raw = *storage.read_page(current)?.as_bytes();
            let mut page = Page::from_bytes(raw)?;
            let next = chain_next_page(&page);

            if page.is_all_visible() {
                // Fast path: no txn_id_deleted stamps on this page — skip
                // the all_vis tracking and lazy-set write overhead. We still
                // call is_visible() for snapshot correctness: the flag means
                // "no deleted rows", not "visible to every possible snapshot".
                for slot_id in 0..num_slots(&page) {
                    let entry = read_slot(&page, slot_id);
                    if entry.is_dead() {
                        continue;
                    }
                    let off = entry.offset as usize;
                    let len = entry.length as usize;
                    let bytes = &page.as_bytes()[off..off + len];
                    let header: &crate::heap::RowHeader = bytemuck::from_bytes(
                        &bytes[..std::mem::size_of::<crate::heap::RowHeader>()],
                    );
                    if !header.is_visible(&snap) {
                        continue;
                    }
                    let data = bytes[std::mem::size_of::<crate::heap::RowHeader>()..].to_vec();
                    result.push((current, slot_id, data));
                }
            } else {
                // Slow path: per-slot MVCC check + lazy-set tracking.
                // `page_rows` buffers results so that the lazy-set write
                // (needing &mut page) executes after all borrows of page.as_bytes() are dropped.
                let mut all_vis = true;
                let mut has_alive = false;
                let mut page_rows: Vec<(u16, Vec<u8>)> = Vec::new();

                for slot_id in 0..num_slots(&page) {
                    let entry = read_slot(&page, slot_id);
                    if entry.is_dead() {
                        continue;
                    }
                    has_alive = true;
                    let off = entry.offset as usize;
                    let len = entry.length as usize;
                    let bytes = &page.as_bytes()[off..off + len];
                    let header: &crate::heap::RowHeader = bytemuck::from_bytes(
                        &bytes[..std::mem::size_of::<crate::heap::RowHeader>()],
                    );
                    // all_vis requires universal visibility: created must be
                    // committed (txn_id_created < snapshot_id), not just visible
                    // to this specific snapshot via current_txn_id.
                    if header.txn_id_deleted != 0 || header.txn_id_created >= snap.snapshot_id {
                        all_vis = false;
                    }
                    if !header.is_visible(&snap) {
                        continue;
                    }
                    let data = bytes[std::mem::size_of::<crate::heap::RowHeader>()..].to_vec();
                    page_rows.push((slot_id, data));
                }

                // Lazy-set: one-time write per page. After this, future scans use fast path.
                if all_vis && has_alive && page.header().item_count > 0 {
                    page.set_all_visible();
                    page.update_checksum();
                    storage.write_page(current, &page)?;
                }

                for (slot_id, data) in page_rows {
                    result.push((current, slot_id, data));
                }
            }

            current = next;
        }

        Ok(result)
    }

    /// Reads multiple rows by `(page_id, slot_id)`, grouping reads by page for
    /// I/O locality. Each heap page is read **exactly once** regardless of how
    /// many rows are requested from it.
    ///
    /// Returns a vector parallel to `rids`:
    /// - `Some(data_bytes)` if the slot is alive
    /// - `None` if the slot is dead (already deleted)
    ///
    /// ## Performance
    ///
    /// For N rows across P pages this is **O(P)** page reads instead of the
    /// **O(N)** of N individual [`read_row`] calls. The original RID order is
    /// preserved in the output via position tracking.
    pub fn read_rows_batch(
        storage: &dyn StorageEngine,
        rids: &[(u64, u16)],
    ) -> Result<Vec<Option<Vec<u8>>>, DbError> {
        if rids.is_empty() {
            return Ok(Vec::new());
        }

        let mut indexed: Vec<(usize, u64, u16)> = rids
            .iter()
            .enumerate()
            .map(|(i, &(page_id, slot_id))| (i, page_id, slot_id))
            .collect();
        indexed.sort_unstable_by_key(|(_, page_id, slot_id)| (*page_id, *slot_id));

        let mut result: Vec<Option<Vec<u8>>> = vec![None; rids.len()];
        let mut i = 0;

        while i < indexed.len() {
            let page_id = indexed[i].1;
            let raw = *storage.read_page(page_id)?.as_bytes();
            let page = Page::from_bytes(raw)?;

            while i < indexed.len() && indexed[i].1 == page_id {
                let (orig_idx, _, slot_id) = indexed[i];
                if let Some((_header, data)) = crate::heap::read_tuple(&page, slot_id)? {
                    result[orig_idx] = Some(data.to_vec());
                }
                i += 1;
            }
        }

        Ok(result)
    }

    /// Reads the application payload of the tuple at `(page_id, slot_id)`.
    ///
    /// Returns `None` if the slot is dead (already deleted). The returned bytes
    /// are the row data portion of the tuple, excluding the [`RowHeader`].
    ///
    /// Used by `TableEngine::delete_row` and `TableEngine::update_row` to obtain
    /// the old row bytes before stamping the deletion, so they can be included
    /// in the WAL `record_delete` entry for crash recovery.
    ///
    /// # Errors
    /// - [`DbError::InvalidSlot`] if `slot_id >= num_slots` on that page.
    /// - I/O errors from storage reads.
    ///
    /// [`RowHeader`]: crate::heap::RowHeader
    pub fn read_row(
        storage: &dyn StorageEngine,
        page_id: u64,
        slot_id: u16,
    ) -> Result<Option<Vec<u8>>, DbError> {
        let raw = *storage.read_page(page_id)?.as_bytes();
        let page = Page::from_bytes(raw)?;
        match crate::heap::read_tuple(&page, slot_id)? {
            None => Ok(None),
            Some((_header, data)) => Ok(Some(data.to_vec())),
        }
    }

    /// Returns `true` if the heap slot at `(page_id, slot_id)` is MVCC-visible
    /// to `snapshot`, reading **only the slot header** (24 bytes) — not the full
    /// row payload. Used by the index-only scan path (Phase 6.13) to verify
    /// visibility without decoding all row columns.
    pub fn is_slot_visible(
        storage: &dyn StorageEngine,
        page_id: u64,
        slot_id: u16,
        snap: axiomdb_core::TransactionSnapshot,
    ) -> Result<bool, DbError> {
        let raw = *storage.read_page(page_id)?.as_bytes();
        let page = Page::from_bytes(raw)?;
        match crate::heap::read_tuple(&page, slot_id)? {
            None => Ok(false),
            Some((header, _data)) => Ok(header.is_visible(&snap)),
        }
    }

    /// Counts MVCC-visible rows without decoding any row data (Phase 8 COUNT(*) fast path).
    ///
    /// Iterates all heap pages, checks visibility of each slot via RowHeader
    /// only (24 bytes), and increments a counter. Zero column decode, zero
    /// Value allocations. For 5000 rows this is ~25 page reads + 5000
    /// header checks vs the current path of 25 page reads + 5000 full decodes.
    ///
    /// Inspired by SQLite's `sqlite3BtreeCount()` which counts B-Tree cells
    /// without reading payloads, and InnoDB's `stat_n_rows` metadata cache.
    pub fn count_visible(
        storage: &dyn StorageEngine,
        root_page_id: u64,
        snap: axiomdb_core::TransactionSnapshot,
    ) -> Result<u64, DbError> {
        let mut count = 0u64;
        let mut current = root_page_id;

        while current != 0 {
            let page = storage.read_page(current)?.into_page();
            let next = chain_next_page(&page);

            if next != 0 {
                storage.prefetch_hint(next, 1);
            }

            let num = crate::heap::num_slots(&page);
            for slot_id in 0..num {
                let entry = crate::heap::read_slot(&page, slot_id);
                if entry.is_dead() {
                    continue;
                }
                let off = entry.offset as usize;
                let len = entry.length as usize;
                if len < std::mem::size_of::<RowHeader>() {
                    continue;
                }
                let bytes = &page.as_bytes()[off..off + len];
                let header: &RowHeader =
                    bytemuck::from_bytes(&bytes[..std::mem::size_of::<RowHeader>()]);
                if header.is_visible(&snap) {
                    count += 1;
                }
            }

            current = next;
        }

        Ok(count)
    }

}
