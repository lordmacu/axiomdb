# Spec: 40.1 — Clustered Insert Batch

## What to build (not how)

A staging buffer (`ClusteredInsertBatch`) inside `SessionContext` that accumulates
encoded rows for consecutive INSERT statements into the same clustered table during
an explicit user transaction. Rows are **not** written to the B-tree until the batch
is flushed. Flush happens just before any barrier statement (SELECT/UPDATE/DELETE on
the same table, COMMIT, SAVEPOINT, table switch, error) or on an explicit flush call.

At flush time the batch uses the existing `try_insert_rightmost_leaf_batch` primitive
for monotonically-increasing PKs and falls back to the normal `apply_clustered_insert_rows`
path for non-monotonic or split-requiring rows.

**This eliminates O(N) CoW page-clone operations per transaction and replaces them
with O(N / leaf_capacity) page writes — matching InnoDB's buffer-pool amortization
pattern for sequential-PK workloads.**

---

## Inputs / Outputs

### Inputs
- A stream of `INSERT INTO <clustered_table> VALUES (...)` statements inside one
  explicit `BEGIN ... COMMIT` block.
- May include rows in any PK order (sequential, random, or mixed).
- May include secondary indexes, FK constraints, CHECK constraints on the table.

### Outputs
- All rows visible **after the batch is flushed** (i.e., after COMMIT or before a
  SELECT on the same table).
- Correct UNIQUE/PK violation errors surfaced no later than enqueue time for PK
  duplicates within the batch and at enqueue time for duplicates against committed data.
- ROLLBACK discards the batch with **no storage mutations**.
- Secondary indexes consistent with the clustered data after flush.
- WAL contains one `ClusteredRowInsert` record per staged row (same format as today),
  written atomically at flush time.

### Errors
- `DbError::DuplicateKey` when a staged PK collides with another staged PK or with a
  committed row — detected at enqueue time, batch discarded.
- `DbError::ForeignKeyViolation` when a staged row's FK child reference is not found
  in the parent table — detected at enqueue time.
- Any storage error during flush propagates upward; the transaction is left in an
  error state and the client must ROLLBACK.

---

## Use cases

### 1. Happy path — sequential PK bulk insert (benchmark case)
```sql
BEGIN;
INSERT INTO players VALUES (1, 'alice', 100);
INSERT INTO players VALUES (2, 'bob',   200);
-- ... 50 000 rows total ...
COMMIT;
```
All rows go into `ClusteredInsertBatch`. At COMMIT: rows are sorted by PK (already
in order), `try_insert_rightmost_leaf_batch` handles batches that fit in the current
rightmost leaf. When the leaf fills a split occurs, a new rightmost leaf is created,
and the batch continues appending. Total leaf writes ≈ N / leaf_capacity.

### 2. SELECT barrier mid-transaction
```sql
BEGIN;
INSERT INTO scores VALUES (1, 99);
INSERT INTO scores VALUES (2, 88);
SELECT * FROM scores WHERE id = 1;   -- must see id=1
INSERT INTO scores VALUES (3, 77);
COMMIT;
```
When `SELECT ... FROM scores` is dispatched, the executor detects a pending
`ClusteredInsertBatch` for `scores` → flushes it → then executes the SELECT.
Row (3, 77) is staged in a fresh batch. Final COMMIT flushes the second batch.

### 3. ROLLBACK discards the batch
```sql
BEGIN;
INSERT INTO items VALUES (10, 'hat');
INSERT INTO items VALUES (20, 'coat');
ROLLBACK;
```
The batch is discarded without writing any page. No WAL entries are written.
The table remains empty. No undo operations required.

### 4. ROLLBACK TO SAVEPOINT
```sql
BEGIN;
INSERT INTO items VALUES (1, 'a');
SAVEPOINT sp1;
INSERT INTO items VALUES (2, 'b');
ROLLBACK TO SAVEPOINT sp1;            -- discard row 2
INSERT INTO items VALUES (3, 'c');
COMMIT;                               -- commits rows 1 and 3
```
SAVEPOINT creation flushes the current batch to storage (rows already staged are
written via WAL). The WAL undo path handles rollback to sp1 normally (delete row 2).
Row 3 is staged in a fresh batch and committed.

### 5. PK duplicate within batch
```sql
BEGIN;
INSERT INTO t VALUES (5, 'first');
INSERT INTO t VALUES (5, 'second');   -- duplicate PK detected at enqueue
-- Error returned immediately; batch is discarded
ROLLBACK;
```

