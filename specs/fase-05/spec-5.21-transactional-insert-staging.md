# Spec: 5.21 — Transactional INSERT staging and batch flush

## What to build
Add a transaction-local staging path for consecutive `INSERT ... VALUES` statements so AxiomDB can batch heap/WAL writes and index maintenance across multiple single-row INSERT statements inside the same open transaction.

This subphase exists because the current tracker text for `5.21` is stale: the benchmark that hurts today is [`local_bench.py --scenario insert --rows 50000 --table`](/Users/cristian/nexusdb/benches/comparison/local_bench.py), which runs many single-row INSERT statements inside one explicit transaction. That workload already pays only one final `COMMIT`, so existing WAL group commit is not the primary bottleneck. The real cost is that every statement still does heap insert + WAL append + PK/index maintenance row-by-row instead of staging rows and flushing them in one batch.

The new behavior must:
- buffer eligible rows in the active transaction instead of physically inserting them immediately;
- flush that buffer in one batch before any barrier statement (`SELECT`, `UPDATE`, `DELETE`, DDL, `COMMIT`, `SAVEPOINT`, `ROLLBACK TO`, table switch, or any ineligible INSERT shape);
- use batch heap insertion even when indexes exist, then apply index maintenance in a grouped post-heap phase;
- preserve SQL-visible semantics: per-statement affected rows, `LAST_INSERT_ID()`, CHECK/FK/UNIQUE validation, rollback, and read-your-own-writes.

## Research synthesis

### These files were reviewed before writing this spec

