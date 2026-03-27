# Spec: Indexed UPDATE candidate fast path (Phase 6.17)

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/executor/update.rs`
- `crates/axiomdb-sql/src/executor/delete.rs`
- `crates/axiomdb-sql/src/planner.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-sql/src/fk_enforcement.rs`
- `benches/comparison/local_bench.py`
- `specs/fase-05/spec-5.19-btree-batch-delete.md`
- `specs/fase-05/spec-5.20-stable-rid-update-fast-path.md`

These research files were reviewed for behavior/technique references:

- `research/sqlite/src/where.c`
- `research/sqlite/src/update.c`
- `research/postgres/src/backend/executor/nodeModifyTable.c`
- `research/mariadb-server/sql/sql_update.cc`
- `research/duckdb/src/execution/operator/persistent/physical_update.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_update_op.cpp`
- `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`

## Research synthesis

### What AxiomDB already does today

AxiomDB's UPDATE write path improved substantially in `5.20`, but candidate
discovery is still full-scan:

- both ctx and non-ctx UPDATE paths start with `TableEngine::scan_table(...)`
- only after scanning every visible row do they evaluate `WHERE`
- only then do they stage rows for stable-RID vs fallback update

That means `UPDATE ... WHERE id BETWEEN ...` still pays O(n) heap discovery
even though the row rewrite path is no longer the main bottleneck.

### Borrow / reject / adapt

#### SQLite — `research/sqlite/src/where.c`, `research/sqlite/src/update.c`
- Borrow: DML may consume row identities produced by the WHERE planner rather
  than rediscovering rows inside the update loop.
- Reject: VDBE one-pass opcode machinery.
- Adapt: AxiomDB will materialize candidate `RecordId`s before any heap or
  index mutation.

#### PostgreSQL — `research/postgres/src/backend/executor/nodeModifyTable.c`
- Borrow: UPDATE consumes row-locating info from the subplan.
- Reject: full `ModifyTable` executor architecture and EPQ handling.
- Adapt: a dedicated UPDATE candidate planner feeding the existing AxiomDB
  update executor.

#### MariaDB — `research/mariadb-server/sql/sql_update.cc`
- Borrow: UPDATE should keep changed-column/write-path logic separate from row
  access path selection.
- Reject: MySQL handler layering and direct engine bulk-update API.
- Adapt: preserve AxiomDB's `5.20` write semantics and only replace candidate
  discovery.

#### DuckDB — `research/duckdb/src/execution/operator/persistent/physical_update.cpp`
- Borrow: physical update mode and row discovery mode are separate concerns.
- Reject: vectorized update sink/operator pipeline.
- Adapt: keep the stable-RID/fallback write path, but feed it only the rows
  found via an indexed candidate plan when possible.

#### OceanBase — `research/oceanbase/src/sql/engine/dml/ob_table_update_op.cpp`
- Borrow: explicit staging of rows to update before storage mutation.
- Reject: DAS/distributed execution machinery.
- Adapt: local RID collection plus existing row staging in AxiomDB.

#### DataFusion — `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`
- Borrow: DML filter extraction should be explicit and testable.
- Reject: provider API as an implementation template.
- Adapt: explicit planner tests asserting when UPDATE uses indexed candidates.

### AxiomDB-first decision

This subphase is not a rewrite of the UPDATE write path. That already belongs to
`5.20`.

The chosen scope is:

- add `plan_update_candidates` / `plan_update_candidates_ctx`
- reuse the same indexability rules as indexed DELETE where possible
- materialize candidate RIDs before mutation
- then pass the surviving rows into the existing `5.20` update pipeline

## What to build

Add an indexed candidate-discovery path for single-table `UPDATE ... WHERE`
statements so AxiomDB no longer performs a full heap scan when the predicate is
sargable by an available index.

The new path must:

- support PRIMARY KEY, UNIQUE, non-unique secondary, and eligible partial
  indexes
- materialize candidate `RecordId`s before any heap or index mutation
- recheck the full original `WHERE` against fetched row values before updating
- preserve `5.20` stable-RID semantics, FK checks, partial-index correctness,
  and existing affected-row behavior

## Inputs / Outputs

- Input:
  - `UpdateStmt`
  - resolved table metadata (`TableDef`, `ColumnDef`, `IndexDef`, FK metadata)
  - mutable storage / txn / bloom / optional `SessionContext`
- Output:
  - unchanged `QueryResult::Affected { count, last_insert_id: None }`
  - faster indexed UPDATE candidate discovery
- Errors:
  - unchanged SQL/execution/FK/index errors
  - no new SQL syntax or wire-visible error classes

## Use cases

1. `UPDATE bench_users SET score = score + 1 WHERE id >= 1000 AND id < 2000`
   With `PRIMARY KEY (id)`, AxiomDB discovers candidate rows from the PK B-Tree
   instead of scanning the full heap.

2. `UPDATE users SET email = 'b@x.com' WHERE email = 'a@x.com'`
   With a UNIQUE secondary index on `email`, AxiomDB uses an indexed candidate
   path and then applies the existing update/index-maintenance semantics.

3. `UPDATE users SET score = score + 1 WHERE active = TRUE`
   If there is no usable index on `active`, AxiomDB falls back to the current
   scan path unchanged.

4. `UPDATE users SET name = 'jose' WHERE name = 'josé'`
   Under non-binary session collation on `TEXT`, binary-order-dependent text
   index plans are rejected and UPDATE falls back to scan.

## Acceptance criteria

- [ ] `UPDATE ... WHERE` no longer always starts from `scan_table()` when a
      usable index exists.
- [ ] PRIMARY KEY and eligible secondary indexes are available to UPDATE
      candidate planning.
- [ ] The executor materializes candidate `RecordId`s before any heap or index
      mutation.
- [ ] The executor rechecks the full original `WHERE` on fetched rows before
      applying updates.
- [ ] Existing `5.20` stable-RID and fallback paths remain the source of truth
      for physical update semantics.
- [ ] FK child and parent enforcement continue to run with unchanged semantics.
- [ ] `local_bench.py --scenario update_range` improves because UPDATE no
      longer discovers candidates by full heap scan.

## Out of scope

- Write-path redesign beyond `5.20`
- Non-sargable predicate optimization
- Multi-table UPDATE
- New SQL syntax
- Batch heap fetch by page if the simpler RID fetch path is already sufficient
- MVCC redesign

## Dependencies

- `5.19` batch delete of old index keys
- `5.20` stable-RID update fast path
- `6.3b` indexed DELETE candidate discovery as the closest architectural model
- `6.7` partial-index implication guard
- `6.9` PK / FK index population

## ⚠️ DEFERRED

- Shared generic DML candidate-planner abstraction across DELETE and UPDATE →
  pending in a future planner cleanup subphase
- Additional optimization for non-indexed predicates (`WHERE active = TRUE`
  without an index) → separate performance subphase
