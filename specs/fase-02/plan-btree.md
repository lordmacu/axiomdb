# Plan: B+ Tree (Phase 2)

## Files to create / modify

```
crates/axiomdb-index/
├── Cargo.toml                     — add deps: axiomdb-core, axiomdb-storage, bytemuck
└── src/
    ├── lib.rs                     — public re-exports
    ├── page_layout.rs             — InternalNodePage, LeafNodePage (bytemuck; no node.rs abstraction — direct page I/O)
    ├── tree.rs                    — BTree, CRUD operations, CoW
    ├── iter.rs                    — RangeIter (lazy, tree-traversal; next_leaf not followed — deferred to Phase 7)
    └── prefix.rs                  — CompressedNode, prefix compression

crates/axiomdb-index/tests/
└── integration_btree.rs           — integration tests

crates/axiomdb-index/benches/
└── btree.rs                       — Criterion benchmarks

Cargo.toml (workspace root)        — verify that axiomdb-index is already in members[]
```

---

## Layout constants (derived from spec)

```rust
// src/page_layout.rs
pub const MAX_KEY_LEN: usize = 64;
pub const ORDER_INTERNAL: usize = 223;
pub const ORDER_LEAF: usize = 217;  // as-built: RID is [u8;10] (page_id:8+slot_id:2), not 12 bytes
pub const NULL_PAGE: u64 = u64::MAX;  // sentinel: "no next leaf" / "no child"

// Compile-time assertions (in page_layout.rs)
const _: () = assert!(size_of::<InternalNodePage>() <= PAGE_BODY_SIZE);
const _: () = assert!(size_of::<LeafNodePage>() <= PAGE_BODY_SIZE);
```

---

## Page data structures (page_layout.rs)

```rust
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
// As-built: RecordId is encoded as [u8; 10] (page_id: 8 bytes LE + slot_id: 2 bytes LE).
// No padding — plain byte arrays, not a struct, to keep bytemuck simple.

// ──── Internal Node ────────────────────────────────────────────────────
// Layout in page body (16,320 bytes):
//   header:    8 B  (is_leaf + _pad + num_keys + _pad)
//   key_lens: 223 B (1 byte per key: actual length, 0 = empty slot)
//   _align:    1 B  (pad to multiple of 8 for children)
//   children: 1,792 B (224 * u64)
//   keys:    14,272 B (223 * [u8; 64])
//   ─────────────────
//   Total:   16,296 B ≤ 16,320 ✓

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InternalNodePage {
    pub is_leaf:   u8,
    pub _pad0:     u8,
    pub num_keys:  u16,
    pub _pad1:     [u8; 4],
    pub key_lens:  [u8; ORDER_INTERNAL],
    pub _align:    [u8; 1],
    pub children:  [u64; ORDER_INTERNAL + 1],
    pub keys:      [[u8; MAX_KEY_LEN]; ORDER_INTERNAL],
}

// ──── Leaf Node (as-built: ORDER_LEAF = 217) ──────────────────────────
// Layout in page body (16,320 bytes):
//   header:    8 B  (is_leaf + _pad + num_keys + _pad)
//   next_leaf: 8 B  (u64, NULL_PAGE — field exists on disk; not followed by RangeIter)
//   key_lens: 217 B
//   rids:     2,170 B (217 * 10)  ← [u8;10] = page_id:8 + slot_id:2
//   keys:    13,888 B (217 * 64)
//   ─────────────────
//   Total:   16,291 B ≤ 16,320 ✓

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct LeafNodePage {
    pub is_leaf:   u8,
    pub _pad0:     u8,
    pub num_keys:  u16,
    pub _pad1:     [u8; 4],
    pub next_leaf: u64,                          // on-disk, not followed by RangeIter
    pub key_lens:  [u8; ORDER_LEAF],
    pub rids:      [[u8; 10]; ORDER_LEAF],       // [page_id:8 LE][slot_id:2 LE]
    pub keys:      [[u8; MAX_KEY_LEN]; ORDER_LEAF],
}
```

### Zero-copy access from `Page::body()`

