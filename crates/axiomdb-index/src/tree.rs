//! Persistent B+ Tree with Copy-on-Write and an atomic root.
//!
//! ## Invariants
//! - An internal node with `n` keys has `n+1` children.
//! - For all `i`: `children[i]` contains keys `>= keys[i-1]` and `< keys[i]`.
//! - Leaves are linked in ascending order by `next_leaf`
//!   (though the iterator uses tree traversal to avoid stale pointers in CoW).
//! - Each write creates new pages before freeing the old ones.
//! - `root_pid` is updated with `AtomicU64::store(Release)` at the end of each mutation.

use std::sync::atomic::{AtomicU64, Ordering};

use axiomdb_core::{error::DbError, RecordId};
use axiomdb_storage::{Page, PageType, StorageEngine};

use crate::iter::RangeIter;
use crate::page_layout::{
    cast_internal, cast_internal_mut, cast_leaf, cast_leaf_mut, InternalNodePage, LeafNodePage,
    MAX_KEY_LEN, MIN_KEYS_INTERNAL, MIN_KEYS_LEAF, NULL_PAGE, ORDER_INTERNAL, ORDER_LEAF,
};
use bytemuck::Zeroable;

// ── Fill factor ──────────────────────────────────────────────────────────────

/// Returns the maximum number of keys a leaf page may hold before splitting.
///
/// Uses ceiling division so `fillfactor = 100` gives exactly `order`
/// (identical to the pre-6.8 behavior — no regression).
///
/// # Examples (ORDER_LEAF = 217)
/// ```text
/// fillfactor=100 → 217  (no change)
/// fillfactor=90  → 196  (default)
/// fillfactor=70  → 152
/// fillfactor=10  →  22  (minimum useful)
/// ```
pub const fn fill_threshold(order: usize, fillfactor: u8) -> usize {
    let ff = fillfactor as usize;
    // usize::div_ceil is stable since Rust 1.73; use it for correctness and
    // to satisfy clippy's "manually reimplementing div_ceil" lint.
    let t = (order * ff).div_ceil(100);
    if t < 1 {
        1
    } else {
        t
    }
}

// Compile-time guarantee: fillfactor=100 produces exactly ORDER_LEAF.
const _: () = assert!(
    fill_threshold(ORDER_LEAF, 100) == ORDER_LEAF,
    "fill_threshold(ORDER_LEAF, 100) must equal ORDER_LEAF — regression guard"
);

// ── Internal types ───────────────────────────────────────────────────────────

enum InsertResult {
    Ok(u64),
    Split {
        left_pid: u64,
        right_pid: u64,
        sep: Vec<u8>,
        /// `true` when the split was a leaf split (Phase 7.9: predecessor
        /// next_leaf must be updated).
        leaf_split: bool,
    },
}

enum DeleteResult {
    NotFound,
    Deleted { new_pid: u64, underfull: bool },
}

/// Copied version of a node read from a page (releases the storage borrow).
enum NodeCopy {
    Leaf(LeafNodePage),
    Internal(InternalNodePage),
}

impl NodeCopy {
    fn read(storage: &dyn StorageEngine, pid: u64) -> Result<Self, DbError> {
        let page = storage.read_page(pid)?;
        if page.body()[0] == 1 {
            Ok(Self::Leaf(*cast_leaf(&page)))
        } else {
            Ok(Self::Internal(*cast_internal(&page)))
        }
    }
}

// ── BTree ────────────────────────────────────────────────────────────────────

/// Persistent B+ Tree over a `StorageEngine`.
pub struct BTree {
    storage: Box<dyn StorageEngine>,
    root_pid: AtomicU64,
}

