//! Lazy range scan iterator over the B+ Tree.
//!
//! ## Leaf chain traversal via `next_leaf` (Phase 7.9)
//!
//! Each leaf stores a `next_leaf` pointer to the next leaf in ascending key
//! order. Split operations update the predecessor leaf's `next_leaf` to
//! maintain a correct chain under Copy-on-Write.
//!
//! The iterator follows `next_leaf` pointers at O(1) per leaf boundary,
//! instead of re-descending the tree at O(log n).

use std::ops::Bound;

use axiomdb_core::{error::DbError, RecordId};
use axiomdb_storage::StorageEngine;

use crate::page_layout::{cast_leaf, NULL_PAGE};

/// Lazy range scan iterator.
///
/// Each `next()` loads an entry from the current leaf.
/// When a leaf is exhausted, it traverses the tree to find the next one.
pub struct RangeIter<'a> {
    storage: &'a dyn StorageEngine,
    current_pid: u64,
    slot_idx: usize,
    from: Bound<Vec<u8>>,
    to: Bound<Vec<u8>>,
    done: bool,
}

impl<'a> RangeIter<'a> {
    pub(crate) fn new(
        storage: &'a dyn StorageEngine,
        _root_pid: u64,
        start_pid: u64,
        from: Bound<Vec<u8>>,
        to: Bound<Vec<u8>>,
    ) -> Self {
        Self {
            storage,
            current_pid: start_pid,
            slot_idx: 0,
            from,
            to,
            done: false,
        }
    }

    /// Checks whether `key` is within the lower bound of the range.
    fn above_lower(&self, key: &[u8]) -> bool {
        match &self.from {
            Bound::Unbounded => true,
            Bound::Included(lo) => key >= lo.as_slice(),
            Bound::Excluded(lo) => key > lo.as_slice(),
        }
    }

    /// Checks whether `key` is within the upper bound of the range.
    fn below_upper(&self, key: &[u8]) -> bool {
        match &self.to {
            Bound::Unbounded => true,
            Bound::Included(hi) => key <= hi.as_slice(),
            Bound::Excluded(hi) => key < hi.as_slice(),
        }
    }
}

impl<'a> Iterator for RangeIter<'a> {
    type Item = Result<(Vec<u8>, RecordId), DbError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        loop {
            if self.current_pid == NULL_PAGE {
                self.done = true;
                return None;
            }

            // Read the leaf and find the next in-range slot.
            // The block ensures the borrow of `page` expires before `find_next_leaf`.
            enum SlotResult {
                Found(Vec<u8>, RecordId),
                Before,
                After,
                Exhausted,
            }

            let result = {
                let page = match self.storage.read_page(self.current_pid) {
                    Ok(p) => p,
                    Err(e) => return Some(Err(e)),
                };
                let node = cast_leaf(&page);
                let num = node.num_keys();

                if self.slot_idx >= num {
                    SlotResult::Exhausted
                } else {
                    let key = node.key_at(self.slot_idx).to_vec();
                    let rid = node.rid_at(self.slot_idx);
                    self.slot_idx += 1;

                    if !self.above_lower(&key) {
                        SlotResult::Before
                    } else if !self.below_upper(&key) {
                        SlotResult::After
                    } else {
                        SlotResult::Found(key, rid)
                    }
                }
            };

            match result {
                SlotResult::Before => {
                    // Before the range: continue to the next slot
                    continue;
                }
                SlotResult::After => {
                    self.done = true;
                    return None;
                }
                SlotResult::Found(key, rid) => {
                    return Some(Ok((key, rid)));
                }
                SlotResult::Exhausted => {
                    // Phase 7.9: follow next_leaf pointer (O(1)) instead of
                    // re-descending the tree from root (O(log n)).
                    // Split operations maintain correct next_leaf under CoW.
                    let next_pid = {
                        let page = match self.storage.read_page(self.current_pid) {
                            Ok(p) => p,
                            Err(e) => {
                                self.done = true;
                                return Some(Err(e));
                            }
                        };
                        let node = cast_leaf(&page);
                        node.next_leaf_val()
                    };

                    if next_pid == NULL_PAGE {
                        self.done = true;
                        return None;
                    }

                    self.current_pid = next_pid;
                    self.slot_idx = 0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::BTree;
    use axiomdb_core::RecordId;
    use axiomdb_storage::MemoryStorage;

    fn rid(n: u64) -> RecordId {
        RecordId {
            page_id: n,
            slot_id: 0,
        }
    }

    fn build_tree(count: usize) -> BTree {
        let mut tree = BTree::new(Box::new(MemoryStorage::new()), None).unwrap();
        for i in 0..count {
            let key = format!("{:04}", i);
            tree.insert(key.as_bytes(), rid(i as u64)).unwrap();
        }
        tree
    }

    #[test]
    fn test_range_full_scan() {
        let tree = build_tree(100);
        let results: Vec<_> = tree
            .range(Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.len(), 100);
        for i in 0..99 {
            assert!(results[i].0 < results[i + 1].0, "out of order at index {i}");
        }
    }

    #[test]
    fn test_range_included_bounds() {
        let tree = build_tree(50);
        let from = b"0010".to_vec();
        let to = b"0020".to_vec();
        let results: Vec<_> = tree
            .range(
                Bound::Included(from.as_slice()),
                Bound::Included(to.as_slice()),
            )
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.len(), 11);
        assert_eq!(results.first().unwrap().0, b"0010");
        assert_eq!(results.last().unwrap().0, b"0020");
    }

    #[test]
    fn test_range_excluded_bounds() {
        let tree = build_tree(50);
        let from = b"0010".to_vec();
        let to = b"0020".to_vec();
        let results: Vec<_> = tree
            .range(
                Bound::Excluded(from.as_slice()),
                Bound::Excluded(to.as_slice()),
            )
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.len(), 9);
        assert_eq!(results.first().unwrap().0, b"0011");
        assert_eq!(results.last().unwrap().0, b"0019");
    }

    #[test]
    fn test_range_unbounded_start() {
        let tree = build_tree(30);
        let to = b"0009".to_vec();
        let results: Vec<_> = tree
            .range(Bound::Unbounded, Bound::Included(to.as_slice()))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.len(), 10);
    }

    #[test]
    fn test_range_unbounded_end() {
        let tree = build_tree(30);
        let from = b"0025".to_vec();
        let results: Vec<_> = tree
            .range(Bound::Included(from.as_slice()), Bound::Unbounded)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.len(), 5);
    }

    #[test]
    fn test_range_empty() {
        let tree = build_tree(10);
        let from = b"0099".to_vec();
        let to = b"0999".to_vec();
        let results: Vec<_> = tree
            .range(
                Bound::Included(from.as_slice()),
                Bound::Included(to.as_slice()),
            )
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(results.is_empty());
    }

    #[test]
    fn test_range_spans_multiple_leaves() {
        // Use enough keys to force multiple leaves
        let count = 500;
        let tree = build_tree(count);
        let results: Vec<_> = tree
            .range(Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.len(), count, "all keys must be returned");
        for i in 0..results.len() - 1 {
            assert!(results[i].0 < results[i + 1].0, "out of order at index {i}");
        }
    }
}
