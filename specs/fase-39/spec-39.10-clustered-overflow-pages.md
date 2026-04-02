# Spec: 39.10 Clustered overflow pages

## What to build (not how)

Build the first overflow-page layer for clustered rows whose logical payload no
longer fits fully inline inside a clustered leaf cell.

This subphase must extend the clustered storage format so that:

- clustered leaf cells keep the primary key and `RowHeader` inline
- large row payloads spill only their tail bytes to `PageType::Overflow` pages
- clustered lookups and range scans reconstruct the full logical row bytes
  transparently
- clustered insert/update/delete paths work for both fully inline rows and
  overflow-backed rows
- structural clustered operations (`split`, `rebalance`, `merge`) continue to
  move page-local cell descriptors without rewriting logical row contents

The implementation must stay aligned with the Phase 39 objective:

- overflow is a clustered-storage concern, not a generic TOAST/BLOB subsystem
- secondary indexes stay bookmark-based from `39.9` and do not store overflow
  payloads themselves
- clustered `RowHeader` MVCC metadata stays inline in the leaf cell
- compression, WAL, undo, crash recovery, and executor integration remain out
  of scope for this subphase

## Inputs / Outputs

- Input:
  - `&mut dyn StorageEngine`
  - clustered root page id: `Option<u64>`
  - primary key bytes: `&[u8]`
  - row header: `&RowHeader`
  - logical row bytes: `&[u8]`
  - update/delete transaction id: `u64`
  - `&TransactionSnapshot`
- Output:
  - existing clustered-tree APIs (`insert`, `lookup`, `range`,
    `update_in_place`, `update_with_relocation`, `delete_mark`) continue to be
    the public entry points
  - large logical rows that exceed the local inline budget are accepted when
    their key plus inline descriptor still fits on a clustered leaf page
  - `lookup` and `range` return the full logical row bytes even when the row is
    overflow-backed
- Errors:
  - `DbError::KeyTooLong` when the key cannot fit on an otherwise empty
    clustered leaf page even with zero local row bytes
  - `DbError::ValueTooLarge` when the logical row length cannot be represented
    by the clustered overflow format
  - `DbError::HeapPageFull` when same-leaf rewrite still cannot fit after
    compaction and structural relocation is required
  - `DbError::BTreeCorrupted` when a clustered cell or overflow chain is
    truncated, references the wrong page type, ends early, or contains trailing
    pages beyond the expected payload length
  - page-not-found and storage I/O errors while reading, writing, allocating, or
    freeing clustered or overflow pages

## Use cases

1. A logical row fits inside the local inline budget for its primary key: the
   clustered leaf stores the entire row inline and no overflow page is
   allocated.
2. A logical row exceeds the local inline budget: the clustered leaf stores a
   local prefix inline and spills the remaining tail bytes to an overflow-page
   chain.
3. A point lookup or range scan reaches an overflow-backed clustered row: the
   engine follows the chain, reconstructs the logical row bytes, and returns the
   same user-visible result shape as an inline row.
4. `update_in_place` grows a row from inline to overflow-backed or rewrites one
   overflow-backed row into another overflow-backed row with the same key.
5. `update_in_place` shrinks an overflow-backed row back to a fully inline row
   and frees the obsolete overflow chain.
6. `delete_mark` stamps `txn_id_deleted` on an overflow-backed row but keeps its
   overflow chain reachable, because physical purge remains deferred.
7. Physical clustered row removal during relocate-update or merge frees the
   removed row's overflow pages after the physical delete succeeds.

## Acceptance criteria

- [ ] Clustered leaf pages can encode and decode both inline rows and
      overflow-backed rows while keeping the primary key and `RowHeader`
      inline.
- [ ] A dedicated clustered overflow-page helper exists for allocating, reading,
      and freeing overflow chains.
- [ ] Clustered insert transparently spills large logical rows to overflow pages
      instead of rejecting them solely because they exceed the old inline-only
      limit.
- [ ] Clustered lookup reconstructs the full logical row bytes for an
      overflow-backed row.
- [ ] Clustered range scan reconstructs the full logical row bytes for
      overflow-backed rows while preserving existing key-order iteration.
- [ ] `update_in_place` supports inline → overflow, overflow → overflow, and
      overflow → inline transitions for the current visible clustered version.
- [ ] `update_with_relocation` and physical clustered delete free obsolete
      overflow chains for rows that are physically removed or replaced.
- [ ] `delete_mark` does not free overflow pages; dead clustered cells and their
      chains remain physically present until later purge/VACUUM work.
- [ ] Clustered split/rebalance/merge keep overflow references valid without
      rewriting logical row bytes or reallocating chains during pure page
      movement.
- [ ] Unit/integration coverage exercises inline rows, overflow-backed rows,
      mixed scans, updates across overflow transitions, and physical removal of
      overflow-backed rows.

## Out of scope

- generic TOAST/BLOB reference storage shared across the row codec or SQL types
- LZ4 or any other compression for clustered overflow payloads
- WAL logging for overflow-page allocation/free or clustered overflow writes
- undo logging or reconstruction of older overflow-backed row versions
- crash recovery for partially written overflow chains
- executor-visible clustered `INSERT` / `UPDATE` / `DELETE`
- freeing overflow chains during logical delete-mark
- secondary-index storage of off-page row payloads

## Dependencies

- `specs/fase-39/spec-39.3-clustered-btree-insert.md`
- `specs/fase-39/spec-39.4-clustered-btree-point-lookup.md`
- `specs/fase-39/spec-39.5-clustered-btree-range-scan.md`
- `specs/fase-39/spec-39.6-clustered-btree-update-in-place.md`
- `specs/fase-39/spec-39.7-clustered-btree-delete.md`
- `specs/fase-39/spec-39.8-clustered-structural-rebalance.md`
- `specs/fase-39/spec-39.9-secondary-index-pk-bookmarks.md`
- `crates/axiomdb-storage/src/clustered_leaf.rs`
- `crates/axiomdb-storage/src/clustered_tree.rs`
- `crates/axiomdb-storage/src/page.rs`
- `crates/axiomdb-storage/src/engine.rs`
- `crates/axiomdb-core/src/error.rs`

## Research citations

- `research/sqlite/src/btreeInt.h` — borrow the idea of keeping a local payload
  prefix inline and spilling only the surplus bytes to an overflow-page chain.
- `research/mariadb-server/storage/innobase/handler/ha_innodb.cc` — borrow the
  product rule that only clustered records may externalize long variable-length
  columns; secondary-index records stay inline.
- `research/mariadb-server/storage/innobase/row/row0ins.cc` and
  `research/mariadb-server/storage/innobase/row/row0upd.cc` — borrow the
  separation between row insertion/update and the helper that materializes or
  rewrites externally stored payload.
- AxiomDB adaptation:
  unlike SQLite or InnoDB, the clustered leaf cell must keep `RowHeader` inline
  because Phase 39 still relies on inline MVCC visibility checks at the leaf.

## ⚠️ DEFERRED

- LZ4 compression and generic TOAST/BLOB references → pending in Phase 11
- WAL and crash recovery for overflow-backed clustered writes → pending in 39.11
  and 39.12
- reconstruction of older invisible clustered versions, including
  overflow-backed ones → pending in 39.11 and 39.12
- physical purge of delete-marked clustered rows and their overflow chains →
  pending in 39.18
- SQL executor integration for clustered overflow-backed rows → pending in 39.13
  through 39.17
