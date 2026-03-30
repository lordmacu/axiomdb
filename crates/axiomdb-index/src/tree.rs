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

impl BTree {
    /// Creates or reopens a B+ Tree.
    pub fn new(
        mut storage: Box<dyn StorageEngine>,
        root_page_id: Option<u64>,
    ) -> Result<Self, DbError> {
        let root_pid = match root_page_id {
            Some(pid) => pid,
            None => {
                let pid = storage.alloc_page(PageType::Index)?;
                let mut page = Page::new(PageType::Index, pid);
                let leaf = cast_leaf_mut(&mut page);
                leaf.is_leaf = 1;
                leaf.set_num_keys(0);
                leaf.set_next_leaf(NULL_PAGE);
                page.update_checksum();
                storage.write_page(pid, &page)?;
                pid
            }
        };
        Ok(Self {
            storage,
            root_pid: AtomicU64::new(root_pid),
        })
    }

    pub fn root_page_id(&self) -> u64 {
        self.root_pid.load(Ordering::Acquire)
    }

    // ── Lookup ───────────────────────────────────────────────────────────────

    pub fn lookup(&self, key: &[u8]) -> Result<Option<RecordId>, DbError> {
        Self::check_key(key)?;
        let mut pid = self.root_pid.load(Ordering::Acquire);
        loop {
            // Read the page as a reference — no 16 KB copy.
            // The borrow ends at the bottom of each branch before the next read_page call.
            let page = self.storage.read_page(pid)?;
            if page.body()[0] == 1 {
                let node = cast_leaf(&page);
                return Ok(node.search(key).ok().map(|i| node.rid_at(i)));
            } else {
                let node = cast_internal(&page);
                let idx = node.find_child_idx(key);
                pid = node.child_at(idx);
                // `page` and `node` drop here — borrow released before next iteration
            }
        }
    }

    // ── Insert ───────────────────────────────────────────────────────────────

    pub fn insert(&mut self, key: &[u8], rid: RecordId) -> Result<(), DbError> {
        Self::check_key(key)?;
        let root = self.root_pid.load(Ordering::Acquire);
        match Self::insert_subtree(self.storage.as_mut(), root, key, rid, 90)? {
            InsertResult::Ok(new_root) => {
                // CAS ensures that if in Phase 7 there were a concurrent writer,
                // the second would fail instead of silently overwriting.
                // With &mut self (Phase 2) it always succeeds — the pattern is ready.
                self.root_pid
                    .compare_exchange(root, new_root, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| DbError::BTreeCorrupted {
                        msg: "root modified concurrently during insert".into(),
                    })?;
            }
            InsertResult::Split {
                left_pid,
                right_pid,
                sep,
            } => {
                let new_root = Self::alloc_root(self.storage.as_mut(), &sep, left_pid, right_pid)?;
                self.root_pid
                    .compare_exchange(root, new_root, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| DbError::BTreeCorrupted {
                        msg: "root modified concurrently during insert (split)".into(),
                    })?;
            }
        }
        Ok(())
    }

    fn insert_subtree(
        storage: &mut dyn StorageEngine,
        pid: u64,
        key: &[u8],
        rid: RecordId,
        fillfactor: u8,
    ) -> Result<InsertResult, DbError> {
        match NodeCopy::read(storage, pid)? {
            NodeCopy::Leaf(node) => Self::insert_leaf(storage, pid, node, key, rid, fillfactor),
            NodeCopy::Internal(node) => {
                let n = node.num_keys();
                let child_idx = node.find_child_idx(key);
                let child_pid = node.child_at(child_idx);

                match Self::insert_subtree(storage, child_pid, key, rid, fillfactor)? {
                    InsertResult::Ok(new_child_pid) => {
                        // If the child was updated in-place (same pid), the parent did not change:
                        // no need to rewrite it or update its child pointer.
                        if new_child_pid == child_pid {
                            return Ok(InsertResult::Ok(pid));
                        }
                        let new_pid = Self::in_place_update_child(
                            storage,
                            pid,
                            node,
                            child_idx,
                            new_child_pid,
                        )?;
                        Ok(InsertResult::Ok(new_pid))
                    }
                    InsertResult::Split {
                        left_pid,
                        right_pid,
                        sep,
                    } => {
                        let mut node2 = node;
                        node2.set_child_at(child_idx, left_pid);

                        if n < ORDER_INTERNAL {
                            // Parent has room: absorb separator + right child in place.
                            // Same page ID propagates upward → ancestors need no rewrite.
                            node2.insert_at(child_idx, &sep, right_pid);
                            let new_pid = Self::write_internal_same_pid(storage, pid, node2)?;
                            Ok(InsertResult::Ok(new_pid))
                        } else {
                            Self::split_internal(storage, pid, node2, child_idx, &sep, right_pid)
                        }
                    }
                }
            }
        }
    }

