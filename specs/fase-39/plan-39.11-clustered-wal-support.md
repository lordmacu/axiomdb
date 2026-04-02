# Plan: 39.11 Clustered WAL support

## Files to create/modify

- `crates/axiomdb-wal/src/entry.rs` — add clustered `EntryType` variants
- `crates/axiomdb-wal/src/clustered.rs` — new clustered WAL payload codec for
  exact row images and root metadata
- `crates/axiomdb-wal/src/txn.rs` — add clustered record helpers, clustered
  undo ops, per-table clustered root tracking, and rollback/savepoint handling
- `crates/axiomdb-wal/src/lib.rs` — export clustered WAL helper types when
  useful for tests
- `crates/axiomdb-storage/src/clustered_tree.rs` — add undo-oriented helpers to
  physically remove the current row by key and to restore an exact clustered row
  image
- `crates/axiomdb-wal/tests/integration_wal_entry.rs` — extend roundtrip
  coverage to the new clustered entry types
- `crates/axiomdb-wal/tests/integration_clustered_wal.rs` — new rollback and
  savepoint coverage for clustered inserts, deletes, and updates

## Algorithm / Data structure

Use logical clustered WAL with exact row images. Reject heap-style
`page_id + slot_id` undo because clustered leaf slots are not stable across
defragmentation, split, merge, and relocate-update.

### 1. Clustered WAL payload shape

Add a small codec dedicated to clustered row images:

```text
ClusteredRowImage
  [root_pid: u64 LE]
  [row_header bytes]
  [row_len: u32 LE]
  [row_data bytes]
```

`WalEntry.key` continues to store the primary key bytes.

Entry kinds:

- `ClusteredInsert`
- `ClusteredDeleteMark`
- `ClusteredUpdate`

Payload contract:

- insert:
  - `old_value = []`
  - `new_value = ClusteredRowImage(inserted version)`
- delete-mark:
  - `old_value = ClusteredRowImage(pre-delete version)`
  - `new_value = ClusteredRowImage(delete-marked version)`
- update:
  - `old_value = ClusteredRowImage(pre-update version)`
  - `new_value = ClusteredRowImage(replacement version)`

The row image always contains the full logical row bytes, even when the row is
overflow-backed on disk.

### 2. Active transaction state

Extend `ActiveTxn` with:

```text
clustered_roots: HashMap<u32, u64>   // table_id -> latest root pid seen by this txn
```

Rules:

- every clustered record helper updates `clustered_roots[table_id]` to the
  effective root after the successful forward write
- clustered undo looks up the current root in this map instead of trusting the
  root stored in an older undo entry
- if a table has no root entry yet, clustered undo treats that as corruption or
  caller misuse because clustered WAL must only be recorded after a successful
  clustered write

### 3. Undo model

Add clustered undo variants:

```text
UndoClusteredInsert {
  table_id,
  key,
}

UndoClusteredRestore {
  table_id,
  key,
  old_row_header,
  old_row_data,
}
```

Why this split:

- insert rollback only needs to remove the inserted version by key
- delete-mark and update rollback both reduce to “restore this exact old row
  image by key”

### 4. Storage helpers for rollback

Add two undo-oriented helpers in `clustered_tree`:

```text
delete_physical_by_key(storage, root_pid, key) -> Result<u64, DbError>
restore_exact_row_image(storage, root_pid, key, row_header, row_data) -> Result<u64, DbError>
```

Behavior:

- `delete_physical_by_key(...)`
  - find the current row by PK
  - physically remove it, freeing any current overflow chain
  - return the effective root pid after any internal rebalance
- `restore_exact_row_image(...)`
  - if a current row version exists, physically remove it first
  - insert the exact old row image using the provided `RowHeader` and logical
    row bytes
  - allocate a fresh overflow chain when the restored image needs it
  - return the effective root pid after the restore

Important invariant:

- rollback restores logical row contents and MVCC metadata, but it does not
  promise to rebuild the exact pre-write page topology

### 5. Forward record helpers

Add `TxnManager` helpers shaped around current clustered storage APIs:

```text
record_clustered_insert(table_id, root_pid_after, key, row_header, row_data)
record_clustered_delete_mark(table_id, root_pid_after, key, old_row, new_row)
record_clustered_update(table_id, root_pid_after, key, old_row, new_row)
```

Where `old_row` / `new_row` are exact `(RowHeader, row_data)` pairs.

All helpers:

1. require an active transaction
2. append the clustered WAL entry
3. update `clustered_roots[table_id]`
4. push the clustered undo op

