# Spec: 39.12 Clustered crash recovery

## What to build (not how)

Build crash recovery support for the clustered WAL path introduced in `39.11`.

This subphase must extend `CrashRecovery`, `TxnManager::open(...)`, and
`TxnManager::open_with_recovery(...)` so that clustered rows remain logically
correct after an abrupt process crash.

The recovery contract must stay aligned with the current AxiomDB crash model
and the Phase 39 objective:

- recover clustered writes logically by primary key plus exact row image, not
  by heap-style `(page_id, slot_id)`
- undo in-progress clustered transactions during recovery by reusing the same
  clustered row-image semantics introduced in `39.11`
- reconstruct the latest known clustered root page id per `table_id` from the
  surviving WAL scan so a reopened `TxnManager` can keep using clustered tables
  in the storage-first Phase 39 world
- preserve clustered-tree integrity even when rollback leaves a different
  split/merge/root shape than the pre-crash forward path
- keep heap WAL behavior unchanged

This subphase is still storage-first:

- no clustered SQL executor integration
- no catalog persistence of clustered roots
- no page-LSN-based physical REDO for clustered pages

## Inputs / Outputs

- Input:
  - `&mut dyn StorageEngine`
  - WAL path
  - clustered WAL entries from `39.11`:
    - `EntryType::ClusteredInsert`
    - `EntryType::ClusteredDeleteMark`
    - `EntryType::ClusteredUpdate`
  - `ClusteredRowImage` payloads carrying:
    - `root_pid`
    - exact `RowHeader`
    - exact logical row bytes
- Output:
  - `CrashRecovery` understands clustered entries instead of returning
    `DbError::NotImplemented`
  - in-progress clustered transactions are undone during recovery in reverse
    order
  - `RecoveryResult` contains the last recovered clustered root per touched
    `table_id`
  - `TxnManager::open_with_recovery(...)` seeds `last_clustered_roots` from the
    recovery result
  - `TxnManager::open(...)` reconstructs `last_clustered_roots` from the WAL
    scan on clean reopen
- Errors:
  - malformed clustered WAL payloads
  - storage I/O errors during clustered undo
  - existing clustered-tree errors such as `DuplicateKey`, `BTreeCorrupted`,
    `PageNotFound`, `HeapPageFull`, or overflow-chain decode errors while
    applying recovery

## Use cases

1. A transaction inserts clustered rows, crashes before `COMMIT`, and recovery
   removes those rows from the clustered tree.
2. A transaction delete-marks a clustered row, crashes before `COMMIT`, and
   recovery restores the exact old row image.
3. A transaction updates an overflow-backed clustered row, crashes before
   `COMMIT`, and recovery restores the old large row image even if the old
   overflow chain had been freed on the forward path.
4. A transaction performs relocate-update or split/merge side effects, crashes
   before `COMMIT`, and recovery restores correct logical lookup/range results
   even if page topology differs from the original pre-update layout.
5. The database reopens cleanly after committed clustered writes, and
   `TxnManager::open(...)` still knows the last clustered root per table.
6. Recovery is run twice on the same WAL window and remains idempotent.

## Acceptance criteria

- [ ] `CrashRecovery::recover(...)` no longer returns `NotImplemented` for
      in-progress clustered WAL entries.
- [ ] Recovery scan builds clustered undo work for `ClusteredInsert`,
      `ClusteredDeleteMark`, and `ClusteredUpdate`.
- [ ] Recovery undoes in-progress clustered writes in reverse order by primary
      key plus exact row image.
- [ ] Recovery correctly updates the current clustered root per `table_id` as
      clustered undo changes tree shape.
- [ ] `RecoveryResult` returns the last recovered clustered root per touched
      table.
- [ ] `TxnManager::open_with_recovery(...)` seeds `last_clustered_roots` from
      `RecoveryResult`.
- [ ] `TxnManager::open(...)` reconstructs `last_clustered_roots` from the WAL
      scan on clean reopen.
- [ ] Clustered crash recovery works for overflow-backed rows.
- [ ] Clustered crash recovery preserves correct logical lookup/range results
      after recovering split/merge/relocate-update cases.
- [ ] Targeted unit/integration coverage proves clustered recovery for insert,
      delete-mark, update, relocate-update, and clean reopen root restoration.

## Out of scope

- page-LSN-based physical REDO for clustered pages
- exact physical replay of clustered split/merge/root-collapse topology
- catalog persistence of clustered roots outside WAL scan
- WAL rotation / checkpoint persistence of clustered roots after the relevant
  clustered entries have been truncated from the WAL
- clustered SQL executor integration
- snapshot-visible old-version reconstruction on clustered reads
- VACUUM / purge of delete-marked clustered rows

## Dependencies

- `specs/fase-39/spec-39.11-clustered-wal-support.md`
- `crates/axiomdb-wal/src/recovery.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `crates/axiomdb-wal/src/clustered.rs`
- `crates/axiomdb-storage/src/clustered_tree.rs`
- `crates/axiomdb-storage/src/clustered_overflow.rs`

## Research citations

- `research/mariadb-server/storage/innobase/row/row0uins.cc` — use as evidence
  that rollback of clustered inserts can re-find the clustered row logically and
  remove it without a stable heap slot contract.
- `research/mariadb-server/storage/innobase/row/row0umod.cc` — use as evidence
  that rollback of clustered delete/update restores row state and tolerates
  intermediate structural side effects.
- `research/mariadb-server/storage/innobase/row/row0upd.cc` — reinforces the
  delete-mark-first clustered lifecycle that recovery must reverse logically.
- `research/postgres/src/backend/access/nbtree/nbtxlog.c` — use as the contrast
  point: PostgreSQL replays B-tree topology physically, which is broader than
  the Phase 39 storage-first clustered recovery cut.
- AxiomDB adaptation:
  clustered crash recovery should reuse `39.11`'s PK + row-image semantics so
  the engine can keep marching toward clustered index integration without
  inventing heap-style slot stability on slotted clustered pages.

## ⚠️ DEFERRED

- clustered root persistence across WAL checkpoint/rotation when clustered WAL
  history has been truncated → pending in later clustered catalog/root
  persistence work
- page-LSN-based REDO or physical clustered page replay → pending in later WAL
  durability phases
- clustered SQL executor integration → pending in `39.13` through `39.17`
- clustered purge / VACUUM → pending in `39.18`
