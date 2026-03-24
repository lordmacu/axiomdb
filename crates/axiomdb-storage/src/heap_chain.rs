//! Multi-page heap chain — links multiple slotted pages into a single logical heap.
//!
//! ## Chain layout
//!
//! The `PageHeader._reserved[0..8]` field stores `next_page_id: u64 LE` for
//! heap pages in a chain. Value `0` means "end of chain" (no next page).
//!
//! ```text
//! root_page ──next──► page_2 ──next──► page_3 ──next=0──► (end)
//! ```
//!
//! The root page ID for each system table is stored in the meta page and is
//! read via [`CatalogBootstrap::page_ids`].
//!
//! ## Crash safety: write order for chain growth
//!
//! When a page fills and a new page must be appended:
//! 1. Write the new page (with its data) to storage first.
//! 2. Then update `next_page_id` in the previous last page.
//!
//! If the process crashes between step 1 and 2, the new page is orphaned
//! (unreachable) but the chain is intact and consistent. The orphaned page
//! will be reclaimed by a future VACUUM. If the crash happens after step 2,
//! crash recovery can replay the WAL insert entry using the physical location
//! (page_id, slot_id) already recorded before this call.

use axiomdb_core::{error::DbError, TransactionSnapshot, TxnId};

use crate::{
    heap::{clear_deletion, insert_tuple, num_slots, read_slot, read_tuple_header, scan_visible},
    page::{Page, PageType},
    StorageEngine,
};

// Compile-time: _reserved must have room for 8 bytes (next_page_id).
const _: () = assert!(
    std::mem::size_of::<[u8; 28]>() >= 8,
    "_reserved must be at least 8 bytes"
);

// ── Chain pointer helpers ──────────────────────────────────────────────────────

/// Reads `next_page_id` from `PageHeader._reserved[0..8]`.
///
/// Returns `0` if this is the last page in the chain.
pub fn chain_next_page(page: &Page) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&page.header()._reserved[0..8]);
    u64::from_le_bytes(bytes)
}

/// Writes `next_page_id` into `PageHeader._reserved[0..8]`.
///
/// The caller must call `page.update_checksum()` and `storage.write_page()`
/// after this call to persist the change.
pub fn chain_set_next_page(page: &mut Page, next: u64) {
    page.header_mut()._reserved[0..8].copy_from_slice(&next.to_le_bytes());
}

// ── HeapChain ─────────────────────────────────────────────────────────────────

/// Stateless operations over a linked list of slotted heap pages.
///
/// The chain is identified by its `root_page_id` (stored in the meta page).
/// All methods traverse the chain from the root on each call — there is no
/// cached state. This is intentional: the chain is short in practice (catalog
/// tables rarely exceed a few pages).
pub struct HeapChain;