This keeps the contract parallel to existing heap WAL helpers while adapting it
to clustered row identity.

### 6. Rollback / savepoint execution

Pseudocode:

```text
undo op in reverse:
  match op:
    UndoClusteredInsert { table_id, key }:
      root = clustered_roots[table_id]
      new_root = clustered_tree::delete_physical_by_key(storage, root, key)?
      clustered_roots[table_id] = new_root

    UndoClusteredRestore { table_id, key, old_row_header, old_row_data }:
      root = clustered_roots[table_id]
      new_root = clustered_tree::restore_exact_row_image(
        storage, root, key, old_row_header, old_row_data
      )?
      clustered_roots[table_id] = new_root
```

If a later split/merge changed the tree shape, the undo still uses the latest
root tracked by the transaction, not the root from the older forward write.

### 7. Scope boundary with 39.12

Do not implement clustered crash recovery replay here.

Minimal `39.11` expectation:

- new clustered WAL entries can be serialized and read back
- `TxnManager` can undo them in-process during `ROLLBACK` or
  `rollback_to_savepoint(...)`

Deferred to `39.12`:

- `CrashRecovery::recover(...)` replay/undo of clustered WAL entries after a
  crash
- handling truncated WAL or partially persisted clustered page state

## Implementation phases

1. Add clustered WAL entry variants and a dedicated clustered payload codec.
2. Extend `TxnManager::ActiveTxn` and `UndoOp` with clustered state and clustered
   undo variants.
3. Add rollback-oriented clustered storage helpers in `clustered_tree`.
4. Add clustered `record_*` helpers in `TxnManager` and wire them to update the
   per-table root map.
5. Extend `rollback()` and `rollback_to_savepoint()` to execute clustered undo.
6. Add targeted WAL roundtrip plus rollback/savepoint tests for clustered
   insert/delete/update, including overflow-backed rows.

## Tests to write

- unit: clustered WAL payload roundtrip for insert, delete-mark, and update
- unit: clustered row image roundtrip for inline and overflow-backed logical
  rows
- unit: clustered rollback removes an inserted row by key
- unit: clustered rollback restores the old row image after delete-mark
- unit: clustered rollback restores the old row image after update-in-place
- unit: clustered rollback restores the old row image after relocate-update
- unit: `rollback_to_savepoint()` undoes only the clustered writes after the
  savepoint
- unit: per-table clustered root tracking follows root changes across multiple
  clustered writes in one transaction
- integration: clustered rollback on an overflow-backed row reconstructs a fresh
  overflow chain and preserves lookup/range correctness afterward
- integration: clustered insert that caused a split can be rolled back without
  breaking later point lookup and range scan
- bench: none in `39.11`; clustered crash recovery and SQL-visible benchmarks
  remain for `39.12` and `39.20`

## Anti-patterns to avoid

- Do not reuse heap `PHYSICAL_LOC_LEN` encoding for clustered WAL; clustered
  pages do not provide stable slot identifiers.
- Do not attempt to fully reverse split/merge/root-collapse layout as part of
  statement rollback; restore logical row state instead.
- Do not store only local row prefixes in clustered WAL; rollback must be able
  to rebuild overflow-backed rows without relying on old overflow pages still
  existing.
- Do not couple `axiomdb-storage` back to `axiomdb-wal`; keep clustered storage
  helpers generic and callable from the WAL crate.
- Do not add `todo!()` or silent no-op branches for clustered undo paths.
- Do not broaden this subphase into crash recovery replay; that belongs to
  `39.12`.

## Risks

- A stale root pid in rollback could search the wrong tree after later root
  changes in the same transaction:
  mitigate with `ActiveTxn::clustered_roots` as the single source of truth for
  current roots during undo.
- Restoring an old overflow-backed row could fail after the current row's
  overflow chain was already freed:
  mitigate by storing the full logical old row bytes in WAL/undo, not the old
  overflow page ids.
- Logical rollback may leave a more fragmented tree than before:
  acceptable by design; mitigate by documenting that logical correctness, not
  exact page topology, is the rollback invariant in `39.11`.
- `delete_physical_by_key(...)` or `restore_exact_row_image(...)` could recurse
  into existing structural helpers and accidentally change the root:
  mitigate by always propagating and recording the returned effective root pid.
- Future `39.12` recovery could need more metadata than `39.11` stores:
  mitigate by keeping the clustered payload codec self-contained and including
  exact row images plus the latest root pid at write time.
