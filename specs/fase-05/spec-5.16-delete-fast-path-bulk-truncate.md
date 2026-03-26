# Spec: 5.16 — DELETE fast-path bulk truncate

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-sql/src/fk_enforcement.rs`
- `crates/axiomdb-sql/src/bloom.rs`
- `crates/axiomdb-storage/src/engine.rs`
- `crates/axiomdb-storage/src/heap.rs`
- `crates/axiomdb-storage/src/heap_chain.rs`
- `crates/axiomdb-index/src/tree.rs`
- `crates/axiomdb-catalog/src/schema.rs`
- `crates/axiomdb-catalog/src/writer.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `crates/axiomdb-wal/src/recovery.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
- `crates/axiomdb-network/src/mysql/group_commit.rs`
- `benches/comparison/local_bench.py`
- `benches/comparison/axiomdb_bench/src/main.rs`
- `docs-site/src/internals/storage.md`
- `docs-site/src/internals/wal.md`
- `docs-site/src/internals/mvcc.md`

These research files were reviewed before writing this spec:

- `research/mariadb-server/sql/handler.cc`
- `research/mariadb-server/sql/sql_truncate.h`
- `research/mariadb-server/sql/sql_base.cc`
- `research/mariadb-server/storage/maria/ma_delete_all.c`
- `research/mariadb-server/storage/innobase/handler/ha_innodb.cc`
- `research/mariadb-server/mysql-test/main/commit_1innodb.result`
- `research/postgres/src/backend/commands/tablecmds.c`
- `research/sqlite/src/delete.c`
- `research/sqlite/src/vdbe.c`
- `research/sqlite/src/btree.c`
- `research/sqlite/test/auth3.test`
- `research/sqlite/test/hook2.test`
- `research/duckdb/src/execution/operator/persistent/physical_delete.cpp`
- `research/duckdb/src/storage/data_table.cpp`
- `research/duckdb/src/execution/index/bound_index.cpp`
- `research/oceanbase/src/rootserver/ob_ddl_operator.cpp`
- `research/oceanbase/src/storage/ob_storage_schema.h`
- `research/oceanbase/src/sql/engine/basic/ob_truncate_filter_struct.h`
- `research/oceanbase/src/sql/engine/basic/ob_truncate_filter_struct.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_delete_op.cpp`
- `research/oceanbase/src/storage/ls/ob_ls_tablet_service.cpp`
- `research/datafusion/datafusion/sql/src/statement.rs`
- `research/datafusion/datafusion/core/src/physical_planner.rs`
- `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`

## Research synthesis

### AxiomDB-first constraints

- `DELETE FROM t` without `WHERE` is only fast today when `secondary_indexes.is_empty()`,
  but that set includes the PRIMARY KEY too, so real tables miss the fast path.
- `TRUNCATE TABLE` currently stamps heap deletions but does not reset index roots.
- AxiomDB's PK / UNIQUE / FK existence checks use `BTree::lookup_in(...)` directly
  and do not have DuckDB-style delete indexes or SQLite-style root clearing.
- Savepoints and group commit already exist, so any bulk strategy must preserve:
  - statement-level rollback
  - full rollback
  - deferred fsync batches

### What we borrow

- `research/mariadb-server/sql/handler.cc`
  - borrow: full-table delete and truncate deserve dedicated engine paths
- `research/mariadb-server/storage/maria/ma_delete_all.c`
  - borrow: delete-all is not row-by-row; it is a table-wide state transition
- `research/postgres/src/backend/commands/tablecmds.c`
  - borrow: swap to fresh empty storage and free old storage only after the
    operation is durably committed
- `research/sqlite/src/delete.c`
  - borrow: when deleting all rows, reset table and every index root together
- `research/sqlite/test/auth3.test`
  - borrow: disable the truncate shortcut whenever it would bypass observable
    semantics; in AxiomDB that means parent-FK enforcement
- `research/oceanbase/src/sql/engine/basic/ob_truncate_filter_struct.cpp`
  - borrow: model truncate as a generation/root switch, not as N row deletes
- `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`
  - borrow: `DELETE FROM t` with no `WHERE` is a distinct execution shape
    (`filters.is_empty()`), while `TRUNCATE` is its own hook with no input rows

### What we reject

- leaving stale index roots and trying to “fix” correctness by adding more
  heap visibility checks everywhere
- in-place destruction of heap/index roots before commit, because that breaks
  rollback and savepoint semantics in the current codebase
- per-row `delete_from_indexes(...)` for full-table delete
- reusing `record_truncate()` for indexed tables, because the current truncate
  WAL/undo contract only knows about heap deletion stamps, not root rotation

### How AxiomDB adapts it

- AxiomDB will implement full-table delete / truncate as **root rotation**:
  allocate fresh empty roots, update catalog rows inside the transaction, and
  free old pages only after commit durability is confirmed
- the same bulk helper will back:
  - `DELETE FROM t` with no `WHERE`
  - `TRUNCATE TABLE t`
- `DELETE` and `TRUNCATE` keep different SQL semantics:
  - `DELETE` returns the visible-row count and does not reset AUTO_INCREMENT
  - `TRUNCATE` returns `0`, resets AUTO_INCREMENT, and rejects parent-FK tables

## What to build (not how)

Implement a bulk table-emptying path for indexed tables that replaces the
current row-by-row delete fallback.

### Surface 1: `DELETE FROM t` with no `WHERE`

When the statement targets a single table and has no `WHERE`, AxiomDB must:

- detect the full-table-delete case before it falls into per-row index maintenance
- use a bulk path even when the table has:
  - a PRIMARY KEY
  - UNIQUE indexes
  - non-unique secondary indexes
  - FK auto-indexes
  - partial indexes
- keep row-count semantics identical to `DELETE`:
  - return the number of rows visible to the statement snapshot
  - do not reset AUTO_INCREMENT

### Surface 2: `TRUNCATE TABLE t`

`TRUNCATE TABLE` must use the same root-rotation machinery so that indexed
tables are truly empty after the statement.

Its SQL-visible semantics remain:

- affected rows = `0`
- AUTO_INCREMENT reset
- no row-by-row warnings or per-row hooks

### Shared correctness contract

After either operation succeeds:

- heap scans see an empty table
- PK / UNIQUE lookups do not see pre-delete rows
- FK parent existence checks do not see pre-delete rows
- secondary/range/index-only scans do not walk stale entries from the old tree

### Transactional contract

The bulk path must be transaction-safe:

- inside an explicit transaction, the table becomes empty immediately for the
  current transaction
- `ROLLBACK TO SAVEPOINT` restores the pre-statement roots
- `ROLLBACK` restores the pre-transaction roots
- old heap/index pages are freed only after commit durability is confirmed:
  - immediate commit mode: after WAL fsync succeeds
  - group commit mode: after the batch fsync succeeds

### FK rule

`DELETE FROM t` with no `WHERE` may use the fast path only when no FK
constraint references `t` as the parent table.

If parent-FK references exist:

- `DELETE FROM t` keeps the current row-by-row path so RESTRICT/CASCADE/SET NULL
  semantics still work
- `TRUNCATE TABLE t` must fail with a parent-FK error because AxiomDB does not
  implement `TRUNCATE ... CASCADE`

## Inputs / Outputs

- Input:
  - `DeleteStmt` with `where_clause == None`
  - `TruncateTableStmt`
  - resolved `TableDef`, `ColumnDef`, and `IndexDef` list from catalog
  - `StorageEngine`
  - `TxnManager`
  - `BloomRegistry`
  - optional `SessionContext`
- Output:
  - `QueryResult::Affected { count, last_insert_id: None }`
  - `count = visible rows deleted` for `DELETE FROM t`
  - `count = 0` for `TRUNCATE TABLE`
- Errors:
  - `DbError::ForeignKeyParentViolation { .. }` when `TRUNCATE TABLE` targets a
    table referenced by child FKs
  - existing catalog/storage/WAL errors

## Use cases

1. `DELETE FROM bench_users` on a table with only `PRIMARY KEY (id)` uses the
   bulk path and no longer falls back to per-row `delete_from_indexes(...)`.
2. `TRUNCATE TABLE users` on a table with PRIMARY KEY and UNIQUE email index
   leaves the table truly empty, and a reinsertion of the same keys succeeds.
3. `BEGIN; DELETE FROM users; INSERT INTO users VALUES (old_pk, ...); COMMIT;`
   succeeds because the current transaction sees the rotated empty roots.
4. `BEGIN; SAVEPOINT s; DELETE FROM users; ROLLBACK TO SAVEPOINT s;`
   restores the original table contents and index visibility.
5. `TRUNCATE TABLE parent` fails if child rows reference `parent`, while
   `DELETE FROM parent` still uses the existing FK-enforcing slow path.

## Acceptance criteria

- [ ] `DELETE FROM t` with no `WHERE` uses the bulk path on tables that have a
      PRIMARY KEY and/or secondary indexes; it is no longer gated by
      “no indexes at all”.
- [ ] After bulk `DELETE FROM t`, `SELECT * FROM t WHERE pk = ...` returns no
      rows even when the access path is an index lookup.
- [ ] After bulk `TRUNCATE TABLE t`, reinserting the same PRIMARY KEY or UNIQUE
      key in the same session succeeds; no stale index entry causes
      `UniqueViolation`.
- [ ] After bulk `TRUNCATE TABLE child`, inserting a row with the same FK value
      succeeds; no stale FK auto-index entry blocks the insert.
- [ ] `DELETE FROM t` returns the number of rows visible to the statement
      snapshot; `TRUNCATE TABLE t` returns `0`.
- [ ] `TRUNCATE TABLE t` resets AUTO_INCREMENT; `DELETE FROM t` does not.
- [ ] `BEGIN; DELETE FROM t; ROLLBACK;` restores the original table contents and
      index visibility.
- [ ] `BEGIN; SAVEPOINT s; DELETE FROM t; ROLLBACK TO SAVEPOINT s;` restores the
      original table contents and index visibility.
- [ ] In group-commit mode, old heap/index pages are freed only after the batch
      fsync succeeds; a failed fsync leaves the deferred free queue unapplied.
- [ ] `TRUNCATE TABLE parent` fails with `ForeignKeyParentViolation` when child
      FKs reference `parent`.
- [ ] `DELETE FROM parent` with parent-FK references keeps the current row-by-row
      path and preserves RESTRICT/CASCADE/SET NULL behavior.
- [ ] `tools/wire-test.py` proves the behavior through the MySQL wire for:
      - bulk delete on PK table
      - truncate + reinsertion
      - explicit `BEGIN/ROLLBACK`

## Out of scope

- `DELETE ... WHERE ...`
- multi-table DELETE
- `TRUNCATE ... CASCADE`, `RESTRICT`, partition truncation, or sequence options
- durable garbage collection for the rare crash window after WAL fsync succeeds
  but before deferred page frees finish

## Dependencies

- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-catalog/src/writer.rs`
- `crates/axiomdb-storage/src/heap_chain.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `crates/axiomdb-network/src/mysql/group_commit.rs`

## ⚠️ DEFERRED

- Durable cleanup of old pages if the process crashes after commit durability is
  confirmed but before the post-commit free pass completes
  → revisit in Phase 7 space-reclamation / page-GC work