### 6. PK duplicate against committed data
```sql
-- Committed row: (7, 'existing')
BEGIN;
INSERT INTO t VALUES (7, 'new');   -- BTree lookup at enqueue → duplicate found
-- Error returned immediately
ROLLBACK;
```

### 7. Non-monotonic PKs (random inserts)
```sql
BEGIN;
INSERT INTO t VALUES (500, ...);
INSERT INTO t VALUES (1,   ...);    -- key < current rightmost → not rightmost
INSERT INTO t VALUES (250, ...);
COMMIT;
```
`try_insert_rightmost_leaf_batch` returns 0 or partial for non-monotonic rows.
Those rows fall back to individual `clustered_tree::insert()` calls. No data loss.
Throughput is lower than sequential case but correct.

### 8. Autocommit (no batching)
```sql
INSERT INTO t VALUES (1, 'a');   -- autocommit: no batch, immediate write (unchanged)
INSERT INTO t VALUES (2, 'b');
```
`ClusteredInsertBatch` is only active inside explicit transactions
(`ctx.in_explicit_txn == true`). Autocommit path is unchanged.

### 9. Table switch
```sql
BEGIN;
INSERT INTO a VALUES (1, 'x');
INSERT INTO a VALUES (2, 'y');
INSERT INTO b VALUES (1, 'z');   -- different table → flush batch for 'a' first
COMMIT;
```
When the executor detects a new INSERT for table `b` while a batch for table `a`
is pending, it flushes `a` first.

### 10. Secondary indexes maintained at flush
```sql
CREATE TABLE products (id INT PRIMARY KEY, name TEXT, category INT);
CREATE INDEX idx_cat ON products (category);

BEGIN;
INSERT INTO products VALUES (1, 'apple', 10);
INSERT INTO products VALUES (2, 'pear',  10);
INSERT INTO products VALUES (3, 'hat',   20);
COMMIT;
```
At flush: rows are written to the clustered tree AND all three secondary index
entries are inserted into `idx_cat`. Result is identical to three individual inserts.

---

## Acceptance criteria

- [ ] Local bench `insert` scenario: AxiomDB ≥ 35 K r/s for 50 K clustered rows in
      one explicit txn (beats MySQL 8.0 reference ~35 K r/s).
- [ ] `cargo test -p axiomdb-sql integration_clustered` — all existing tests pass.
- [ ] ROLLBACK before COMMIT leaves zero rows in the table and zero pages written.
- [ ] SELECT after two staged INSERTs sees both rows (flush-before-read barrier works).
- [ ] ROLLBACK TO SAVEPOINT after two inserts correctly reverts one insert.
- [ ] PK duplicate within batch returns `DuplicateKey` before any storage write.
- [ ] PK duplicate against committed data returns `DuplicateKey` at enqueue time.
- [ ] Non-monotonic PK batch (random order) produces the same row set as sequential
      individual inserts (correctness, not performance).
- [ ] Secondary indexes match clustered data after flush (verify with SELECT on indexed col).
- [ ] FK violations detected at enqueue time (conservative: no deferred FK check).
- [ ] `cargo test --workspace` passes clean.
- [ ] wire-test.py updated with ≥ 5 new assertions covering the new batch path.

---

## Out of scope

- Autocommit mode — no batching, path unchanged.
- Heap tables — already handled by existing `PendingInsertBatch`.
- UPDATE / DELETE batching — separate subfase (40.3 or later).
- Statement plan cache — separate subfase (40.2).
- Transaction Write Set (page-level CoW coalescing) — separate subfase (40.3).
- Batch WAL record (`ClusteredBatchInsert` single-entry WAL) — future optimization;
  this spec uses N individual `ClusteredRowInsert` WAL entries per batch at flush.
- Per-batch overflow cell streaming — rows with overflow chains fall back to individual
  insert path (overflow detection at enqueue time via `row_data_len > MAX_INLINE`).

---

## Data structure

```
ClusteredInsertBatch (in SessionContext)
  table_id:        u32
  table_def:       TableDef
  columns:         Vec<ColumnDef>
  indexes:         Vec<IndexDef>          -- secondary indexes only
  compiled_preds:  Vec<Option<Expr>>      -- partial-index predicates (parallel to indexes)
  rows:            Vec<StagedClusteredRow>  -- pre-encoded, sorted invariant NOT required
  staged_pks:      HashSet<Vec<u8>>       -- for O(1) PK duplicate detection within batch
  committed_empty: HashSet<u32>           -- index_ids known empty at batch creation
  max_rows:        usize (= 200_000)      -- hard cap; flush and re-init if exceeded
```