include!("tree_insert.rs");
include!("tree_delete.rs");
include!("tree_range.rs");
include!("tree_bulk.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_storage::MemoryStorage;

    fn rid(n: u64) -> RecordId {
        RecordId {
            page_id: n,
            slot_id: 0,
        }
    }

    #[test]
    fn test_insert_lookup_single() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        tree.insert(b"hello", rid(1)).unwrap();
        assert_eq!(tree.lookup(b"hello").unwrap(), Some(rid(1)));
        assert_eq!(tree.lookup(b"world").unwrap(), None);
    }

    #[test]
    fn test_duplicate_key_error() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        tree.insert(b"key", rid(1)).unwrap();
        assert!(matches!(
            tree.insert(b"key", rid(2)),
            Err(DbError::DuplicateKey)
        ));
    }

    #[test]
    fn test_key_too_long() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        let long_key = vec![b'x'; MAX_KEY_LEN + 1];
        assert!(matches!(
            tree.insert(&long_key, rid(1)),
            Err(DbError::KeyTooLong { .. })
        ));
    }

    #[test]
    fn test_insert_forces_leaf_split() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        for i in 0..=(ORDER_LEAF as u64) {
            let key = format!("{:08}", i);
            tree.insert(key.as_bytes(), rid(i)).unwrap();
        }
        for i in 0..=(ORDER_LEAF as u64) {
            let key = format!("{:08}", i);
            assert_eq!(tree.lookup(key.as_bytes()).unwrap(), Some(rid(i)));
        }
    }

    #[test]
    fn test_insert_forces_root_split() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        let count = ORDER_LEAF * 2 + 5;
        for i in 0..count {
            let key = format!("{:08}", i);
            tree.insert(key.as_bytes(), rid(i as u64)).unwrap();
        }
        for i in 0..count {
            let key = format!("{:08}", i);
            assert_eq!(tree.lookup(key.as_bytes()).unwrap(), Some(rid(i as u64)));
        }
    }

    #[test]
    fn test_delete_existing() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        tree.insert(b"aaa", rid(1)).unwrap();
        tree.insert(b"bbb", rid(2)).unwrap();
        tree.insert(b"ccc", rid(3)).unwrap();
        assert!(tree.delete(b"bbb").unwrap());
        assert_eq!(tree.lookup(b"bbb").unwrap(), None);
        assert_eq!(tree.lookup(b"aaa").unwrap(), Some(rid(1)));
        assert_eq!(tree.lookup(b"ccc").unwrap(), Some(rid(3)));
    }

    #[test]
    fn test_delete_nonexistent() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        tree.insert(b"aaa", rid(1)).unwrap();
        assert!(!tree.delete(b"zzz").unwrap());
    }

    #[test]
    fn test_insert_delete_1k_sequential() {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        let count = 1000usize;
        for i in 0..count {
            let key = format!("{:08}", i);
            tree.insert(key.as_bytes(), rid(i as u64)).unwrap();
        }
        for i in (0..count).step_by(2) {
            let key = format!("{:08}", i);
            assert!(tree.delete(key.as_bytes()).unwrap());
        }
        for i in 0..count {
            let key = format!("{:08}", i);
            let expected = if i % 2 == 0 {
                None
            } else {
                Some(rid(i as u64))
            };
            assert_eq!(tree.lookup(key.as_bytes()).unwrap(), expected, "key={key}");
        }
    }

    // ── Phase 5.21b: bulk_load_sorted ────────────────────────────────────────

    fn make_entries(n: usize) -> Vec<(Vec<u8>, RecordId)> {
        (0..n)
            .map(|i| {
                let key = format!("{:08}", i);
                (key.into_bytes(), rid(i as u64))
            })
            .collect()
    }

    fn bulk_setup() -> MemoryStorage {
        MemoryStorage::new()
    }

    fn alloc_empty_root(s: &mut MemoryStorage) -> u64 {
        use axiomdb_storage::PageType;
        let pid = s.alloc_page(PageType::Index).unwrap();
        let mut p = Page::new(PageType::Index, pid);
        let leaf = cast_leaf_mut(&mut p);
        leaf.is_leaf = 1;
        leaf.set_num_keys(0);
        p.update_checksum();
        s.write_page(pid, &p).unwrap();
        pid
    }

    #[test]
    fn test_bulk_load_empty_entries() {
        let mut s = bulk_setup();
        let root = alloc_empty_root(&mut s);
        let new_root = BTree::bulk_load_sorted(&mut s, root, &[], 90).unwrap();
        assert_eq!(new_root, root, "empty entries must return old root");
    }

    #[test]
    fn test_bulk_load_single_entry() {
        let mut s = bulk_setup();
        let root = alloc_empty_root(&mut s);
        let entries = make_entries(1);
        let refs: Vec<(&[u8], RecordId)> =
            entries.iter().map(|(k, r)| (k.as_slice(), *r)).collect();
        let new_root = BTree::bulk_load_sorted(&mut s, root, &refs, 90).unwrap();
        assert_ne!(new_root, root);
        assert_eq!(
            BTree::lookup_in(&mut s, new_root, entries[0].0.as_slice()).unwrap(),
            Some(entries[0].1)
        );
    }

    #[test]
    fn test_bulk_load_full_leaf() {
        let mut s = bulk_setup();
        let root = alloc_empty_root(&mut s);
        let threshold = fill_threshold(ORDER_LEAF, 90);
        let entries = make_entries(threshold);
        let refs: Vec<(&[u8], RecordId)> =
            entries.iter().map(|(k, r)| (k.as_slice(), *r)).collect();
        let new_root = BTree::bulk_load_sorted(&mut s, root, &refs, 90).unwrap();

        // All keys findable.
        for (key, rid) in &entries {
            assert_eq!(
                BTree::lookup_in(&mut s, new_root, key).unwrap(),
                Some(*rid),
                "key={}",
                String::from_utf8_lossy(key)
            );
        }
    }

    #[test]
    fn test_bulk_load_one_split() {
        let mut s = bulk_setup();
        let root = alloc_empty_root(&mut s);
        let threshold = fill_threshold(ORDER_LEAF, 90);
        let entries = make_entries(threshold + 1);
        let refs: Vec<(&[u8], RecordId)> =
            entries.iter().map(|(k, r)| (k.as_slice(), *r)).collect();
        let new_root = BTree::bulk_load_sorted(&mut s, root, &refs, 90).unwrap();

        for (key, rid) in &entries {
            assert_eq!(
                BTree::lookup_in(&mut s, new_root, key).unwrap(),
                Some(*rid),
                "key={}",
                String::from_utf8_lossy(key)
            );
        }
    }

    #[test]
    fn test_bulk_load_50k_entries() {
        let mut s = bulk_setup();
        let root = alloc_empty_root(&mut s);
        let entries = make_entries(50_000);
        let refs: Vec<(&[u8], RecordId)> =
            entries.iter().map(|(k, r)| (k.as_slice(), *r)).collect();
        let new_root = BTree::bulk_load_sorted(&mut s, root, &refs, 90).unwrap();

        // Spot-check first, middle, last.
        for &i in &[0, 25_000, 49_999] {
            let (key, rid) = &entries[i];
            assert_eq!(
                BTree::lookup_in(&mut s, new_root, key).unwrap(),
                Some(*rid),
                "key={}",
                String::from_utf8_lossy(key)
            );
        }

        // Range scan returns all in order.
        let all = BTree::range_in(&s, new_root, None, None).unwrap();
        assert_eq!(all.len(), 50_000);
        assert_eq!(all[0].1, entries[0].0);
        assert_eq!(all[49_999].1, entries[49_999].0);
    }
}