```rust
// in page_layout.rs — SAFETY commented
pub fn read_internal(page: &Page) -> &InternalNodePage {
    // SAFETY: InternalNodePage is Pod, alignment 1 (all u8/u16/u64 packed in repr(C)).
    // Page::body() has PAGE_BODY_SIZE bytes >= size_of::<InternalNodePage>().
    // Content was written as InternalNodePage (verified by is_leaf == 0).
    bytemuck::from_bytes(&page.body()[..size_of::<InternalNodePage>()])
}

pub fn read_leaf(page: &Page) -> &LeafNodePage {
    // SAFETY: analogous to read_internal. is_leaf == 1 verified before calling.
    bytemuck::from_bytes(&page.body()[..size_of::<LeafNodePage>()])
}

pub fn write_internal(page: &mut Page, node: &InternalNodePage) {
    let bytes: &[u8] = bytemuck::bytes_of(node);
    page.body_mut()[..bytes.len()].copy_from_slice(bytes);
    page.update_checksum();
}

pub fn write_leaf(page: &mut Page, node: &LeafNodePage) {
    let bytes: &[u8] = bytemuck::bytes_of(node);
    page.body_mut()[..bytes.len()].copy_from_slice(bytes);
    page.update_checksum();
}
```

---

## As-built: no node.rs in-memory abstraction

> The as-built implementation does not have `node.rs`. The B+ Tree operates
> directly on `InternalNodePage` / `LeafNodePage` page casts — no intermediate
> in-memory `BTreeNode` enum. Operations read the page, perform in-place
> mutations on the struct, then write the modified page back.

```rust
// This in-memory enum was NOT implemented — keeping here as historical sketch.
pub enum BTreeNode {
    Internal {
        page_id:  u64,
        num_keys: usize,
        keys:     Vec<Box<[u8]>>,
        children: Vec<u64>,
    },
    Leaf {
        page_id:   u64,
        num_keys:  usize,
        keys:      Vec<Box<[u8]>>,
        rids:      Vec<RecordId>,
        next_leaf: u64,
    },
}

impl BTreeNode {
    pub fn load(storage: &dyn StorageEngine, page_id: u64) -> Result<Self, DbError>;
    pub fn flush(&self, storage: &mut dyn StorageEngine) -> Result<(), DbError>;
    pub fn is_full(&self) -> bool;       // num_keys >= ORDER - 1
    pub fn is_underfull(&self) -> bool;  // num_keys < ORDER / 2
}
```

---

## Main BTree (tree.rs)

```rust
pub struct BTree {
    storage:  Box<dyn StorageEngine>,
    root_pid: AtomicU64,    // AtomicU64 for CoW root swap
}

impl BTree {
    pub fn new(storage: Box<dyn StorageEngine>, root_page_id: Option<u64>)
        -> Result<Self, DbError>;

    pub fn lookup(&self, key: &[u8]) -> Result<Option<RecordId>, DbError>;
    pub fn insert(&mut self, key: &[u8], rid: RecordId) -> Result<(), DbError>;
    pub fn delete(&mut self, key: &[u8]) -> Result<bool, DbError>;
    pub fn range<'a>(&'a self, from: Bound<&[u8]>, to: Bound<&[u8]>)
        -> Result<RangeIter<'a>, DbError>;
    pub fn root_page_id(&self) -> u64;
}
```

### Lookup algorithm

```
fn lookup(key):
  pid = root_pid.load(Acquire)
  loop:
    page = storage.read_page(pid)
    if page.is_leaf():
      return binary_search(page.keys, key).map(|i| page.rids[i])
    else:
      pid = page.children[upper_bound(page.keys, key)]
```

### Insert algorithm (CoW)

