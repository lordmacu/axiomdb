# Spec: B+ Tree (Phase 2)

## What to build (not how)

A persistent B+ Tree that lives in pages of the `StorageEngine`, with support for
variable-length binary keys (up to 64 bytes), full CRUD operations,
range scan via leaf linked list, Copy-on-Write with atomic root, and prefix
compression on internal nodes.

---

## Fixed design decisions

| Aspect | Decision | Reason |
|---|---|---|
| Key type in API | `Box<[u8]>` | Immutable, 2 words, supports u64/UUID/serialized strings |
| Concurrency | Real CoW + `AtomicU64` root | Lock-free readers; no refactor in Phase 7 |
| Serialization to page | `bytemuck` / manual `repr(C)` | Fixed layout, zero-copy cast, no heavy dependency |
| Max key length on disk | 64 bytes | Covers u64 (8), UUID (16), short strings, composite keys |
| Target crate | `axiomdb-index` | Already exists as stub |

---

## Page layout constants

Available page body: `16,384 - 64 = 16,320 bytes`

### Internal node (`ORDER_INTERNAL = 223`)

```
[header:    8 bytes] is_leaf=0, pad, num_keys (u16), pad
[key_lens: 223 bytes] actual length of each key (1 byte each)
[_pad:       1 byte]  align to multiple of 8 for children
[children: 1,792 bytes] (223+1) * 8 bytes = 224 u64 pointers
[keys:    14,272 bytes] 223 * 64 bytes = fixed arrays zero-padded
────────────────────────────────────────────────
Total used: 16,296 bytes ≤ 16,320 ✓
```

### Leaf node (`ORDER_LEAF = 217`)

```
[header:    16 bytes] is_leaf=1, pad, num_keys (u16), pad + next_leaf (u64)
[key_lens: 217 bytes] actual length of each key (1 byte per key)
[rids:    2,170 bytes] 217 × 10 bytes: page_id([u8;8] LE) + slot_id([u8;2] LE)
                       — no padding: bytemuck uses [u8;10], alignment=1
[keys:   13,888 bytes] 217 × 64 bytes, zero-padded
────────────────────────────────────────────────
Total used: 16,291 bytes ≤ 16,320 ✓

Note: the original spec used 12-byte RIDs with alignment padding.
The implementation uses 10 bytes without padding (bytemuck+[u8;N] needs no alignment),
achieving 217 entries per leaf vs 211 — 3% more capacity, 17% less space per RID.
```

---

## Inputs / Outputs

### `BTree::new(storage, root_page_id) -> BTree`
- Creates an empty tree. Allocates a root leaf if `root_page_id == None`.

### `BTree::lookup(key: &[u8]) -> Result<Option<RecordId>>`
- Input: byte slice, length 1..=64
- Output: `Some(RecordId)` if it exists, `None` if not
- Errors: `KeyTooLong { len }`, `StorageError`

### `BTree::insert(key: &[u8], rid: RecordId) -> Result<()>`
- Input: key + RecordId
- Output: `Ok(())` or error
- Errors: `DuplicateKey { key }`, `KeyTooLong`, `StorageError`
- CoW semantics: copies the path root→leaf, CAS of root at the end

### `BTree::delete(key: &[u8]) -> Result<bool>`
- Input: key
- Output: `true` if it existed and was deleted, `false` if it did not exist
- With merge/redistribution when node has < ORDER/2 keys

### `BTree::range(from: Bound<&[u8]>, to: Bound<&[u8]>) -> RangeIter`
- Iterator that traverses the leaf linked list
- Supports `Bound::Included`, `Bound::Excluded`, `Bound::Unbounded`

### `BTree::root_page_id() -> u64`
- Returns the current page_id of the root (for persisting in catalog)

---

## Use cases

