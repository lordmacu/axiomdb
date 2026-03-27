# Spec: Indexed multi-row INSERT batch path (Phase 6.18)

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/executor/insert.rs`
- `crates/axiomdb-sql/src/executor/staging.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/fk_enforcement.rs`
- `benches/comparison/local_bench.py`
- `specs/fase-05/spec-5.21-transactional-insert-staging.md`

These research files were reviewed for behavior/technique references:

- `research/postgres/src/backend/access/heap/heapam.c`
- `research/postgres/src/backend/executor/nodeModifyTable.c`
- `research/mariadb-server/sql/sql_insert.cc`
- `research/mariadb-server/sql/handler.cc`
- `research/duckdb/src/main/appender.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_insert_op.cpp`
- `research/sqlite/src/insert.c`

## Research synthesis

### What AxiomDB already does today

This debt is narrower than it first looked:

- the immediate multi-row `INSERT ... VALUES (...), (... )` path in
  `executor/insert.rs` still falls back to per-row heap insert + per-row index
  maintenance whenever indexes exist
- but `5.21` already introduced the primitives we need:
  - `insert_rows_batch[_with_ctx](...)`
  - `batch_insert_into_indexes(...)`
  - grouped root persistence once per index in `executor/staging.rs`

So the main gap is not missing core primitives. It is that the immediate
multi-row INSERT path still does not reuse them.

### Borrow / reject / adapt

#### PostgreSQL — `research/postgres/src/backend/access/heap/heapam.c`,
#### `research/postgres/src/backend/executor/nodeModifyTable.c`
- Borrow: multi-insert gains come from batching heap/WAL work and then applying
  the modify pipeline over the batch.
- Reject: full `ExecBatchInsert` / COPY stack as an architectural transplant.
- Adapt: reuse AxiomDB's existing batch heap and grouped index-maintenance
  primitives for plain multi-row `VALUES`.

#### MariaDB — `research/mariadb-server/sql/sql_insert.cc`,
#### `research/mariadb-server/sql/handler.cc`
- Borrow: multi-row INSERT deserves a dedicated bulk path, including row-count
  awareness.
- Reject: engine bulk-insert callbacks and handler lifecycle as a template.
- Adapt: keep one executor surface, but route eligible multi-row statements
  through a grouped heap+index path.

#### DuckDB — `research/duckdb/src/main/appender.cpp`
- Borrow: accumulate rows until a flush boundary, then write them as a batch.
- Reject: chunked vector pipeline and appender API as the user-facing model.
- Adapt: immediate multi-row `VALUES` already gives AxiomDB a natural batch;
  no new public API is needed.

#### OceanBase — `research/oceanbase/src/sql/engine/dml/ob_table_insert_op.cpp`
- Borrow: explicit insert staging before the storage write path.
- Reject: DAS/distributed insert buffering.
- Adapt: immediate statement-local staging only.

#### SQLite — `research/sqlite/src/insert.c`
- Borrow: PRIMARY KEY / table-write interaction should be handled by a dedicated
  insert path, not forced through generic per-row fallback forever.
- Reject: SQLite's rowid/WITHOUT ROWID storage model as a template.
- Adapt: keep heap + separate indexes, but stop paying the row-by-row fallback
  when the statement already provides a full batch.

### AxiomDB-first decision

This subphase should not invent a new B-Tree bulk-insert algorithm unless the
existing batch primitives prove insufficient.

The chosen scope is:

- reuse `insert_rows_batch[_with_ctx](...)` for multi-row `VALUES` even when
  indexes exist
- reuse `batch_insert_into_indexes(...)` for grouped index maintenance
- persist changed roots once per index per statement
- keep `5.21` staged-transaction path unchanged and avoid duplicate logic by
  extracting shared helpers where useful

## What to build

Add a batch path for immediate `INSERT ... VALUES (...), (... )` on indexed
tables so AxiomDB no longer falls back to per-row heap insert + per-row index
maintenance when the statement itself already provides a batch.

The new path must:

- apply to plain multi-row `VALUES` in both ctx and non-ctx insert execution
- preserve CHECK, FK, coercion, auto-increment, UNIQUE, and partial-index
  semantics
- batch heap insertion first, then grouped index maintenance, then root
  persistence once per index
- keep single-row INSERT behavior unchanged

## Inputs / Outputs

- Input:
  - `InsertStmt` with `InsertSource::Values` and more than one row
  - resolved `TableDef`, `ColumnDef`, `IndexDef`, constraints, FK metadata
  - mutable storage / txn / bloom / optional `SessionContext`
- Output:
  - unchanged `QueryResult::Affected`
  - higher throughput for `insert_multi_values` on indexed tables
- Errors:
  - unchanged type/coercion/FK/UNIQUE/CHECK errors
  - no new SQL syntax or wire-visible errors

## Use cases

1. `INSERT INTO bench_users VALUES (...), (...), (...)`
   On a table with `PRIMARY KEY (id)`, heap insert happens in batch and index
   maintenance is grouped per index instead of per row.

2. `INSERT INTO users(id, email) VALUES (1,'a'),(2,'b')`
   UNIQUE and PK constraints are still enforced correctly.

3. `INSERT INTO users(id, deleted_at) VALUES (1,NULL),(2,NOW())`
   Partial indexes still include or exclude rows according to their predicate.

4. `INSERT INTO users VALUES (1,'a')`
   Single-row INSERT remains on the current path with no extra batching
   overhead.

## Acceptance criteria

- [ ] Multi-row `INSERT ... VALUES` on indexed tables no longer falls back to
      per-row heap insertion by default.
- [ ] The ctx and non-ctx insert paths both reuse grouped batch primitives.
- [ ] Root persistence happens once per changed index per statement, not once
      per inserted row.
- [ ] UNIQUE, FK, CHECK, partial-index, and auto-increment semantics remain
      unchanged.
- [ ] `local_bench.py --scenario insert_multi_values` improves because indexed
      tables now use the batch path too.

## Out of scope

- `INSERT ... SELECT`
- Autocommit group-commit tuning
- New public appender API
- New B-Tree `insert_many_in(...)` primitive unless the existing grouped helper
  proves insufficient
- SQL syntax changes

## Dependencies

- `5.18` heap tail-pointer cache
- `5.21` transactional INSERT staging and grouped index-maintenance primitives
- `6.2b` secondary index maintenance correctness
- `6.9` PK/fk index population

## ⚠️ DEFERRED

- Statement-local bulk-load path for empty committed indexes if the current
  grouped insert helper is still too slow → future insert performance subphase
- Extending the same batching to `INSERT ... SELECT` → future executor subphase
