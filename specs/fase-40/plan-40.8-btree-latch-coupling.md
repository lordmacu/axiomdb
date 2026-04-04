# Plan: 40.8 — B-Tree Latch Coupling

## Files to create/modify

| File | Change |
|---|---|
| `crates/axiomdb-index/src/latch.rs` | **NEW**: ParentStack, LatchMode enum, tree-level latch helpers |
| `crates/axiomdb-index/src/tree.rs` | Integrate latch coupling into insert/delete/search/range |
| `crates/axiomdb-index/src/iter.rs` | RangeIter: S-latch per leaf during iteration |
| `crates/axiomdb-storage/src/clustered_tree.rs` | Same latch protocol for clustered B-tree |
| `crates/axiomdb-index/src/lib.rs` | Export latch module |

## Implementation phases

### Phase 1: LatchMode + ParentStack infrastructure

```rust
// latch.rs

/// Latch modes for B-tree descent.
pub enum TreeLatchMode {
    /// Read-only: S-latch coupling, S-latch leaf.
    SearchLeaf,
    /// Optimistic write: S-latch coupling on internals, X-latch leaf.
    ModifyLeaf,
    /// Pessimistic write: X-latch coupling from root, X-latch all.
    ModifyTree,
}

/// Stack of parent pages during descent for pessimistic split handling.
pub struct ParentStack {
    entries: Vec<ParentEntry>,
}

struct ParentEntry {
    page_id: u64,
    child_idx: usize,
    /// Whether X-latch is still held on this parent (pessimistic mode only).
    /// In optimistic mode, all parent latches are released during coupling.
    x_latch_held: bool,
}

impl ParentStack {
    pub fn push(&mut self, page_id: u64, child_idx: usize, x_held: bool);
    pub fn pop(&mut self) -> Option<ParentEntry>;
    pub fn peek(&self) -> Option<&ParentEntry>;
    pub fn release_all(&mut self, page_locks: &PageLockTable);
}
```

### Phase 2: Latch-aware descent function

```rust
/// Descend from root to leaf with latch coupling.
/// Returns (leaf_page_id, parent_stack) where parent_stack contains
/// X-latched parents if mode is ModifyTree and pages are not "safe".
fn descend_with_latches(
    storage: &dyn StorageEngine,
    page_locks: &PageLockTable,
    root_pid: u64,
    key: &[u8],
    mode: TreeLatchMode,
) -> Result<(u64, ParentStack), DbError> {
    let mut stack = ParentStack::new();
    let mut current_pid = root_pid;

    // Acquire initial latch on root
    let root_guard = match mode {
        SearchLeaf | ModifyLeaf => page_locks.read(root_pid),
        ModifyTree => page_locks.write(root_pid),
    };

    loop {
        let page = storage.read_page(current_pid)?;
        if is_leaf(&page) {
            // At leaf: upgrade to X if needed
            match mode {
                SearchLeaf => { /* already S-latched, keep */ }
                ModifyLeaf => {
                    drop(current_guard); // release S
                    let _x_guard = page_locks.write(current_pid); // acquire X
                }
                ModifyTree => { /* already X-latched */ }
            }
            return Ok((current_pid, stack));
        }

        // Internal page: find child
        let child_idx = find_child(page, key);
        let child_pid = get_child(page, child_idx);

        // Latch coupling: acquire child, then decide about parent
        match mode {
            SearchLeaf | ModifyLeaf => {
                let child_guard = page_locks.read(child_pid); // S-latch child
                drop(current_guard); // release parent S-latch
                current_guard = child_guard;
            }
            ModifyTree => {
                let child_guard = page_locks.write(child_pid); // X-latch child
                let child_page = storage.read_page(child_pid)?;
                if is_safe_for_operation(&child_page, mode) {
                    // Child is safe: won't split/merge → release parent X early
                    stack.push(current_pid, child_idx, false);
                    drop(current_guard); // release parent X-latch
                } else {
                    // Child NOT safe: keep parent X-latched for split/merge
                    stack.push(current_pid, child_idx, true);
                    // parent guard stays alive (moved into stack or kept in scope)
                }
                current_guard = child_guard;
            }
        }
        current_pid = child_pid;
    }
}
```

