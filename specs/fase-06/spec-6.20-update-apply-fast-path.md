# Spec: UPDATE apply fast path (Phase 6.20)

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `docs/fase-06.md`
- `benches/comparison/local_bench.py`
- `crates/axiomdb-sql/src/executor/update.rs`
- `crates/axiomdb-sql/src/executor/delete.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-storage/src/heap_chain.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `specs/fase-05/spec-5.20-stable-rid-update-fast-path.md`
- `specs/fase-06/spec-6.17-indexed-update-candidates.md`
- `specs/fase-06/spec-6.18-indexed-multi-row-insert-batch.md`

These research sources were reviewed for behavior and technique references:

- `research/mariadb-server/sql/sql_update.cc`
- `research/mariadb-server/storage/innobase/row/row0upd.cc`
- `research/postgres/src/backend/access/heap/heapam.c`
- `research/postgres/src/backend/executor/nodeModifyTable.c`
- `research/sqlite/src/update.c`
- `research/duckdb/src/execution/operator/persistent/physical_update.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_update_op.cpp`
- MySQL 8.4 reference manual:
  - `InnoDB multi-versioning`
  - `InnoDB buffer pool read-ahead`

## Research synthesis

### Benchmark reality

The current `update_range` comparison is narrower than it first looked:

- `local_bench.py --scenario update_range` runs a single `UPDATE ... WHERE id >= lo AND id < hi`
  inside one explicit transaction
- the default benchmark schema creates only `PRIMARY KEY (id)` unless
  `--indexes ...` is passed
- the default workload updates `score`, not `id`

That means the remaining gap after `6.17` is not primarily "planner still scans"
and not even primarily "secondary-index maintenance is still expensive" on the
default benchmark.

On the hot path, a winning implementation should behave like this:

1. use the PK B-Tree only to discover candidate `RecordId`s
2. fetch old rows in page batches, not one heap read per RID
3. skip physical work entirely for rows whose evaluated value image is unchanged
4. keep stable-RID rows on the same heap slot and batch their WAL append
5. only touch indexes when a key, predicate membership, or RID actually changes

The current AxiomDB path still leaves cost in steps 2, 3, and 4, and still
falls back to per-row new-key insertion on step 5 when index maintenance is
required.

### Borrow / reject / adapt

#### MariaDB — `research/mariadb-server/sql/sql_update.cc`

- Borrow: `compare_record(table)` style no-op skipping and explicit bulk-update
  framing.
- Reject: handler-specific `ha_direct_update_rows()` / `ha_bulk_update_row()`
  layering.
- Adapt: AxiomDB keeps one executor API, but stages UPDATE rows statement-locally
  and skips physical work for byte-identical rows.

#### InnoDB / MySQL — `research/mariadb-server/storage/innobase/row/row0upd.cc`
#### and MySQL 8.4 docs

- Borrow: clustered rows can stay local while secondary index entries are
  delete+insert only when affected.
- Borrow: sequential page access should benefit from page-local batching rather
  than repeated point reads.
- Reject: change buffering, background merge, and full InnoDB buffer-pool
  architecture.
- Adapt: AxiomDB keeps the current heap design, but batches RID reads by page
  and treats index maintenance as conditional work after stable-RID classification.

#### PostgreSQL — `research/postgres/src/backend/access/heap/heapam.c`

- Borrow: `modified_attrs` / HOT reasoning: if the tuple stays local and indexed
  columns do not change, index rewrites should disappear.
- Reject: full HOT chains and MVCC-visible old tuple preservation in this subphase.
- Adapt: AxiomDB keeps the existing `5.20` stable-RID model, but makes the
  stable-RID path materially cheaper by batching fetch and WAL work.

#### SQLite — `research/sqlite/src/update.c`

- Borrow: `indexColumnIsBeingUpdated()` and
  `indexWhereClauseMightChange()` style reasoning.
- Reject: VDBE register machine implementation details.
- Adapt: AxiomDB derives per-index "maybe affected" masks from changed columns
  and partial-index predicate dependencies before touching any B-Tree.

#### DuckDB — `research/duckdb/src/execution/operator/persistent/physical_update.cpp`

- Borrow: separate "no index work needed" from "delete+insert/index update"
  execution modes.
- Reject: full vectorized update sink/operator pipeline.
- Adapt: AxiomDB keeps row-at-a-time SQL semantics but adds a statement-level
  fast bailout when the UPDATE does not affect any index-relevant columns and
  every row stays stable-RID.

#### OceanBase — `research/oceanbase/src/sql/engine/dml/ob_table_update_op.cpp`

- Borrow: explicit primary/index write-set accounting.
- Reject: DAS / distributed DML architecture.
- Adapt: AxiomDB keeps single-node execution but requires batch helpers to
  preserve heap/index write parity and root updates once per index.

### Alternative approaches considered

#### Approach A — only batch UPDATE index inserts

- Pros: smallest diff; reuses `6.18` helpers immediately.
- Cons: insufficient for the default `update_range` benchmark because that
  benchmark is PK-only and should do no index work when stable-RID succeeds.

#### Approach B — chosen: UPDATE apply fast path

- Batch old-row fetches by RID page.
- Skip no-op rows before heap mutation.
- Batch WAL append for stable-RID rewrites.
- Batch fallback/index-affected new-key insertion and root persistence.

This is the chosen approach because it attacks the common PK-only benchmark and
still closes the remaining indexed fallback debt.

#### Approach C — full streaming/vectorized UPDATE executor

- Pros: highest theoretical ceiling; removes most `Vec<Value>` materialization.
- Cons: too wide for one subphase and not aligned with current executor design.

This is deferred.

## What to build (not how)

Add an UPDATE apply fast path after `6.17` so range/index-discovered UPDATE no
longer pays per-row heap reads and per-row WAL append on the common stable-RID
case, while also eliminating per-row new-key insertion when index maintenance
is required.

### Surface 1: batch row materialization from candidate RIDs

For `IndexLookup` and `IndexRange` candidate discovery:

- fetch candidate rows by grouping `RecordId`s per heap page
- preserve the logical candidate order returned by the access method
- keep the same snapshot visibility and full `WHERE` recheck semantics

The goal is to stop calling `TableEngine::read_row(...)` once per RID for
clustered PK/range updates.

### Surface 2: no-op UPDATE skip

Before mutating heap or indexes:

- evaluate the new row values
- compare against the old row values / encoded image
- if the row is unchanged, skip heap and index work for that row

This changes physical work, not SQL-visible matched-row semantics. The statement
must continue reporting the same affected-row count it reports today.

### Surface 3: batched stable-RID WAL append

For rows that remain on the same `(page_id, slot_id)`:

- keep using the existing `UpdateInPlace` WAL record format
- but append the batch using `reserve_lsns(...) + write_batch(...)`
  instead of one `append_with_buf(...)` call per row
- keep rollback / savepoint / crash-recovery semantics identical to the current
  `UpdateInPlace` path

This subphase does not introduce a new WAL entry type.

### Surface 4: conditional index maintenance with grouped apply

Index maintenance must become explicitly conditional:

- if RID stays stable and neither key columns nor partial-index membership
  inputs changed, skip that index entirely
- if RID changes, keep existing correctness semantics
- when an index is affected, do at most:
  - one grouped delete pass
  - one grouped insert pass
  - one root persistence write
  per index per statement

The current per-row new-key insertion/root persistence path is no longer allowed
for UPDATE apply.

### Surface 5: statement-level index fast bailout

If:

- no updated column participates in any index key or partial-index predicate,
  and
- every physically changed row stays stable-RID,

then UPDATE must skip index maintenance for the whole statement.

### Surface 6: preserve current SQL behavior

This subphase must preserve:

- `QueryResult::Affected`
- FK child/parent enforcement
- UNIQUE correctness
- partial-index correctness
- strict-mode / warning behavior
- fallback correctness when a row no longer fits in its slot

## Inputs / Outputs

- Input:
  - `UpdateStmt`
  - resolved `TableDef`, `ColumnDef`, `IndexDef`, FK metadata
  - candidate `RecordId`s or candidate `(RecordId, row)` pairs from `6.17`
  - mutable storage, active transaction, bloom registry, optional `SessionContext`
- Output:
  - unchanged `QueryResult::Affected { count, last_insert_id: None }`
  - materially faster `update_range` apply path on PK/range updates
- Errors:
  - unchanged SQL/type/FK/UNIQUE/partial-index errors
  - storage/WAL errors from batched fetch, rewrite, or logging
  - no new SQL syntax or wire-visible error classes

## Use cases

1. `UPDATE bench_users SET score = score + 1.0 WHERE id >= 1000 AND id < 6000`
   On the default benchmark schema, PK range discovery returns candidate RIDs,
   heap rows are fetched in page batches, stable-RID rewrites are WAL-logged in
   one batch append, and no index maintenance runs.

2. `UPDATE bench_users SET score = score WHERE id >= 1000 AND id < 6000`
   Rows still match the predicate, but unchanged rows perform no heap or index
   mutation.

3. `UPDATE users SET email = 'b@x.com' WHERE id BETWEEN 1 AND 1000`
   Stable-RID heap rewrite may still succeed, but the UNIQUE `email` index is
   maintained in grouped delete+insert form, with root persistence once.

4. `UPDATE users SET bio = bio || '...' WHERE id BETWEEN 1 AND 1000`
   Rows that no longer fit in place fall back to the current heap move path, but
   index maintenance still executes in grouped form rather than per row.

## Acceptance criteria

- [ ] `IndexLookup` / `IndexRange` UPDATE candidate reads no longer default to
      one `read_row(...)` call per candidate RID.
- [ ] Rows whose evaluated value image is unchanged perform no heap or index
      mutation, while current affected-row semantics remain unchanged.
- [ ] Stable-RID UPDATE batches append their WAL records with one batch writer
      call per statement instead of one append call per row.
- [ ] UPDATE index maintenance no longer performs per-row new-key insertion or
      per-row root persistence.
- [ ] `local_bench.py --scenario update_range --rows 50000 --range-rows 5000 --table`
      improves over the `6.17` baseline because PK-only range updates remain in
      the batched apply path.

## Out of scope

- New planner access-path rules beyond `6.17`
- New WAL entry types such as page-image or page-delta UPDATE records
- Background/deferred index maintenance or change buffering
- Full PostgreSQL-style HOT chains / Phase 7 MVCC redesign
- Full streaming/vectorized UPDATE execution

## Dependencies

- `5.20` stable-RID update fast path
- `6.17` indexed UPDATE candidate fast path
- `6.18` grouped insert/index apply helpers
- `3.18` WAL batch-append pattern (`reserve_lsns(...) + write_batch(...)`)

## ⚠️ DEFERRED

- Dedicated `PageRewrite` / delta-style WAL record if batched `UpdateInPlace`
  serialization is still too expensive after this subphase
- Fully streaming UPDATE execution that avoids most `Vec<Value>` materialization
- Background read-ahead / buffer-pool style prefetch infrastructure
