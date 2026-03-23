# Spec: B+ Tree Performance Optimization (subfases 2.5.1 and 2.5.2)

## What to build (not how)

Eliminate the two performance blockers in the B+ Tree that prevent meeting the
budget defined in CLAUDE.md:

1. **Point lookup too slow** — 96K ops/s vs target 800K ops/s.
2. **Insert too slow** — 60-104K ops/s vs target 180K ops/s.

## Problem context

### Blocker 1 — `find_child_compressed` allocates in the hot path

In `tree.rs::lookup`, each visited internal node calls `find_child_compressed`,
which constructs three `Vec`s per node:

```rust
let keys: Vec<Box<[u8]>> = (0..n).map(|i| node.key_at(i).to_vec().into_boxed_slice()).collect();
let children: Vec<u64> = (0..=n).map(|i| node.child_at(i)).collect();
let compressed = CompressedNode::from_keys(&keys, children);
```

Each heap allocation in the hot path destroys the CPU cache. A tree with 1M keys
has 3-4 levels → 9-12 heap allocations + hundreds of string copies per lookup.

In addition, `InternalNodePage::find_child_idx` uses O(n) linear scan over up to 223 keys.

### Blocker 2 — CoW in inserts without concurrency

`insert_leaf` and `cow_update_child` always perform alloc+write+free for each page in
the path, even when there is no split and no concurrent reader. With `&mut self`, the
exclusive ownership guarantees that no reader can observe the intermediate state — CoW
is unnecessary for each individual page (it is only needed to ensure that `root_pid`
changes atomically, which is already handled by `AtomicU64`).

## Subfase 2.5.1 — Lookup without allocations

### Inputs / Outputs
- Input: `BTree::lookup(key: &[u8])` — no changes to the signature
- Output: `Result<Option<RecordId>, DbError>` — no changes
- Observable behavior: identical to current (same results)
- Performance: ≥ 800K ops/s for 1M keys

### What changes

**`page_layout.rs` — `InternalNodePage::find_child_idx`:**
Replace the linear scan with binary search over the node's keys.
No allocation needed. Keys are already sorted by the B+ Tree invariant.

```
Before: (0..n).find(|&i| self.key_at(i) > key).unwrap_or(n)   → O(n) linear
After:  binary search over (0..n)                               → O(log n)
```

**`tree.rs` — `find_child_compressed`:**
Remove the method. Replace the call in `lookup` with `node.find_child_idx(key)`
directly on the `InternalNodePage`, without constructing a `CompressedNode`.

The prefix compression (`CompressedNode`) remains available as a utility for other
future uses (bulk load, statistical analysis), but is NOT used in the lookup hot path.

### Use cases
1. Lookup in empty tree → `Ok(None)`
2. Lookup of existing key in tree with 1M entries → `Ok(Some(rid))`
3. Lookup of non-existent key → `Ok(None)`
4. Internal node with keys sharing no common prefix → binary search works the same
5. Internal node with keys sharing a long prefix → binary search works the same

### Acceptance criteria
- [ ] `point_lookup/axiomdb_btree/1000000` ≥ 800K ops/s in benchmark
- [ ] All existing B+ Tree tests continue passing
- [ ] Zero heap allocations in the lookup path (verifiable by removing the Vec)
- [ ] `find_child_idx` uses binary search (verifiable by reading the code)

### Out of scope
- Changing the on-disk format
- Changing the public API of `BTree`
- Modifying `CompressedNode` (only stop calling it from the hot path)

### Dependencies
- None

---

## Subfase 2.5.2 — In-place insert when there is no split

### Inputs / Outputs
- Input: `BTree::insert(key: &[u8], rid: RecordId)` — no changes to the signature
- Output: `Result<(), DbError>` — no changes
- Observable behavior: identical (same data, same errors)
- Performance: ≥ 180K ops/s sequential insert of 1M entries

### What changes

**`tree.rs` — `insert_leaf` (no-split case):**
When `num_keys < ORDER_LEAF`, instead of alloc+write+free, write directly
to `old_pid`. Eliminates 1 alloc_page + 1 free_page per no-split insert.

**`tree.rs` — `cow_update_child`:**
Rename / replace with `in_place_update_child`: instead of alloc+write+free,
write the modified page directly to `old_pid`. Eliminates N alloc+free per
no-split insert where N = tree depth.

**Splits remain CoW:**
When there is a split (leaf or internal), two new pages are still allocated and the
original is freed — this is correct and unavoidable because a split creates two nodes from one page.

**`root_pid` still uses CAS:**
The `compare_exchange` pattern on `root_pid` is kept because it lays the foundation for
concurrency in Phase 7. With `&mut self` it always succeeds.

### Correctness invariant
With `&mut self`, Rust guarantees exclusivity — no other reader can access the
pages during the insert. Therefore, modifying a page in-place is correct:
there is no window where another thread sees a page in an intermediate state.

### Use cases
1. Sequential insert of 1M entries without frequent splits → in-place dominant
2. Random insert of 100K entries with frequent splits → in-place on upper levels
3. Insert causing leaf split → CoW on leaf (alloc 2 new), in-place on internals
4. Insert causing split up to root → full CoW (new root)

### Acceptance criteria
- [ ] `insert_1m_sequential/axiomdb_btree_1m` ≥ 180K ops/s
- [ ] `insert_sequential/axiomdb_btree_1k` ≥ 180K ops/s
- [ ] All existing tests continue passing
- [ ] Data is correct after 1M inserts + 1M lookups

### Out of scope
- Concurrency (handled in Phase 7)
- Changing the behavior of splits
- Bulk load / bulk insert

### Dependencies
- Subfase 2.5.1 completed (not technical, just logical order)
