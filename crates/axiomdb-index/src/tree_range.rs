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

        let raw = *storage.read_page(pred_pid)?.as_bytes();
        let mut page = Page::from_bytes(raw)?;
        let leaf = cast_leaf_mut(&mut page);
        leaf.set_next_leaf(new_left_pid);
        page.update_checksum();
        storage.write_page(pred_pid, &page)?;
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
        // Phase 40.8: S-latch coupling for concurrent reader safety.
        let locks = self.storage.page_lock_table();
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
                    let child_guard = locks.read(child_pid);
                    drop(current_guard);
                    current_guard = child_guard;
                    pid = child_pid;
                }
            }
        }
    }

    fn find_leaf_for(&self, key: &[u8]) -> Result<u64, DbError> {
        // Phase 40.8: S-latch coupling for concurrent reader safety.
        let locks = self.storage.page_lock_table();
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
            let child_guard = locks.read(child_pid);
            drop(current_guard);
            current_guard = child_guard;
            pid = child_pid;
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
        // Phase 40.8: S-latch coupling for concurrent reader safety.
        let locks = storage.page_lock_table();
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
            let child_guard = locks.read(child_pid);
            drop(current_guard);
            current_guard = child_guard;
            pid = child_pid;
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
                ..
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
        // Phase 40.8: S-latch coupling for concurrent reader safety.
        let locks = storage.page_lock_table();
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
                    let child_guard = locks.read(child_pid);
                    drop(current_guard);
                    current_guard = child_guard;
                    pid = child_pid;
                }
            }
        }
    }

    fn find_leaf_for_in(
        storage: &dyn StorageEngine,
        root_pid: u64,
        key: &[u8],
    ) -> Result<u64, DbError> {
        // Phase 40.8: S-latch coupling for concurrent reader safety.
        let locks = storage.page_lock_table();
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
            let child_guard = locks.read(child_pid);
            drop(current_guard);
            current_guard = child_guard;
            pid = child_pid;
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
}
