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
        // Phase 40.8: S-latch coupling for concurrent reader safety.
        // At each level, acquire S-latch on child BEFORE releasing parent.
        // Inspired by InnoDB BTR_SEARCH_LEAF protocol (btr0cur.cc:1866-1950).
        let locks = self.storage.page_lock_table();
        let mut pid = self.root_pid.load(Ordering::Acquire);
        let mut current_guard = locks.read(pid);
        loop {
            let page = self.storage.read_page(pid)?;
            if page.body()[0] == 1 {
                // Leaf: search under current S-latch.
                let node = cast_leaf(&page);
                let result = node.search(key).ok().map(|i| node.rid_at(i));
                drop(current_guard);
                return Ok(result);
            }
            let node = cast_internal(&page);
            let idx = node.find_child_idx(key);
            let child_pid = node.child_at(idx);
            drop(page);
            // Coupling: acquire child before releasing parent.
            let child_guard = locks.read(child_pid);
            drop(current_guard);
            current_guard = child_guard;
            pid = child_pid;
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
                ..
            } => {
                // Root split: no predecessor to update (left_pid is the leftmost leaf).
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
                    storage.write_page(pid, &page)?;
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
        storage.write_page(pid, &page)?;
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
        storage.write_page(pid, &page)?;
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
