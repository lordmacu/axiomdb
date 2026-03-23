# Plan: B+ Tree Performance Optimization

## Files to modify

- `crates/axiomdb-index/src/page_layout.rs` — binary search in `find_child_idx`
- `crates/axiomdb-index/src/tree.rs` — remove alloc in lookup + in-place in insert

## Subfase 2.5.1 — Lookup without allocations

### Step 1 — Binary search in `InternalNodePage::find_child_idx`

**File:** `page_layout.rs`, function `find_child_idx` (line ~134)

```
Current:
    (0..n).find(|&i| self.key_at(i) > key).unwrap_or(n)

New (binary search):
    // Invariant: keys[0..n] are sorted by the B+ Tree.
    // We look for the first i such that keys[i] > key → that is the child idx.
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if self.key_at(mid) <= key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
```

This is the standard "lower bound" pattern adapted for `>`.

### Step 2 — Remove `find_child_compressed` and its allocations

**File:** `tree.rs`

1. Remove the `find_child_compressed` function entirely.
2. In `lookup`, replace:
   ```rust
   // Before:
   pid = Self::find_child_compressed(&node, key);
   // After:
   pid = node.child_at(node.find_child_idx(key));
   ```
3. Verify that `use crate::prefix::CompressedNode` is no longer used in `tree.rs`.
   If it is not used anywhere else in the crate, remove the import (but keep the module).

### Tests subfase 2.5.1
- Run all workspace tests: `cargo test --workspace`
- Run benchmark to verify: `cargo bench --bench btree -- point_lookup`
- Target: ≥ 800K ops/s for 1M keys

---

## Subfase 2.5.2 — In-place insert

### Step 3 — In-place in `insert_leaf` (no-split case)

**File:** `tree.rs`, function `insert_leaf` (line ~203)

Current case (no split, lines ~215-224):
```rust
if node.num_keys() < ORDER_LEAF {
    let new_pid = storage.alloc_page(PageType::Index)?;
    let mut p = Page::new(PageType::Index, new_pid);
    let n = cast_leaf_mut(&mut p);
    *n = node;
    n.insert_at(ins_pos, key, rid);
    p.update_checksum();
    storage.write_page(new_pid, &p)?;
    storage.free_page(old_pid)?;
    return Ok(InsertResult::Ok(new_pid));
}
```

New (in-place):
```rust
if node.num_keys() < ORDER_LEAF {
    let mut p = Page::new(PageType::Index, old_pid);
    let n = cast_leaf_mut(&mut p);
    *n = node;
    n.insert_at(ins_pos, key, rid);
    p.update_checksum();
    storage.write_page(old_pid, &p)?;
    // No alloc_page, no free_page
    return Ok(InsertResult::Ok(old_pid));
}
```

The page continues using `old_pid` — the parent does not need to update its child pointer,
which also allows optimizing the upper level.

### Step 4 — In-place in `cow_update_child`

**File:** `tree.rs`, function `cow_update_child` (line ~379)

Rename to `in_place_update_child` and change implementation:

```
Current:
    node.set_child_at(child_idx, new_child);
    let new_pid = storage.alloc_page(PageType::Index)?;
    let mut p = Page::new(PageType::Index, new_pid);
    *cast_internal_mut(&mut p) = node;
    p.update_checksum();
    storage.write_page(new_pid, &p)?;
    storage.free_page(old_pid)?;
    Ok(new_pid)

New:
    node.set_child_at(child_idx, new_child);
    let mut p = Page::new(PageType::Index, old_pid);
    *cast_internal_mut(&mut p) = node;
    p.update_checksum();
    storage.write_page(old_pid, &p)?;
    Ok(old_pid)
```

Update all call sites of `cow_update_child` in `insert_subtree` to call
the renamed function.

### Step 5 — Verify that splits remain CoW

Splits are NOT touched:
- `insert_leaf` (split case): allocates `left_pid` and `right_pid`, frees `old_pid` ✓
- `split_internal`: same ✓
- `alloc_root`: always allocates a new page ✓

The CAS on `root_pid` only executes when the root changes (split reaching the root).
With in-place, if there is no split, `root_pid` does not change → the CAS does not execute → zero overhead.

Wait: with in-place in insert_leaf returning `Ok(old_pid)`, the parent
receives `InsertResult::Ok(old_pid)` and calls `cow_update_child(parent_pid, parent_node, child_idx, old_pid)`.
But `new_child == old_pid` → the child pointer did not change → `set_child_at` writes the same value.
With `in_place_update_child` this is still correct: we rewrite the parent page
with the same child pointer (semantic no-op), but update the checksum.

**Additional optimization:** If `new_child == old_child_pid`, skip the write entirely.
This avoids rewriting internal nodes when the child page did not change.

In `insert_subtree`, the match on `InsertResult::Ok`:
```rust
InsertResult::Ok(new_child_pid) => {
    // If new_child_pid == child_pid (in-place, no pid change):
    // no need to update the parent.
    if new_child_pid == child_pid {
        return Ok(InsertResult::Ok(pid)); // pid did not change either
    }
    let new_pid = Self::in_place_update_child(storage, pid, node, child_idx, new_child_pid)?;
    Ok(InsertResult::Ok(new_pid))
}
```

This means a no-split insert (the most common case) writes ONLY the leaf and does not touch
any internal node. The entire tree remains intact except for the modified leaf page.

### Tests subfase 2.5.2
- `cargo test --workspace` — all tests must pass
- `cargo bench --bench btree -- insert` — verify ≥ 180K ops/s
- Specific correctness test: insert 1M keys then lookup all → no errors

---

## Anti-patterns to avoid

- **NO** `unwrap()` anywhere (already prohibited in src/)
- **NO** breaking the CAS pattern on `root_pid` — kept for Phase 7
- **NO** changing the split path — only the no-split path is in-place
- **NO** removing `CompressedNode` — may be needed for analysis or future bulk load

## Risks

| Risk | Mitigation |
|---|---|
| In-place would break concurrent reads | There are no concurrent readers with `&mut self` — Rust guarantees it |
| Binary search with `<=` comparison instead of `<` may return wrong child | Verify with invariant test: lookup after 1M inserts |
| `new_child == old_pid` optimization introduces a correctness bug | Add test: insert → lookup of the same key immediately |

## Implementation order

```
1. [page_layout.rs] Binary search in find_child_idx
2. [tree.rs] Remove find_child_compressed, use node.find_child_idx directly
3. cargo test && cargo bench -- point_lookup → verify ≥800K ops/s
4. [tree.rs] insert_leaf in-place (no split)
5. [tree.rs] cow_update_child → in_place_update_child
6. [tree.rs] Optimization: skip parent write if pid did not change
7. cargo test && cargo bench -- insert → verify ≥180K ops/s
8. Full close protocol
```
