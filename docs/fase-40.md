# Phase 40 — Clustered Engine Performance Optimizations

## Subfase 40.1 — ClusteredInsertBatch

### What was implemented

A staging buffer (`ClusteredInsertBatch`) in `SessionContext` that accumulates
pre-encoded rows from consecutive `INSERT ... VALUES` statements into the same
clustered table during an explicit user transaction. Rows are not written to the
B-tree until the batch is flushed.

### Key components

**`crates/axiomdb-sql/src/session.rs`**
- `StagedClusteredRow`: pre-encoded row (values, encoded_row, pk_bytes, pk_values).
  Defined here to avoid a circular dependency with `clustered_table.rs`.
- `ClusteredInsertBatch`: staging buffer with `table_def`, `primary_idx`,
  `secondary_indexes`, `secondary_layouts`, `compiled_preds`, `rows`, and
  `staged_pks: HashSet<Vec<u8>>` for O(1) intra-batch PK duplicate detection.
- `SessionContext::clustered_insert_batch: Option<ClusteredInsertBatch>` field.
- `discard_clustered_insert_batch()` — drops batch without storage writes (ROLLBACK path).

**`crates/axiomdb-sql/src/executor/staging.rs`**
- `flush_clustered_insert_batch()` — sorts staged rows by PK (ascending), converts
  to `PreparedClusteredInsertRow`, calls existing `apply_clustered_insert_rows`
  which handles `try_insert_rightmost_leaf_batch` fast path, WAL recording,
  secondary index maintenance, and root persistence.

**`crates/axiomdb-sql/src/executor/insert.rs`**
- `enqueue_clustered_insert_ctx()` — enqueues rows for explicit-txn VALUES inserts:
  validates constraints/FK, encodes via `prepare_row_with_ctx`, checks intra-batch
  PK duplicates, pushes to batch. Flushes if different table or batch >= 200K rows.
- Routing in `execute_insert_ctx`: explicit txn + VALUES source → batch path;
  autocommit or SELECT source → existing direct path.

**`crates/axiomdb-sql/src/executor/mod.rs`**
- `flush_clustered_insert_batch` called at: COMMIT, SAVEPOINT, DDL, and any
  non-INSERT barrier statement (`should_flush_clustered_batch_before_stmt`).
- `discard_clustered_insert_batch` called at: ROLLBACK, ROLLBACK TO SAVEPOINT,
  and error paths that abort the transaction.

### Flush barrier detection

| Trigger | Action |
|---|---|
| COMMIT | flush then commit WAL |
| ROLLBACK | discard (no storage writes) |
| SAVEPOINT | flush before creating savepoint marker |
| ROLLBACK TO SAVEPOINT | discard staged rows after sp |
| SELECT / UPDATE / DELETE on same table | flush first |
| INSERT on different table | flush current, start fresh batch |
| DDL | flush before implicit commit |

### Performance

| Scenario | AxiomDB 40.1 | MySQL 8.0 (InnoDB) | Verdict |
|---|---|---|---|
| 50K sequential PK inserts, 1 txn | **55.9K rows/s** | ~35K rows/s | ✅ +59% |

The gain comes from replacing O(N) CoW page-clone operations (one `read_page +
write_page` per insert at 16 KB each) with O(N / leaf_capacity) page writes via
`try_insert_rightmost_leaf_batch`.

### Tests

- `crates/axiomdb-sql/tests/integration_clustered_insert_batch.rs` — 10 tests:
  - Sequential PK bulk insert (COMMIT visibility)
  - SELECT barrier (flush-before-read)
  - ROLLBACK discards all staged rows
  - SAVEPOINT flush + ROLLBACK TO SAVEPOINT correctness
  - Intra-batch PK duplicate → DuplicateKey immediately
  - Committed-data PK duplicate → detected at flush
  - Non-monotonic PK order → correct sorted result
  - Table switch → first batch flushed before second table
  - Secondary (unique) index bookmarks correct after flush
  - Autocommit path unchanged (no batch)

- `tools/wire-test.py` — 9 new assertions (section 40.1)

### WAL interaction

No changes to WAL format. At flush time, the existing
`txn.record_clustered_insert(table_id, key, row_image)` path is called once per
staged row. Recovery is identical to pre-40.1 behavior.

Crash scenarios:
- Crash before flush (before COMMIT): WAL has no entries for staged rows → nothing
  to recover (transaction was uncommitted). ✓
- Crash after flush but before COMMIT WAL record: recovery undoes via existing
  `UndoClusteredInsert` path. ✓
- Crash after COMMIT WAL record: recovery replays normally. ✓

## Subfase 40.1b — CREATE INDEX on clustered tables

### What was implemented

Removed the `ensure_heap_runtime` guard in `execute_create_index` (ddl.rs) that was
blocking `CREATE INDEX` on clustered tables with `NotImplemented`. Now `CREATE INDEX`
works identically on both heap and clustered tables.

### Key change

**`crates/axiomdb-sql/src/executor/ddl.rs`** — `execute_create_index`:
- Removed: `table_def.ensure_heap_runtime("CREATE INDEX on clustered table — Phase 39.13+")?;`
- Added clustered scan + insert branch before the existing heap branch:
  - Fetches the primary `IndexDef` from the catalog.
  - Builds a `preview_index_def` (no index_id yet) for `ClusteredSecondaryLayout::derive`.
  - Scans via `TableEngine::scan_clustered_table` (same `Vec<(RecordId, Vec<Value>)>` type as heap scan).
  - For each row: calls `layout.entry_from_row` to get the physical key for bloom, then `layout.insert_row` for the B-Tree write + uniqueness check.
  - Partial index predicates and NULL secondary values are handled identically to the heap path.
  - Stats bootstrap at step 8 reuses the same `rows` Vec — no extra I/O.

### Behavioral parity with heap indexes

| Feature | Heap | Clustered 40.1b |
|---|---|---|
| Non-unique index | ✅ | ✅ |
| UNIQUE index — build-time dedup check | ✅ | ✅ |
| UNIQUE index — runtime INSERT enforcement | ✅ | ✅ |
| Partial index (WHERE predicate) | ✅ | ✅ |
| NULL values not indexed | ✅ | ✅ |
| Bloom filter populated | ✅ | ✅ |
| Per-column NDV stats | ✅ | ✅ |
| Duplicate name check | ✅ | ✅ |

### Tests

- `crates/axiomdb-sql/tests/integration_clustered_create_index.rs` — 9 tests:
  - Empty table: catalog entry created
  - Populated table: existing rows indexed and scannable via layout.scan_prefix
  - Unique index rejects existing duplicates
  - Unique index succeeds with distinct values
  - INSERT after CREATE INDEX maintains secondary index
  - SELECT uses secondary index after CREATE INDEX
  - NULL secondary values not indexed
  - Unique index enforces on subsequent INSERTs
  - Duplicate index name returns IndexAlreadyExists
  - Partial index: only matching rows indexed
  - Heap table CREATE INDEX unchanged (regression)

- `tools/wire-test.py` — 7 new wire-test assertions (section 40.2)
