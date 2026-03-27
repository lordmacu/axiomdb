# Spec: 5.20 — Stable-RID UPDATE fast path

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/executor/update.rs`
- `crates/axiomdb-sql/src/executor/delete.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-sql/src/executor/shared.rs`
- `crates/axiomdb-sql/src/partial_index.rs`
- `crates/axiomdb-sql/src/fk_enforcement.rs`
- `crates/axiomdb-storage/src/heap.rs`
- `crates/axiomdb-storage/src/heap_chain.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `crates/axiomdb-wal/src/recovery.rs`
- `crates/axiomdb-sql/tests/integration_executor.rs`
- `crates/axiomdb-index/tests/integration_btree.rs`
- `benches/comparison/local_bench.py`
- `specs/fase-05/spec-5.18-heap-insert-tail-pointer-cache.md`
- `specs/fase-05/spec-5.19-btree-batch-delete.md`
- `specs/fase-05/spec-5.19a-executor-decomposition.md`

These research files were reviewed before writing this spec:

- `research/sqlite/src/update.c`
- `research/sqlite/src/delete.c`
- `research/mariadb-server/sql/sql_update.cc`
- `research/mariadb-server/storage/innobase/include/btr0cur.h`
- `research/mariadb-server/storage/innobase/row/row0upd.cc`
- `research/postgres/src/backend/executor/nodeModifyTable.c`
- `research/postgres/src/backend/access/heap/heapam.c`
- `research/duckdb/src/execution/operator/persistent/physical_update.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_update_op.cpp`
- `research/oceanbase/src/sql/das/ob_das_update_op.cpp`
- `research/datafusion/datafusion/core/src/physical_planner.rs`

## Research synthesis

### AxiomDB-first constraints

- `UPDATE` still discovers candidates with `scan_table(...)` in
  `crates/axiomdb-sql/src/executor/update.rs`. That is not the dominant
  bottleneck for the current benchmark, because `SELECT ... WHERE active=TRUE`
  is already competitive.
- The dominant cost is physical rewrite and index maintenance:
  `TableEngine::update_row[_with_ctx](...)` in
  `crates/axiomdb-sql/src/table.rs` is still implemented as heap
  `delete + insert`, so the `RecordId` changes.
- Because the `RecordId` changes, the current tracker text in
  `docs/progreso.md` that says "skip indexes whose columns are not in SET"
  is not correct for the current heap design. Even unchanged index keys still
  point to the old `RecordId`.
- `5.19` already batch-deletes old index keys, but the update path still pays
  per-row heap rewrite and per-row new-key insert, so its gain is limited.
- Phase 5 still operates under the current single-writer / no-concurrent-reader
  assumption used by other write-path optimizations. A full MVCC-safe HOT chain
  is not available yet.

### What we borrow

- `research/sqlite/src/update.c`
  - borrow: determine exactly which columns changed, and use that information to
    decide which indexes actually require maintenance
  - adapt: AxiomDB will compute `changed_cols` from evaluated UPDATE
    assignments, not from parsed syntax alone

- `research/mariadb-server/sql/sql_update.cc`
  - borrow: compare old vs new row content and use write-set information to
    avoid unnecessary work
  - adapt: AxiomDB will derive per-row `changed_cols` and per-index
    `index_affected` decisions before touching B-Trees

- `research/mariadb-server/storage/innobase/include/btr0cur.h`
  - borrow: separate cheap local updates from structural fallback paths
  - adapt: AxiomDB will add a stable-RID same-slot rewrite path and keep the
    existing delete+insert path as fallback

- `research/postgres/src/backend/access/heap/heapam.c`
  - borrow: unchanged indexed columns should avoid index rewrites whenever the
    physical row identity can stay stable
  - reject: full HOT chain and concurrent-snapshot guarantees in this subphase
  - adapt: AxiomDB will implement a Phase 5 stable-RID path, not full HOT

- `research/postgres/src/backend/executor/nodeModifyTable.c`
  - borrow: executor explicitly tracks which attributes were updated and uses
    that to decide whether index work is needed
  - adapt: AxiomDB will compute the same decision per row and per index