1. **Integer PK lookup**: `key = 42u64.to_be_bytes()` → `Some(RecordId)`
2. **Non-existent lookup**: key not in tree → `None`
3. **Insert causing leaf split**: full leaf → split → new internal node
4. **Insert propagating split to root**: full root → new root
5. **Range scan `[10..=50]`**: traverses linked list, returns exactly the rows in range
6. **Delete with redistribution**: node has ORDER/2 - 1 keys → borrows from sibling
7. **Delete with merge**: sibling is also at minimum → merge + update parent
8. **Concurrent reads during CoW write**: readers see previous snapshot until root CAS

---

## Acceptance criteria

### 2.1 — Node structures
- [ ] `InternalNodePage` and `LeafNodePage` are `bytemuck::Pod + Zeroable`
- [ ] `size_of::<InternalNodePage>() <= PAGE_BODY_SIZE` (compile-time assert)
- [ ] `size_of::<LeafNodePage>() <= PAGE_BODY_SIZE` (compile-time assert)
- [ ] Zero-copy conversion: cast of `Page::body()` to `&InternalNodePage` without copy
- [ ] Leaf linked list: `next_leaf` points to next leaf or `u64::MAX` if it is the last one

### 2.2 — Lookup
- [ ] Lookup of existing key returns correct `RecordId`
- [ ] Lookup of non-existent key returns `None`
- [ ] O(log n) complexity: does not traverse leaves unnecessarily
- [ ] Requires no lock

### 2.3 — Insert with split
- [ ] Insert into empty leaf works
- [ ] Insert into full leaf performs correct split (left half / right half)
- [ ] Split propagates key to the internal parent node
- [ ] Root split creates a new root (tree grows in height)
- [ ] After N inserts, all keys are recoverable with lookup
- [ ] `DuplicateKey` error if the key already exists

### 2.4 — Range scan
- [ ] `Bound::Unbounded` returns all rows in order
- [ ] `Bound::Included(k)` includes k
- [ ] `Bound::Excluded(k)` excludes k
- [ ] Iterator order is ascending by key (lexicographic byte comparison)
- [ ] Iterator is lazy (does not load all pages into memory)

### 2.5 — Delete with merge
- [ ] Delete of existing key returns `true`
- [ ] Delete of non-existent key returns `false`
- [ ] Redistribution when sibling has extra keys (borrow from neighbor)
- [ ] Merge when sibling is also at minimum
- [ ] Merge reduces tree height if root becomes empty

### 2.6 — Copy-on-Write
- [ ] `root_page_id()` uses `AtomicU64::load(Acquire)`
- [ ] Write copies only the path root→leaf (O(log n) new pages)
- [ ] Root swap is `AtomicU64::compare_exchange` (CAS)
- [ ] Orphaned pages (previous version) are freed with `free_page()` post-commit
- [ ] Concurrent readers: threads holding a reference to the old root see consistent data

### 2.7 — Prefix compression
- [ ] Internal nodes compute the common prefix of their keys
- [ ] `CompressedNode::reconstruct_key(idx)` returns the full original key
- [ ] Keys with a common prefix occupy fewer bytes in the node
- [ ] Lookup and range scan work the same with compression enabled

### 2.8 — Tests + benchmarks
- [ ] Unit tests for each operation (MemoryStorage, no I/O)
- [ ] Integration test: insert 10K rows + lookup all + range scan + delete half
- [ ] Crash recovery test: insert → flush → reopen → lookup
- [ ] Benchmark vs `std::collections::BTreeMap` for point lookup and range scan
- [ ] Benchmark: 1M sequential inserts (measures throughput and splits)

---

## Out of scope

- Multi-column composite keys (Phase 6)
- Bloom filter per index (Phase 6)
- Partial / covering indexes (Phase 6)
- Sparse index (Phase 6)
- Collations / sort keys (Phase 6)
- rkyv zero-copy deserialization (decision: bytemuck is sufficient)
- Keys > 64 bytes (overflow pages — later phase)

---

## Dependencies

- `axiomdb-core`: `RecordId`, `PageId`, `DbError`
- `axiomdb-storage`: `StorageEngine` trait, `Page`, `PAGE_SIZE`, `HEADER_SIZE`, `PageType::Index`
- New crates: `bytemuck = { version = "1", features = ["derive"] }` (already in workspace deps)
