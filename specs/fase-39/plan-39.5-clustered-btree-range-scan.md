# Plan: 39.5 Clustered B-Tree range scan

## Files to create/modify

- `crates/axiomdb-storage/src/clustered_tree.rs` — add `ClusteredRangeIter`, range API, start-leaf descent helpers, bound checks, and leaf-chain iteration with prefetch
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs` — add integration coverage for bounded/full clustered range scans across multiple leaves

## Algorithm / Data structure

Use a dedicated lazy iterator in `clustered_tree`, similar in shape to the old
fixed-layout `RangeIter`, but over clustered leaves and returning `ClusteredRow`.

Core API shape:

```text
ClusteredRangeIter {
  storage: &dyn StorageEngine
  current_pid: u64
  next_leaf_cache: u64
  slot_idx: usize
  from: Bound<Vec<u8>>
  to: Bound<Vec<u8>>
  snapshot: TransactionSnapshot
  done: bool
}

range(storage, root_opt, from, to, snapshot) -> Result<ClusteredRangeIter<'_>, DbError>
```

Pseudocode:

```text
range(storage, root_opt, from, to, snapshot):
  if root_opt is None:
    return empty iterator

  start_pid = find_start_leaf(storage, root_pid, from)
  start_slot = find_start_slot(start_pid, from)
  return iterator { current_pid = start_pid, slot_idx = start_slot, ... }

iterator.next():
  loop:
    if done or current_pid == NULL_PAGE:
      return None

    page = read current leaf
    cache next_leaf on first access

    while slot_idx < num_cells:
      cell = read_cell(slot_idx)
      slot_idx += 1

      if key < lower bound:
        continue
      if key > upper bound:
        done = true
        return None
      if !cell.row_header.is_visible(snapshot):
        continue

      return Some(ClusteredRow { ...owned copies... })

    if next_leaf_cache == NULL_PAGE:
      done = true
      return None

    storage.prefetch_hint(next_leaf_cache, PREFETCH_DEPTH)
    current_pid = next_leaf_cache
    next_leaf_cache = NULL_PAGE
    slot_idx = 0
```

Implementation notes:

- Reuse `descend_to_leaf` shape from 39.4, but add a bound-aware helper for the start leaf.
- For bounded ranges, compute the initial slot with the existing exact-search helper result:
  - `Ok(pos)` or `Err(insert_pos)` both give the correct start position for included lower bounds
  - exclusive lower bounds may need to skip one more row on exact hit
- For full scans (`Bound::Unbounded` lower bound), start at the leftmost leaf and slot 0.
- Keep bound storage owned (`Bound<Vec<u8>>`) inside the iterator so it is self-contained.
- Prefetch depth should mirror the old B-tree iterator default unless a better constant is justified locally.

## Implementation phases

1. Add lower/upper bound helpers plus a leftmost-leaf / start-leaf descent helper.
2. Add `ClusteredRangeIter` and `range(...)` API to `clustered_tree.rs`.
3. Implement lazy leaf scanning with `next_leaf` traversal and prefetch hints.
4. Apply snapshot visibility filtering and explicit skip semantics for invisible current versions.
5. Add unit/integration tests for full scan, bounded scan, multi-leaf traversal, and invisible rows.

## Tests to write

- unit: empty tree range returns no rows
- unit: full scan returns sorted rows
- unit: included/excluded bounds return the correct slice
- unit: scan begins at the correct leaf/slot for a mid-key lower bound
- unit: invisible current version is skipped
- integration: build a 10K-row clustered tree, run bounded scans crossing many leaves, and verify row order and payload bytes
- bench: none in 39.5; clustered read benchmarking starts after the range path is stable and before SQL integration

## Anti-patterns to avoid

- Do not always start from the leftmost leaf for bounded ranges.
- Do not materialize the entire scan into `Vec<ClusteredRow>` as the primary API.
- Do not reuse the old `axiomdb-index::RangeIter` by genericizing the fixed-layout tree.
- Do not follow `next_leaf` before finding the correct start leaf for bounded scans.
- Do not invent undo/version-chain traversal before clustered undo exists.

## Risks

- Off-by-one mistakes in inclusive/exclusive lower bounds → mitigate with direct bound tests around exact-hit and miss-start cases.
- Re-reading leaves unnecessarily at boundaries → mitigate with cached `next_leaf` and slot position, following the old iterator shape.
- Confusing “invisible” with “end of range” → mitigate by keeping MVCC filtering orthogonal to bound checks and testing both together.

## Research citations

- `research/mariadb-server/sql/handler.cc` — adapt `read_range_first(...)` /
  `read_range_next()` into the clustered-tree controller as “seek once, then
  advance until end bound”.
- `research/sqlite/src/btree.c` — adapt the `sqlite3BtreeFirst()` /
  `sqlite3BtreeNext()` cursor model into a lazy iterator that owns its current
  scan position.
- Rejected:
  no SQL-layer handler abstraction, no lock-release semantics, and no generic
  reuse of the fixed-layout `axiomdb-index::RangeIter`, because `39.5` is still
  a storage-first clustered rewrite.