### Phase 3: "Safe page" predicate

```rust
/// A page is "safe" if the pending operation won't cause structural changes.
fn is_safe_for_insert(page: &Page, record_size: usize) -> bool {
    let free = page_free_space(page);
    let limit = PAGE_REORGANIZE_LIMIT + record_size;
    free >= limit && free >= record_size * 2
    // InnoDB's heuristic: enough for insert + potential reorganization
}

fn is_safe_for_delete(page: &Page) -> bool {
    let num_keys = page_num_keys(page);
    num_keys > MIN_KEYS + 1
    // Won't underflow even after one deletion
}
```

**Why `record_size * 2`**: If page has room for 2× the record, split is extremely
unlikely even with concurrent inserts. InnoDB uses this heuristic (btr0cur.cc:741-772).

### Phase 4: Optimistic insert with latch coupling

```rust
fn insert_optimistic(
    storage: &dyn StorageEngine,
    page_locks: &PageLockTable,
    root_pid: u64,
    key: &[u8],
    rid: RecordId,
) -> Result<InsertOutcome, DbError> {
    // 1. Descend with S-latches on internals, acquire X at leaf
    let (leaf_pid, _stack) = descend_with_latches(
        storage, page_locks, root_pid, key, TreeLatchMode::ModifyLeaf
    )?;

    // 2. X-latch already held on leaf (from descent)
    let mut page = storage.read_page(leaf_pid)?.into_page();
    let leaf = cast_leaf_mut(&mut page);

    // 3. Try insert
    match leaf.search(key) {
        Ok(_) => return Err(DbError::DuplicateKey),
        Err(pos) => {
            if leaf.num_keys() < fill_threshold(ORDER_LEAF, 90) {
                leaf.insert_at(pos, key, rid);
                page.update_checksum();
                storage.write_page(leaf_pid, &page)?;
                return Ok(InsertOutcome::Done(leaf_pid));
            }
        }
    }

    // 4. Leaf is full → drop X-latch, return NeedPessimistic
    Ok(InsertOutcome::NeedPessimistic)
}
```

### Phase 5: Pessimistic insert with latch coupling

```rust
fn insert_pessimistic(
    storage: &dyn StorageEngine,
    page_locks: &PageLockTable,
    root_pid: &AtomicU64,
    key: &[u8],
    rid: RecordId,
) -> Result<u64, DbError> {
    let current_root = root_pid.load(Acquire);

    // 1. Descend with X-latches, early-releasing "safe" parents
    let (leaf_pid, mut stack) = descend_with_latches(
        storage, page_locks, current_root, key, TreeLatchMode::ModifyTree
    )?;

    // 2. Insert at leaf (may trigger split)
    let mut page = storage.read_page(leaf_pid)?.into_page();
    // ... try insert, if full → split_leaf() ...

    // 3. If split: propagate separator to parent
    if let Some(split_result) = maybe_split {
        // Parent is still X-latched (from stack, not "safe")
        let parent = stack.pop().unwrap();
        // Insert separator into parent page
        // If parent also full → split parent (parent's parent also X-latched if not safe)
        // Propagate up the stack until absorbed or root split
    }

    // 4. Release all remaining latches (bottom-up via stack)
    stack.release_all(page_locks);

    // 5. If root split: CAS new root
    if root_split {
        root_pid.compare_exchange(current_root, new_root, AcqRel, Acquire)?;
    }

    Ok(effective_root)
}
```

### Phase 6: Range scan with latch coupling

```rust
impl RangeIter {
    fn next(&mut self) -> Option<Result<(Vec<u8>, RecordId), DbError>> {
        loop {
            // S-latch current leaf
            let _guard = self.page_locks.read(self.current_pid);
            let page = self.storage.read_page(self.current_pid)?;
            let leaf = cast_leaf(&page);

            if self.slot_idx < leaf.num_keys() {
                let key = leaf.key_at(self.slot_idx);
                let rid = leaf.rid_at(self.slot_idx);
                self.slot_idx += 1;

                if self.in_range(key) {
                    return Some(Ok((key.to_vec(), rid)));
                }
            } else {
                // Move to next leaf
                let next = leaf.next_leaf_val();
                drop(_guard); // release current S-latch BEFORE acquiring next
                if next == NULL_PAGE {
                    return None;
                }
                self.page_locks.prefetch_hint(next);
                self.current_pid = next;
                self.slot_idx = 0;
                // Loop back: acquire S-latch on next leaf
            }
        }
    }
}
```