impl HeapChain {
    /// Inserts `data` with `txn_id` into the chain rooted at `root_page_id`.
    ///
    /// Walks to the last page in the chain. If that page is full, allocates a
    /// new `Data` page, links it to the chain, and inserts there.
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
    ) -> Result<(u64, u16), DbError> {
        // Walk to the last page.
        let last_page_id = Self::last_page_id(storage, root_page_id)?;

        // Try to insert into the last page.
        let raw = *storage.read_page(last_page_id)?.as_bytes();
        let mut page = Page::from_bytes(raw)?;

        match insert_tuple(&mut page, data, txn_id) {
            Ok(slot_id) => {
                page.update_checksum();
                storage.write_page(last_page_id, &page)?;
                Ok((last_page_id, slot_id))
            }
            Err(DbError::HeapPageFull { .. }) => {
                // Chain is full on last page. Allocate a new page.
                let new_page_id = storage.alloc_page(PageType::Data)?;
                let mut new_page = Page::new(PageType::Data, new_page_id);

                // Insert into the empty new page — guaranteed to fit.
                let slot_id = insert_tuple(&mut new_page, data, txn_id)?;
                new_page.update_checksum();

                // Step 1: write the new page (with data) first.
                storage.write_page(new_page_id, &new_page)?;

                // Step 2: update chain pointer in the previous last page.
                let raw2 = *storage.read_page(last_page_id)?.as_bytes();
                let mut prev_page = Page::from_bytes(raw2)?;
                chain_set_next_page(&mut prev_page, new_page_id);
                prev_page.update_checksum();
                storage.write_page(last_page_id, &prev_page)?;

                Ok((new_page_id, slot_id))
            }
            Err(e) => Err(e),
        }
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
        let raw = *storage.read_page(page_id)?.as_bytes();
        let mut page = Page::from_bytes(raw)?;
        crate::heap::delete_tuple(&mut page, slot_id, txn_id)?;
        page.update_checksum();
        storage.write_page(page_id, &page)?;
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

        while i < sorted.len() {
            let page_id = sorted[i].0;

            // ── Read page ONCE ────────────────────────────────────────────────
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
            storage.write_page(page_id, &page)?;
        }

        Ok(result)
    }

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
            let next = chain_next_page(page);
            for (slot_id, data) in scan_visible(page, &snap) {
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
    pub fn insert_batch(
        storage: &mut dyn StorageEngine,
        root_page_id: u64,
        rows: &[Vec<u8>],
        txn_id: TxnId,
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

        for data in rows {
            match insert_tuple(&mut page, data, txn_id) {
                // ── Row fits on current page ───────────────────────────────────
                Ok(slot_id) => {
                    result.push((last_id, slot_id));
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
            let next = chain_next_page(page);
            if next == 0 {
                return Ok(current);
            }
            current = next;
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_core::TransactionSnapshot;

    use crate::MemoryStorage;

    /// Creates a MemoryStorage with one root heap page allocated.
    fn storage_with_root() -> (MemoryStorage, u64) {
        let mut storage = MemoryStorage::new();
        let root = storage.alloc_page(PageType::Data).unwrap();
        let page = Page::new(PageType::Data, root);
        storage.write_page(root, &page).unwrap();
        (storage, root)
    }

    fn committed_snap() -> TransactionSnapshot {
        TransactionSnapshot::committed(0)
    }

    #[test]
    fn test_chain_next_page_default_zero() {
        let page = Page::new(PageType::Data, 1);
        assert_eq!(chain_next_page(&page), 0);
    }

    #[test]
    fn test_chain_set_and_get_next_page() {
        let mut page = Page::new(PageType::Data, 1);
        chain_set_next_page(&mut page, 42);
        assert_eq!(chain_next_page(&page), 42);
    }

    #[test]
    fn test_insert_single_page_found_in_scan() {
        let (mut storage, root) = storage_with_root();
        let snap_before = committed_snap();

        // Insert with txn_id=1 (autocommit: visible to snapshot_id=2+).
        HeapChain::insert(&mut storage, root, b"hello", 1).unwrap();

        // Snapshot that sees txn 1 as committed.
        let snap = TransactionSnapshot::committed(1);
        let rows = HeapChain::scan_visible(&mut storage, root, snap).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, b"hello");

        // Snapshot before the insert sees nothing.
        let rows_before = HeapChain::scan_visible(&mut storage, root, snap_before).unwrap();
        assert_eq!(rows_before.len(), 0);
    }

    #[test]
    fn test_insert_multi_tuple_same_page() {
        let (mut storage, root) = storage_with_root();

        HeapChain::insert(&mut storage, root, b"row1", 1).unwrap();
        HeapChain::insert(&mut storage, root, b"row2", 1).unwrap();
        HeapChain::insert(&mut storage, root, b"row3", 1).unwrap();

        let snap = TransactionSnapshot::committed(1);
        let rows = HeapChain::scan_visible(&mut storage, root, snap).unwrap();
        assert_eq!(rows.len(), 3);
        let payloads: Vec<&[u8]> = rows.iter().map(|(_, _, d)| d.as_slice()).collect();
        assert!(payloads.contains(&b"row1".as_slice()));
        assert!(payloads.contains(&b"row2".as_slice()));
        assert!(payloads.contains(&b"row3".as_slice()));
    }

    #[test]
    fn test_deleted_tuple_not_visible() {
        let (mut storage, root) = storage_with_root();

        let (page_id, slot_id) = HeapChain::insert(&mut storage, root, b"alive", 1).unwrap();
        HeapChain::insert(&mut storage, root, b"also_alive", 1).unwrap();

        // Delete first tuple with txn_id=2.
        HeapChain::delete(&mut storage, page_id, slot_id, 2).unwrap();

        // Snapshot at max_committed=2 sees only the non-deleted row.
        let snap = TransactionSnapshot::committed(2);
        let rows = HeapChain::scan_visible(&mut storage, root, snap).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, b"also_alive");
    }

    #[test]
    fn test_chain_grows_when_page_full() {
        let (mut storage, root) = storage_with_root();

        // Fill the root page with large tuples until it overflows.
        // Each tuple is 4000 bytes of data + 24-byte RowHeader + 4-byte SlotEntry = 4028 bytes.
        // A 16KB page body (16320 bytes) fits ~4 such tuples before HeapPageFull.
        let big_data = vec![0xABu8; 4000];
        let mut inserted = 0usize;
        for _ in 0..10 {
            HeapChain::insert(&mut storage, root, &big_data, 1).unwrap();
            inserted += 1;
        }

        // At least one page must have been chained.
        let page = storage.read_page(root).unwrap();
        let next = chain_next_page(page);
        assert_ne!(next, 0, "chain must have grown beyond root page");

        // All inserted rows must be visible.
        let snap = TransactionSnapshot::committed(1);
        let rows = HeapChain::scan_visible(&mut storage, root, snap).unwrap();
        assert_eq!(rows.len(), inserted, "all inserted rows must be visible");
    }

    #[test]
    fn test_scan_empty_chain_returns_empty() {
        let (mut storage, root) = storage_with_root();
        let snap = committed_snap();
        let rows = HeapChain::scan_visible(&mut storage, root, snap).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn test_chain_pointer_survives_write_page() {
        let mut storage = MemoryStorage::new();
        let p1 = storage.alloc_page(PageType::Data).unwrap();
        let p2 = storage.alloc_page(PageType::Data).unwrap();

        // Initialize both pages.
        let page2 = Page::new(PageType::Data, p2);
        storage.write_page(p2, &page2).unwrap();

        let mut page1 = Page::new(PageType::Data, p1);
        chain_set_next_page(&mut page1, p2);
        page1.update_checksum();
        storage.write_page(p1, &page1).unwrap();

        // Read back and verify chain pointer is preserved.
        let read_back = storage.read_page(p1).unwrap();
        assert_eq!(chain_next_page(read_back), p2);
    }

    // ── HeapChain::insert_batch tests ─────────────────────────────────────────

    #[test]
    fn test_insert_batch_empty_is_noop() {
        let (mut storage, root) = storage_with_root();
        let result = HeapChain::insert_batch(&mut storage, root, &[], 1).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_insert_batch_single_page_matches_individual() {
        let n = 20usize;
        let rows: Vec<Vec<u8>> = (0..n).map(|i| vec![i as u8; 32]).collect();

        let (mut s1, root1) = storage_with_root();
        let batch_rids = HeapChain::insert_batch(&mut s1, root1, &rows, 1).unwrap();

        let (mut s2, root2) = storage_with_root();
        let mut indiv_rids = Vec::new();
        for row in &rows {
            indiv_rids.push(HeapChain::insert(&mut s2, root2, row, 1).unwrap());
        }

        assert_eq!(batch_rids.len(), n);
        assert_eq!(
            batch_rids, indiv_rids,
            "batch must assign same (page_id, slot_id) pairs as individual inserts"
        );
    }

    #[test]
    fn test_insert_batch_multi_page_chain_growth() {
        // 300-byte rows → ~47 rows per 16KB page → 150 rows forces ~3 pages
        let n = 150usize;
        let rows: Vec<Vec<u8>> = (0..n).map(|i| vec![i as u8; 300]).collect();
        let (mut storage, root) = storage_with_root();
        let rids = HeapChain::insert_batch(&mut storage, root, &rows, 1).unwrap();
        assert_eq!(rids.len(), n, "all rows must be inserted");

        // committed(txn_id) makes txn visible: snapshot_id = txn_id+1 > txn_id_created
        let snap = TransactionSnapshot::committed(1);
        let scanned = HeapChain::scan_visible(&mut storage, root, snap).unwrap();
        assert_eq!(
            scanned.len(),
            n,
            "all rows must be scannable after batch insert"
        );
    }

    #[test]
    fn test_insert_batch_same_heap_contents_as_individual() {
        // 500 rows × 40 bytes each — exercises multiple pages
        let n = 500usize;
        let rows: Vec<Vec<u8>> = (0..n)
            .map(|i| {
                let mut r = vec![0u8; 40];
                r[0..8].copy_from_slice(&(i as u64).to_le_bytes());
                r
            })
            .collect();

        let (mut s1, root1) = storage_with_root();
        HeapChain::insert_batch(&mut s1, root1, &rows, 1).unwrap();

        let (mut s2, root2) = storage_with_root();
        for row in &rows {
            HeapChain::insert(&mut s2, root2, row, 1).unwrap();
        }

        let snap = TransactionSnapshot::committed(1);
        let mut d1: Vec<Vec<u8>> = HeapChain::scan_visible(&mut s1, root1, snap)
            .unwrap()
            .into_iter()
            .map(|(_, _, d)| d)
            .collect();
        let mut d2: Vec<Vec<u8>> = HeapChain::scan_visible(&mut s2, root2, snap)
            .unwrap()
            .into_iter()
            .map(|(_, _, d)| d)
            .collect();
        d1.sort();
        d2.sort();
        assert_eq!(
            d1, d2,
            "batch and individual insert must produce identical heap contents"
        );
    }
}