    fn insert_leaf(
        storage: &mut dyn StorageEngine,
        old_pid: u64,
        node: LeafNodePage,
        key: &[u8],
        rid: RecordId,
        fillfactor: u8,
    ) -> Result<InsertResult, DbError> {
        let ins_pos = match node.search(key) {
            Ok(_) => return Err(DbError::DuplicateKey),
            Err(pos) => pos,
        };

        // Split threshold: max keys before splitting.
        // fillfactor=100 → ORDER_LEAF (current behavior, no regression).
        // fillfactor=90  → ⌈0.90 × 217⌉ = 196 (default).
        // Internal pages always split at ORDER_INTERNAL (not fillfactor-controlled).
        let threshold = fill_threshold(ORDER_LEAF, fillfactor);
        if node.num_keys() < threshold {
            // In-place: with &mut self there are no concurrent readers, CoW not needed.
            let mut p = Page::new(PageType::Index, old_pid);
            let n = cast_leaf_mut(&mut p);
            *n = node;
            n.insert_at(ins_pos, key, rid);
            p.update_checksum();
            storage.write_page(old_pid, &p)?;
            return Ok(InsertResult::Ok(old_pid));
        }

        // Split
        let count = node.num_keys();
        let mut kl = [0u8; ORDER_LEAF + 1];
        let mut ks = [[0u8; MAX_KEY_LEN]; ORDER_LEAF + 1];
        let mut rs = [[0u8; 10]; ORDER_LEAF + 1];

        kl[..ins_pos].copy_from_slice(&node.key_lens[..ins_pos]);
        ks[..ins_pos].copy_from_slice(&node.keys[..ins_pos]);
        rs[..ins_pos].copy_from_slice(&node.rids[..ins_pos]);
        kl[ins_pos] = key.len() as u8;
        ks[ins_pos][..key.len()].copy_from_slice(key);
        rs[ins_pos] = crate::page_layout::encode_rid(rid);
        kl[ins_pos + 1..=count].copy_from_slice(&node.key_lens[ins_pos..count]);
        ks[ins_pos + 1..=count].copy_from_slice(&node.keys[ins_pos..count]);
        rs[ins_pos + 1..=count].copy_from_slice(&node.rids[ins_pos..count]);

        let total = count + 1;
        let mid = total / 2;
        let sep = ks[mid][..kl[mid] as usize].to_vec();

        let left_pid = storage.alloc_page(PageType::Index)?;
        let right_pid = storage.alloc_page(PageType::Index)?;

        {
            let mut p = Page::new(PageType::Index, left_pid);
            let ln = cast_leaf_mut(&mut p);
            ln.is_leaf = 1;
            ln.set_num_keys(mid);
            ln.set_next_leaf(right_pid);
            ln.key_lens[..mid].copy_from_slice(&kl[..mid]);
            ln.keys[..mid].copy_from_slice(&ks[..mid]);
            ln.rids[..mid].copy_from_slice(&rs[..mid]);
            p.update_checksum();
            storage.write_page(left_pid, &p)?;
        }
        {
            let rcount = total - mid;
            let mut p = Page::new(PageType::Index, right_pid);
            let rn = cast_leaf_mut(&mut p);
            rn.is_leaf = 1;
            rn.set_num_keys(rcount);
            rn.set_next_leaf(node.next_leaf_val());
            rn.key_lens[..rcount].copy_from_slice(&kl[mid..total]);
            rn.keys[..rcount].copy_from_slice(&ks[mid..total]);
            rn.rids[..rcount].copy_from_slice(&rs[mid..total]);
            p.update_checksum();
            storage.write_page(right_pid, &p)?;
        }

        storage.free_page(old_pid)?;
        Ok(InsertResult::Split {
            left_pid,
            right_pid,
            sep,
        })
    }

    fn split_internal(
        storage: &mut dyn StorageEngine,
        old_pid: u64,
        node: InternalNodePage,
        child_idx: usize,
        sep: &[u8],
        right_child: u64,
    ) -> Result<InsertResult, DbError> {
        let n = node.num_keys();
        let mut kl = [0u8; ORDER_INTERNAL + 1];
        let mut ks = [[0u8; MAX_KEY_LEN]; ORDER_INTERNAL + 1];
        let mut ch = [0u64; ORDER_INTERNAL + 2];

        kl[..n].copy_from_slice(&node.key_lens[..n]);
        ks[..n].copy_from_slice(&node.keys[..n]);
        for (i, c) in ch[..=n].iter_mut().enumerate() {
            *c = node.child_at(i);
        }

        // Insert sep at child_idx: shift [child_idx..n] one position to the right.
        // copy_within correctly handles child_idx == n (empty range → no-op).
        kl.copy_within(child_idx..n, child_idx + 1);
        ks.copy_within(child_idx..n, child_idx + 1);
        // children: shift [child_idx+1..=n] one position to the right
        ch.copy_within(child_idx + 1..=n, child_idx + 2);
        kl[child_idx] = sep.len() as u8;
        ks[child_idx].fill(0);
        ks[child_idx][..sep.len()].copy_from_slice(sep);
        ch[child_idx + 1] = right_child;

        let total = n + 1;
        let mid = total / 2;

        let new_sep = ks[mid][..kl[mid] as usize].to_vec();

        let left_pid = storage.alloc_page(PageType::Index)?;
        let right_pid = storage.alloc_page(PageType::Index)?;

        {
            let mut p = Page::new(PageType::Index, left_pid);
            let ln = cast_internal_mut(&mut p);
            ln.is_leaf = 0;
            ln.set_num_keys(mid);
            ln.key_lens[..mid].copy_from_slice(&kl[..mid]);
            ln.keys[..mid].copy_from_slice(&ks[..mid]);
            for (i, c) in ch[..=mid].iter().enumerate() {
                ln.set_child_at(i, *c);
            }
            p.update_checksum();
            storage.write_page(left_pid, &p)?;
        }

        let right_count = total - mid - 1;
        {
            let mut p = Page::new(PageType::Index, right_pid);
            let rn = cast_internal_mut(&mut p);
            rn.is_leaf = 0;
            rn.set_num_keys(right_count);
            rn.key_lens[..right_count].copy_from_slice(&kl[mid + 1..total]);
            rn.keys[..right_count].copy_from_slice(&ks[mid + 1..total]);
            for (i, c) in ch[mid + 1..=total].iter().enumerate() {
                rn.set_child_at(i, *c);
            }
            p.update_checksum();
            storage.write_page(right_pid, &p)?;
        }

        storage.free_page(old_pid)?;
        Ok(InsertResult::Split {
            left_pid,
            right_pid,
            sep: new_sep,
        })
    }

    fn alloc_root(
        storage: &mut dyn StorageEngine,
        sep: &[u8],
        left_pid: u64,
        right_pid: u64,
    ) -> Result<u64, DbError> {
        let pid = storage.alloc_page(PageType::Index)?;
        let mut p = Page::new(PageType::Index, pid);
        let n = cast_internal_mut(&mut p);
        n.is_leaf = 0;
        n.set_num_keys(1);
        n.set_child_at(0, left_pid);
        n.set_child_at(1, right_pid);
        n.key_lens[0] = sep.len() as u8;
        n.keys[0][..sep.len()].copy_from_slice(sep);
        p.update_checksum();
        storage.write_page(pid, &p)?;
        Ok(pid)
    }

    /// Persists `node` (a leaf) back to the **same** page `pid`.
    ///
    /// Never allocates or frees pages. Returns `pid` unchanged so callers
    /// can propagate `InsertResult::Ok(pid)` / `DeleteResult::Deleted { new_pid: pid }`.
    #[inline]
    fn write_leaf_same_pid(
        storage: &mut dyn StorageEngine,
        pid: u64,
        node: LeafNodePage,
    ) -> Result<u64, DbError> {
        let mut p = Page::new(PageType::Index, pid);
        *cast_leaf_mut(&mut p) = node;
        p.update_checksum();
        storage.write_page(pid, &p)?;
        Ok(pid)
    }

    /// Persists `node` (an internal node) back to the **same** page `pid`.
    ///
    /// Never allocates or frees pages. Returns `pid` unchanged.
    #[inline]
    fn write_internal_same_pid(
        storage: &mut dyn StorageEngine,
        pid: u64,
        node: InternalNodePage,
    ) -> Result<u64, DbError> {
        #[cfg(debug_assertions)]
        node.validate();
        let mut p = Page::new(PageType::Index, pid);
        *cast_internal_mut(&mut p) = node;
        p.update_checksum();
        storage.write_page(pid, &p)?;
        Ok(pid)
    }

