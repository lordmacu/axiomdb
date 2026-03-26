# Spec: Indexed DELETE WHERE fast path (Phase 6.3b)

## Reviewed first

These AxiomDB files were reviewed before writing this spec:
- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/src/planner.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-storage/src/heap_chain.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-sql/src/fk_enforcement.rs`
- `benches/comparison/local_bench.py`

These research files were reviewed for behavior/technique references:
- `research/sqlite/src/delete.c`
- `research/sqlite/src/update.c`
- `research/postgres/src/backend/executor/README`
- `research/postgres/src/backend/executor/nodeModifyTable.c`
- `research/postgres/src/backend/access/table/tableam.c`
- `research/mariadb-server/sql/sql_delete.cc`
- `research/duckdb/src/planner/binder/statement/bind_delete.cpp`
- `research/duckdb/src/execution/operator/persistent/physical_delete.cpp`
- `research/oceanbase/src/sql/das/ob_das_delete_op.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_delete_op.cpp`
- `research/datafusion/datafusion/core/src/physical_planner.rs`
- `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`

## Research synthesis

### What AxiomDB already does today

`DELETE ... WHERE` in AxiomDB already preserves the important correctness
properties:
- full `WHERE` evaluation on decoded row values
- FK parent enforcement before heap mutation
- per-row index maintenance via `delete_from_indexes`
- savepoint / rollback safety
- root refresh after B-Tree mutations

The current bottleneck is not correctness. It is candidate discovery.
`execute_delete_ctx` and `execute_delete` still find rows by scanning the full
heap and filtering in memory, even when the predicate is sargable by an
available index.

This is especially visible in `benches/comparison/local_bench.py`, where
`DELETE ... WHERE id > ...` runs on a table with only `PRIMARY KEY (id)`, but
the current planner path cannot help because the SELECT-oriented planner does
not treat PK lookup as a usable index access path for this case.

### Borrow / reject / adapt

#### SQLite — `research/sqlite/src/delete.c`, `research/sqlite/src/update.c`
- Borrow: separate "find rowids first" from "delete rows later".
- Reject: VDBE opcode architecture and one-pass VM machinery.
- Adapt: AxiomDB will materialize `RecordId`s into a Rust vector before any
  heap or B-Tree mutation.

#### PostgreSQL — `research/postgres/src/backend/executor/README`,
#### `research/postgres/src/backend/executor/nodeModifyTable.c`,
#### `research/postgres/src/backend/access/table/tableam.c`
- Borrow: DML should consume row identity from a planning/access phase instead
  of discovering rows inside the physical delete loop.
- Reject: full `ModifyTable`, EPQ, trigger, and table-AM architecture.
- Adapt: AxiomDB will introduce a DELETE-specific candidate planner that
  returns index access metadata, and the executor will materialize
  `RecordId`s before deletion.

#### MariaDB — `research/mariadb-server/sql/sql_delete.cc`
- Borrow: keep `DELETE FROM t` full-table fast path separate from
  `DELETE ... WHERE` planned access path.
- Reject: server/handler abstraction and direct engine handler contracts.
- Adapt: keep Phase 5.16 bulk-empty DELETE path unchanged, and add a distinct
  indexed candidate-discovery path only for `DELETE ... WHERE`.

#### DuckDB — `research/duckdb/src/planner/binder/statement/bind_delete.cpp`,
#### `research/duckdb/src/execution/operator/persistent/physical_delete.cpp`
- Borrow: the delete pipeline should know which columns are needed downstream,
  not blindly fetch all row data.
- Reject: vectorized sink pipeline and delete-index machinery.
- Adapt: AxiomDB will compute a column decode mask for `WHERE` recheck, FK
  enforcement, index maintenance, and partial-index predicates.

#### OceanBase — `research/oceanbase/src/sql/das/ob_das_delete_op.cpp`,
#### `research/oceanbase/src/sql/engine/dml/ob_table_delete_op.cpp`
- Borrow: clear separation between candidate production and delete execution.
- Reject: DAS/distributed tablet execution model.
- Adapt: AxiomDB will stage local RID collection first, then call the existing
  heap/index delete machinery.

#### DataFusion — `research/datafusion/datafusion/core/src/physical_planner.rs`,
#### `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`
- Borrow: DML filter extraction and pushdown decisions should be explicit and
  testable.
- Reject: provider API as a storage-engine template.
- Adapt: AxiomDB will add explicit DELETE planner tests asserting when indexed
  candidate discovery is chosen vs. rejected.

## What to build

Add an indexed candidate-discovery path for single-table `DELETE ... WHERE`
statements so AxiomDB no longer performs a full heap scan when the predicate is
sargable by an available index.

This subphase changes only how DELETE finds candidate rows. It does not change:
- DELETE semantics
- transaction semantics
- FK behavior
- index-maintenance correctness
- affected-row semantics

The new path must:
- support PRIMARY KEY, UNIQUE, non-unique secondary, and eligible partial
  indexes
- materialize candidate `RecordId`s before any heap or index mutation
- recheck the full original `WHERE` predicate against fetched row values before
  deletion
- preserve all existing FK enforcement, partial-index maintenance, bloom
  dirtying, savepoint, rollback, and root-refresh behavior

## Inputs / Outputs

- Input:
  - `DeleteStmt`
  - resolved table metadata (`TableDef`, `ColumnDef`, `IndexDef`, FK metadata)
  - mutable storage / txn / bloom / catalog state
  - session collation and optional planner statistics in the ctx path
- Output:
  - unchanged `QueryResult::Affected { count, last_insert_id: None }`
- Errors:
  - unchanged SQL/execution/FK/index errors from current DELETE paths
  - no new user-visible SQL syntax or wire-protocol errors in this subphase

## Use cases

1. `DELETE FROM bench_users WHERE id > 5000`
   With `PRIMARY KEY (id)`, AxiomDB discovers candidate rows from the PK index
   instead of scanning the full heap.

2. `DELETE FROM users WHERE email = 'a@b.com'`
   With a UNIQUE secondary index on `email`, AxiomDB uses an index candidate
   path and then deletes the matching row with normal per-row index
   maintenance.

3. `DELETE FROM orders WHERE status = 'cancelled'`
   If no usable index exists, AxiomDB falls back to the current heap-scan path
   unchanged.

4. `DELETE FROM users WHERE name = 'jose'`
   Under non-binary session collation on `TEXT`, any candidate plan whose
   correctness depends on binary text ordering is rejected and the executor
   falls back to scan.

5. `DELETE FROM parent WHERE id = 1`
   If child tables reference `parent`, AxiomDB may still use indexed candidate
   discovery for parent rows, but FK enforcement must still run before heap
   delete and preserve RESTRICT/CASCADE/SET NULL behavior.

## Acceptance criteria

- [ ] `DELETE ... WHERE` no longer always calls `scan_table()` over the full
      heap when a usable index exists.
- [ ] PRIMARY KEY predicates are eligible for indexed DELETE candidate
      discovery.
- [ ] The DELETE candidate planner does not reuse the SELECT cost gate that can
      reject indexes on high-selectivity grounds.
- [ ] The executor materializes all candidate `RecordId`s before mutating heap
      or B-Tree state.
- [ ] The executor rechecks the full original `WHERE` predicate on fetched row
      values before deletion.
- [ ] Session collation rules remain correct: non-binary text collation rejects
      binary-order-dependent text index candidate plans.
- [ ] Partial indexes are only used when the query `WHERE` implies the partial
      index predicate.
- [ ] FK parent enforcement still runs before heap delete and preserves
      existing RESTRICT/CASCADE/SET NULL behavior.
- [ ] Existing per-row `delete_from_indexes()` remains the source of truth for
      correctness in this subphase.
- [ ] `DELETE FROM bench_users WHERE id > 5000` on the benchmark schema uses
      the indexed candidate path.
- [ ] Fallback behavior remains unchanged for non-sargable predicates, joins,
      OR-heavy predicates, and unsupported shapes.

## Out of scope

- Bulk index deletion or range leaf pruning in the B+Tree
- UPDATE candidate fast path
- Multi-table DELETE
- Covering delete that reconstructs all needed values from index key bytes
- New SQL syntax
- Reworking SELECT planner semantics
- MVCC/concurrency redesign beyond current Phase 6 assumptions

## Dependencies

- `5.16` bulk-empty DELETE path already complete
- `5.17` in-place B+Tree delete/write-path improvements already complete
- `6.3` basic planner/access methods
- `6.7` partial-index implication guard
- `6.9` FK auto-index support and current FK delete enforcement helpers

## ⚠️ DEFERRED

- Bulk index-delete path for large DELETE ranges → pending in a future DML
  optimization subphase after profiling this indexed candidate path
- Shared UPDATE candidate planner/path → pending in a future DML optimization
  subphase
- SELECT planner reconciliation for PRIMARY KEY access → separate planner
  cleanup subphase