#### AxiomDB
- [`db.md`](/Users/cristian/nexusdb/db.md)
- [`docs/progreso.md`](/Users/cristian/nexusdb/docs/progreso.md)
- [`benches/comparison/local_bench.py`](/Users/cristian/nexusdb/benches/comparison/local_bench.py)
- [`crates/axiomdb-sql/src/executor/mod.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/src/executor/mod.rs)
- [`crates/axiomdb-sql/src/executor/insert.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/src/executor/insert.rs)
- [`crates/axiomdb-sql/src/table.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/src/table.rs)
- [`crates/axiomdb-sql/src/index_maintenance.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/src/index_maintenance.rs)
- [`crates/axiomdb-sql/src/session.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/src/session.rs)
- [`crates/axiomdb-storage/src/heap_chain.rs`](/Users/cristian/nexusdb/crates/axiomdb-storage/src/heap_chain.rs)
- [`crates/axiomdb-wal/src/txn.rs`](/Users/cristian/nexusdb/crates/axiomdb-wal/src/txn.rs)
- [`crates/axiomdb-network/src/mysql/database.rs`](/Users/cristian/nexusdb/crates/axiomdb-network/src/mysql/database.rs)
- [`crates/axiomdb-network/src/mysql/commit_coordinator.rs`](/Users/cristian/nexusdb/crates/axiomdb-network/src/mysql/commit_coordinator.rs)
- [`crates/axiomdb-server/src/main.rs`](/Users/cristian/nexusdb/crates/axiomdb-server/src/main.rs)

#### Research
- [`research/postgres/src/backend/access/heap/heapam.c`](/Users/cristian/nexusdb/research/postgres/src/backend/access/heap/heapam.c)
- [`research/postgres/src/backend/access/transam/xlog.c`](/Users/cristian/nexusdb/research/postgres/src/backend/access/transam/xlog.c)
- [`research/sqlite/src/insert.c`](/Users/cristian/nexusdb/research/sqlite/src/insert.c)
- [`research/mariadb-server/sql/sql_insert.cc`](/Users/cristian/nexusdb/research/mariadb-server/sql/sql_insert.cc)
- [`research/mariadb-server/storage/innobase/trx/trx0trx.cc`](/Users/cristian/nexusdb/research/mariadb-server/storage/innobase/trx/trx0trx.cc)
- [`research/mariadb-server/storage/innobase/log/log0sync.cc`](/Users/cristian/nexusdb/research/mariadb-server/storage/innobase/log/log0sync.cc)
- [`research/duckdb/src/main/appender.cpp`](/Users/cristian/nexusdb/research/duckdb/src/main/appender.cpp)
- [`research/oceanbase/src/sql/engine/dml/ob_table_insert_op.cpp`](/Users/cristian/nexusdb/research/oceanbase/src/sql/engine/dml/ob_table_insert_op.cpp)

### Borrow / Reject / Adapt

- PostgreSQL [`heapam.c`](/Users/cristian/nexusdb/research/postgres/src/backend/access/heap/heapam.c)
  Borrow: `heap_multi_insert()` batches heap writes and WAL once per page instead of once per tuple.
  Reject: full table-AM abstraction and COPY-only integration.
  Adapt: AxiomDB stages consecutive INSERT statements in the transaction, then flushes through `insert_rows_batch[_with_ctx]`.

- PostgreSQL [`xlog.c`](/Users/cristian/nexusdb/research/postgres/src/backend/access/transam/xlog.c)
  Borrow: the explicit distinction between commit-fsync batching and tuple-write batching.
  Reject: using group-commit tuning as the primary fix for this benchmark.
  Adapt: use this file as negative evidence that commit batching only helps when multiple commits can pile up.

- SQLite [`insert.c`](/Users/cristian/nexusdb/research/sqlite/src/insert.c)
  Borrow: append-biased insert fast path when inserts are likely sequential.
  Reject: VDBE opcode architecture and rowid-only assumptions.
  Adapt: combine transaction staging with AxiomDB's existing heap tail hint from `5.18`.

- MariaDB [`sql_insert.cc`](/Users/cristian/nexusdb/research/mariadb-server/sql/sql_insert.cc)
  Borrow: dedicated insert-specific execution path rather than forcing all INSERT traffic through the generic row-write loop.
  Reject: delayed insert thread architecture and server handler layering.
  Adapt: AxiomDB uses a transaction-local staging buffer, not a daemon queue.

- MariaDB / InnoDB [`trx0trx.cc`](/Users/cristian/nexusdb/research/mariadb-server/storage/innobase/trx/trx0trx.cc) and [`log0sync.cc`](/Users/cristian/nexusdb/research/mariadb-server/storage/innobase/log/log0sync.cc)
  Borrow: group commit helps when multiple writers gather behind one physical log write.
  Reject: assuming that mechanism solves a single-connection explicit transaction benchmark.
  Adapt: keep the existing `CommitCoordinator`, but defer its autocommit tuning because it is a different workload.

- DuckDB [`appender.cpp`](/Users/cristian/nexusdb/research/duckdb/src/main/appender.cpp)
  Borrow: stage rows in chunks and flush only when a threshold or barrier is reached.
  Reject: vectorized append collection as a full replacement for row-wise SQL semantics.
  Adapt: AxiomDB stages rows per transaction/table and flushes on SQL barriers.

- OceanBase [`ob_table_insert_op.cpp`](/Users/cristian/nexusdb/research/oceanbase/src/sql/engine/dml/ob_table_insert_op.cpp)
  Borrow: separate row production from storage/DAS write.
  Reject: distributed DAS architecture.
  Adapt: local transaction-scoped staging followed by one storage flush step.

## Inputs / Outputs
- Input:
  - `InsertStmt` with `InsertSource::Values`
  - active `TxnManager`
  - resolved `TableDef`, `ColumnDef`, `IndexDef`, constraints, FKs
  - optional `SessionContext`
- Output:
  - same `QueryResult::Affected` / `affected_with_id` as today for each INSERT statement
- Errors:
  - unchanged SQL-visible errors for type coercion, FK violations, UNIQUE violations, CHECK failures, and I/O
  - no new SQL syntax

## Use cases
1. `BEGIN; INSERT INTO bench_users VALUES (...); ... 50000x ...; COMMIT;`
   Rows are staged during the transaction and flushed in a grouped heap/WAL/index pass before the final commit.

2. `BEGIN; INSERT INTO t VALUES (1); SELECT * FROM t;`
   The pending insert buffer is flushed before `SELECT`, so read-your-own-writes still works.

3. `BEGIN; INSERT INTO t VALUES (1); INSERT INTO u VALUES (2);`
   Changing target table is a barrier; staged rows for `t` are flushed before buffering rows for `u`.

4. `BEGIN; INSERT INTO t VALUES (1); SAVEPOINT s1;`
   `SAVEPOINT` is a barrier; staged rows are flushed before savepoint creation.

5. `BEGIN; INSERT INTO t VALUES (1); ROLLBACK;`
   If rows never flushed, the buffer is discarded. If rows were flushed by a barrier, rollback continues to undo them through the existing WAL/undo path.

## Acceptance criteria
- [ ] Consecutive eligible `INSERT ... VALUES` statements inside one active transaction are staged instead of immediately calling the row-at-a-time heap path.
- [ ] Any non-INSERT barrier statement flushes staged rows before it runs.
- [ ] `COMMIT` flushes any remaining staged rows before writing the Commit WAL entry.
- [ ] `ROLLBACK` discards unflushed staged rows without touching heap or WAL.
- [ ] Batch flush uses `insert_rows_batch[_with_ctx]` even when the target table has PK or secondary indexes.
- [ ] Index maintenance after a staged flush persists each changed index root at most once per flush, not once per row.
- [ ] Duplicate UNIQUE/PK keys across staged rows are detected before heap mutation.
- [ ] `LAST_INSERT_ID()` and per-statement affected-row counts remain identical to current MySQL-visible behavior.
- [ ] `BEGIN; INSERT ...; SELECT ...;` still returns the inserted row within the same transaction.
- [ ] `local_bench.py --scenario insert --rows 50000 --table` improves materially over the current 47–67K rows/s range.

## Out of scope
- Autocommit single-row insert throughput across multiple concurrent connections
- Replacing the existing WAL group commit design
- INSERT SELECT staging
- General transaction-local visibility overlays for arbitrary buffered DML
- New SQL syntax

## Dependencies
- `4.16c` multi-row INSERT batch heap path
- `4.16d` compact `PageWrite` WAL path
- `5.18` heap tail hint reuse
- `5.19a` executor decomposition
- `5.19b` eval decomposition
- `6.2b` index maintenance on INSERT

## ⚠️ DEFERRED
- Autocommit insert throughput via actual server-side group-commit activation/tuning → pending in `5.21b`
- True B+Tree `insert_many_in` primitive for sorted index bulk load → pending in a follow-up subphase if post-flush per-row index insertion remains the bottleneck
