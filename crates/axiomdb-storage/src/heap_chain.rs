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
    heap::{
        clear_deletion, insert_tuple, num_slots, read_slot, read_tuple_header, scan_visible,
        try_rewrite_tuple_same_slot_no_checksum, RowHeader,
    },
    local_page_batch::{batch_alloc_page, LocalPageBatch},
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

// ── HeapAppendHint ────────────────────────────────────────────────────────────

/// Transient runtime hint that caches the tail page of a heap chain.
///
/// Storing `root_page_id` alongside `tail_page_id` lets the cache detect
/// root-rotation (Phase 5.16 bulk-empty) and automatically invalidate.
///
/// This type is **never persisted** — it is a runtime-only optimization.
/// Any code path may discard it freely; correctness does not depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapAppendHint {
    /// The chain root page this hint was computed for.
    /// If the caller's `data_root_page_id != root_page_id`, the hint is invalid.
    pub root_page_id: u64,
    /// The last page of the chain at the time this hint was recorded.
    /// Must be validated (`chain_next_page == 0`) before use.
    pub tail_page_id: u64,
}

// ── HeapChain ─────────────────────────────────────────────────────────────────

/// Stateless operations over a linked list of slotted heap pages.
///
/// The chain is identified by its `root_page_id` (stored in the meta page).
/// All methods traverse the chain from the root on each call — there is no
/// cached state. This is intentional: the chain is short in practice (catalog
/// tables rarely exceed a few pages).
pub struct HeapChain;

