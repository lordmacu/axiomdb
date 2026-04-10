impl BTree {
    // ── Delete ───────────────────────────────────────────────────────────────

    pub fn delete(&mut self, key: &[u8]) -> Result<bool, DbError> {
        Self::check_key(key)?;
        loop {
            let root = self.root_pid.load(Ordering::Acquire);
            match Self::try_delete_leaf_optimistically(self.storage.as_mut(), root, key)? {
                OptimisticLeafResult::Done(deleted) => return Ok(deleted),
                OptimisticLeafResult::Retry => continue,
                OptimisticLeafResult::NeedPessimistic => {}
            }

            match Self::delete_subtree_batched(self.storage.as_mut(), None, root, key, true)? {
                DeleteResult::NotFound => {
                    if self.root_pid.load(Ordering::Acquire) == root {
                        return Ok(false);
                    }
                }
                DeleteResult::Deleted { new_pid, .. } => {
                    let final_root = Self::collapse_root(self.storage.as_mut(), new_pid)?;
                    if self
                        .root_pid
                        .compare_exchange(root, final_root, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return Ok(true);
                    }
                    if self.lookup(key)?.is_none() {
                        return Ok(true);
                    }
                }
            }
        }
    }

    #[allow(clippy::needless_option_as_deref)]
    fn delete_subtree_batched(
        storage: &dyn StorageEngine,
        mut batch: Option<&mut LocalPageBatch>,
        pid: u64,
        key: &[u8],
        is_root: bool,
    ) -> Result<DeleteResult, DbError> {
        let parent_guard = storage.page_lock_table().write(pid);
        match NodeCopy::read(storage, pid)? {
            NodeCopy::Leaf(node) => Self::delete_leaf(storage, pid, node, key, is_root),
            NodeCopy::Internal(node) => {
                let child_idx = node.find_child_idx(key);
                let child_pid = node.child_at(child_idx);

                // Phase 40.8c: early X-latch release on safe descent.
                // The recursive call cannot need a parent update when the
                // immediate child is a leaf with strictly more than
                // `MIN_KEYS_LEAF` keys: leaves keep their pid in the delete
                // fast path, and the underflow check guarantees no upward
                // propagation. See `child_is_safe_for_delete` for the full
                // rationale.
                if Self::child_is_safe_for_delete(storage, child_pid)? {
                    drop(parent_guard);
                    let result = Self::delete_subtree_batched(storage, batch, child_pid, key, false)?;
                    return match result {
                        DeleteResult::NotFound => Ok(DeleteResult::NotFound),
                        DeleteResult::Deleted { new_pid, underfull } => {
                            debug_assert_eq!(
                                new_pid, child_pid,
                                "safe child must return Deleted with the same pid"
                            );
                            debug_assert!(
                                !underfull,
                                "safe child must not underflow under delete"
                            );
                            Ok(DeleteResult::Deleted {
                                new_pid: pid,
                                underfull: false,
                            })
                        }
                    };
                }

                match Self::delete_subtree_batched(storage, batch.as_deref_mut(), child_pid, key, false)? {
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
                                Self::rebalance_batched(storage, batch, pid, node, child_idx, new_child_pid)?;
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
        storage: &dyn StorageEngine,
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

        // Structural path: leaf will underflow — write back in-place (same page
        // ID) so the predecessor leaf's next_leaf pointer remains valid.
        //
        // The old CoW approach allocated a new page and freed the old one, which
        // silently broke the predecessor → old_pid → ... chain; rotate/merge
        // would then create more new IDs for the involved leaves, making the
        // broken chain permanent.  Using write_leaf_same_pid here (and doing the
        // same in rotate_right/rotate_left/merge_children for leaf nodes) keeps
        // the next_leaf chain intact without any predecessor fixup pass.
        let new_pid = Self::write_leaf_same_pid(storage, old_pid, node)?;

        Ok(DeleteResult::Deleted {
            new_pid,
            underfull: true,
        })
    }

    fn rebalance_batched(
        storage: &dyn StorageEngine,
        batch: Option<&mut LocalPageBatch>,
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
                return Self::rotate_right_batched(
                    storage,
                    batch,
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
                return Self::rotate_left_batched(
                    storage,
                    batch,
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
            return Self::merge_children_batched(
                storage,
                batch,
                parent_pid,
                parent,
                child_idx - 1,
                left_pid,
                child_pid,
                child_is_leaf,
            );
        }
        let right_pid = parent.child_at(child_idx + 1);
        Self::merge_children_batched(
            storage,
            batch,
            parent_pid,
            parent,
            child_idx,
            child_pid,
            right_pid,
            child_is_leaf,
        )
    }

    #[allow(clippy::needless_option_as_deref)]
    fn rotate_right_batched(
        storage: &dyn StorageEngine,
        mut batch: Option<&mut LocalPageBatch>,
        parent_pid: u64,
        mut parent: InternalNodePage,
        child_idx: usize,
        child_pid: u64,
        left_pid: u64,
        is_leaf: bool,
    ) -> Result<u64, DbError> {
        let np = batch_alloc_page(storage, batch.as_deref_mut(), PageType::Index)?;
        let (_guard_a, _guard_b) = if left_pid <= child_pid {
            (
                storage.page_lock_table().write(left_pid),
                storage.page_lock_table().write(child_pid),
            )
        } else {
            (
                storage.page_lock_table().write(child_pid),
                storage.page_lock_table().write(left_pid),
            )
        };

        if is_leaf {
            // In-place leaf rotation: reuse left_pid and child_pid so the
            // predecessor's next_leaf pointer (→ left_pid) stays valid.
            // left.next_leaf = child_pid is unchanged — still correct after
            // moving the last key of left into child.
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
            parent.set_child_at(child_idx - 1, left_pid);
            parent.set_child_at(child_idx, child_pid);

            let mut lp = Page::new(PageType::Index, left_pid);
            *cast_leaf_mut(&mut lp) = left;
            lp.update_checksum();
            storage.write_page_under_page_lock(left_pid, &lp)?;
            let mut cp = Page::new(PageType::Index, child_pid);
            *cast_leaf_mut(&mut cp) = child;
            cp.update_checksum();
            storage.write_page_under_page_lock(child_pid, &cp)?;
            // Do NOT free left_pid or child_pid — they are still live leaves.
        } else {
            let nc = batch_alloc_page(storage, batch.as_deref_mut(), PageType::Index)?;
            let nl = batch_alloc_page(storage, batch.as_deref_mut(), PageType::Index)?;
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
            // CoW: old internal node pages are superseded.
            storage.free_page(child_pid)?;
            storage.free_page(left_pid)?;
        }

        #[cfg(debug_assertions)]
        parent.validate();
        let mut pp = Page::new(PageType::Index, np);
        *cast_internal_mut(&mut pp) = parent;
        pp.update_checksum();
        storage.write_page(np, &pp)?;

        storage.free_page(parent_pid)?;
        Ok(np)
    }

    #[allow(clippy::needless_option_as_deref)]
    fn rotate_left_batched(
        storage: &dyn StorageEngine,
        mut batch: Option<&mut LocalPageBatch>,
        parent_pid: u64,
        mut parent: InternalNodePage,
        child_idx: usize,
        child_pid: u64,
        right_pid: u64,
        is_leaf: bool,
    ) -> Result<u64, DbError> {
        let np = batch_alloc_page(storage, batch.as_deref_mut(), PageType::Index)?;
        let (_guard_a, _guard_b) = if child_pid <= right_pid {
            (
                storage.page_lock_table().write(child_pid),
                storage.page_lock_table().write(right_pid),
            )
        } else {
            (
                storage.page_lock_table().write(right_pid),
                storage.page_lock_table().write(child_pid),
            )
        };

        if is_leaf {
            // In-place leaf rotation: reuse child_pid and right_pid so
            // next_leaf pointers remain valid.
            // child.next_leaf = right_pid is unchanged — still correct after
            // appending right's first key to child.
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
            parent.set_child_at(child_idx, child_pid);
            parent.set_child_at(child_idx + 1, right_pid);

            let mut cp = Page::new(PageType::Index, child_pid);
            *cast_leaf_mut(&mut cp) = child;
            cp.update_checksum();
            storage.write_page_under_page_lock(child_pid, &cp)?;
            let mut rp = Page::new(PageType::Index, right_pid);
            *cast_leaf_mut(&mut rp) = right;
            rp.update_checksum();
            storage.write_page_under_page_lock(right_pid, &rp)?;
            // Do NOT free child_pid or right_pid — they are still live leaves.
        } else {
            let nc = batch_alloc_page(storage, batch.as_deref_mut(), PageType::Index)?;
            let nr = batch_alloc_page(storage, batch.as_deref_mut(), PageType::Index)?;
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
            // CoW: old internal node pages are superseded.
            storage.free_page(child_pid)?;
            storage.free_page(right_pid)?;
        }

        #[cfg(debug_assertions)]
        parent.validate();
        let mut pp = Page::new(PageType::Index, np);
        *cast_internal_mut(&mut pp) = parent;
        pp.update_checksum();
        storage.write_page(np, &pp)?;

        storage.free_page(parent_pid)?;
        Ok(np)
    }

    #[allow(clippy::needless_option_as_deref)]
    fn merge_children_batched(
        storage: &dyn StorageEngine,
        mut batch: Option<&mut LocalPageBatch>,
        parent_pid: u64,
        mut parent: InternalNodePage,
        sep_idx: usize,
        left_pid: u64,
        right_pid: u64,
        is_leaf: bool,
    ) -> Result<u64, DbError> {
        let npp = batch_alloc_page(storage, batch.as_deref_mut(), PageType::Index)?;
        let (_guard_a, _guard_b) = if left_pid <= right_pid {
            (
                storage.page_lock_table().write(left_pid),
                storage.page_lock_table().write(right_pid),
            )
        } else {
            (
                storage.page_lock_table().write(right_pid),
                storage.page_lock_table().write(left_pid),
            )
        };

        // `merged_pid` is the page ID that will hold the merged node.
        // For leaves we reuse `left_pid` in-place so the predecessor's
        // next_leaf pointer (→ left_pid) remains valid — no fixup needed.
        // For internal nodes we allocate a fresh page (CoW).
        let merged_pid = if is_leaf {
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

            let mut pg = Page::new(PageType::Index, left_pid);
            *cast_leaf_mut(&mut pg) = merged;
            pg.update_checksum();
            storage.write_page_under_page_lock(left_pid, &pg)?;
            // right_pid is freed below; left_pid stays live.
            left_pid
        } else {
            let mp = batch_alloc_page(storage, batch.as_deref_mut(), PageType::Index)?;
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
            storage.free_page(left_pid)?;
            mp
        };

        parent.set_child_at(sep_idx, merged_pid);
        parent.remove_at(sep_idx, sep_idx + 1);

        #[cfg(debug_assertions)]
        parent.validate();
        let mut pp = Page::new(PageType::Index, npp);
        *cast_internal_mut(&mut pp) = parent;
        pp.update_checksum();
        storage.write_page(npp, &pp)?;

        storage.free_page(parent_pid)?;
        storage.free_page(right_pid)?;
        Ok(npp)
    }

    fn collapse_root(storage: &dyn StorageEngine, root_pid: u64) -> Result<u64, DbError> {
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

}
