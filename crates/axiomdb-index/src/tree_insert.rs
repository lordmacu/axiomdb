impl BTree {
    /// Creates or reopens a B+ Tree.
    pub fn new(
        storage: Box<dyn StorageEngine>,
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
        let locks = self.storage.page_lock_table();
        // Phase 40.8: descend with no-wait child coupling. If a writer is
        // queued on the child, drop the current latch and restart instead of
        // blocking while holding the parent latch. This follows the
        // retry-oriented posture described in PostgreSQL/InnoDB research when
        // latch state/topology changes under us.
        'restart: loop {
            let mut pid = self.root_pid.load(Ordering::Acquire);
            let mut current_guard = locks.read(pid);
            loop {
                let page = self.storage.read_page(pid)?;
                if page.body()[0] == 1 {
                    let node = cast_leaf(&page);
                    let result = node.search(key).ok().map(|i| node.rid_at(i));
                    drop(current_guard);
                    return Ok(result);
                }
                let node = cast_internal(&page);
                let idx = node.find_child_idx(key);
                let child_pid = node.child_at(idx);
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

    // ── Insert ───────────────────────────────────────────────────────────────

    pub fn insert(&mut self, key: &[u8], rid: RecordId) -> Result<(), DbError> {
        Self::check_key(key)?;
        loop {
            let root = self.root_pid.load(Ordering::Acquire);
            match Self::try_insert_leaf_optimistically(self.storage.as_mut(), root, key, rid, 90)? {
                OptimisticLeafResult::Done(()) => return Ok(()),
                OptimisticLeafResult::Retry => continue,
                OptimisticLeafResult::NeedPessimistic => {}
            }

            let final_root = match Self::insert_subtree(self.storage.as_mut(), root, key, rid, 90)? {
                InsertResult::Ok(new_root) => new_root,
                InsertResult::Split {
                    left_pid,
                    right_pid,
                    sep,
                    ..
                } => Self::alloc_root(self.storage.as_mut(), &sep, left_pid, right_pid)?,
            };

            if self
                .root_pid
                .compare_exchange(root, final_root, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(());
            }

            if self.lookup(key)? == Some(rid) {
                return Ok(());
            }
        }
    }

    fn try_insert_leaf_optimistically(
        storage: &mut dyn StorageEngine,
        root_pid: u64,
        key: &[u8],
        rid: RecordId,
        fillfactor: u8,
    ) -> Result<OptimisticLeafResult<()>, DbError> {
        let leaf_pid = Self::find_leaf_for_in(storage, root_pid, key)?;
        let _leaf_guard = storage.page_lock_table().write(leaf_pid);
        let mut page = storage.read_page(leaf_pid)?.into_page();

        if page.body()[0] != 1 {
            return Ok(OptimisticLeafResult::Retry);
        }

        let (insert_pos, can_fit) = {
            let leaf = cast_leaf(&page);
            let insert_pos = match leaf.search(key) {
                Ok(_) => return Err(DbError::DuplicateKey),
                Err(pos) => pos,
            };
            let threshold = fill_threshold(ORDER_LEAF, fillfactor);
            (insert_pos, leaf.num_keys() < threshold)
        };

        if !can_fit {
            return Ok(OptimisticLeafResult::NeedPessimistic);
        }

        cast_leaf_mut(&mut page).insert_at(insert_pos, key, rid);
        page.update_checksum();
        storage.write_page_under_page_lock(leaf_pid, &page)?;
        Ok(OptimisticLeafResult::Done(()))
    }

    /// Phase 40.8c: returns `true` when descending into `child_pid` from a
    /// pessimistic insert is guaranteed not to require an update on the
    /// parent (no split propagation upward).
    ///
    /// A child is "safe for insert" iff it has room to absorb one additional
    /// key without itself splitting:
    ///
    /// - Leaf child: `num_keys < fill_threshold(ORDER_LEAF, fillfactor)` —
    ///   matches the fast-path predicate in `insert_subtree` so a single
    ///   in-place leaf insert is guaranteed.
    /// - Internal child: `num_keys < ORDER_INTERNAL` — matches the in-place
    ///   parent absorb predicate, so any split that propagates from a deeper
    ///   level stops at this child.
    ///
    /// Reading the child page without holding its latch is safe because all
    /// writers on the same tree are serialized (instance API: `&mut self`;
    /// static API: tree-level X-latch in `insert_in` / `delete_in`). The
    /// parent X-latch is held during this check, so concurrent readers cannot
    /// observe a half-modified child either.
    fn child_is_safe_for_insert(
        storage: &dyn StorageEngine,
        child_pid: u64,
        fillfactor: u8,
    ) -> Result<bool, DbError> {
        let page = storage.read_page(child_pid)?;
        if page.body()[0] == 1 {
            let leaf = cast_leaf(&page);
            let threshold = fill_threshold(ORDER_LEAF, fillfactor);
            Ok(leaf.num_keys() < threshold)
        } else {
            let internal = cast_internal(&page);
            Ok(internal.num_keys() < ORDER_INTERNAL)
        }
    }

    /// Phase 40.8c: returns `true` when descending into `child_pid` from a
    /// pessimistic delete is guaranteed not to require an update on the
    /// parent (no rebalance propagation upward).
    ///
    /// In the index B-tree the parent's pid only changes when its child's
    /// rebalance routine allocates a new parent page (CoW for internal-node
    /// rotations and merges). That CoW only happens when an underflow
    /// propagates upward into the immediate child, which in turn requires
    /// the immediate child or one of its descendants to underflow.
    ///
    /// To avoid scanning the entire subtree, we only treat the descent as
    /// "safe" when the immediate child is a **leaf** with strictly more keys
    /// than `MIN_KEYS_LEAF`. Leaves never change pid in the delete path
    /// (`write_leaf_same_pid` keeps the pid; rebalance for leaves is also
    /// in-place), and the underflow check is `num_keys < MIN_KEYS_LEAF`
    /// **after** the removal — so `num_keys > MIN_KEYS_LEAF` before delete
    /// guarantees no underflow.
    fn child_is_safe_for_delete(
        storage: &dyn StorageEngine,
        child_pid: u64,
    ) -> Result<bool, DbError> {
        let page = storage.read_page(child_pid)?;
        if page.body()[0] == 1 {
            let leaf = cast_leaf(&page);
            Ok(leaf.num_keys() > MIN_KEYS_LEAF)
        } else {
            // Internal children might have a deeper rebalance that allocates
            // a fresh pid for them, so we cannot safely drop the parent latch
            // even if the immediate child has plenty of keys.
            Ok(false)
        }
    }

    fn insert_subtree(
        storage: &mut dyn StorageEngine,
        pid: u64,
        key: &[u8],
        rid: RecordId,
        fillfactor: u8,
    ) -> Result<InsertResult, DbError> {
        let parent_guard = storage.page_lock_table().write(pid);
        // Fast path: for leaf non-split inserts, avoid copying the entire
        // LeafNodePage struct (16KB). Read the page, check if it needs
        // splitting, and if not, modify the body directly via into_page().
        {
            let page_ref = storage.read_page(pid)?;
            if page_ref.body()[0] == 1 {
                let leaf = cast_leaf(&page_ref);
                let ins_pos = match leaf.search(key) {
                    Ok(_) => return Err(DbError::DuplicateKey),
                    Err(pos) => pos,
                };
                let threshold = fill_threshold(ORDER_LEAF, fillfactor);
                if leaf.num_keys() < threshold {
                    // Non-split: convert PageRef to owned Page, modify, write back.
                    // One read + one write, no intermediate alloc or struct copy.
                    let mut page = page_ref.into_page();
                    cast_leaf_mut(&mut page).insert_at(ins_pos, key, rid);
                    page.update_checksum();
                    storage.write_page_under_page_lock(pid, &page)?;
                    return Ok(InsertResult::Ok(pid));
                }
                // Fall through to split path (needs the full node copy).
            }
        }

        match NodeCopy::read(storage, pid)? {
            NodeCopy::Leaf(node) => Self::insert_leaf(storage, pid, node, key, rid, fillfactor),
            NodeCopy::Internal(node) => {
                let n = node.num_keys();
                let child_idx = node.find_child_idx(key);
                let child_pid = node.child_at(child_idx);

                // Phase 40.8c: early X-latch release on safe descent.
                // If the child can absorb the worst-case upward propagation
                // (one new key) without itself splitting, the recursive call
                // cannot return InsertResult::Split, so the parent will never
                // need an update. Drop our X-latch now so concurrent readers
                // can resume on this internal page.
                if Self::child_is_safe_for_insert(storage, child_pid, fillfactor)? {
                    drop(parent_guard);
                    let result =
                        Self::insert_subtree(storage, child_pid, key, rid, fillfactor)?;
                    debug_assert!(
                        matches!(result, InsertResult::Ok(p) if p == child_pid),
                        "safe child must return InsertResult::Ok with the same pid"
                    );
                    return Ok(InsertResult::Ok(pid));
                }

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
                        leaf_split,
                    } => {
                        // Phase 7.9: update predecessor leaf's next_leaf pointer
                        // so the leaf chain stays correct under CoW.
                        if leaf_split && child_idx > 0 {
                            Self::update_predecessor_next_leaf(
                                storage, &node, child_idx, left_pid,
                            )?;
                        }

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

    /// Splits a full leaf page into left and right halves.
    ///
    /// Called only when `node.num_keys() >= threshold` (non-split path is
    /// handled directly in `insert_subtree` via the fast path).
    fn insert_leaf(
        storage: &mut dyn StorageEngine,
        old_pid: u64,
        node: LeafNodePage,
        key: &[u8],
        rid: RecordId,
        _fillfactor: u8,
    ) -> Result<InsertResult, DbError> {
        let ins_pos = match node.search(key) {
            Ok(_) => return Err(DbError::DuplicateKey),
            Err(pos) => pos,
        };

        // Split (non-split case already handled by fast path in insert_subtree)
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
            leaf_split: true,
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
            leaf_split: false,
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
    /// Persists a leaf node back to the **same** page `pid`.
    ///
    /// Reads the existing page, overwrites the body, writes back — avoids
    /// allocating a 16KB zeroed `Page::new` + 16KB struct copy.
    fn write_leaf_same_pid(
        storage: &mut dyn StorageEngine,
        pid: u64,
        node: LeafNodePage,
    ) -> Result<u64, DbError> {
        let mut page = storage.read_page(pid)?.into_page();
        *cast_leaf_mut(&mut page) = node;
        page.update_checksum();
        storage.write_page_under_page_lock(pid, &page)?;
        Ok(pid)
    }

    /// Persists `node` (an internal node) back to the **same** page `pid`.
    ///
    /// Never allocates or frees pages. Returns `pid` unchanged.
    #[inline]
    /// Persists an internal node back to the **same** page `pid`.
    ///
    /// Reads the existing page, overwrites the body, writes back — avoids
    /// allocating a 16KB zeroed `Page::new` + 16KB struct copy.
    fn write_internal_same_pid(
        storage: &mut dyn StorageEngine,
        pid: u64,
        node: InternalNodePage,
    ) -> Result<u64, DbError> {
        #[cfg(debug_assertions)]
        node.validate();
        let mut page = storage.read_page(pid)?.into_page();
        *cast_internal_mut(&mut page) = node;
        page.update_checksum();
        storage.write_page_under_page_lock(pid, &page)?;
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
}
