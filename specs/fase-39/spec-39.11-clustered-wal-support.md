# Spec: 39.11 Clustered WAL support

## What to build (not how)

Build the first WAL/undo layer for the clustered-tree mutation APIs that now
exist in storage (`insert`, `delete_mark`, `update_in_place`,
`update_with_relocation`).

This subphase must add clustered-specific WAL records and rollback support that:

- records clustered inserts, delete-marks, and updates as dedicated WAL entry
  kinds instead of reusing heap-oriented `page_id + slot_id` records
- stores exact logical row images, including overflow-backed row payloads, so a
  rollback can reconstruct the previous clustered row version even when the old
  overflow chain was already freed
- undoes clustered writes by primary key plus exact row image, not by heap-style
  physical slot location
- keeps clustered tree structure valid after rollback, but does not require page
  splits, merges, or root changes to be physically reversed to their pre-write
  shape
- tracks the current clustered root page id per touched table inside the active
  transaction so rollback can find the latest tree shape in the storage-first
  Phase 39 world where clustered tables are not yet catalog-driven

The implementation must stay aligned with the Phase 39 objective:

- this is still storage-first and WAL-first, not clustered executor integration
- clustered WAL covers logical row mutations now; crash recovery replay of those
  records is deferred to `39.12`
- structural side effects such as split/merge/root-collapse may persist after
  rollback as long as the logical row set and MVCC-visible row images are
  restored correctly
- heap/index WAL behavior must remain unchanged

## Inputs / Outputs

- Input:
  - `&mut TxnManager`
  - `&mut dyn StorageEngine`
  - `table_id: u32`
  - current clustered root page id
  - primary key bytes: `&[u8]`
  - exact old/new clustered row image:
    - `RowHeader`
    - logical row bytes `&[u8]`
  - update/delete transaction snapshot and txn id already used by the storage
    path
- Output:
  - dedicated clustered WAL entry kinds for clustered insert, delete-mark, and
    update
  - `TxnManager` record helpers for clustered insert/delete/update that update
    the active transaction's current clustered root page id for the table
  - `rollback()` and `rollback_to_savepoint()` restore clustered logical row
    state for all recorded clustered operations in reverse order
- Errors:
  - `DbError::NoActiveTransaction` when recording clustered WAL outside an
    explicit transaction
  - storage I/O errors while applying clustered rollback helpers
  - existing clustered-tree errors such as `DuplicateKey`, `KeyTooLong`,
    `ValueTooLarge`, `HeapPageFull`, `BTreeCorrupted`, and page-not-found
    errors
  - WAL serialization/deserialization errors for malformed clustered payloads

## Use cases

1. A clustered insert succeeds, including a split or an overflow-chain
   allocation, and later `ROLLBACK` removes the inserted row by primary key.
2. A clustered delete-mark succeeds and `ROLLBACK` restores the exact old row
   image, including the old `RowHeader`.
3. A same-leaf clustered update succeeds and `ROLLBACK` restores the exact old
   row image and version metadata.
4. A relocate-update succeeds because the replacement row had to move, and
   `ROLLBACK` deletes the replacement version and reinserts the old exact row
   image even if the tree keeps the post-split/post-merge shape.
5. A statement inside an explicit transaction mutates clustered rows and then
   fails; `rollback_to_savepoint(...)` restores only that statement's clustered
   writes while keeping the transaction active.
6. A clustered row is overflow-backed before or after the mutation; WAL still
   stores the full logical row bytes so rollback is not blocked by freed
   overflow pages.

## Acceptance criteria

- [ ] `EntryType` contains dedicated clustered WAL variants for clustered
      insert, delete-mark, and update.
- [ ] Clustered WAL payloads round-trip through `WalEntry` serialization and
      deserialization.
- [ ] Clustered WAL payloads store exact logical row images, not heap-style
      `page_id + slot_id` locators.
- [ ] `TxnManager` has clustered record helpers that maintain the latest
      clustered root page id per touched table inside the active transaction.
- [ ] `UndoOp` contains clustered undo variants that restore logical row state
      by primary key plus exact row image.
- [ ] `rollback()` restores clustered inserts, delete-marks, and updates in
      reverse order.
- [ ] `rollback_to_savepoint()` restores clustered inserts, delete-marks, and
      updates recorded after the savepoint, leaving the transaction active.
- [ ] Clustered rollback works for overflow-backed rows whose old overflow chain
      was already freed during the forward write path.
- [ ] Clustered rollback preserves correct logical lookup/range results even if
      page splits, merges, or root changes are not physically reversed to the
      exact pre-write layout.
- [ ] Targeted unit/integration coverage exercises clustered WAL round-trip plus
      rollback/savepoint recovery for insert, delete-mark, update-in-place, and
      relocate-update.

## Out of scope

- crash recovery replay of clustered WAL records during database open
- REDO of clustered WAL after process crash or power failure
- page-LSN tracking for clustered pages
- physical reversal of clustered split/merge/root-collapse as part of
  statement-level rollback
- executor-visible clustered `INSERT` / `UPDATE` / `DELETE`
- catalog persistence of clustered roots
- VACUUM / purge of delete-marked clustered rows
- benchmarks for SQL-visible clustered writes

## Dependencies

- `specs/fase-39/spec-39.3-clustered-btree-insert.md`
- `specs/fase-39/spec-39.6-clustered-btree-update-in-place.md`
- `specs/fase-39/spec-39.7-clustered-btree-delete.md`
- `specs/fase-39/spec-39.8-clustered-structural-rebalance.md`
- `specs/fase-39/spec-39.10-clustered-overflow-pages.md`
- `crates/axiomdb-storage/src/clustered_tree.rs`
- `crates/axiomdb-storage/src/clustered_leaf.rs`
- `crates/axiomdb-storage/src/clustered_overflow.rs`
- `crates/axiomdb-wal/src/entry.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `crates/axiomdb-core/src/error.rs`

## Research citations

- `research/mariadb-server/storage/innobase/row/row0uins.cc` — borrow the idea
  that clustered insert undo is row-logical: find the clustered record again and
  remove the inserted version instead of relying on a stable heap-style slot id.
- `research/mariadb-server/storage/innobase/row/row0umod.cc` and
  `research/mariadb-server/storage/innobase/row/row0upd.cc` — borrow the idea
  that rollback of clustered delete-mark/update restores row state from undo
  payload and does not require every structural B-tree side effect to be
  physically reversed.
- `research/postgres/src/backend/access/nbtree/nbtxlog.c` and
  `research/postgres/src/backend/access/transam/generic_xlog.c` — use as the
  contrast point: PostgreSQL replays B-tree structural changes physically during
  WAL redo, which belongs to crash recovery, not to statement-level rollback.
- AxiomDB adaptation:
  clustered leaf cells use variable-size slotted storage with no stable `slot_id`
  contract across defragmentation, split, merge, or relocate-update, so
  clustered undo must be keyed by primary key plus exact row image.

## ⚠️ DEFERRED

- clustered crash recovery replay during `open_with_recovery()` → pending in
  `39.12`
- REDO/idempotent recovery handlers for clustered WAL records → pending in
  `39.12`
- executor integration of clustered WAL record helpers → pending in `39.13`
  through `39.17`
- physical purge/VACUUM of delete-marked clustered rows → pending in `39.18`
- end-to-end clustered write benchmarks through the SQL path → pending in
  `39.20`
