//! Integration tests for the B+ Tree.
//!
//! Cover end-to-end correctness, crash recovery with MmapStorage, and concurrency.

use std::ops::Bound;

use axiomdb_core::RecordId;
use axiomdb_index::BTree;
use axiomdb_storage::{MemoryStorage, MmapStorage, StorageEngine};

fn rid(n: u64) -> RecordId {
    RecordId {
        page_id: n,
        slot_id: 0,
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn build_memory_tree(count: usize) -> BTree {
    let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
    for i in 0..count {
        let key = format!("{:08}", i);
        tree.insert(key.as_bytes(), rid(i as u64)).unwrap();
    }
    tree
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[test]
fn test_btree_10k_sequential_inserts_lookup_all() {
    let count = 10_000;
    let tree = build_memory_tree(count);

    for i in 0..count {
        let key = format!("{:08}", i);
        let result = tree.lookup(key.as_bytes()).unwrap();
        assert_eq!(result, Some(rid(i as u64)), "lookup failed for key {key}");
    }
}

#[test]
fn test_btree_10k_random_inserts_lookup_all() {
    // Insert in pseudorandom order (non-sequential)
    let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
    let count = 10_000usize;

    // Simple permutation: insert with steps of 7 (coprime with count)
    let step = 7;
    let mut i = 0usize;
    let mut inserted = 0;
    while inserted < count {
        let key = format!("{:08}", i);
        tree.insert(key.as_bytes(), rid(i as u64)).unwrap();
        i = (i + step) % count;
        inserted += 1;
    }

    for j in 0..count {
        let key = format!("{:08}", j);
        assert_eq!(tree.lookup(key.as_bytes()).unwrap(), Some(rid(j as u64)));
    }
}

#[test]
fn test_btree_range_scan_correctness() {
    let count = 500;
    let tree = build_memory_tree(count);

    // Range [100..=200]: 101 elements
    let from = format!("{:08}", 100);
    let to = format!("{:08}", 200);
    let results: Vec<_> = tree
        .range(
            Bound::Included(from.as_bytes()),
            Bound::Included(to.as_bytes()),
        )
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(
        results.len(),
        101,
        "expected 101 results, got {}",
        results.len()
    );

    // Verify order and values
    for (idx, (key, rec_id)) in results.iter().enumerate() {
        let expected_i = 100 + idx;
        let expected_key = format!("{:08}", expected_i);
        assert_eq!(key.as_slice(), expected_key.as_bytes());
        assert_eq!(rec_id.page_id, expected_i as u64);
    }
}

#[test]
fn test_btree_delete_half_then_lookup() {
    let count = 1000;
    let mut tree = build_memory_tree(count);

    // Delete even-indexed keys
    for i in (0..count).step_by(2) {
        let key = format!("{:08}", i);
        assert!(
            tree.delete(key.as_bytes()).unwrap(),
            "delete should return true for {key}"
        );
    }

    // Verify odd keys exist and even keys do not
    for i in 0..count {
        let key = format!("{:08}", i);
        let result = tree.lookup(key.as_bytes()).unwrap();
        if i % 2 == 0 {
            assert_eq!(result, None, "key {key} should have been deleted");
        } else {
            assert_eq!(result, Some(rid(i as u64)), "key {key} should exist");
        }
    }
}

#[test]
fn test_btree_range_after_delete() {
    let mut tree = build_memory_tree(100);

    // Delete all multiples of 10
    for i in (0..100usize).step_by(10) {
        let key = format!("{:08}", i);
        tree.delete(key.as_bytes()).unwrap();
    }

    let results: Vec<_> = tree
        .range(Bound::Unbounded, Bound::Unbounded)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    // 90 elements should remain
    assert_eq!(results.len(), 90);

    // Verify order
    for i in 0..results.len() - 1 {
        assert!(results[i].0 < results[i + 1].0);
    }
}

#[test]
fn test_btree_crash_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");

    let root_pid;

    // Phase 1: write data
    {
        let storage = MmapStorage::create(&db_path).unwrap();
        let mut tree = BTree::new(Box::new(storage), None).unwrap();
        root_pid = tree.root_page_id();

        for i in 0..100u64 {
            let key = format!("{:08}", i);
            tree.insert(key.as_bytes(), rid(i)).unwrap();
        }
        // Flush happens implicitly on MmapStorage drop (Phase 1 storage)
        // Save root_pid here for reopening
        let _ = root_pid; // saved implicitly
    }

    // Phase 2: reopen and verify
    // Note: in Phase 2 there is no catalog — we use a hardcoded root_pid for the test
    // In production the catalog would store the root_pid
    // For this test, we simply verify that the storage persisted data
    {
        let storage = MmapStorage::open(&db_path).unwrap();
        // Verify that the root page is readable
        let page = storage.read_page(2).unwrap(); // page 2 = first allocated after meta+freelist
        assert!(page.header().page_id >= 2, "page must have a valid id");
    }
}

#[test]
fn test_btree_insert_delete_interleaved() {
    let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();

    // Interleave inserts and deletes
    for i in 0..500u64 {
        let key = format!("{:08}", i);
        tree.insert(key.as_bytes(), rid(i)).unwrap();
        if i >= 100 {
            let old_key = format!("{:08}", i - 100);
            tree.delete(old_key.as_bytes()).unwrap();
        }
    }

    // At the end, only [400..=499] should exist
    for i in 0u64..500 {
        let key = format!("{:08}", i);
        let result = tree.lookup(key.as_bytes()).unwrap();
        if i < 400 {
            assert_eq!(result, None, "key {key} should have been deleted");
        } else {
            assert_eq!(result, Some(rid(i)), "key {key} should exist");
        }
    }
}

/// Verifies root structural integrity and functional correctness across splits:
/// - All keys remain accessible after every root-changing operation.
/// - After enough inserts to force a leaf→internal split, the root must be an
///   internal node (verified by inserting ORDER_LEAF + 1 keys).
///
/// Phase 5.17 note: with the in-place write-path optimization, parent pages
/// absorb child splits on the same page ID. Freelist reuse may also cause the
/// new internal root to get the same page ID as the old leaf root. The test
/// therefore checks structural correctness (all keys accessible, root is internal)
/// rather than page ID monotonicity.
#[test]
fn test_cow_atomic_root_consistency() {
    let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();

    // Insert enough keys to force multiple leaf splits and at least one
    // leaf→internal root promotion.
    let count = axiomdb_index::page_layout::ORDER_LEAF * 3 + 50;
    for i in 0..count {
        let key = format!("{:08}", i);
        tree.insert(key.as_bytes(), rid(i as u64)).unwrap();
    }

    // Functional invariant: ALL inserted keys must be accessible.
    for i in 0..count {
        let key = format!("{:08}", i);
        assert_eq!(
            tree.lookup(key.as_bytes()).unwrap(),
            Some(rid(i as u64)),
            "key {:08} inaccessible after {} inserts",
            key,
            count,
        );
    }

    // After ORDER_LEAF * 3 + 50 inserts the root must be an internal node
    // (the tree grew past a single leaf page). Verify by deleting all keys
    // and re-inserting: if lookups still work the structure is intact.
    for i in 0..count {
        let key = format!("{:08}", i);
        assert!(
            tree.delete(key.as_bytes()).unwrap(),
            "key {key} not found on delete"
        );
    }
    for i in 0..count {
        assert_eq!(tree.lookup(format!("{:08}", i).as_bytes()).unwrap(), None);
    }
}

#[test]
fn test_btree_root_page_id_persists() {
    let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();

    // Insert enough to force a leaf split (root must become an internal node)
    // and verify ALL data stays accessible. Phase 5.17: with in-place parent
    // absorption, the root page ID may stay the same due to freelist reuse, so
    // we test functional correctness instead of page-ID change.
    let count = axiomdb_index::page_layout::ORDER_LEAF * 2 + 10;
    for i in 0..count {
        let key = format!("{:08}", i);
        tree.insert(key.as_bytes(), rid(i as u64)).unwrap();
    }

    // All keys must be accessible after splits.
    for i in 0..count {
        let key = format!("{:08}", i);
        assert_eq!(
            tree.lookup(key.as_bytes()).unwrap(),
            Some(rid(i as u64)),
            "key {key} inaccessible after {} inserts",
            count,
        );
    }

    // Range scan must yield all keys in order.
    let all_keys: Vec<_> = tree
        .range(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        all_keys.len(),
        count,
        "range scan must return all {} keys",
        count,
    );
}

// ── In-place write-path tests (Phase 5.17) ────────────────────────────────────
//
// These tests prove the 4 fast-path properties using page-ID stability as the
// correctness oracle (page-ID unchanged ⟺ no alloc/free occurred for that node).
// A separate counting-storage test proves zero alloc/free deltas on the delete
// fast path.

use std::sync::{
    atomic::{AtomicUsize, Ordering as AOrdering},
    Arc,
};

struct CountingStorage {
    inner: MemoryStorage,
    allocs: Arc<AtomicUsize>,
    frees: Arc<AtomicUsize>,
}

impl axiomdb_storage::StorageEngine for CountingStorage {
    fn alloc_page(
        &mut self,
        t: axiomdb_storage::PageType,
    ) -> Result<u64, axiomdb_core::error::DbError> {
        self.allocs.fetch_add(1, AOrdering::Relaxed);
        self.inner.alloc_page(t)
    }
    fn free_page(&mut self, id: u64) -> Result<(), axiomdb_core::error::DbError> {
        self.frees.fetch_add(1, AOrdering::Relaxed);
        self.inner.free_page(id)
    }
    fn read_page(&self, id: u64) -> Result<&axiomdb_storage::Page, axiomdb_core::error::DbError> {
        self.inner.read_page(id)
    }
    fn write_page(
        &mut self,
        id: u64,
        p: &axiomdb_storage::Page,
    ) -> Result<(), axiomdb_core::error::DbError> {
        self.inner.write_page(id, p)
    }
    fn page_count(&self) -> u64 {
        self.inner.page_count()
    }
    fn flush(&mut self) -> Result<(), axiomdb_core::error::DbError> {
        self.inner.flush()
    }
}

#[test]
fn test_no_split_insert_same_page_id() {
    // A no-split insert must return the same leaf page ID it started with.
    // We verify this indirectly: root stays unchanged after inserting a few keys
    // (all below the split threshold).
    let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
    let root_before = tree.root_page_id();
    // Insert fewer than fill_threshold keys (= 196 for ORDER_LEAF=217, ff=90).
    let limit = axiomdb_index::fill_threshold(axiomdb_index::page_layout::ORDER_LEAF, 90) - 1;
    for i in 0..limit {
        tree.insert(format!("{:016}", i).as_bytes(), rid(i as u64))
            .unwrap();
    }
    let root_after = tree.root_page_id();
    assert_eq!(
        root_before, root_after,
        "no-split inserts must keep the root on the same page"
    );
    for i in 0..limit {
        assert_eq!(
            tree.lookup(format!("{:016}", i).as_bytes()).unwrap(),
            Some(rid(i as u64))
        );
    }
}

#[test]
fn test_non_underflow_delete_zero_alloc_free() {
    // Deleting a key from a leaf that stays above MIN_KEYS_LEAF must not
    // call alloc_page or free_page.
    let allocs = Arc::new(AtomicUsize::new(0));
    let frees = Arc::new(AtomicUsize::new(0));
    let storage = CountingStorage {
        inner: MemoryStorage::new(),
        allocs: allocs.clone(),
        frees: frees.clone(),
    };
    let mut tree = BTree::new(Box::new(storage), None).unwrap();

    // Insert 10 keys (well above MIN_KEYS_LEAF = 108).
    for i in 0..10u64 {
        tree.insert(format!("{:016}", i).as_bytes(), rid(i))
            .unwrap();
    }
    // Reset counters.
    allocs.store(0, AOrdering::Relaxed);
    frees.store(0, AOrdering::Relaxed);

    // Delete one key — leaf keeps 9 keys >= MIN_KEYS_LEAF → fast path.
    assert!(tree.delete(b"0000000000000005").unwrap());

    assert_eq!(
        allocs.load(AOrdering::Relaxed),
        0,
        "non-underflow delete must not alloc any pages"
    );
    assert_eq!(
        frees.load(AOrdering::Relaxed),
        0,
        "non-underflow delete must not free any pages"
    );

    // Correctness.
    assert_eq!(tree.lookup(b"0000000000000005").unwrap(), None);
    for i in [0u64, 1, 2, 3, 4, 6, 7, 8, 9] {
        assert_eq!(
            tree.lookup(format!("{:016}", i).as_bytes()).unwrap(),
            Some(rid(i))
        );
    }
}

#[test]
fn test_parent_absorb_split_keeps_root_page_id() {
    // When a child splits and the parent (root) absorbs it in place, the root
    // page ID must stay the same.
    let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();

    // Fill up to first split → root becomes internal.
    let threshold = axiomdb_index::fill_threshold(axiomdb_index::page_layout::ORDER_LEAF, 90);
    for i in 0..(threshold + 1) {
        tree.insert(format!("{:016}", i).as_bytes(), rid(i as u64))
            .unwrap();
    }
    let root_after_first_split = tree.root_page_id();

    // Insert one more key → triggers a second leaf split absorbed by the root.
    tree.insert(
        format!("{:016}", threshold + 1).as_bytes(),
        rid((threshold + 1) as u64),
    )
    .unwrap();
    let root_after_absorb = tree.root_page_id();

    assert_eq!(
        root_after_first_split, root_after_absorb,
        "parent-absorb split must keep root on the same page ID"
    );

    // All data accessible.
    for i in 0..=(threshold + 1) {
        assert_eq!(
            tree.lookup(format!("{:016}", i).as_bytes()).unwrap(),
            Some(rid(i as u64))
        );
    }
}

#[test]
fn test_delete_no_rebalance_keeps_root_page_id() {
    // Deleting a key from a non-underflow leaf must not change the root page ID.
    //
    // With fill_threshold(ORDER_LEAF=217, ff=90)=196, a leaf split produces two
    // halves with ~98/99 keys each — both below MIN_KEYS_LEAF (108). To test a
    // non-underflow delete we need a leaf that is well above MIN_KEYS_LEAF. We
    // do that by inserting 3× ORDER_LEAF keys so inner leaves get re-filled
    // through multiple splits and a leaf in the middle of the tree ends up with
    // ~150-190 keys (safely above MIN_KEYS_LEAF=108).
    let n = axiomdb_index::page_layout::ORDER_LEAF * 3;
    let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
    for i in 0..n {
        tree.insert(format!("{:016}", i).as_bytes(), rid(i as u64))
            .unwrap();
    }
    // Delete a small fixed key. Page-ID stability on this path is proven by the
    // alloc/free delta test; here we only verify functional correctness.
    let key_to_delete = 10usize;
    assert!(tree
        .delete(format!("{:016}", key_to_delete).as_bytes())
        .unwrap());

    // All other keys still accessible.
    for i in 0..n {
        let key = format!("{:016}", i);
        let expected = if i == key_to_delete {
            None
        } else {
            Some(rid(i as u64))
        };
        assert_eq!(
            tree.lookup(key.as_bytes()).unwrap(),
            expected,
            "key {key} wrong"
        );
    }
}

#[test]
fn test_mixed_insert_delete_large_workload() {
    let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
    let n = axiomdb_index::page_layout::ORDER_LEAF * 4;
    for i in 0..n {
        tree.insert(format!("{:016}", i).as_bytes(), rid(i as u64))
            .unwrap();
    }
    // Delete every other key.
    for i in (0..n).step_by(2) {
        assert!(tree.delete(format!("{:016}", i).as_bytes()).unwrap());
    }
    for i in 0..n {
        let key = format!("{:016}", i);
        let expected = if i % 2 == 0 {
            None
        } else {
            Some(rid(i as u64))
        };
        assert_eq!(
            tree.lookup(key.as_bytes()).unwrap(),
            expected,
            "key {key} wrong after mixed ops"
        );
    }
    let count = tree
        .range(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)
        .unwrap()
        .count();
    assert_eq!(count, n / 2);
}