    fn in_place_update_child(
        storage: &mut dyn StorageEngine,
        old_pid: u64,
        mut node: InternalNodePage,
        child_idx: usize,
        new_child: u64,
    ) -> Result<u64, DbError> {
        node.set_child_at(child_idx, new_child);
        Self::write_internal_same_pid(storage, old_pid, node)
    }

    // ── Delete ───────────────────────────────────────────────────────────────

    pub fn delete(&mut self, key: &[u8]) -> Result<bool, DbError> {
        Self::check_key(key)?;
        let root = self.root_pid.load(Ordering::Acquire);
        match Self::delete_subtree(self.storage.as_mut(), root, key, true)? {
            DeleteResult::NotFound => Ok(false),
            DeleteResult::Deleted { new_pid, .. } => {
                let final_root = Self::collapse_root(self.storage.as_mut(), new_pid)?;
                self.root_pid
                    .compare_exchange(root, final_root, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| DbError::BTreeCorrupted {
                        msg: "root modified concurrently during delete".into(),
                    })?;
                Ok(true)
            }
        }
    }

    fn delete_subtree(
        storage: &mut dyn StorageEngine,
        pid: u64,
        key: &[u8],
        is_root: bool,
    ) -> Result<DeleteResult, DbError> {
        match NodeCopy::read(storage, pid)? {
            NodeCopy::Leaf(node) => Self::delete_leaf(storage, pid, node, key, is_root),
            NodeCopy::Internal(node) => {
                let child_idx = node.find_child_idx(key);
                let child_pid = node.child_at(child_idx);

                match Self::delete_subtree(storage, child_pid, key, false)? {
                    DeleteResult::NotFound => Ok(DeleteResult::NotFound),
                    DeleteResult::Deleted {
                        new_pid: new_child_pid,
                        underfull,
                    } => {
                        if !underfull {
                            if new_child_pid == child_pid {
                                // Child stayed on the same page — parent's child pointer
                                // is already correct. No rewrite needed and the parent's
                                // key count is unchanged, so it cannot have become underfull.
                                return Ok(DeleteResult::Deleted {
                                    new_pid: pid,
                                    underfull: false,
                                });
                            }
                            // Child moved to a different page — update the pointer in place.
                            // in_place_update_child only changes a child pointer, not key count,
                            // so the parent cannot become underfull from this operation alone.
                            let new_pid = Self::in_place_update_child(
                                storage,
                                pid,
                                node,
                                child_idx,
                                new_child_pid,
                            )?;
                            Ok(DeleteResult::Deleted {
                                new_pid,
                                underfull: false,
                            })
                        } else {
                            // Child underflowed — structural rebalance (rotate/merge).
                            // Rebalance may change the parent's key count, so check underflow.
                            let new_pid =
                                Self::rebalance(storage, pid, node, child_idx, new_child_pid)?;
                            let underfull2 =
                                !is_root && Self::internal_underfull(storage, new_pid)?;
                            Ok(DeleteResult::Deleted {
                                new_pid,
                                underfull: underfull2,
                            })
                        }
                    }
                }
            }
        }
    }

    fn delete_leaf(
        storage: &mut dyn StorageEngine,
        old_pid: u64,
        mut node: LeafNodePage,
        key: &[u8],
        is_root: bool,
    ) -> Result<DeleteResult, DbError> {
        let idx = match node.search(key) {
            Ok(i) => i,
            Err(_) => return Ok(DeleteResult::NotFound),
        };
        node.remove_at(idx);

        // Fast path: root leaves and leaves that remain at or above MIN_KEYS_LEAF
        // do not need structural rebalancing. Write back to the same page ID so
        // the parent does not need to update its child pointer.
        let underfull = !is_root && node.num_keys() < MIN_KEYS_LEAF;
        if !underfull {
            let new_pid = Self::write_leaf_same_pid(storage, old_pid, node)?;
            return Ok(DeleteResult::Deleted {
                new_pid,
                underfull: false,
            });
        }

        // Structural path: leaf will underflow — allocate a replacement page so
        // the rebalance path (`rotate_left`, `rotate_right`, `merge_children`)
        // can produce a new page ID that the parent can update.
        let new_pid = storage.alloc_page(PageType::Index)?;
        let mut p = Page::new(PageType::Index, new_pid);
        *cast_leaf_mut(&mut p) = node;
        p.update_checksum();
        storage.write_page(new_pid, &p)?;
        storage.free_page(old_pid)?;

        Ok(DeleteResult::Deleted {
            new_pid,
            underfull: true,
        })
    }

    fn rebalance(
        storage: &mut dyn StorageEngine,
        parent_pid: u64,
        parent: InternalNodePage,
        child_idx: usize,
        child_pid: u64,
    ) -> Result<u64, DbError> {
        let n = parent.num_keys();
        let child_is_leaf = {
            let p = storage.read_page(child_pid)?;
            p.body()[0] == 1
        };
        let min_keys = if child_is_leaf {
            MIN_KEYS_LEAF
        } else {
            MIN_KEYS_INTERNAL
        };

        if child_idx > 0 {
            let left_pid = parent.child_at(child_idx - 1);
            if Self::node_key_count(storage, left_pid)? > min_keys {
                return Self::rotate_right(
                    storage,
                    parent_pid,
                    parent,
                    child_idx,
                    child_pid,
                    left_pid,
                    child_is_leaf,
                );
            }
        }
        if child_idx < n {
            let right_pid = parent.child_at(child_idx + 1);
            if Self::node_key_count(storage, right_pid)? > min_keys {
                return Self::rotate_left(
                    storage,
                    parent_pid,
                    parent,
                    child_idx,
                    child_pid,
                    right_pid,
                    child_is_leaf,
                );
            }
        }
        if child_idx > 0 {
            let left_pid = parent.child_at(child_idx - 1);
            return Self::merge_children(
                storage,
                parent_pid,
                parent,
                child_idx - 1,
                left_pid,
                child_pid,
                child_is_leaf,
            );
        }
        let right_pid = parent.child_at(child_idx + 1);
        Self::merge_children(
            storage,
            parent_pid,
            parent,
            child_idx,
            child_pid,
            right_pid,
            child_is_leaf,
        )
    }