include!("heap_chain_write.rs");
include!("heap_chain_scan.rs");
include!("heap_chain_batch.rs");

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use axiomdb_core::TransactionSnapshot;

    use crate::MemoryStorage;

    struct CountingStorage {
        inner: MemoryStorage,
        reads: Arc<AtomicUsize>,
        writes: Arc<AtomicUsize>,
    }

    impl crate::StorageEngine for CountingStorage {
        fn read_page(&self, page_id: u64) -> Result<crate::page_ref::PageRef, DbError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.inner.read_page(page_id)
        }

        fn write_page(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            self.inner.write_page(page_id, page)
        }

        fn write_page_under_page_lock(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            self.inner.write_page_under_page_lock(page_id, page)
        }

        fn alloc_page(&self, page_type: PageType) -> Result<u64, DbError> {
            self.inner.alloc_page(page_type)
        }

        fn free_page(&self, page_id: u64) -> Result<(), DbError> {
            self.inner.free_page(page_id)
        }

        fn flush(&self) -> Result<(), DbError> {
            self.inner.flush()
        }

        fn page_count(&self) -> u64 {
            self.inner.page_count()
        }

        fn page_lock_table(&self) -> &crate::page_lock::PageLockTable {
            self.inner.page_lock_table()
        }
    }

    /// Creates a MemoryStorage with one root heap page allocated.
    fn storage_with_root() -> (MemoryStorage, u64) {
        let storage = MemoryStorage::new();
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
        let (storage, root) = storage_with_root();
        let snap_before = committed_snap();

        // Insert with txn_id=1 (autocommit: visible to snapshot_id=2+).
        HeapChain::insert(&storage, root, b"hello", 1, None).unwrap();

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
        let (storage, root) = storage_with_root();

        HeapChain::insert(&storage, root, b"row1", 1, None).unwrap();
        HeapChain::insert(&storage, root, b"row2", 1, None).unwrap();
        HeapChain::insert(&storage, root, b"row3", 1, None).unwrap();

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
        let (storage, root) = storage_with_root();

        let (page_id, slot_id) = HeapChain::insert(&storage, root, b"alive", 1, None).unwrap();
        HeapChain::insert(&storage, root, b"also_alive", 1, None).unwrap();

        // Delete first tuple with txn_id=2.
        HeapChain::delete(&storage, page_id, slot_id, 2).unwrap();

        // Snapshot at max_committed=2 sees only the non-deleted row.
        let snap = TransactionSnapshot::committed(2);
        let rows = HeapChain::scan_visible(&storage, root, snap).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, b"also_alive");
    }

    #[test]
    fn test_chain_grows_when_page_full() {
        let (storage, root) = storage_with_root();

        // Fill the root page with large tuples until it overflows.
        // Each tuple is 4000 bytes of data + 24-byte RowHeader + 4-byte SlotEntry = 4028 bytes.
        // A 16KB page body (16320 bytes) fits ~4 such tuples before HeapPageFull.
        let big_data = vec![0xABu8; 4000];
        let mut inserted = 0usize;
        for _ in 0..10 {
            HeapChain::insert(&storage, root, &big_data, 1, None).unwrap();
            inserted += 1;
        }

        // At least one page must have been chained.
        let page = storage.read_page(root).unwrap();
        let next = chain_next_page(&page);
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
        let storage = MemoryStorage::new();
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
        assert_eq!(chain_next_page(&read_back), p2);
    }

    // ── HeapChain::insert_batch tests ─────────────────────────────────────────

    #[test]
    fn test_insert_batch_empty_is_noop() {
        let (storage, root) = storage_with_root();
        let result = HeapChain::insert_batch(&storage, root, &[], 1).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_insert_batch_single_page_matches_individual() {
        let n = 20usize;
        let rows: Vec<Vec<u8>> = (0..n).map(|i| vec![i as u8; 32]).collect();

        let (s1, root1) = storage_with_root();
        let batch_rids = HeapChain::insert_batch(&s1, root1, &rows, 1).unwrap();

        let (s2, root2) = storage_with_root();
        let mut indiv_rids = Vec::new();
        for row in &rows {
            indiv_rids.push(HeapChain::insert(&s2, root2, row, 1, None).unwrap());
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
        let (storage, root) = storage_with_root();
        let rids = HeapChain::insert_batch(&storage, root, &rows, 1).unwrap();
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

        let (s1, root1) = storage_with_root();
        HeapChain::insert_batch(&s1, root1, &rows, 1).unwrap();

        let (s2, root2) = storage_with_root();
        for row in &rows {
            HeapChain::insert(&s2, root2, row, 1, None).unwrap();
        }

        let snap = TransactionSnapshot::committed(1);
        let mut d1: Vec<Vec<u8>> = HeapChain::scan_visible(&s1, root1, snap.clone())
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

    #[test]
    fn test_read_rows_batch_preserves_order_and_reads_page_once() {
        let reads = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(AtomicUsize::new(0));
        let storage = CountingStorage {
            inner: MemoryStorage::new(),
            reads: reads.clone(),
            writes,
        };
        let root = storage.alloc_page(PageType::Data).unwrap();
        storage
            .write_page(root, &Page::new(PageType::Data, root))
            .unwrap();

        let rid1 = HeapChain::insert(&storage, root, b"alpha", 1, None).unwrap();
        let rid2 = HeapChain::insert(&storage, root, b"bravo", 1, None).unwrap();
        let rid3 = HeapChain::insert(&storage, root, b"charlie", 1, None).unwrap();
        assert_eq!(rid1.0, rid2.0);
        assert_eq!(rid2.0, rid3.0);

        reads.store(0, Ordering::Relaxed);

        let rows = HeapChain::read_rows_batch(
            &storage,
            &[(rid3.0, rid3.1), (rid1.0, rid1.1), (rid2.0, rid2.1)],
        )
        .unwrap();

        assert_eq!(
            rows,
            vec![
                Some(b"charlie".to_vec()),
                Some(b"alpha".to_vec()),
                Some(b"bravo".to_vec()),
            ],
            "batch read must preserve caller-visible RID order",
        );
        assert_eq!(
            reads.load(Ordering::Relaxed),
            1,
            "same-page batch read must read the page once",
        );
    }

    #[test]
    fn test_rewrite_batch_same_slot_reads_and_writes_page_once_per_batch() {
        let reads = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(AtomicUsize::new(0));
        let storage = CountingStorage {
            inner: MemoryStorage::new(),
            reads: reads.clone(),
            writes: writes.clone(),
        };
        let root = storage.alloc_page(PageType::Data).unwrap();
        storage
            .write_page(root, &Page::new(PageType::Data, root))
            .unwrap();

        let rid1 = HeapChain::insert(&storage, root, b"alpha", 1, None).unwrap();
        let rid2 = HeapChain::insert(&storage, root, b"bravo", 1, None).unwrap();
        assert_eq!(rid1.0, rid2.0, "test rows must share one page");
        let rid1 = axiomdb_core::RecordId {
            page_id: rid1.0,
            slot_id: rid1.1,
        };
        let rid2 = axiomdb_core::RecordId {
            page_id: rid2.0,
            slot_id: rid2.1,
        };

        reads.store(0, Ordering::Relaxed);
        writes.store(0, Ordering::Relaxed);

        let rewritten = HeapChain::rewrite_batch_same_slot(
            &storage,
            root,
            &[(rid1, b"ALPHA".to_vec()), (rid2, b"BRAVO".to_vec())],
            2,
        )
        .unwrap();

        assert_eq!(rewritten.len(), 2);
        assert!(rewritten.iter().all(Option::is_some));
        assert_eq!(
            reads.load(Ordering::Relaxed),
            1,
            "same-page batch rewrite must read the page once"
        );
        assert_eq!(
            writes.load(Ordering::Relaxed),
            1,
            "same-page batch rewrite must write the page once"
        );

        let rows =
            HeapChain::scan_visible(&storage, root, TransactionSnapshot::committed(2)).unwrap();
        let payloads: Vec<Vec<u8>> = rows.into_iter().map(|(_, _, d)| d).collect();
        assert!(payloads.contains(&b"ALPHA".to_vec()));
        assert!(payloads.contains(&b"BRAVO".to_vec()));
    }
}

// ── HeapAppendHint unit tests (Phase 5.18) ────────────────────────────────────
#[cfg(test)]
mod hint_tests {
    use super::*;
    use crate::memory::MemoryStorage;

    fn make_storage() -> MemoryStorage {
        MemoryStorage::new()
    }

    #[test]
    fn valid_hint_skips_walk() {
        let storage = make_storage();
        let root = storage.alloc_page(PageType::Data).unwrap();
        storage
            .write_page(root, &Page::new(PageType::Data, root))
            .unwrap();

        // First insert: no hint → seeds hint with root as tail.
        let mut hint = None;
        HeapChain::insert_with_hint(&storage, root, b"row1", 1, hint.as_mut(), None).unwrap();

        // Seed hint manually.
        let mut h = HeapAppendHint {
            root_page_id: root,
            tail_page_id: root,
        };
        // Second insert using hint.
        HeapChain::insert_with_hint(&storage, root, b"row2", 1, Some(&mut h), None).unwrap();
        // Hint must still point to root (no chain growth for 2 rows).
        assert_eq!(h.tail_page_id, root);
    }

    #[test]
    fn stale_hint_falls_back_and_heals() {
        let storage = make_storage();
        let root = storage.alloc_page(PageType::Data).unwrap();
        storage
            .write_page(root, &Page::new(PageType::Data, root))
            .unwrap();

        // Give a completely wrong tail_page_id.
        let mut h = HeapAppendHint {
            root_page_id: root,
            tail_page_id: 9999,
        };
        // Insert must succeed (falls back to full walk) and heals the hint.
        HeapChain::insert_with_hint(&storage, root, b"row1", 1, Some(&mut h), None).unwrap();
        assert_eq!(h.tail_page_id, root, "hint must be healed to actual tail");
    }

    #[test]
    fn root_mismatch_ignores_hint() {
        let storage = make_storage();
        let root1 = storage.alloc_page(PageType::Data).unwrap();
        storage
            .write_page(root1, &Page::new(PageType::Data, root1))
            .unwrap();
        let root2 = storage.alloc_page(PageType::Data).unwrap();
        storage
            .write_page(root2, &Page::new(PageType::Data, root2))
            .unwrap();

        // Hint for root1, but we insert into root2.
        let mut h = HeapAppendHint {
            root_page_id: root1,
            tail_page_id: root1,
        };
        HeapChain::insert_with_hint(&storage, root2, b"data", 1, Some(&mut h), None).unwrap();
        // Hint must now reflect root2.
        assert_eq!(h.root_page_id, root2);
        assert_eq!(h.tail_page_id, root2);
    }
}
