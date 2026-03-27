# Plan: 5.21 — Transactional INSERT staging and batch flush

## Files to create/modify
- [`crates/axiomdb-wal/src/txn.rs`](/Users/cristian/nexusdb/crates/axiomdb-wal/src/txn.rs) — add transaction-local staged INSERT buffer and discard/flush hooks
- [`crates/axiomdb-sql/src/executor/mod.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/src/executor/mod.rs) — add barrier-aware `flush_pending_inserts[_ctx]` calls before statement dispatch and commit paths
- [`crates/axiomdb-sql/src/executor/insert.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/src/executor/insert.rs) — enqueue eligible rows instead of immediately mutating heap; keep current fallback for ineligible shapes
- [`crates/axiomdb-sql/src/table.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/src/table.rs) — reuse `insert_rows_batch[_with_ctx]` for staged flushes with indexed tables
- [`crates/axiomdb-sql/src/index_maintenance.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/src/index_maintenance.rs) — add batch insert precheck + grouped post-heap insert helper that persists roots once per flush
- [`crates/axiomdb-sql/tests/integration_executor.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/tests/integration_executor.rs) — explicit transaction staging/barrier semantics
- [`crates/axiomdb-sql/tests/integration_indexes.rs`](/Users/cristian/nexusdb/crates/axiomdb-sql/tests/integration_indexes.rs) — PK/secondary correctness after staged flush
- [`crates/axiomdb-wal/tests/integration_page_write.rs`](/Users/cristian/nexusdb/crates/axiomdb-wal/tests/integration_page_write.rs) — staged flush still emits compact page-level WAL and recovers correctly
- [`tools/wire-test.py`](/Users/cristian/nexusdb/tools/wire-test.py) — live explicit-transaction INSERT smoke/regression
- [`benches/comparison/local_bench.py`](/Users/cristian/nexusdb/benches/comparison/local_bench.py) — no logic change required, but benchmark numbers from `insert` become the proof target

## Algorithm / Data structure

### 1. Transaction-local staging buffer

Add a transaction-owned structure in `TxnManager::active`, not in `SessionContext`:

```text
BufferedInsertBatch {
  table_id: u32,
  data_root_page_id: u64,
  rows: Vec<Vec<Value>>,
  first_statement_last_insert_id: Option<u64>,   // returned immediately, not used at flush
  unique_seen: HashMap<u32, HashSet<Vec<u8>>>,   // index_id -> encoded unique key
}
```

Rules:
- Only one active staged table at a time.
- If the next eligible INSERT targets the same table, append rows.
- If it targets a different table, flush first, then start a new batch.
- If the statement is not eligible, flush first and run the old path.

### 2. Eligibility rules

Eligible for staging:
- active transaction already exists (`BEGIN` or `autocommit=0` implicit txn already open)
- `InsertSource::Values`
- no self-referential FK requirement that would need same-table pending rows to be visible immediately

Not eligible:
- autocommit single-statement INSERT
- `INSERT ... SELECT`
- any future trigger/error-logging shapes that require immediate physical write ordering

Ineligible statements keep the existing path.

### 3. Statement-time enqueue path

For each row:
1. Evaluate expressions.
2. Expand omitted columns.
3. Assign AUTO_INCREMENT now.
4. Run CHECK constraints now.
5. Run FK child validation now.
6. Precheck UNIQUE/PK collisions:
   - against committed indexes using encoded key lookup
   - against `unique_seen` in the current buffer
7. Append the fully materialized row to `BufferedInsertBatch.rows`.
8. Return `Affected(1)` or `affected_with_id(1, id)` immediately.

No heap write and no WAL append happen at this point.

### 4. Barrier-driven flush

Before running any barrier statement, call:

```text
flush_pending_inserts(storage, txn, maybe_bloom, maybe_ctx)
```

Barriers:
- `SELECT`
- `UPDATE`
- `DELETE`
- DDL
- `COMMIT`
- `SAVEPOINT`
- `ROLLBACK TO`
- table switch to another INSERT target
- any ineligible INSERT shape

`ROLLBACK` discards the in-memory buffer without flushing.

### 5. Flush algorithm

```text
flush_pending_inserts(...)
  if no pending batch:
    return

  resolve table + columns + indexes once
  compile partial-index predicates once

  rids = TableEngine::insert_rows_batch[_with_ctx](..., rows)

  insert_many_into_indexes(indexes, rows, rids):
    for each index:
      keep mutable current_root = idx.root_page_id
      for each (row, rid):
        apply same NULL/predicate/unique rules as insert_into_indexes
        BTree::insert_in(current_root, key, rid)
        update current_root if split changes it
      if current_root != original_root:
        persist once via CatalogWriter::update_index_root(index_id, current_root)

  update stats once
  clear pending batch
```

Important: heap insert happens before index insert, but only after all logical prechecks have already succeeded. This keeps flush failures limited to storage/I/O, not late UNIQUE surprises.

### 6. Why this approach

This is the AxiomDB adaptation of three research patterns:
- PostgreSQL `heap_multi_insert()` — batch heap+WAL by page
- DuckDB appender — stage rows in memory and flush at a threshold/barrier
- OceanBase table insert op — separate row production from storage write

It explicitly rejects using PostgreSQL/MariaDB group-commit ideas as the primary fix here, because the hurting benchmark is one explicit transaction with many INSERT statements, not many concurrent commits.

## Implementation phases
1. Add `BufferedInsertBatch` to `TxnManager::active` plus lifecycle helpers: start/append/take/discard.
2. Add executor barrier helper(s) in `executor/mod.rs`; wire them before dispatch and before `COMMIT`/savepoint operations.
3. Change `execute_insert_ctx` and `execute_insert` to enqueue eligible `INSERT ... VALUES` rows.
4. Add UNIQUE precheck helper for staged rows in `index_maintenance.rs`.
5. Add grouped post-heap index insert helper that persists final roots once per flush.
6. Add ctx and non-ctx flush implementations using `insert_rows_batch_with_ctx` / `insert_rows_batch`.
7. Add rollback/discard handling.
8. Add tests and benchmark verification.

## Tests to write
- unit:
  - staged PK duplicate detection across buffered rows
  - `ROLLBACK` drops an unflushed buffer
  - barrier detection flushes on table switch and `SELECT`
- integration:
  - `BEGIN; INSERT; INSERT; COMMIT;` writes rows correctly
  - `BEGIN; INSERT; SELECT;` sees staged row because barrier flushed it
  - `BEGIN; INSERT INTO t; INSERT INTO u;` flushes `t` before `u`
  - PK and secondary indexes remain correct after staged flush
  - `SAVEPOINT` creates a barrier and still preserves semantics
- WAL:
  - staged flush uses page-level WAL entries and recovers cleanly after crash before/after commit
- wire:
  - explicit transaction with many INSERT statements still returns per-statement OK packets and final committed rows
- bench:
  - `python3 benches/comparison/local_bench.py --scenario insert --rows 50000 --table`
  - optional contrast: `--scenario insert_multi_values --rows 50000 --table`

## Anti-patterns to avoid
- Do not keep pending inserts invisible to later `SELECT`/`UPDATE`/`DELETE` in the same transaction.
- Do not buffer across table switches without flushing.
- Do not defer UNIQUE/PK checking until after heap mutation.
- Do not persist index roots once per row during a staged flush.
- Do not redefine `5.21` as “just enable existing group commit” — that does not solve the measured benchmark.

## Risks
- Risk: late flush could hide rows from same-transaction reads.
  Mitigation: barrier flush before any non-INSERT statement.

- Risk: buffered duplicates slip through and fail after heap mutation.
  Mitigation: explicit precheck against both committed indexes and in-buffer key sets.

- Risk: savepoint semantics become ambiguous.
  Mitigation: flush before savepoint creation / rollback-to-savepoint instead of teaching savepoints about buffered rows in this subphase.

- Risk: indexed tables still remain slow if per-row BTree insert dominates after heap batching.
  Mitigation: add grouped post-heap index maintenance now; if still insufficient, open deferred `insert_many_in` follow-up based on profiler/bench evidence.

## Assumptions
- Phase 5 still runs under the current single-writer model.
- The benchmark target is the current `insert` scenario in [`local_bench.py`](/Users/cristian/nexusdb/benches/comparison/local_bench.py), not `insert_autocommit`.
- Existing group commit remains available for future autocommit/concurrent-write tuning, but is not the main deliverable of this subphase.