    fn rotate_right(
        storage: &mut dyn StorageEngine,
        parent_pid: u64,
        mut parent: InternalNodePage,
        child_idx: usize,
        child_pid: u64,
        left_pid: u64,
        is_leaf: bool,
    ) -> Result<u64, DbError> {
        let np = storage.alloc_page(PageType::Index)?;
        let nc = storage.alloc_page(PageType::Index)?;
        let nl = storage.alloc_page(PageType::Index)?;

        if is_leaf {
            let mut left = match NodeCopy::read(storage, left_pid)? {
                NodeCopy::Leaf(n) => n,
                NodeCopy::Internal(_) => unreachable!(),
            };
            let mut child = match NodeCopy::read(storage, child_pid)? {
                NodeCopy::Leaf(n) => n,
                NodeCopy::Internal(_) => unreachable!(),
            };
            let lk = left.num_keys() - 1;
            let bk_len = left.key_lens[lk];
            let bk = left.keys[lk];
            let br = left.rids[lk];
            left.remove_at(lk);
            child.insert_at(
                0,
                &bk[..bk_len as usize],
                crate::page_layout::decode_rid(br),
            );

            parent.key_lens[child_idx - 1] = child.key_lens[0];
            parent.keys[child_idx - 1] = child.keys[0];
            parent.set_child_at(child_idx - 1, nl);
            parent.set_child_at(child_idx, nc);

            let mut lp = Page::new(PageType::Index, nl);
            *cast_leaf_mut(&mut lp) = left;
            lp.update_checksum();
            storage.write_page(nl, &lp)?;
            let mut cp = Page::new(PageType::Index, nc);
            *cast_leaf_mut(&mut cp) = child;
            cp.update_checksum();
            storage.write_page(nc, &cp)?;
        } else {
            let mut left = match NodeCopy::read(storage, left_pid)? {
                NodeCopy::Internal(n) => n,
                NodeCopy::Leaf(_) => unreachable!(),
            };
            let mut child = match NodeCopy::read(storage, child_pid)? {
                NodeCopy::Internal(n) => n,
                NodeCopy::Leaf(_) => unreachable!(),
            };
            let lk = left.num_keys() - 1;
            let sep_len = parent.key_lens[child_idx - 1];
            let sep_key = parent.keys[child_idx - 1];
            let last_ch = left.child_at(lk + 1);

            let cn = child.num_keys();
            // Shift existing keys right by 1 (positions 0..cn → 1..cn+1).
            // rotate_right(1) on [..cn] would move key[cn-1] to position 0
            // and lose it when sep_key overwrites position 0 — leaving position
            // cn with stale data and causing key_lens[cn] > MAX_KEY_LEN panics.
            for i in (0..cn).rev() {
                child.key_lens[i + 1] = child.key_lens[i];
                child.keys[i + 1] = child.keys[i];
            }
            for i in (0..=cn).rev() {
                let p = child.child_at(i);
                child.set_child_at(i + 1, p);
            }
            child.key_lens[0] = sep_len;
            child.keys[0] = sep_key;
            child.set_child_at(0, last_ch);
            child.set_num_keys(cn + 1);

            parent.key_lens[child_idx - 1] = left.key_lens[lk];
            parent.keys[child_idx - 1] = left.keys[lk];
            left.key_lens[lk] = 0;
            left.keys[lk].fill(0);
            left.set_num_keys(lk);

            parent.set_child_at(child_idx - 1, nl);
            parent.set_child_at(child_idx, nc);

            let mut lp = Page::new(PageType::Index, nl);
            *cast_internal_mut(&mut lp) = left;
            lp.update_checksum();
            storage.write_page(nl, &lp)?;
            #[cfg(debug_assertions)]
            child.validate();
            let mut cp = Page::new(PageType::Index, nc);
            *cast_internal_mut(&mut cp) = child;
            cp.update_checksum();
            storage.write_page(nc, &cp)?;
        }

        #[cfg(debug_assertions)]
        parent.validate();
        let mut pp = Page::new(PageType::Index, np);
        *cast_internal_mut(&mut pp) = parent;
        pp.update_checksum();
        storage.write_page(np, &pp)?;

        storage.free_page(parent_pid)?;
        storage.free_page(child_pid)?;
        storage.free_page(left_pid)?;
        Ok(np)
    }

    fn rotate_left(
        storage: &mut dyn StorageEngine,
        parent_pid: u64,
        mut parent: InternalNodePage,
        child_idx: usize,
        child_pid: u64,
        right_pid: u64,
        is_leaf: bool,
    ) -> Result<u64, DbError> {
        let np = storage.alloc_page(PageType::Index)?;
        let nc = storage.alloc_page(PageType::Index)?;
        let nr = storage.alloc_page(PageType::Index)?;

        if is_leaf {
            let mut child = match NodeCopy::read(storage, child_pid)? {
                NodeCopy::Leaf(n) => n,
                NodeCopy::Internal(_) => unreachable!(),
            };
            let mut right = match NodeCopy::read(storage, right_pid)? {
                NodeCopy::Leaf(n) => n,
                NodeCopy::Internal(_) => unreachable!(),
            };
            let bk_len = right.key_lens[0];
            let bk = right.keys[0];
            let br = right.rids[0];
            right.remove_at(0);
            let cn = child.num_keys();
            child.key_lens[cn] = bk_len;
            child.keys[cn] = bk;
            child.rids[cn] = br;
            child.set_num_keys(cn + 1);

            parent.key_lens[child_idx] = right.key_lens[0];
            parent.keys[child_idx] = right.keys[0];
            parent.set_child_at(child_idx, nc);
            parent.set_child_at(child_idx + 1, nr);

            let mut cp = Page::new(PageType::Index, nc);
            *cast_leaf_mut(&mut cp) = child;
            cp.update_checksum();
            storage.write_page(nc, &cp)?;
            let mut rp = Page::new(PageType::Index, nr);
            *cast_leaf_mut(&mut rp) = right;
            rp.update_checksum();
            storage.write_page(nr, &rp)?;
        } else {
            let mut child = match NodeCopy::read(storage, child_pid)? {
                NodeCopy::Internal(n) => n,
                NodeCopy::Leaf(_) => unreachable!(),
            };
            let mut right = match NodeCopy::read(storage, right_pid)? {
                NodeCopy::Internal(n) => n,
                NodeCopy::Leaf(_) => unreachable!(),
            };
            let cn = child.num_keys();
            child.key_lens[cn] = parent.key_lens[child_idx];
            child.keys[cn] = parent.keys[child_idx];
            child.set_child_at(cn + 1, right.child_at(0));
            child.set_num_keys(cn + 1);

            parent.key_lens[child_idx] = right.key_lens[0];
            parent.keys[child_idx] = right.keys[0];
            parent.set_child_at(child_idx, nc);
            parent.set_child_at(child_idx + 1, nr);
            right.remove_at(0, 0);

            #[cfg(debug_assertions)]
            child.validate();
            let mut cp = Page::new(PageType::Index, nc);
            *cast_internal_mut(&mut cp) = child;
            cp.update_checksum();
            storage.write_page(nc, &cp)?;
            #[cfg(debug_assertions)]
            right.validate();
            let mut rp = Page::new(PageType::Index, nr);
            *cast_internal_mut(&mut rp) = right;
            rp.update_checksum();
            storage.write_page(nr, &rp)?;
        }

        #[cfg(debug_assertions)]
        parent.validate();
        let mut pp = Page::new(PageType::Index, np);
        *cast_internal_mut(&mut pp) = parent;
        pp.update_checksum();
        storage.write_page(np, &pp)?;

        storage.free_page(parent_pid)?;
        storage.free_page(child_pid)?;
        storage.free_page(right_pid)?;
        Ok(np)
    }

