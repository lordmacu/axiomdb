impl BTree {
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
        storage: &dyn StorageEngine,
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

        // Write the last (possibly partial) leaf with next_leaf = NULL_PAGE.
        // NULL_PAGE = u64::MAX, NOT 0 — LeafNodePage::zeroed() initialises
        // next_leaf to 0, so we must set it explicitly to avoid the range
        // iterator following page 0 (the meta page) as if it were a real leaf.
        {
            cur_leaf.set_next_leaf(NULL_PAGE);
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
        storage: &dyn StorageEngine,
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
        storage: &dyn StorageEngine,
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
        storage: &dyn StorageEngine,
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
        storage: &dyn StorageEngine,
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
            current_parent_pid = Self::rebalance_batched(
                storage,
                None, // bulk_load: no per-txn batch
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
