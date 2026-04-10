impl BTree {
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

    /// Phase 7.9: after a leaf split at `child_idx` in `parent`, update the
    /// predecessor leaf's `next_leaf` to point to `new_left_pid`.
    ///
    /// The predecessor is the rightmost leaf of the subtree rooted at
    /// `parent.child(child_idx - 1)`. Cost: O(height) descent — acceptable
    /// because splits are rare (~1 per ORDER_LEAF inserts).
    fn update_predecessor_next_leaf(
        storage: &mut dyn StorageEngine,
        parent: &InternalNodePage,
        child_idx: usize,
        new_left_pid: u64,
    ) -> Result<(), DbError> {
        debug_assert!(child_idx > 0, "child_idx must be > 0 for predecessor");
        let left_sibling_pid = parent.child_at(child_idx - 1);
        let pred_pid = Self::descend_rightmost_leaf(storage, left_sibling_pid)?;

        let _pred_guard = storage.page_lock_table().write(pred_pid);
        let raw = *storage.read_page(pred_pid)?.as_bytes();
        let mut page = Page::from_bytes(raw)?;
        let leaf = cast_leaf_mut(&mut page);
        leaf.set_next_leaf(new_left_pid);
        page.update_checksum();
        storage.write_page_under_page_lock(pred_pid, &page)?;
        Ok(())
    }

    /// Descends from `pid` to the rightmost leaf in its subtree.
    fn descend_rightmost_leaf(storage: &dyn StorageEngine, mut pid: u64) -> Result<u64, DbError> {
        loop {
            let page = storage.read_page(pid)?;
            if page.body()[0] == 1 {
                // Leaf node — this is the rightmost leaf.
                return Ok(pid);
            }
            let node = cast_internal(&page);
            let n = node.num_keys();
            // Rightmost child is at index n (n keys → n+1 children).
            pid = node.child_at(n);
        }
    }

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
        let locks = self.storage.page_lock_table();
        'restart: loop {
            let mut pid = self.root_pid.load(Ordering::Acquire);
            let mut current_guard = locks.read(pid);
            loop {
                match NodeCopy::read(self.storage.as_ref(), pid)? {
                    NodeCopy::Leaf(_) => {
                        drop(current_guard);
                        return Ok(pid);
                    }
                    NodeCopy::Internal(n) => {
                        let child_pid = n.child_at(0);
                        let Some(child_guard) = locks.try_read(child_pid) else {
                            drop(current_guard);
                            continue 'restart;
                        };
                        drop(current_guard);
                        current_guard = child_guard;
                        pid = child_pid;
                    }
                }
            }
        }
    }

    fn find_leaf_for(&self, key: &[u8]) -> Result<u64, DbError> {
        let locks = self.storage.page_lock_table();
        'restart: loop {
            let mut pid = self.root_pid.load(Ordering::Acquire);
            let mut current_guard = locks.read(pid);
            loop {
                let page = self.storage.read_page(pid)?;
                if page.body()[0] == 1 {
                    drop(current_guard);
                    return Ok(pid);
                }
                let node = cast_internal(&page);
                let child_pid = node.child_at(node.find_child_idx(key));
                drop(page);
                let Some(child_guard) = locks.try_read(child_pid) else {
                    drop(current_guard);
                    continue 'restart;
                };
                drop(current_guard);
                current_guard = child_guard;
                pid = child_pid;
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
        let locks = storage.page_lock_table();
        'restart: loop {
            let mut pid = root_pid;
            let mut current_guard = locks.read(pid);
            loop {
                let page = storage.read_page(pid)?;
                if page.body()[0] == 1 {
                    let node = cast_leaf(&page);
                    let result = node.search(key).ok().map(|i| node.rid_at(i));
                    drop(current_guard);
                    return Ok(result);
                }
                let node = cast_internal(&page);
                let child_pid = node.child_at(node.find_child_idx(key));
                drop(page);
                let Some(child_guard) = locks.try_read(child_pid) else {
                    drop(current_guard);
                    continue 'restart;
                };
                drop(current_guard);
                current_guard = child_guard;
                pid = child_pid;
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
        Self::insert_in_with_batch(storage, None, root_pid, key, rid, fillfactor)
    }

    /// Like [`insert_in`] but routes page allocations through `batch`.
    #[allow(clippy::needless_option_as_deref)]
    pub fn insert_in_with_batch(
        storage: &mut dyn StorageEngine,
        mut batch: Option<&mut LocalPageBatch>,
        root_pid: &AtomicU64,
        key: &[u8],
        rid: RecordId,
        fillfactor: u8,
    ) -> Result<(), DbError> {
        Self::check_key(key)?;
        let _tree_guard = storage.page_lock_table().write(Self::static_api_lock_id(root_pid));
        loop {
            let root = root_pid.load(Ordering::Acquire);
            match Self::try_insert_leaf_optimistically(storage, root, key, rid, fillfactor)? {
                OptimisticLeafResult::Done(()) => return Ok(()),
                OptimisticLeafResult::Retry => continue,
                OptimisticLeafResult::NeedPessimistic => {}
            }

            let final_root = match Self::insert_subtree_batched(storage, batch.as_deref_mut(), root, key, rid, fillfactor)? {
                InsertResult::Ok(new_root) => new_root,
                InsertResult::Split {
                    left_pid,
                    right_pid,
                    sep,
                    ..
                } => Self::alloc_root_batched(storage, batch.as_deref_mut(), &sep, left_pid, right_pid)?,
            };

            if root_pid
                .compare_exchange(root, final_root, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(());
            }

            if Self::lookup_in(storage, root_pid.load(Ordering::Acquire), key)? == Some(rid) {
                return Ok(());
            }
        }
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
        Self::delete_in_with_batch(storage, None, root_pid, key)
    }

    /// Like [`delete_in`] but routes page allocations through `batch`.
    #[allow(clippy::needless_option_as_deref)]
    pub fn delete_in_with_batch(
        storage: &mut dyn StorageEngine,
        mut batch: Option<&mut LocalPageBatch>,
        root_pid: &AtomicU64,
        key: &[u8],
    ) -> Result<bool, DbError> {
        Self::check_key(key)?;
        let _tree_guard = storage.page_lock_table().write(Self::static_api_lock_id(root_pid));
        loop {
            let root = root_pid.load(Ordering::Acquire);
            match Self::try_delete_leaf_optimistically(storage, root, key)? {
                OptimisticLeafResult::Done(deleted) => return Ok(deleted),
                OptimisticLeafResult::Retry => continue,
                OptimisticLeafResult::NeedPessimistic => {}
            }

            match Self::delete_subtree_batched(storage, batch.as_deref_mut(), root, key, true)? {
                DeleteResult::NotFound => {
                    if root_pid.load(Ordering::Acquire) == root {
                        return Ok(false);
                    }
                }
                DeleteResult::Deleted { new_pid, .. } => {
                    let final_root = Self::collapse_root(storage, new_pid)?;
                    if root_pid
                        .compare_exchange(root, final_root, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return Ok(true);
                    }
                    if Self::lookup_in(storage, root_pid.load(Ordering::Acquire), key)?.is_none() {
                        return Ok(true);
                    }
                }
            }
        }
    }

    fn leftmost_leaf_in(storage: &dyn StorageEngine, root_pid: u64) -> Result<u64, DbError> {
        let locks = storage.page_lock_table();
        'restart: loop {
            let mut pid = root_pid;
            let mut current_guard = locks.read(pid);
            loop {
                match NodeCopy::read(storage, pid)? {
                    NodeCopy::Leaf(_) => {
                        drop(current_guard);
                        return Ok(pid);
                    }
                    NodeCopy::Internal(n) => {
                        let child_pid = n.child_at(0);
                        let Some(child_guard) = locks.try_read(child_pid) else {
                            drop(current_guard);
                            continue 'restart;
                        };
                        drop(current_guard);
                        current_guard = child_guard;
                        pid = child_pid;
                    }
                }
            }
        }
    }

    fn find_leaf_for_in(
        storage: &dyn StorageEngine,
        root_pid: u64,
        key: &[u8],
    ) -> Result<u64, DbError> {
        let locks = storage.page_lock_table();
        'restart: loop {
            let mut pid = root_pid;
            let mut current_guard = locks.read(pid);
            loop {
                let page = storage.read_page(pid)?;
                if page.body()[0] == 1 {
                    drop(current_guard);
                    return Ok(pid);
                }
                let node = cast_internal(&page);
                let child_pid = node.child_at(node.find_child_idx(key));
                drop(page);
                let Some(child_guard) = locks.try_read(child_pid) else {
                    drop(current_guard);
                    continue 'restart;
                };
                drop(current_guard);
                current_guard = child_guard;
                pid = child_pid;
            }
        }
    }

    fn try_delete_leaf_optimistically(
        storage: &mut dyn StorageEngine,
        root_pid: u64,
        key: &[u8],
    ) -> Result<OptimisticLeafResult<bool>, DbError> {
        let leaf_pid = Self::find_leaf_for_in(storage, root_pid, key)?;
        let _leaf_guard = storage.page_lock_table().write(leaf_pid);
        let mut page = storage.read_page(leaf_pid)?.into_page();

        if page.body()[0] != 1 {
            return Ok(OptimisticLeafResult::Retry);
        }

        let (idx, would_underflow) = {
            let leaf = cast_leaf(&page);
            let idx = match leaf.search(key) {
                Ok(idx) => idx,
                Err(_) => return Ok(OptimisticLeafResult::Done(false)),
            };
            let remaining = leaf.num_keys().saturating_sub(1);
            (idx, leaf_pid != root_pid && remaining < MIN_KEYS_LEAF)
        };

        if would_underflow {
            return Ok(OptimisticLeafResult::NeedPessimistic);
        }

        cast_leaf_mut(&mut page).remove_at(idx);
        page.update_checksum();
        storage.write_page_under_page_lock(leaf_pid, &page)?;
        Ok(OptimisticLeafResult::Done(true))
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
}