```
fn insert(key, rid):
  path = traverse_to_leaf(key)   // Vec<(page_id, index_in_parent)>

  // Copy leaf
  leaf = load(path.last())
  new_leaf_pid = alloc_page(Index)
  new_leaf = leaf.clone_with_insert(key, rid)

  if !new_leaf.is_full():
    // Simple case: write new leaf, update parent
    write(new_leaf_pid, new_leaf)
    update_parent_pointer(path, new_leaf_pid)
    CAS(root_pid, old_root, new_root)
    free_old_pages(path)
    return Ok(())

  // Leaf split
  (left, right, separator) = new_leaf.split()
  left_pid  = alloc_page(Index)
  right_pid = alloc_page(Index)
  write(left_pid, left)
  write(right_pid, right)

  // Propagate split upward (may iterate if parent is also full)
  propagate_split(path, separator, left_pid, right_pid)

  // CAS of root
  CAS(root_pid, old_root, new_root)

  // Free orphaned pages
  free_old_pages(path)
```

### Delete algorithm (CoW)

```
fn delete(key):
  path = traverse_to_leaf(key)
  leaf = load(path.last())

  if !leaf.contains(key): return Ok(false)

  new_leaf = leaf.clone_with_delete(key)

  if !new_leaf.is_underfull() || path.len() == 1 (is root):
    // Simple case
    write(new_leaf_pid, new_leaf)
    CAS(root_pid, ...)
    return Ok(true)

  // Try redistribution with sibling
  sibling = load_sibling(path)
  if sibling.can_lend():
    redistribute(new_leaf, sibling)
  else:
    // Merge
    merged = merge(new_leaf, sibling)
    propagate_merge(path, ...)

  CAS(root_pid, ...)
  Ok(true)
```

---

## Lazy iterator (iter.rs) — as-built: tree traversal, not next_leaf

```rust
// As-built RangeIter traverses from root to find successive leaves.
// next_leaf is NOT followed — it is stale after CoW splits.
// Phase 7 (epoch-based reclamation) will enable safe next_leaf maintenance.
pub struct RangeIter<'a> {
    tree:      &'a BTree,
    slot_idx:  usize,
    // current leaf is re-loaded by re-traversing from root when slot_idx overflows
    end_bound: Bound<Box<[u8]>>,
    next_key:  Option<Box<[u8]>>,  // first key of next traversal range
}

impl Iterator for RangeIter<'_> {
    type Item = Result<(Box<[u8]>, RecordId), DbError>;

    fn next(&mut self) -> Option<Self::Item> {
        // 1. If slot_idx >= num_keys of current leaf →
        //      re-traverse from root using next_key as the new lower bound
        // 2. Check end bound → return None if we exceeded the limit
        // 3. Return (key, rid) from current slot
        // 4. Increment slot_idx
    }
}
```

---

## Prefix compression (prefix.rs)

```rust
pub struct CompressedNode {
    pub common_prefix: Vec<u8>,
    pub suffixes:      Vec<Vec<u8>>,
    pub children:      Vec<u64>,
}

impl CompressedNode {
    pub fn from_keys(keys: &[Box<[u8]>], children: Vec<u64>) -> Self;
    pub fn reconstruct_key(&self, idx: usize) -> Vec<u8>;
    pub fn find_child(&self, search_key: &[u8]) -> u64;
    pub fn common_prefix_len(keys: &[Box<[u8]>]) -> usize;
}
```

> Note: prefix compression in subfase 2.7 is optional in the page layout.
> It is implemented as an in-memory transformation over `BTreeNode::Internal`.
> It does not change the on-disk layout (keys are stored uncompressed in pages).
> Compression reduces RAM usage and improves cache locality in internal nodes.

---

## Implementation phases

### Phase 2.1 — page_layout.rs
1. Define `InternalNodePage`, `LeafNodePage` with bytemuck (no `node.rs` — direct page I/O)
2. Compile-time size asserts
3. Functions `read_internal`, `read_leaf`, `write_internal`, `write_leaf`
4. Unit tests: roundtrip serialize/deserialize of node

### Phase 2.2 — Lookup (tree.rs)
1. `BTree::new` — create empty root leaf
2. `BTree::lookup` — internal traverse + binary search in leaf
3. Tests: lookup in empty tree, lookup hit, lookup miss

