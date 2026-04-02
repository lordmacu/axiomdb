# Spec: 39.6 Clustered B-Tree update in place

## What to build (not how)

Build the first clustered-row update path that rewrites non-key columns directly
inside the clustered leaf that already owns the row.

This subphase must add a dedicated clustered-tree update path that:

- accepts the current clustered root page id, or absence of a root for an empty tree
- locates one row by primary key through the clustered internal-page search path
- rewrites the row payload in the same clustered leaf page without changing the primary key
- updates the inline `RowHeader` for the new version
- keeps the row in the same logical leaf and preserves leaf-chain / parent structure
- succeeds when the new payload fits in the same leaf page after reusing the old cell space and, if needed, compacting the leaf
- rejects updates that would require a structural tree change in this subphase
- returns a storage-layer result that distinguishes “row updated” from “row not found / not visible”

The implementation must remain storage-first: it adds clustered-row mutation
semantics on top of the new clustered page primitives without wiring clustered
tables into the SQL executor yet.

## Inputs / Outputs

- Input:
  - `&mut dyn StorageEngine`
  - current clustered root page id, or absence of a root for an empty clustered tree
  - primary key bytes: `&[u8]`
  - replacement row payload bytes: `&[u8]`
  - update transaction id: `u64`
  - `&TransactionSnapshot`
- Output:
  - `Result<bool, DbError>`
  - `true` when the current inline version was found, visible to the supplied snapshot, and rewritten in the same clustered leaf
  - `false` when the tree is empty, the key does not exist, or the current inline version is not visible to the supplied snapshot
- Errors:
  - malformed/corrupt clustered leaf or internal page layout
  - invalid page-type mismatch during clustered traversal
  - page-not-found when the root or a child pointer references a missing page
  - `DbError::ValueTooLarge` when the replacement payload cannot fit on an otherwise empty clustered leaf for the given key
  - `DbError::HeapPageFull` when the replacement payload would require leaving the current leaf page or a structural tree change

## Use cases

1. A visible row is updated in place and remains reachable by the same primary key.
2. A replacement payload larger than the old payload still succeeds if it fits after reclaiming the old cell space and compacting the same leaf.
3. A replacement payload that no longer fits in the current leaf fails explicitly instead of silently turning into delete+insert tree surgery.
4. Updating a missing key returns `false`.
5. Updating a row whose current inline version is invisible to the supplied snapshot returns `false`.
6. The updated row is visible to the updating transaction and reflects a bumped `row_version`.

## Acceptance criteria

- [ ] A dedicated clustered-tree update API exists and is separate from heap-table update code.
- [ ] The update path descends to the owning clustered leaf by primary key and rewrites only that row.
- [ ] The primary key bytes remain unchanged after update.
- [ ] The updated row stays in the same clustered leaf page; parent separators and `next_leaf` links do not change on success.
- [ ] The inline `RowHeader` is rewritten with `txn_id_created = update_txn_id`, `txn_id_deleted = 0`, and `row_version` incremented from the previous inline version.
- [ ] Growth within the same leaf page succeeds when the page can accommodate the new cell after reclaiming the old cell space and compacting the page.
- [ ] Updates that would require a structural tree change return `DbError::HeapPageFull` in 39.6.
- [ ] Missing keys and invisible current inline versions return `false` instead of mutating the row.
- [ ] Unit/integration tests cover same-size rewrite, same-leaf growth rewrite, no-fit failure, missing key, and invisible-row behavior.

## Out of scope

- changing the primary key value
- splitting or merging leaves as part of update
- delete-mark semantics on clustered rows
- undo/version-chain reconstruction for older visible versions
- WAL logging or recovery for clustered updates
- executor integration for SQL `UPDATE`
- secondary-index maintenance for clustered updates

## Dependencies

- `specs/fase-39/spec-39.5-clustered-btree-range-scan.md`
- `crates/axiomdb-storage/src/clustered_tree.rs`
- `crates/axiomdb-storage/src/clustered_leaf.rs`
- `crates/axiomdb-storage/src/heap.rs`
- `crates/axiomdb-storage/src/engine.rs`
- `crates/axiomdb-core/src/traits.rs`
- `crates/axiomdb-core/src/error.rs`

## Research citations

- `research/sqlite/src/btree.c` — borrow the overwrite optimization idea:
  when the replacement payload fits the existing entry budget, prefer direct
  overwrite instead of delete+insert.
- `research/postgres/src/backend/access/heap/heapam.c` — borrow the caution that
  in-place updates are a special semantic mode and must stay explicit, not an
  accidental side effect of generic update code.
- AxiomDB adaptation:
  this subphase limits itself to same-leaf rewrite in `axiomdb-storage::clustered_tree`;
  it does not adopt PostgreSQL HOT chains, WAL-visible in-place update protocol,
  or SQLite's full table/index update surface.

## ⚠️ DEFERRED

- structural relocation for updates that no longer fit in the same leaf → pending in 39.8 and 39.10
- clustered delete-mark semantics → pending in 39.7
- undo/version chains for old-version visibility after update → pending in 39.11 and 39.12
- executor integration for clustered `UPDATE` → pending in 39.16
- secondary-index maintenance with PK bookmarks → pending in 39.9
