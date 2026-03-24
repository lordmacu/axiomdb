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
    heap::{insert_tuple, scan_visible},
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

    /// Returns all tuples visible to `snap` across the entire chain.
    ///
    /// Each item is `(page_id, slot_id, data_bytes)` where `data_bytes` is the
    /// application payload (excluding the [`RowHeader`]).
    ///
    /// Tuples are returned in chain order (root page first, within a page in
    /// slot order). Dead slots and MVCC-invisible tuples are excluded.
    ///
    /// [`RowHeader`]: crate::heap::RowHeader
    pub fn scan_visible(
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
                    let mut new_page = Page::new(PageType::Data, new_id);

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
                    dirty = false;

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
        let rows = HeapChain::scan_visible(&storage, root, snap).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, b"hello");

        // Snapshot before the insert sees nothing.
        let rows_before = HeapChain::scan_visible(&storage, root, snap_before).unwrap();
        assert_eq!(rows_before.len(), 0);
    }

    #[test]
    fn test_insert_multi_tuple_same_page() {
        let (mut storage, root) = storage_with_root();

        HeapChain::insert(&mut storage, root, b"row1", 1).unwrap();
        HeapChain::insert(&mut storage, root, b"row2", 1).unwrap();
        HeapChain::insert(&mut storage, root, b"row3", 1).unwrap();

        let snap = TransactionSnapshot::committed(1);
        let rows = HeapChain::scan_visible(&storage, root, snap).unwrap();
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
        let rows = HeapChain::scan_visible(&storage, root, snap).unwrap();
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
        let rows = HeapChain::scan_visible(&storage, root, snap).unwrap();
        assert_eq!(rows.len(), inserted, "all inserted rows must be visible");
    }

    #[test]
    fn test_scan_empty_chain_returns_empty() {
        let (storage, root) = storage_with_root();
        let snap = committed_snap();
        let rows = HeapChain::scan_visible(&storage, root, snap).unwrap();
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
        let scanned = HeapChain::scan_visible(&storage, root, snap).unwrap();
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
        let mut d1: Vec<Vec<u8>> = HeapChain::scan_visible(&s1, root1, snap)
            .unwrap()
            .into_iter()
            .map(|(_, _, d)| d)
            .collect();
        let mut d2: Vec<Vec<u8>> = HeapChain::scan_visible(&s2, root2, snap)
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