```
StagedClusteredRow
  pk_key:    Vec<u8>       -- encoded primary key bytes (for tree insert + dedup check)
  header:    RowHeader     -- MVCC header
  row_data:  Vec<u8>       -- encoded row payload (inline only; no overflow)
  has_overflow: bool       -- if true, this row bypasses the batch and inserts immediately
```

---

## Algorithm overview

### Enqueue (one row per INSERT statement)

```
enqueue_clustered_row(values, ...) →
  1. Evaluate CHECK constraints
  2. Check FK child references (against committed + staged not needed for FK parents)
  3. Encode PK key
  4. Check staged_pks: if pk_key ∈ staged_pks → DuplicateKey error, discard batch
  5. Lookup PK in committed clustered tree → DuplicateKey if found
  6. Check UNIQUE secondary indexes (committed + staged, same as heap batch)
  7. Encode RowHeader + row_data via row codec
  8. If row_data > inline threshold → has_overflow = true (will bypass batch at flush)
  9. Push StagedClusteredRow into batch.rows
  10. Insert pk_key into staged_pks
  11. If batch.rows.len() >= max_rows → flush (safety valve)
```

### Flush

```
flush_clustered_batch(batch, storage, txn, bloom, ctx) →
  1. Sort batch.rows by pk_key (ascending)
  2. Separate into:
       monotonic:    rows where pk > current rightmost leaf's last key (rightmost batch)
       fallback:     the rest (non-monotonic or non-rightmost)

  3. For monotonic rows (rightmost batch):
       a. Build RightmostAppendRow slices
       b. Loop:
            n = try_insert_rightmost_leaf_batch(storage, hinted_pid, &rows[cursor..])
            if n == 0:
              do one normal clustered_tree::insert for rows[cursor]
              update hinted_pid to new rightmost leaf
            else:
              cursor += n
              record N WAL ClusteredRowInsert entries
              push N UndoClusteredInsert entries
            while cursor < monotonic.len()

  4. For fallback rows:
       for row in fallback:
         apply_clustered_insert_rows(storage, txn, ..., &[row])
         (records WAL + undo individually, same as today)

  5. Maintain secondary indexes for ALL rows (sorted batch secondary insert)

  6. ctx.clustered_insert_batch = None
```

### Discard (ROLLBACK path)
```
discard_clustered_batch(ctx) →
  ctx.clustered_insert_batch = None   // no storage writes, no WAL
```

---

## Flush triggers (barrier detection)

The executor must flush before:

| Trigger | Where to add flush call |
|---|---|
| COMMIT | `txn.rs` commit path, after WAL fsync |
| ROLLBACK | `txn.rs` rollback path — discard (no flush) |
| ROLLBACK TO SAVEPOINT | `txn.rs` savepoint path — flush BEFORE savepoint creation |
| SELECT / UPDATE / DELETE on same table_id | `execute_with_ctx` dispatch |
| INSERT on different table_id | `execute_insert_ctx` when table_id differs |
| DDL (any) | `execute_ddl_ctx` |
| Autocommit INSERT (same or different table) | Not applicable (no batch created) |

---

## WAL and recovery interaction

No changes to WAL format. At flush time, the existing
`txn.record_clustered_insert(table_id, key, row_image)` path is called once per
staged row, identical to today. Recovery already knows how to replay these entries.

Crash scenarios:
- Crash before flush (before COMMIT): WAL has no entries for staged rows → nothing
  to recover, batch is silently lost on restart (correct: transaction was uncommitted).
- Crash after flush but before COMMIT WAL record: WAL has row entries but no COMMIT →
  recovery undoes via existing `UndoClusteredInsert` path.
- Crash after COMMIT WAL record: recovery replays normally.

---

## Dependencies

- `try_insert_rightmost_leaf_batch` — `crates/axiomdb-storage/src/clustered_tree.rs:216` ✅
- `apply_clustered_insert_rows` — `crates/axiomdb-sql/src/executor/insert.rs:1000` ✅
- `PendingInsertBatch` (heap) — `crates/axiomdb-sql/src/session.rs:403` ✅ (model to follow)
- `flush_pending_inserts_ctx` — `crates/axiomdb-sql/src/executor/insert.rs` ✅ (pattern to follow)
- `RowHeader` encode / row codec — `crates/axiomdb-types/src/` ✅
- `SessionContext.in_explicit_txn` — `crates/axiomdb-sql/src/session.rs` ✅
