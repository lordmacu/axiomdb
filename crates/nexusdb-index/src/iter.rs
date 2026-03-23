//! Lazy range scan iterator over the B+ Tree.
//!
//! ## Why we do not use `next_leaf`
//!
//! With Copy-on-Write, each write to a leaf creates a new page_id. The left
//! leaf that pointed to `old_leaf_pid` via `next_leaf` is left with a pointer
//! to an already-freed page. To avoid this problem, the iterator traverses
//! the tree from the root whenever it needs to advance to the next leaf.
//!
//! **Cost**: O(log n) per leaf boundary crossing — acceptable for
//! range scans where most time is spent in the leaves.

use std::ops::Bound;

use nexusdb_core::{error::DbError, RecordId};
use nexusdb_storage::StorageEngine;

use crate::page_layout::{cast_internal, cast_leaf, NULL_PAGE};
use crate::prefix::CompressedNode;

/// Lazy range scan iterator.
///
/// Each `next()` loads an entry from the current leaf.
/// When a leaf is exhausted, it traverses the tree to find the next one.
pub struct RangeIter<'a> {
    storage: &'a dyn StorageEngine,
    root_pid: u64,
    current_pid: u64,
    slot_idx: usize,
    from: Bound<Vec<u8>>,
    to: Bound<Vec<u8>>,
    last_key: Option<Vec<u8>>, // last key returned (to find the next leaf)
    done: bool,
}

impl<'a> RangeIter<'a> {
    pub(crate) fn new(
        storage: &'a dyn StorageEngine,
        root_pid: u64,
        start_pid: u64,
        from: Bound<Vec<u8>>,
        to: Bound<Vec<u8>>,
    ) -> Self {
        Self {
            storage,
            root_pid,
            current_pid: start_pid,
            slot_idx: 0,
            from,
            to,
            last_key: None,
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

    /// Finds the next leaf after `after_key`.
    ///
    /// Traverses the tree from the root, descending to the node that would contain
    /// `after_key`, then climbs searching for the first right sibling, and
    /// finally descends to the leftmost leaf of that subtree.
    fn find_next_leaf(&self, after_key: &[u8]) -> Result<Option<u64>, DbError> {
        // 1. Descend and save the stack with (page_id, next_sibling_idx)
        let mut stack: Vec<(u64, usize)> = Vec::new();
        let mut pid = self.root_pid;

        loop {
            let page = self.storage.read_page(pid)?;
            if page.body()[0] == 1 {
                // We reached a leaf. Exit and search for the next sibling.
                break;
            }
            let node = cast_internal(page);
            let n = node.num_keys();

            // Use prefix compression to compare only suffixes in nodes
            // with keys sharing a common prefix (e.g., UUIDs, namespaced keys).
            let keys: Vec<Box<[u8]>> = (0..n)
                .map(|i| node.key_at(i).to_vec().into_boxed_slice())
                .collect();
            let children: Vec<u64> = (0..=n).map(|i| node.child_at(i)).collect();
            let compressed = CompressedNode::from_keys(&keys, children);
            let idx = compressed.find_child_idx(after_key);

            // Save this node with the index of the NEXT sibling (idx+1)
            stack.push((pid, idx + 1));
            pid = compressed.children[idx];
        }

        // 2. Climb until we find a right sibling
        loop {
            let Some((parent_pid, next_idx)) = stack.pop() else {
                return Ok(None); // No more leaves
            };

            let page = self.storage.read_page(parent_pid)?;
            let node = cast_internal(page);
            if next_idx <= node.num_keys() {
                // There is a child at next_idx → descend to the leftmost leaf
                let subtree_root = node.child_at(next_idx);
                return Ok(Some(Self::leftmost_leaf(self.storage, subtree_root)?));
            }
            // This node also has no more children → keep climbing
        }
    }

    /// Returns the page_id of the leftmost leaf of the subtree.
    fn leftmost_leaf(storage: &dyn StorageEngine, pid: u64) -> Result<u64, DbError> {
        let mut pid = pid;
        loop {
            let page = storage.read_page(pid)?;
            if page.body()[0] == 1 {
                return Ok(pid);
            }
            pid = cast_internal(page).child_at(0);
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
                let node = cast_leaf(page);
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
                    self.last_key = Some(key.clone());
                    return Some(Ok((key, rid)));
                }
                SlotResult::Exhausted => {
                    // Leaf exhausted: find the next one via tree traversal
                    let after_key = match self.last_key.take() {
                        Some(k) => k,
                        None => {
                            // Empty leaf or all entries were Before, no more
                            self.done = true;
                            return None;
                        }
                    };

                    match self.find_next_leaf(&after_key) {
                        Err(e) => {
                            self.done = true;
                            return Some(Err(e));
                        }
                        Ok(None) => {
                            self.done = true;
                            return None;
                        }
                        Ok(Some(next_pid)) => {
                            self.current_pid = next_pid;
                            self.slot_idx = 0;
                            self.last_key = Some(after_key);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::BTree;
    use nexusdb_core::RecordId;
    use nexusdb_storage::MemoryStorage;

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
