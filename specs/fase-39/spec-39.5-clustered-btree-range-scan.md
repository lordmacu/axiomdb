# Spec: 39.5 Clustered B-Tree range scan

## What to build (not how)

Build the first ordered scan path for clustered storage: scan a contiguous
primary-key range directly over clustered leaf pages and return visible inline
rows in primary-key order.

This subphase must add a dedicated clustered-tree range scan path that:

- accepts the current clustered root page id, or absence of a root for empty-tree scan
- supports bounded and unbounded lower/upper primary-key bounds
- descends the clustered tree to find the first relevant leaf
- scans leaf cells in sorted order
- follows the `next_leaf` chain to continue the scan across leaves
- applies MVCC visibility using `RowHeader::is_visible(&TransactionSnapshot)`
- issues `StorageEngine::prefetch_hint(...)` when stepping to the next leaf
- returns the current inline version only; invisible current versions are skipped in 39.5 because clustered undo/version-chain reconstruction is still not implemented

The implementation must remain storage-first: it builds a clustered ordered-read
path on top of the new page primitives without replacing the existing heap+index executor yet.

## Inputs / Outputs

- Input:
  - `&dyn StorageEngine`
  - current clustered root page id, or absence of a root for an empty clustered tree
  - lower bound: `std::ops::Bound<Vec<u8>>`
  - upper bound: `std::ops::Bound<Vec<u8>>`
  - `&TransactionSnapshot`
- Output:
  - `Result<ClusteredRangeIter<'_>, DbError>` for lazy scanning
  - each iterator item is `Result<ClusteredRow, DbError>`
- Iterator semantics:
  - yields rows in primary-key ascending order
  - yields only rows whose current inline version is visible to the supplied snapshot
  - stops at the first row beyond the upper bound
  - yields no rows for an empty tree or an empty bound interval
- Errors:
  - malformed/corrupt clustered leaf or internal page layout
  - invalid page-type mismatch during clustered traversal
  - page-not-found when the root or a child pointer references a missing page

## Use cases

1. Full scan over a clustered tree returns all visible rows in primary-key order.
2. Bounded scan returns only rows inside `[lo, hi]`, `(lo, hi)`, `[lo, hi)`, or `(lo, hi]`.
3. Scan starting in the middle of the keyspace descends to the first relevant leaf and does not walk from the leftmost root leaf unnecessarily.
4. Scan crossing multiple leaves follows `next_leaf` and preserves ordering.
5. Rows whose current inline version is invisible to the snapshot are skipped.
6. An empty tree or empty range produces no rows.

## Acceptance criteria

- [ ] A dedicated clustered-tree range API exists and is separate from the current fixed-layout B-tree path.
- [ ] Full scan over a clustered tree yields rows in primary-key ascending order.
- [ ] Bounded scans respect inclusive/exclusive lower and upper bounds.
- [ ] The scan descends to the first relevant leaf instead of always starting from the leftmost leaf for bounded ranges.
- [ ] Multi-leaf scans follow `next_leaf` pointers to continue in `O(1)` per leaf boundary.
- [ ] MVCC visibility is checked with `RowHeader::is_visible(&TransactionSnapshot)` for every yielded row.
- [ ] Invisible current inline versions are skipped rather than reconstructed in 39.5.
- [ ] `StorageEngine::prefetch_hint(...)` is called when stepping to the next leaf.
- [ ] Unit/integration tests cover full scan, bounded scan, multi-leaf scan, invisible rows, and empty tree/range behavior.

## Out of scope

- undo/version-chain traversal for older visible versions
- executor integration for SQL `SELECT`
- clustered secondary-index bookmark lookup
- update/delete semantics on clustered rows
- WAL logging or recovery changes
- benchmarks and user-visible performance claims

## Dependencies

- `specs/fase-39/spec-39.4-clustered-btree-point-lookup.md`
- `crates/axiomdb-storage/src/clustered_tree.rs`
- `crates/axiomdb-storage/src/clustered_leaf.rs`
- `crates/axiomdb-storage/src/clustered_internal.rs`
- `crates/axiomdb-storage/src/engine.rs`
- `crates/axiomdb-core/src/traits.rs`
- `crates/axiomdb-index/src/iter.rs` for iterator-shape reference only

## Research citations

- `research/mariadb-server/sql/handler.cc` — borrow the `read_range_first(...)` / `read_range_next()` shape:
  seek to the first row in range once, then advance sequentially while checking
  the end bound.
- `research/sqlite/src/btree.c` — borrow the cursor lifecycle behind
  `sqlite3BtreeFirst()` / `sqlite3BtreeNext()`:
  a range scan should be a stateful iterator, not repeated tree descent.
- AxiomDB adaptation:
  the current subphase keeps this entirely inside `axiomdb-storage::clustered_tree`
  over `next_leaf` plus `StorageEngine::prefetch_hint(...)`; it does not adopt
  SQL-layer range planning, row locking, or executor-visible clustered reads yet.

## ⚠️ DEFERRED

- older-version reconstruction through clustered undo/version chains → pending in 39.6, 39.7, 39.11, and 39.12
- executor integration for clustered `SELECT` → pending in 39.15
- secondary-index-to-clustered lookup path → pending in 39.9
- clustered read benchmarks and SQL-visible performance claims → pending after 39.5 when the full read path exists end-to-end