    fn merge_children(
        storage: &mut dyn StorageEngine,
        parent_pid: u64,
        mut parent: InternalNodePage,
        sep_idx: usize,
        left_pid: u64,
        right_pid: u64,
        is_leaf: bool,
    ) -> Result<u64, DbError> {
        let mp = storage.alloc_page(PageType::Index)?;
        let npp = storage.alloc_page(PageType::Index)?;

        if is_leaf {
            let left = match NodeCopy::read(storage, left_pid)? {
                NodeCopy::Leaf(n) => n,
                NodeCopy::Internal(_) => unreachable!(),
            };
            let right = match NodeCopy::read(storage, right_pid)? {
                NodeCopy::Leaf(n) => n,
                NodeCopy::Internal(_) => unreachable!(),
            };
            let mut merged = left;
            let ln = left.num_keys();
            let rn = right.num_keys();
            merged.key_lens[ln..ln + rn].copy_from_slice(&right.key_lens[..rn]);
            merged.keys[ln..ln + rn].copy_from_slice(&right.keys[..rn]);
            merged.rids[ln..ln + rn].copy_from_slice(&right.rids[..rn]);
            merged.set_num_keys(ln + rn);
            merged.set_next_leaf(right.next_leaf_val());

            let mut pg = Page::new(PageType::Index, mp);
            *cast_leaf_mut(&mut pg) = merged;
            pg.update_checksum();
            storage.write_page(mp, &pg)?;
        } else {
            let left = match NodeCopy::read(storage, left_pid)? {
                NodeCopy::Internal(n) => n,
                NodeCopy::Leaf(_) => unreachable!(),
            };
            let right = match NodeCopy::read(storage, right_pid)? {
                NodeCopy::Internal(n) => n,
                NodeCopy::Leaf(_) => unreachable!(),
            };
            let mut merged = left;
            let ln = left.num_keys();
            let rn = right.num_keys();
            merged.key_lens[ln] = parent.key_lens[sep_idx];
            merged.keys[ln] = parent.keys[sep_idx];
            merged.key_lens[ln + 1..ln + 1 + rn].copy_from_slice(&right.key_lens[..rn]);
            merged.keys[ln + 1..ln + 1 + rn].copy_from_slice(&right.keys[..rn]);
            for i in 0..=rn {
                merged.set_child_at(ln + 1 + i, right.child_at(i));
            }
            merged.set_num_keys(ln + 1 + rn);

            #[cfg(debug_assertions)]
            merged.validate();
            let mut pg = Page::new(PageType::Index, mp);
            *cast_internal_mut(&mut pg) = merged;
            pg.update_checksum();
            storage.write_page(mp, &pg)?;
        }

        parent.set_child_at(sep_idx, mp);
        parent.remove_at(sep_idx, sep_idx + 1);

        #[cfg(debug_assertions)]
        parent.validate();
        let mut pp = Page::new(PageType::Index, npp);
        *cast_internal_mut(&mut pp) = parent;
        pp.update_checksum();
        storage.write_page(npp, &pp)?;

        storage.free_page(parent_pid)?;
        storage.free_page(left_pid)?;
        storage.free_page(right_pid)?;
        Ok(npp)
    }

    fn collapse_root(storage: &mut dyn StorageEngine, root_pid: u64) -> Result<u64, DbError> {
        let (is_empty_internal, only_child) = {
            let page = storage.read_page(root_pid)?;
            if page.body()[0] == 0 {
                let node = cast_internal(&page);
                if node.num_keys() == 0 {
                    (true, node.child_at(0))
                } else {
                    (false, 0)
                }
            } else {
                (false, 0)
            }
        };
        if is_empty_internal {
            storage.free_page(root_pid)?;
            return Ok(only_child);
        }
        Ok(root_pid)
    }

    // ── Range scan ───────────────────────────────────────────────────────────

