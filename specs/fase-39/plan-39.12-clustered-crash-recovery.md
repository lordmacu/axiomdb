# Plan: 39.12 Clustered crash recovery

## Files to create/modify

- `crates/axiomdb-wal/src/recovery.rs`
  - add clustered recovery ops, clustered root tracking during scan/undo, and
    clustered root map in `RecoveryResult`
- `crates/axiomdb-wal/src/txn.rs`
  - seed `last_clustered_roots` on `open(...)` and `open_with_recovery(...)`
  - add a shared WAL scan helper if needed
- `crates/axiomdb-wal/tests/integration_clustered_recovery.rs`
  - add end-to-end clustered crash recovery coverage
- `crates/axiomdb-wal/tests/integration_clustered_wal.rs`
  - extend or reuse helpers where clustered row-image fixtures already exist
- `docs/fase-39.md`
  - close `39.12` with scope and deferred checkpoint/rotation note
- `docs/progreso.md`
  - mark `39.12` complete and update remaining deferred warnings
- `docs-site/src/internals/wal.md`
  - document clustered crash recovery semantics and clean-open root rebuild
- `docs-site/src/internals/storage.md`
  - document clustered logical recovery guarantees
- `docs-site/src/internals/btree.md`
  - document that recovery restores logical row state, not exact topology
- `docs-site/src/development/roadmap.md`
  - advance phase status to `39.12`
- `memory/project_state.md`
  - record clustered recovery/root rebuild status
- `memory/architecture.md`
  - record ownership of clustered recovery/root scan logic
- `memory/lessons.md`
  - record any new recovery-specific lesson

## Algorithm / Data structure

### 1. Extend recovery scan state

Maintain three clustered maps while scanning WAL forward:

- `committed_clustered_roots: HashMap<u32, u64>`
- `txn_clustered_roots: HashMap<TxnId, HashMap<u32, u64>>`
- `in_progress_clustered_ops: HashMap<TxnId, Vec<RecoveryOp>>`

For each clustered WAL entry:

```text
decode old/new ClusteredRowImage as needed

if txn exists in in_progress_clustered_ops:
  current roots for txn/table := new_row.root_pid (or old/new shared root_pid)
  push clustered RecoveryOp in chronological order

on Commit:
  move txn current clustered roots into committed_clustered_roots
  drop in-progress ops for txn

on Rollback:
  drop in-progress ops for txn
```

### 2. RecoveryOp variants

Add clustered recovery operations:

```text
UndoClusteredInsert {
  table_id,
  key,
}

UndoClusteredRestore {
  table_id,
  key,
  old_row,
}
```

These mirror `39.11` rollback semantics and deliberately avoid heap-style
physical locators.

### 3. Undo in reverse with mutable root map

After the WAL scan:

```text
recovered_roots = committed_clustered_roots.clone()

for each in-progress txn:
  seed txn-local roots from txn_clustered_roots[txn]
  for op in reverse chronological order:
    match op:
      UndoClusteredInsert:
        new_root = clustered_tree::delete_physical_by_key(storage, root, key)?
        root_map[table_id] = new_root_or_existing

      UndoClusteredRestore:
        new_root = clustered_tree::restore_exact_row_image(
          storage, root, key, old_row.header, old_row.data
        )?
        root_map[table_id] = new_root

  merge txn-local root map back into recovered_roots
```

Key invariant:

- root selection for the next undo step must come from the latest recovered root
  for that table, not from the stale `root_pid` embedded in an older WAL entry

### 4. Clean-open clustered root reconstruction

`TxnManager::open(...)` currently restores only `max_committed`. Extend it to
scan the WAL and rebuild:

- `max_committed`
- `last_clustered_roots`

Use the same forward WAL scan logic, but without applying recovery. Only the
last committed clustered root per table must survive.

### 5. RecoveryResult contract

Extend `RecoveryResult` with:

```rust
pub clustered_roots: HashMap<u32, u64>
```

Then `TxnManager::open_with_recovery(...)` seeds:

```rust
last_clustered_roots = result.clustered_roots.clone()
```

## Implementation phases

1. Add clustered recovery op variants and clustered root tracking to
   `recovery.rs`.
2. Replace the current `NotImplemented` branch for clustered WAL entries with
   real decode + recovery-op accumulation.
3. Apply clustered undo during recovery and return recovered clustered roots in
   `RecoveryResult`.
4. Extend `TxnManager::open(...)` and `open_with_recovery(...)` to seed
   `last_clustered_roots`.
5. Add targeted recovery tests for in-progress clustered insert/delete/update.
6. Add clean-reopen test that confirms committed clustered roots survive reopen
   through WAL scan.
7. Close docs/progress/memory after targeted validation passes.

## Tests to write

- unit:
  - recovery scan decodes clustered entry payloads without `NotImplemented`
  - clustered root tracking picks the latest root after multiple writes in one txn
  - clean-open root reconstruction returns the last committed clustered root
- integration:
  - crash recovery undoes uncommitted clustered insert
  - crash recovery undoes uncommitted clustered delete-mark
  - crash recovery undoes uncommitted clustered update on overflow-backed row
  - crash recovery undoes uncommitted relocate-update and preserves logical range order
  - clean reopen after committed clustered writes preserves `clustered_root(table_id)`
- bench:
  - none in `39.12`; clustered SQL-visible recovery/benchmarking remains later

## Anti-patterns to avoid

- Do not redesign clustered WAL into page-image or topology-physical replay here.
- Do not rely on `(page_id, slot_id)` for clustered recovery.
- Do not assume the `root_pid` embedded in each WAL row image remains current for
  all later undo steps.
- Do not silently drop clustered root reconstruction on clean open; that would
  make clustered storage appear correct only inside one process lifetime.
- Do not touch executor/catalog integration in this subphase.

## Risks

- Root drift during reverse undo:
  mitigate by maintaining mutable per-table root maps through the undo pass.
- Duplicate-key collisions while restoring old row images:
  mitigate by always deleting the current PK version first in the restore helper
  and by replaying undo strictly in reverse WAL order.
- Overflow-backed row recovery could depend on freed chains:
  mitigate by always using the exact logical row image already stored in WAL,
  never the old overflow chain.
- Clean-open root reconstruction breaks after WAL rotation:
  document explicitly as deferred in this subphase instead of pretending it is solved.