### Phase 2.3 — Insert with split (tree.rs)
1. `BTreeNode::clone_with_insert` — insert into leaf (ordered)
2. `BTreeNode::split` — leaf split, returns (left, right, separator)
3. `BTree::insert` — case without split
4. `BTree::insert` — case with split + propagation
5. Tests: 1K random inserts → lookup all

### Phase 2.4 — Range scan (iter.rs)
1. `BTree::find_start_leaf` — navigate to first leaf in range
2. `RangeIter` with Bound
3. Tests: range [10..=50], range [..], range (42..)

### Phase 2.5 — Delete with merge (tree.rs)
1. `BTreeNode::clone_with_delete`
2. Redistribution from sibling
3. Merge + propagation to parent
4. Tests: delete → lookup miss, merge reduces height

### Phase 2.6 — Copy-on-Write (tree.rs refactor)
1. `AtomicU64` for `root_pid`
2. `free_old_pages` post-CAS
3. Concurrency test: 4 readers + 1 writer simultaneously

### Phase 2.7 — Prefix compression (prefix.rs)
1. `find_common_prefix_len`
2. `CompressedNode` encode/decode
3. Integrate into `BTreeNode::Internal` load/flush
4. Test: node with 100 keys `"usuario:XXXXX"` → verify savings

### Phase 2.8 — Tests + benchmarks
1. Full integration test (10K inserts + range + delete)
2. Crash recovery test (MmapStorage)
3. `benches/btree.rs` with Criterion: point lookup, range 1K, 1M inserts

---

## Tests to write

### Unit (src/)
```rust
#[cfg(test)]
mod tests {
    // page_layout.rs
    fn test_internal_node_roundtrip()  // serialize → deserialize → same data
    fn test_leaf_node_roundtrip()
    fn test_size_constraints()         // size_of verified at runtime as well

    // tree.rs
    fn test_lookup_empty_tree()
    fn test_insert_single()
    fn test_insert_duplicate_error()
    fn test_insert_causes_leaf_split()
    fn test_insert_causes_root_split()
    fn test_delete_existing()
    fn test_delete_nonexistent()
    fn test_delete_causes_merge()
    fn test_range_full()
    fn test_range_bounds()
    fn test_cow_new_pages_allocated()
    fn test_prefix_compression_roundtrip()
}
```

### Integration (tests/integration_btree.rs)
```rust
fn test_btree_1k_sequential_inserts_lookup_all()
fn test_btree_1k_random_inserts_lookup_all()
fn test_btree_range_scan_correctness()
fn test_btree_delete_half_then_lookup()
fn test_btree_crash_recovery()          // flush → reopen → lookup
fn test_btree_concurrent_reads_during_write()
```

### Benchmarks (benches/btree.rs)
```rust
fn bench_point_lookup_1m()             // vs BTreeMap
fn bench_range_scan_10k()              // 10k result
fn bench_insert_sequential_1m()        // throughput
fn bench_insert_random_100k()          // with splits
```

---

## Anti-patterns to avoid

- **NO** `unwrap()` in `src/` — always `?` or `map_err`
- **NO** `unsafe` without `// SAFETY:` comment
- **NO** loading the entire page into a Vec for simple operations (zero-copy first)
- **NO** unnecessary `Clone` of `Page` in the hot path of lookup
- **NO** implementing CoW with `Mutex` — use `AtomicU64` for the root
- **NO** locks in readers — lookup must be completely lock-free

---

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| `bytemuck::Pod` rejects the struct due to implicit padding | Verify with `assert_eq!(size_of::<InternalNodePage>(), N)` and adjust `_pad` |
| Internal node split when parent is also full | Implement `propagate_split` iteratively (not recursively) to avoid stack overflow |
| CoW frees pages still in use by readers | In Phase 2: single writer at a time (`&mut self`). Freeing is safe. In Phase 7 (MVCC): epoch-based reclamation |
| Incorrect merge corrupts `next_leaf` pointer | As-built: `next_leaf` exists on disk but is not tested at runtime (Phase 7 will validate it when CoW-consistent maintenance is added) |
| `AtomicU64` CAS fails under high contention | In Phase 2: `&mut self` on writes guarantees no contention. CAS in Phase 7 |