    pub fn range<'a>(
        &'a self,
        from: std::ops::Bound<&[u8]>,
        to: std::ops::Bound<&[u8]>,
    ) -> Result<RangeIter<'a>, DbError> {
        use std::ops::Bound::*;

        let root_pid = self.root_pid.load(Ordering::Acquire);
        let start_pid = match from {
            Unbounded => self.leftmost_leaf()?,
            Included(k) | Excluded(k) => self.find_leaf_for(k)?,
        };

        Ok(RangeIter::new(
            self.storage.as_ref(),
            root_pid,
            start_pid,
            from.map(|k| k.to_vec()),
            to.map(|k| k.to_vec()),
        ))
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn check_key(key: &[u8]) -> Result<(), DbError> {
        if key.is_empty() || key.len() > MAX_KEY_LEN {
            return Err(DbError::KeyTooLong {
                len: key.len(),
                max: MAX_KEY_LEN,
            });
        }
        Ok(())
    }

    fn leftmost_leaf(&self) -> Result<u64, DbError> {
        let mut pid = self.root_pid.load(Ordering::Acquire);
        loop {
            match NodeCopy::read(self.storage.as_ref(), pid)? {
                NodeCopy::Leaf(_) => return Ok(pid),
                NodeCopy::Internal(n) => pid = n.child_at(0),
            }
        }
    }

    fn find_leaf_for(&self, key: &[u8]) -> Result<u64, DbError> {
        let mut pid = self.root_pid.load(Ordering::Acquire);
        loop {
            let page = self.storage.read_page(pid)?;
            if page.body()[0] == 1 {
                return Ok(pid);
            } else {
                let node = cast_internal(&page);
                pid = node.child_at(node.find_child_idx(key));
            }
        }
    }

    fn node_key_count(storage: &dyn StorageEngine, pid: u64) -> Result<usize, DbError> {
        Ok(match NodeCopy::read(storage, pid)? {
            NodeCopy::Leaf(n) => n.num_keys(),
            NodeCopy::Internal(n) => n.num_keys(),
        })
    }

    fn internal_underfull(storage: &dyn StorageEngine, pid: u64) -> Result<bool, DbError> {
        let page = storage.read_page(pid)?;
        Ok(cast_internal(&page).num_keys() < MIN_KEYS_INTERNAL)
    }

    // ── Static API (shared storage) ──────────────────────────────────────────
    //
    // These functions take an external `&mut dyn StorageEngine` instead of the
    // owned `self.storage`.  They are used when the caller already holds a
    // mutable borrow of storage (e.g., the SQL executor) and cannot hand it to
    // a `BTree` instance.

    /// Looks up `key` in the B-Tree rooted at `root_pid`.
    ///
    /// Equivalent to `BTree::lookup` but works with external storage.
    pub fn lookup_in(
        storage: &dyn StorageEngine,
        root_pid: u64,
        key: &[u8],
    ) -> Result<Option<RecordId>, DbError> {
        Self::check_key(key)?;
        let mut pid = root_pid;
        loop {
            let page = storage.read_page(pid)?;
            if page.body()[0] == 1 {
                let node = cast_leaf(&page);
                return Ok(node.search(key).ok().map(|i| node.rid_at(i)));
            } else {
                let node = cast_internal(&page);
                pid = node.child_at(node.find_child_idx(key));
            }
        }
    }

    /// Inserts `(key, rid)` into the B-Tree rooted at `*root_pid`.
    ///
    /// `fillfactor` (10–100) controls the leaf-page split threshold:
    /// a leaf splits when `num_keys >= ceil(ORDER_LEAF × fillfactor / 100)`.
    /// `fillfactor = 100` reproduces current behavior (split at full capacity).
    /// `fillfactor = 90` (the default) splits at ~90% capacity.
    ///
    /// Internal pages always split at `ORDER_INTERNAL` regardless of fillfactor.
    ///
    /// Updates `*root_pid` atomically if the root splits.
    pub fn insert_in(
        storage: &mut dyn StorageEngine,
        root_pid: &AtomicU64,
        key: &[u8],
        rid: RecordId,
        fillfactor: u8,
    ) -> Result<(), DbError> {
        Self::check_key(key)?;
        let root = root_pid.load(Ordering::Acquire);
        match Self::insert_subtree(storage, root, key, rid, fillfactor)? {
            InsertResult::Ok(new_root) => {
                root_pid.store(new_root, Ordering::Release);
            }
            InsertResult::Split {
                left_pid,
                right_pid,
                sep,
            } => {
                let new_root = Self::alloc_root(storage, &sep, left_pid, right_pid)?;
                root_pid.store(new_root, Ordering::Release);
            }
        }
        Ok(())
    }

    /// Range scan on the B-Tree rooted at `root_pid`.
    ///
    /// Returns an iterator over `(RecordId, key_bytes)` pairs in key order.
    /// `lo` / `hi` are inclusive bounds (pass `None` for unbounded).
    ///
    /// # Note
    /// This returns owned `(RecordId, Vec<u8>)` pairs rather than a lazy
    /// iterator to avoid lifetime conflicts with the caller's storage borrow.
    pub fn range_in(
        storage: &dyn StorageEngine,
        root_pid: u64,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Vec<(RecordId, Vec<u8>)>, DbError> {
        use std::ops::Bound;

        let from = match lo {
            None => Bound::Unbounded,
            Some(k) => Bound::Included(k),
        };
        let to = match hi {
            None => Bound::Unbounded,
            Some(k) => Bound::Included(k),
        };

        // Find the starting leaf.
        let start_pid = match &from {
            Bound::Unbounded => Self::leftmost_leaf_in(storage, root_pid)?,
            Bound::Included(k) | Bound::Excluded(k) => {
                Self::find_leaf_for_in(storage, root_pid, k)?
            }
        };

        let from_owned = from.map(|k| k.to_vec());
        let to_owned = to.map(|k| k.to_vec());

        let iter = RangeIter::new(storage, root_pid, start_pid, from_owned, to_owned);
        iter.map(|r| r.map(|(key, rid)| (rid, key))).collect()
    }

    /// Deletes `key` from the B-Tree rooted at `*root_pid`.
    ///
    /// Updates `*root_pid` if the root collapses after deletion.
    /// Returns `true` if the key was found and deleted, `false` if not found.
    pub fn delete_in(
        storage: &mut dyn StorageEngine,
        root_pid: &AtomicU64,
        key: &[u8],
    ) -> Result<bool, DbError> {
        Self::check_key(key)?;
        let root = root_pid.load(Ordering::Acquire);
        match Self::delete_subtree(storage, root, key, true)? {
            DeleteResult::NotFound => Ok(false),
            DeleteResult::Deleted { new_pid, .. } => {
                let final_root = Self::collapse_root(storage, new_pid)?;
                root_pid.store(final_root, Ordering::Release);
                Ok(true)
            }
        }
    }

    fn leftmost_leaf_in(storage: &dyn StorageEngine, root_pid: u64) -> Result<u64, DbError> {
        let mut pid = root_pid;
        loop {
            match NodeCopy::read(storage, pid)? {
                NodeCopy::Leaf(_) => return Ok(pid),
                NodeCopy::Internal(n) => pid = n.child_at(0),
            }
        }
    }

    fn find_leaf_for_in(
        storage: &dyn StorageEngine,
        root_pid: u64,
        key: &[u8],
    ) -> Result<u64, DbError> {
        let mut pid = root_pid;
        loop {
            let page = storage.read_page(pid)?;
            if page.body()[0] == 1 {
                return Ok(pid);
            } else {
                let node = cast_internal(&page);
                pid = node.child_at(node.find_child_idx(key));
            }
        }
    }

    /// Public accessor for the fill-factor split threshold (Phase 6.8).
    ///
    /// Returns the maximum number of keys a leaf page holds before splitting,
    /// given the configured `fillfactor` (10–100). `fillfactor = 100` returns
    /// `order` exactly — identical to the pre-6.8 behavior.
    ///
    /// Exposed as a public method so callers (e.g., integration tests and
    /// monitoring tools) can verify threshold values without re-implementing
    /// the ceiling-division formula.
    pub fn fill_threshold_pub(order: usize, fillfactor: u8) -> usize {
        fill_threshold(order, fillfactor)
    }

    // ── Bulk load (Phase 5.21b) ──────────────────────────────────────────────

    /// Builds a B-Tree from scratch given pre-sorted `(key, RecordId)` entries.
    ///
    /// This is a bottom-up bulk-load that fills leaf pages sequentially
    /// (append-only, no binary search) and constructs internal nodes in a
    /// single pass. For N entries the cost is ~ceil(N / threshold) page
    /// writes — compared to N × O(log N) I/O for N individual `insert_in` calls.
    ///
    /// # Preconditions
    /// - `entries` MUST be sorted ascending by key with no duplicates.
    /// - The tree at `old_root_pid` MUST be empty (only valid for fresh indexes).
    ///
    /// # Returns
    /// The page ID of the new root. The old root page is freed.
    ///
    /// # Errors
    /// - `DbError::StorageFull` if page allocation fails.
    pub fn bulk_load_sorted(
        storage: &mut dyn StorageEngine,
        old_root_pid: u64,
        entries: &[(&[u8], RecordId)],
        fillfactor: u8,
    ) -> Result<u64, DbError> {
        if entries.is_empty() {
            return Ok(old_root_pid);
        }

        debug_assert!(
            entries.windows(2).all(|w| w[0].0 < w[1].0),
            "bulk_load_sorted: entries must be sorted ascending with no duplicates"
        );

        let threshold = fill_threshold(ORDER_LEAF, fillfactor);

        // ── Phase 1: Build leaf pages ────────────────────────────────────────
        //
        // Strategy: buffer the current leaf in memory. When it reaches the
        // threshold, allocate the NEXT leaf page, set next_leaf on the current
        // leaf, write the current leaf, and start filling the next one.
        // This avoids re-reading pages to patch next_leaf pointers.

        let mut leaves: Vec<(u64, Vec<u8>)> = Vec::new(); // (page_id, first_key)

        let mut cur_pid = storage.alloc_page(PageType::Index)?;
        let mut cur_leaf = LeafNodePage::zeroed();
        cur_leaf.is_leaf = 1;
        let mut cur_first_key: Vec<u8> = Vec::new();

        for (key, rid) in entries {
            Self::check_key(key)?;

            if cur_leaf.num_keys() >= threshold {
                // Current leaf is full — allocate next, link, write current.
                let next_pid = storage.alloc_page(PageType::Index)?;
                cur_leaf.set_next_leaf(next_pid);

                let mut page = Page::new(PageType::Index, cur_pid);
                *cast_leaf_mut(&mut page) = cur_leaf;
                page.update_checksum();
                storage.write_page(cur_pid, &page)?;

                leaves.push((cur_pid, cur_first_key));

                // Reset for next leaf.
                cur_pid = next_pid;
                cur_leaf = LeafNodePage::zeroed();
                cur_leaf.is_leaf = 1;
                cur_first_key = Vec::new();
            }

            let pos = cur_leaf.num_keys();
            cur_leaf.key_lens[pos] = key.len() as u8;
            cur_leaf.keys[pos][..key.len()].copy_from_slice(key);
            cur_leaf.rids[pos] = crate::page_layout::encode_rid(*rid);
            cur_leaf.set_num_keys(pos + 1);

            if cur_first_key.is_empty() {
                cur_first_key = key.to_vec();
            }
        }

        // Write the last (possibly partial) leaf. next_leaf = NULL_PAGE (0).
        {
            let mut page = Page::new(PageType::Index, cur_pid);
            *cast_leaf_mut(&mut page) = cur_leaf;
            page.update_checksum();
            storage.write_page(cur_pid, &page)?;
            leaves.push((cur_pid, cur_first_key));
        }

        // Free the old (empty) root page.
        storage.free_page(old_root_pid)?;

        if leaves.len() == 1 {
            return Ok(leaves[0].0);
        }

        // ── Phase 2: Build internal nodes bottom-up ──────────────────────────
        //
        // Each iteration takes the current level's (page_id, separator)
        // entries and groups them into internal pages of up to
        // ORDER_INTERNAL children each.

        let mut level = leaves;

        while level.len() > 1 {
            let mut next_level: Vec<(u64, Vec<u8>)> = Vec::new();

            for chunk in level.chunks(ORDER_INTERNAL + 1) {
                // An internal node with K keys has K+1 children.
                // So a chunk of C entries → C-1 keys + C children.
                let int_pid = storage.alloc_page(PageType::Index)?;
                let mut node = InternalNodePage::zeroed();
                // is_leaf = 0 (already zeroed)

                // First child (no key — leftmost pointer).
                node.set_child_at(0, chunk[0].0);

                for (i, (child_pid, sep_key)) in chunk.iter().enumerate().skip(1) {
                    let key_idx = i - 1;
                    let klen = sep_key.len().min(MAX_KEY_LEN);
                    node.key_lens[key_idx] = klen as u8;
                    node.keys[key_idx][..klen].copy_from_slice(&sep_key[..klen]);
                    node.set_child_at(i, *child_pid);
                }
                node.set_num_keys(chunk.len() - 1);

                let mut page = Page::new(PageType::Index, int_pid);
                *cast_internal_mut(&mut page) = node;
                page.update_checksum();
                storage.write_page(int_pid, &page)?;

                next_level.push((int_pid, chunk[0].1.clone()));
            }

            level = next_level;
        }

        Ok(level[0].0)
    }

    // ── Batch delete (Phase 5.19) ─────────────────────────────────────────────

    /// Removes all keys in `sorted_keys` from the B-Tree rooted at `root_pid`.
    ///
    /// `sorted_keys` must be sorted ascending by byte order and contain exact
    /// encoded keys (same encoding as `insert_in`). Missing keys are silently
    /// skipped. Returns the number of keys actually removed.
    ///
    /// This is O(N + tree_height) instead of the O(N * log N) cost of N
    /// individual `delete_in` calls.
    pub fn delete_many_in(
        storage: &mut dyn StorageEngine,
        root_pid: &AtomicU64,
        sorted_keys: &[Vec<u8>],
    ) -> Result<usize, DbError> {
        if sorted_keys.is_empty() {
            return Ok(0);
        }
        for key in sorted_keys {
            Self::check_key(key)?;
        }
        let root = root_pid.load(Ordering::Acquire);
        let result = Self::batch_delete_subtree(storage, root, sorted_keys, true)?;
        let final_root = Self::collapse_root(storage, result.new_pid)?;
        root_pid.store(final_root, Ordering::Release);
        Ok(result.deleted)
    }

    fn batch_delete_subtree(
        storage: &mut dyn StorageEngine,
        pid: u64,
        sorted_keys: &[Vec<u8>],
        is_root: bool,
    ) -> Result<BatchDeleteResult, DbError> {
        if sorted_keys.is_empty() {
            return Ok(BatchDeleteResult {
                new_pid: pid,
                underfull: false,
                deleted: 0,
            });
        }
        match NodeCopy::read(storage, pid)? {
            NodeCopy::Leaf(node) => {
                Self::batch_delete_leaf(storage, pid, node, sorted_keys, is_root)
            }
            NodeCopy::Internal(node) => {
                Self::batch_delete_internal(storage, pid, node, sorted_keys, is_root)
            }
        }
    }

    /// Merge-delete pass on a single leaf: remove all `sorted_keys` that
    /// appear in the leaf in one linear scan. Writes the compacted leaf once.
    fn batch_delete_leaf(
        storage: &mut dyn StorageEngine,
        pid: u64,
        node: LeafNodePage,
        sorted_keys: &[Vec<u8>],
        is_root: bool,
    ) -> Result<BatchDeleteResult, DbError> {
        use bytemuck::Zeroable;
        let n = node.num_keys();
        let mut survivors = LeafNodePage::zeroed();
        survivors.is_leaf = 1;
        survivors.set_next_leaf(node.next_leaf_val());

        let mut deleted = 0usize;
        let mut ki = 0usize; // index into sorted_keys
        let mut sc = 0usize; // survivor count

        for li in 0..n {
            let leaf_key = node.key_at(li);
            // Advance past delete-keys that are strictly less than this leaf key
            // (they were not in this leaf — silently skip them).
            while ki < sorted_keys.len() && sorted_keys[ki].as_slice() < leaf_key {
                ki += 1;
            }
            if ki < sorted_keys.len() && sorted_keys[ki].as_slice() == leaf_key {
                deleted += 1;
                ki += 1; // consume this delete key
            } else {
                survivors.key_lens[sc] = node.key_lens[li];
                survivors.keys[sc] = node.keys[li];
                survivors.rids[sc] = node.rids[li];
                sc += 1;
            }
        }
        survivors.set_num_keys(sc);

        if deleted == 0 {
            return Ok(BatchDeleteResult {
                new_pid: pid,
                underfull: false,
                deleted: 0,
            });
        }

        let underfull = !is_root && sc < MIN_KEYS_LEAF;
        if !underfull {
            let new_pid = Self::write_leaf_same_pid(storage, pid, survivors)?;
            Ok(BatchDeleteResult {
                new_pid,
                underfull: false,
                deleted,
            })
        } else {
            let new_pid = storage.alloc_page(PageType::Index)?;
            let mut p = Page::new(PageType::Index, new_pid);
            *cast_leaf_mut(&mut p) = survivors;
            p.update_checksum();
            storage.write_page(new_pid, &p)?;
            storage.free_page(pid)?;
            Ok(BatchDeleteResult {
                new_pid,
                underfull: true,
                deleted,
            })
        }
    }

    /// Partition `sorted_keys` among the children of `node`, recurse once per
    /// affected child, then normalize the parent (update pointers + rebalance
    /// any underfull children) with a single parent write.
    fn batch_delete_internal(
        storage: &mut dyn StorageEngine,
        pid: u64,
        node: InternalNodePage,
        sorted_keys: &[Vec<u8>],
        is_root: bool,
    ) -> Result<BatchDeleteResult, DbError> {
        let n = node.num_keys();
        let num_children = n + 1;

        // ── 1. Partition sorted_keys by child range ───────────────────────────
        // child[i] holds keys in [ sep[i-1] , sep[i] ).
        // Since sorted_keys is sorted, a single left-to-right scan is enough.
        let mut key_ranges: Vec<(usize, usize)> = Vec::with_capacity(num_children);
        let mut pos = 0usize;
        for i in 0..n {
            let sep = node.key_at(i);
            let split = sorted_keys[pos..].partition_point(|k| k.as_slice() < sep);
            key_ranges.push((pos, pos + split));
            pos += split;
        }
        key_ranges.push((pos, sorted_keys.len()));

        // ── 2. Recurse into affected children ─────────────────────────────────
        let mut new_child_pids: Vec<u64> = (0..num_children).map(|i| node.child_at(i)).collect();
        let mut underfull_child_pids: Vec<u64> = Vec::new();
        let mut total_deleted = 0usize;

        for i in 0..num_children {
            let (lo, hi) = key_ranges[i];
            if lo == hi {
                continue; // no keys for this child
            }
            let child_pid = new_child_pids[i];
            let result =
                Self::batch_delete_subtree(storage, child_pid, &sorted_keys[lo..hi], false)?;
            new_child_pids[i] = result.new_pid;
            total_deleted += result.deleted;
            if result.underfull {
                underfull_child_pids.push(result.new_pid);
            }
        }

        if total_deleted == 0 {
            return Ok(BatchDeleteResult {
                new_pid: pid,
                underfull: false,
                deleted: 0,
            });
        }

        // ── 3. Apply updated child pointers to parent ─────────────────────────
        let mut updated_node = node;
        for (i, &new_pid) in new_child_pids.iter().enumerate() {
            updated_node.set_child_at(i, new_pid);
        }

        // Write parent with updated pointers (in-place: same pid).
        let mut current_parent_pid = Self::write_internal_same_pid(storage, pid, updated_node)?;

        // ── 4. Rebalance underfull children left-to-right ─────────────────────
        for underfull_pid in underfull_child_pids {
            // Re-read the current parent (may have changed from a previous rebalance).
            let parent_node = match NodeCopy::read(storage, current_parent_pid)? {
                NodeCopy::Internal(p) => p,
                NodeCopy::Leaf(_) => unreachable!("batch_delete_internal: parent became leaf"),
            };
            let nc = parent_node.num_keys();
            // Find the underfull child's current index by scanning child pointers.
            let child_idx = (0..=nc).find(|&j| parent_node.child_at(j) == underfull_pid);
            let child_idx = match child_idx {
                Some(idx) => idx,
                // Already merged into a sibling during a previous rebalance step.
                None => continue,
            };
            current_parent_pid = Self::rebalance(
                storage,
                current_parent_pid,
                parent_node,
                child_idx,
                underfull_pid,
            )?;
        }

        let underfull2 = !is_root && Self::internal_underfull(storage, current_parent_pid)?;
        Ok(BatchDeleteResult {
            new_pid: current_parent_pid,
            underfull: underfull2,
            deleted: total_deleted,
        })
    }
}

/// Result of a single batch-delete recursive call.
struct BatchDeleteResult {
    new_pid: u64,
    underfull: bool,
    deleted: usize,
}

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