**Critical**: S-latch on current leaf is released BEFORE acquiring S-latch on next leaf.
Never hold two leaf S-latches simultaneously (prevents deadlock with insert that
X-latches one leaf then splits to create sibling).

### Phase 7: Delete with latch coupling

Same optimistic/pessimistic pattern as insert:
- Optimistic: S-latch descent, X-latch leaf, delete entry
- If underfull → restart pessimistic: X-latch descent, merge/rotate with sibling
- Merge requires X-latch on both siblings + parent (from stack)
- Rotate requires X-latch on sibling + parent

### Phase 8: Clustered B-tree integration

Apply identical protocol to `clustered_tree.rs`:
- `insert()`: optimistic → pessimistic fallback
- `lookup()`: S-latch coupling (read-only)
- `range()`: S-latch per leaf with next_leaf coupling
- `update_in_place()`: X-latch at leaf (optimistic)
- `delete_mark()`: X-latch at leaf (optimistic)

Variable-size cells in clustered leaf don't change the latch protocol —
only the "safe page" predicate changes (check `free_space` vs cell footprint).

### Phase 9: Stress testing

- 8 threads × 10K random inserts → verify tree structure
- 8 threads × mixed insert/delete → verify no orphan pages
- 4 threads insert + 4 threads range scan → verify no missing keys
- 2 threads concurrent splits on same parent → verify correct separator propagation
- Deadlock detection: insert + delete on same leaf → verify no hang

## Tests to write

1. **Latch coupling descent**: verify S-latches released after each level
2. **Optimistic insert (no split)**: single leaf X-latch, verify parallel on different leaves
3. **Optimistic → pessimistic fallback**: leaf full, restart with X-latch coupling
4. **Pessimistic with safe-page early release**: verify parent X-latch released when child safe
5. **Split under latch**: concurrent insert triggers split, tree structure valid
6. **Concurrent search during split**: reader finds correct key despite concurrent split
7. **Range scan during insert**: scanner traverses leaves, inserter modifies different leaf
8. **Range scan crossing split boundary**: scanner on leaf N, leaf N splits, scanner follows next_leaf
9. **Delete with merge under latch**: underfull leaf merges with sibling correctly
10. **Root split under concurrency**: two inserts cause root split simultaneously
11. **8-thread stress**: random keys, random operations, verify tree invariants after
12. **Deadlock free**: no operation combination causes deadlock (latch ordering verified)

## Anti-patterns to avoid

- DO NOT hold S-latch while upgrading to X (causes deadlock if two threads try simultaneously).
  Instead: release S, acquire X (brief window where page may change → re-verify).
- DO NOT hold two leaf S-latches simultaneously during range scan (prevents insert X-latch).
- DO NOT hold X-latch during alloc_page (allocator Mutex + page X-latch = potential ordering violation).
  Instead: alloc page first (under allocator Mutex), then X-latch the new page.
- DO NOT use the old page content after releasing its latch (page may be modified by another thread).
  Always re-read page after acquiring latch.
- DO NOT skip re-verification after pessimistic restart (tree may have changed since optimistic attempt).

## Risks

- **Optimistic → pessimistic restart cost**: ~2× the page reads for the 5% of inserts that split.
  Mitigation: this is InnoDB's proven ratio — 95% optimistic saves far more than 5% restart costs.
- **ParentStack complexity**: managing latch lifetimes across recursive calls is error-prone.
  Mitigation: RAII guards + explicit stack with release_all() safety net.
- **Clustered B-tree variable cells**: "safe page" predicate is harder (depends on incoming cell size).
  Mitigation: conservative estimate — if `free_space > 2 × max_inline_cell_size`, consider safe.
- **Root split race**: two threads both detect root split needed. Only one CAS succeeds.
  Mitigation: losing thread retries from new root (standard CAS loop pattern, already in tree.rs).