- `research/duckdb/src/execution/operator/persistent/physical_update.cpp`
  - borrow: split UPDATE into two execution modes:
    in-place/local update when indexes are unaffected, delete+insert when they
    are not
  - adapt: AxiomDB will keep one SQL surface but pick the physical write path
    row by row

- `research/oceanbase/src/sql/engine/dml/ob_table_update_op.cpp`
  - borrow: explicit old-row / new-row staging before storage mutation
  - adapt: AxiomDB will build update batches with old bytes, new bytes, and
    index-impact metadata before writing pages

- `research/oceanbase/src/sql/das/ob_das_update_op.cpp`
  - borrow: project only the old/new shapes the storage layer actually needs
  - adapt: AxiomDB will keep full-row decode for Phase 5, but will stage both
    old bytes and new encoded bytes once, not recompute them during mutation

- `research/datafusion/datafusion/core/src/physical_planner.rs`
  - borrow: DML filter extraction and assignments should be explicit inputs to
    the mutation path, not hidden in the storage operator
  - adapt: AxiomDB will keep candidate discovery unchanged in this subphase,
    but make assignment-change analysis an explicit UPDATE step

### What we reject

- the current tracker idea "skip all indexes whose columns are not in SET"
  without first stabilizing the `RecordId`
- planner-only optimization as the primary fix for the current benchmark
- full PostgreSQL-style HOT chains in this subphase
- InnoDB-style change buffering or deferred background index merge
- changing SQL-visible affected-row semantics in this subphase

### How AxiomDB adapts it

- Add a new stable-RID heap rewrite path that overwrites a row in the same slot
  when the new encoded row fits in the existing slot payload.
- Stable-RID rewrites are only valid in the current Phase 5 execution model.
  They do not attempt to preserve old row versions for concurrent snapshots.
- When the `RecordId` stays stable, AxiomDB can skip index maintenance for any
  index whose key columns and partial predicate inputs are unchanged.
- If the new row no longer fits in the old slot, AxiomDB falls back to the
  current delete+insert path and treats all indexes as affected.
- Rollback and crash recovery for the stable-RID path use explicit before/after
  image restore, not the current delete+insert undo model.

## What to build (not how)

Implement a stable-RID UPDATE fast path for Phase 5 that preserves the row's
`RecordId` when the new encoded row fits in the existing heap slot, and uses
that stable row identity to skip unnecessary index maintenance.

### Surface 1: stable-RID heap rewrite

The storage layer must gain a new update path that:

- rewrites the row payload in the same `(page_id, slot_id)`
- keeps the `RecordId` stable
- increments `row_version`
- updates the page checksum
- works in batch form grouped by page so many row rewrites on the same page do
  not cause repeated read/write cycles

This path is only valid when the new encoded row fits in the existing slot's
allocated payload area.

### Surface 2: explicit per-row changed-column analysis

The UPDATE executor must:

- evaluate all SET expressions against the old row
- build the new row values
- compute `changed_cols` from the evaluated old/new row, not only from the
  textual SET list
- partition matching rows into:
  - stable-RID candidates
  - fallback delete+insert rows

### Surface 3: selective index maintenance only when RID is stable

When a row uses the stable-RID path:

- indexes whose key columns are unchanged and whose partial predicate inputs are
  unchanged must be skipped
- indexes whose key columns changed must still be maintained
- partial indexes whose predicate result might change must still be maintained
- FK auto-indexes follow the same rule: if the indexed FK columns are unchanged
  and RID is stable, they are skipped

When a row falls back to delete+insert:

- all existing index-maintenance rules remain exactly as today

### Surface 4: stable-RID WAL / rollback / recovery

The WAL layer must gain a dedicated in-place update record that:

- stores both old row bytes and new row bytes for the same slot
- allows savepoint rollback and full rollback to restore the old bytes
- allows crash recovery to undo uncommitted stable-RID updates

This is distinct from the existing delete+insert `record_update(...)` contract.

### Surface 5: preserve current SQL behavior

This subphase changes physical write strategy, not SQL semantics.

It must preserve:

- statement result type
- FK child and parent enforcement
- partial-index correctness
- UNIQUE correctness
- `warning_count`, strict mode, and session behavior
- fallback behavior when a row cannot use the stable-RID path

## Inputs / Outputs

- Input:
  - `UpdateStmt`
  - resolved `TableDef`, `ColumnDef`, `IndexDef`, FK metadata
  - old row values and old row bytes for every matching row
  - evaluated new row values and encoded new row bytes
  - mutable storage, active transaction, bloom registry, and optional
    `SessionContext`
- Output:
  - unchanged `QueryResult::Affected { count, last_insert_id: None }`
  - faster UPDATE execution when rows can keep the same `RecordId`
- Errors:
  - unchanged SQL/execution/FK/index errors from current UPDATE
  - unchanged type/coercion errors
  - storage/WAL errors if in-place rewrite or rollback logging fails
  - no new SQL syntax or wire-visible error classes

## Use cases

1. `UPDATE bench_users SET score = score + 1 WHERE active = TRUE`
   On a table with only `PRIMARY KEY (id)`, rows use the stable-RID path and no
   PK index maintenance is performed because the row identity and PK key stay
   unchanged.

2. `UPDATE users SET email = 'b@x.com' WHERE id = 1`
   The heap row may still use the stable-RID path, but the UNIQUE `email` index
   is affected and must be delete+insert maintained for that one index.

3. `UPDATE users SET bio = bio || ' ...' WHERE id = 1`
   If the new encoded row no longer fits in the original slot, AxiomDB falls
   back to the existing delete+insert heap path and treats all indexes as
   affected.

4. `UPDATE users SET deleted_at = NOW() WHERE deleted_at IS NULL`
   For a partial unique index `WHERE deleted_at IS NULL`, predicate membership
   changes, so that partial index is affected even if the indexed key columns
   themselves did not change.

5. `ROLLBACK` after a stable-RID update
   The old row bytes are restored in the same slot and the row remains visible
   exactly as before the UPDATE.

## Acceptance criteria

- [ ] A stable-RID heap rewrite path exists and preserves `(page_id, slot_id)`
      when the new encoded row fits in the existing slot.
- [ ] The UPDATE executor computes `changed_cols` from evaluated old/new rows
      before index maintenance decisions.
- [ ] Index maintenance is skipped only when both conditions hold:
      stable RID and the index is unaffected by changed key columns or partial
      predicate inputs.
- [ ] The current tracker shortcut "skip untouched indexes" is not implemented
      on fallback delete+insert rows.
- [ ] Stable-RID updates grouped on the same heap page are written with one page
      read and one page write for that page batch.
- [ ] Partial indexes are treated as affected when the predicate may change,
      even if the key columns do not.
- [ ] FK enforcement remains correct for child and parent checks.
- [ ] Rollback and savepoint rollback restore old bytes for stable-RID updates.
- [ ] Crash recovery undoes uncommitted stable-RID updates correctly.
- [ ] `UPDATE bench_users SET score = score + 1 WHERE active = TRUE` on the
      benchmark schema does not perform PK delete+insert maintenance for rows
      that use the stable-RID path.

## Out of scope

- planner/indexed candidate discovery for UPDATE
- full HOT chain / forwarded-record design with concurrent snapshot visibility
- cross-page stable-RID forwarding pointers
- no-op update elision and `FOUND_ROWS` compatibility audit
- batched insert of new keys for affected indexes
- change buffering or background index merge

## Dependencies

- `5.18` heap append hint and batch heap write infrastructure
- `5.19` batch delete for old index keys
- `5.19a` executor decomposition
- current FK enforcement and partial-index predicate compilation

## ⚠️ DEFERRED

- Full MVCC-safe HOT chain / forwarded-row versioning → pending a Phase 7
  subphase dedicated to snapshot-safe stable row identities
- UPDATE candidate planner (`plan_update_candidates`) → pending a future
  optimizer subphase; it is not the main fix for the current benchmark
- Batched insert of new keys for affected indexes → pending a follow-up UPDATE
  optimization subphase after stable-RID writes land
