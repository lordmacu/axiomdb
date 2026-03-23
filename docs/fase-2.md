# Phase 2 — B+ Tree CoW

**Status:** ✅ Completed
**Crate:** `axiomdb-index`
**Spec/Plan:** `specs/fase-02/`

---

## What was implemented

A persistent B+ Tree on top of `StorageEngine` with:

- **Variable binary keys** up to 64 bytes (`Box<[u8]>` in API, `[u8; 64]` zero-padded on disk)
- **Copy-on-Write** with `AtomicU64` for the root (readers lock-free by design)
- **Insert with split** — leaf split and recursive propagation up to root when needed
- **Delete with rebalancing** — redistribution from siblings and merge
- **Lazy range scan** — `RangeIter` iterator that traverses the tree to move between leaves
- **Prefix compression** — `CompressedNode` in memory for internal nodes

---

## Technical decisions

| Decision | Choice | Reason |
|---|---|---|
| KeyType in API | `&[u8]` (max 64 bytes) | Supports u64, UUID, short strings without overhead |
| Disk serialization | `bytemuck::Pod` + manual `unsafe impl` | Large arrays (>32) have no automatic Pod |
| Leaf linked list | Not used in range scan | CoW invalidates `next_leaf` pointers; iterator uses tree traversal |
| Concurrency | `AtomicU64` root, `&mut self` on writes | Correct for Phase 2; extensible to lock-free in Phase 7 |

### Why `next_leaf` is not used in the iterator

With CoW, when copying a leaf `L_old → L_new`, the previous leaf in the linked list
still points to `L_old` (already freed). Keeping the linked list in sync would require
CoW of the previous leaf as well, which requires knowing its page_id during each insert.

**Adopted solution**: the `RangeIter` traverses the tree from root to find
the next leaf. Cost: O(log n) per boundary crossing between leaves (acceptable).

---

## On-disk page layout

### Internal node (`ORDER_INTERNAL = 223`, size: 16,295 bytes)
```
[is_leaf=0: 1B][_pad: 1B][num_keys: 2B LE][_pad: 4B]  = 8B
[key_lens: 223B]                                         = 223B
[children: 224 × [u8;8] = 1,792B]                       = 1,792B
[keys: 223 × [u8;64] = 14,272B]                         = 14,272B
Total: 16,295B ≤ PAGE_BODY_SIZE (16,320B) ✓
```

### Leaf node (`ORDER_LEAF = 217`, size: 16,291 bytes)
```
[is_leaf=1: 1B][_pad: 1B][num_keys: 2B LE][_pad: 4B][next_leaf: 8B LE]  = 16B
[key_lens: 217B]                                                           = 217B
[rids: 217 × [u8;10] = 2,170B]  (page_id:8B + slot_id:2B LE)             = 2,170B
[keys: 217 × [u8;64] = 13,888B]                                           = 13,888B
Total: 16,291B ≤ PAGE_BODY_SIZE (16,320B) ✓
```

---

## Files created

```
crates/axiomdb-index/
├── Cargo.toml                      — deps: bytemuck, axiomdb-storage
└── src/
    ├── lib.rs                      — re-exports BTree, RangeIter
    ├── page_layout.rs              — InternalNodePage, LeafNodePage, bytemuck
    ├── tree.rs                     — BTree: insert, delete, lookup, range
    ├── iter.rs                     — RangeIter (lazy, tree traversal)
    └── prefix.rs                   — CompressedNode (prefix compression)
crates/axiomdb-index/tests/
└── integration_btree.rs            — 8 integration tests
crates/axiomdb-index/benches/
└── btree.rs                        — Criterion: lookup, range scan, insert
specs/fase-02/
├── spec-btree.md
└── plan-btree.md
```

---

## Quality metrics

- **Tests:** 37 pass (28 unit + 8 integration + 1 doctest)
- **Clippy:** 0 errors (`-D warnings`)
- **Fmt:** clean
- **Benchmarks:** compile and run correctly
  - 100K random inserts: no panics
  - Point lookup 10K/100K/1M keys: functional
  - Range scan 100/1K/10K results: functional

---

## Notable bug fixed during implementation

**`rotate_right` in split_internal panics when `child_idx == n`**

Cause: `kl[n+1..=n].rotate_right(1)` calls rotate_right(1) on an empty slice → panic.
Fix: replace with `copy_within(child_idx..n, child_idx+1)` which handles empty ranges.

---

## Deferred items

Nothing critical. The following are left for later phases:

- `next_leaf` linked list maintained in CoW (Phase 7, with MVCC and epoch reclamation)
- Keys > 64 bytes with overflow pages (later phase)
- Bloom filter per index (Phase 6)
- Partial / covering / sparse indexes (Phase 6)
