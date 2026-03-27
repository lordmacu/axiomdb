# Spec: 5.21b — Bulk B-Tree load for staged INSERT flush

## What to build

Add a `BTree::bulk_load_sorted` primitive that builds a B-Tree from scratch given a
pre-sorted slice of `(key, RecordId)` entries. This eliminates the per-key tree
traversal that dominates the flush phase of 5.21 when inserting into an empty index.

The current staged-INSERT flush calls `BTree::insert_in` N times at flush. Each call
traverses root→leaf (O(log N) page reads), binary-searches the leaf, and may split.
For 50 000 sequential keys this produces ~175 000 page I/O operations.

A bottom-up bulk load instead fills leaf pages sequentially (append-only, no search),
links them, and builds internal nodes in a single pass. Total I/O: ~234 page writes,
zero reads. Expected speedup: 5–10× on the index-insertion phase.

## Inputs / Outputs

- Input:
  - `old_root_pid: u64` — the current (empty) root page of the index
  - `entries: &[(&[u8], RecordId)]` — pre-sorted ascending by key, no duplicates
  - `fillfactor: u8` — leaf fill threshold (90 = default)
- Output:
  - `new_root_pid: u64` — root of the newly built tree
- Errors:
  - `DbError::StorageFull` if page allocation fails
  - `DbError::BTreeCorrupted` if `entries` is not sorted (debug assert)

## Use cases

1. `BEGIN; INSERT INTO t VALUES (1..50000); COMMIT;` on a freshly created table with
   a PRIMARY KEY or UNIQUE index. The flush detects `committed_empty` and uses
   `bulk_load_sorted` instead of N × `insert_in`.

2. Multi-column UNIQUE index on an empty table: same benefit, keys are composite but
   still sorted before bulk load.

3. Table with data already committed (non-empty index): falls back to existing
   `insert_in` loop — bulk_load_sorted is NOT used.

## Acceptance criteria

- [ ] `BTree::bulk_load_sorted` builds a valid B-Tree from sorted entries.
- [ ] Leaf pages are filled up to `fill_threshold(ORDER_LEAF, fillfactor)` entries.
- [ ] Leaf pages are linked via `next_leaf` pointers in key order.
- [ ] Internal nodes are built bottom-up, one level at a time.
- [ ] The old root page is freed after the new tree is built.
- [ ] `batch_insert_into_indexes` uses `bulk_load_sorted` when `committed_empty` contains the index.
- [ ] Existing `insert_in` path is NOT changed — only a new code path is added.
- [ ] `local_bench.py --scenario insert --rows 50000` shows measurable improvement.
- [ ] Range scan over the bulk-loaded tree returns all entries in key order.
- [ ] Unit tests verify: empty input, single entry, ORDER_LEAF entries (no split),
      ORDER_LEAF+1 entries (one split), 50 000 entries (multi-level tree).

## Out of scope

- Bulk loading into a non-empty B-Tree (requires merge — follow-up subphase)
- Sorted sequential insert with leaf caching (Approach B from brainstorm)
- Delta encoding or prefix compression on leaf pages

## Dependencies

- 5.21 transactional INSERT staging (provides `batch_insert_into_indexes` + `committed_empty`)
