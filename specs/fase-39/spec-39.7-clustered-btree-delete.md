# Spec: 39.7 Clustered B-Tree delete

## What to build (not how)

Build the first clustered-row delete path that applies an MVCC delete-mark to
the current inline version of a row stored in a clustered leaf.

This subphase must add a dedicated clustered-tree delete path that:

- accepts the current clustered root page id, or absence of a root for an empty tree
- locates one row by primary key through the clustered internal-page search path
- marks the current inline row version as deleted by stamping `txn_id_deleted`
- preserves the primary key bytes, row payload bytes, leaf ownership, and tree structure
- keeps the physical cell on the clustered leaf so older snapshots can still observe it
- returns a storage-layer result that distinguishes “row delete-marked” from “row not found / not visible”

The implementation must remain storage-first: it adds clustered delete-mark
semantics on top of the clustered page primitives without wiring clustered
tables into the SQL executor yet.

## Inputs / Outputs

- Input:
  - `&mut dyn StorageEngine`
  - current clustered root page id, or absence of a root for an empty clustered tree
  - primary key bytes: `&[u8]`
  - delete transaction id: `u64`
  - `&TransactionSnapshot`
- Output:
  - `Result<bool, DbError>`
  - `true` when the current inline version was found, visible to the supplied snapshot, and delete-marked in place
  - `false` when the tree is empty, the key does not exist, or the current inline version is not visible to the supplied snapshot
- Errors:
  - malformed/corrupt clustered leaf or internal page layout
  - invalid page-type mismatch during clustered traversal
  - page-not-found when the root or a child pointer references a missing page
  - `DbError::BTreeCorrupted` when the target slot no longer matches the expected primary key

## Use cases

1. A visible row is delete-marked in place and disappears from lookups in the deleting transaction.
2. A row delete-marked by transaction `T` remains visible to an older snapshot taken before `T`.
3. A delete on a missing key returns `false`.
4. A delete on a row whose current inline version is not visible to the supplied snapshot returns `false`.
5. The row remains physically present on the leaf page after delete-mark so later purge/vacuum phases can reclaim it safely.

## Acceptance criteria

- [ ] A dedicated clustered-tree delete API exists and is separate from heap-table delete code.
- [ ] The delete path descends to the owning clustered leaf by primary key and mutates only that row.
- [ ] Successful delete-mark sets `txn_id_deleted = delete_txn_id` on the inline `RowHeader`.
- [ ] Successful delete-mark preserves `txn_id_created`, `row_version`, `_flags`, key bytes, and row payload bytes.
- [ ] Successful delete-mark does not change parent separators, `next_leaf`, or page ownership.
- [ ] Missing keys and invisible current inline versions return `false` instead of mutating the row.
- [ ] After delete-mark, the deleting transaction and newer snapshots no longer see the row through clustered lookup/range.
- [ ] After delete-mark, an older snapshot taken before the delete still sees the row through clustered lookup/range.
- [ ] Unit/integration tests cover empty-tree, missing-key, invisible-row, same-leaf delete-mark, and old-snapshot visibility behavior.

## Out of scope

- physical removal of dead clustered cells
- page underflow merge or rebalancing after delete
- clustered vacuum / purge
- undo logging or rollback restore for clustered delete
- WAL logging or crash recovery for clustered delete
- executor integration for SQL `DELETE`
- secondary-index maintenance for clustered delete

## Dependencies

- `specs/fase-39/spec-39.6-clustered-btree-update-in-place.md`
- `crates/axiomdb-storage/src/clustered_tree.rs`
- `crates/axiomdb-storage/src/clustered_leaf.rs`
- `crates/axiomdb-storage/src/heap.rs`
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs`
- `crates/axiomdb-core/src/traits.rs`
- `crates/axiomdb-core/src/error.rs`

## Research citations

- `research/mariadb-server/storage/innobase/btr/btr0cur.cc` — borrow the
  delete-mark idea: mutate the clustered record in place and defer physical
  removal to purge.
- `research/mariadb-server/storage/innobase/read/read0read.cc` — borrow the
  visibility rule that purge must not remove delete-marked rows still visible
  to active snapshots.
- `research/postgres/src/backend/access/heap/heapam.c` — borrow the principle
  that tuple deletion is a header-state transition first; physical cleanup is a
  separate concern.
- AxiomDB adaptation:
  this subphase only stamps `txn_id_deleted` on the inline clustered row header
  and relies on the existing `RowHeader::is_visible` rule; it does not add undo,
  WAL, HOT chains, purge, or page merge behavior yet.

## ⚠️ DEFERRED

- physical purge of delete-marked clustered cells → pending in 39.18
- page merge / underflow handling after delete → pending in 39.8
- undo logging and rollback restore for clustered delete → pending in 39.11 and 39.12
- executor integration for clustered `DELETE` → pending in 39.17
- secondary-index maintenance with PK bookmarks → pending in 39.9
