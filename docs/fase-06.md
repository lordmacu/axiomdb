# Phase 6 — Secondary Indexes + Query Planner

## Subfases completed in this session: 6.1, 6.1b, 6.2, 6.2b, 6.3, 6.15, 6.16, 6.17, 6.18

## What was built

### 6.1 — Columns in IndexDef

`IndexDef` in `axiomdb-catalog` now stores `columns: Vec<IndexColumnDef>`, recording
which column positions (`col_idx: u16`) and sort directions (`SortOrder`) each index
covers. The on-disk format is extended backward-compatibly: old rows that end before
the `ncols` byte are read as `columns: []` (treated as unusable by the planner).

New types in `axiomdb-catalog/src/schema.rs`:
- `SortOrder` — `Asc`/`Desc`, repr `u8`
- `IndexColumnDef { col_idx: u16, order: SortOrder }` — one column entry (3 bytes on disk)

### 6.1b — Order-preserving key encoding

`crates/axiomdb-sql/src/key_encoding.rs`:
- `encode_index_key(&[Value]) -> Result<Vec<u8>, DbError>` — encodes a multi-value
  key into bytes such that `encode(a) < encode(b)` iff `a < b` under SQL comparison.
- Handles all Value variants: NULL (sorts first), Bool, Int, BigInt, Real, Decimal,
  Date, Timestamp, Text (NUL-escaped), Bytes (NUL-escaped), Uuid.
- Keys exceeding 768 bytes return `DbError::IndexKeyTooLong`.

### 6.2 — CREATE INDEX executor

`execute_create_index` in the executor was rewritten to:
1. Check for duplicate index name on the table.
2. Build `IndexColumnDef` list from the `CreateIndexStmt.columns`.
3. Allocate and initialize a fresh B-Tree leaf root page.
4. Scan the entire table heap and insert existing rows into the B-Tree (skipping NULLs
   and rows with keys > 768 bytes with a warning).
5. Persist `IndexDef` with the final `root_page_id` (which may change after root splits).

`execute_drop_index` now calls `free_btree_pages` to walk and free all B-Tree pages,
preventing page leaks on DROP INDEX.

New B-Tree static API in `axiomdb-index`:
- `BTree::lookup_in(storage, root_pid, key)` — point lookup without owning storage
- `BTree::insert_in(storage, root_pid, key, rid)` — insert without owning storage
- `BTree::delete_in(storage, root_pid, key)` — delete without owning storage
- `BTree::range_in(storage, root_pid, lo, hi)` — range scan, returns `Vec<(RecordId, Vec<u8>)>`

New `CatalogWriter::update_index_root(index_id, new_root)` persists updated
`root_page_id` when a B-Tree root splits during DML.

### 6.2b — Index maintenance on DML

`crates/axiomdb-sql/src/index_maintenance.rs`:
- `indexes_for_table(table_id, storage, snapshot)` — reads catalog
- `insert_into_indexes(indexes, row, rid, storage)` — inserts key into all secondary
  non-primary indexes; checks UNIQUE constraint (skips NULL keys — NULL ≠ NULL in SQL)
- `delete_from_indexes(indexes, row, storage)` — deletes key from indexes (ignores
  missing keys); skips NULLs and over-length keys

Integrated into `execute_insert`, `execute_update`, `execute_delete`:
- Pre-load secondary indexes once before the row loop.
- After each heap mutation, call the appropriate maintenance function.
- Persist updated `root_page_id` values after B-Tree splits.
- For UPDATE: update in-memory `root_page_id` after delete before insert to avoid
  reading freed pages.

### 6.3 — Query planner

`crates/axiomdb-sql/src/planner.rs`:
- `AccessMethod` enum: `Scan`, `IndexLookup { index_def, key }`, `IndexRange { index_def, lo, hi }`
- `plan_select(where_clause, indexes, columns)` — matches:
  - Rule 1: `col = literal` or `literal = col` → `IndexLookup`
  - Rule 2: `col > lo AND col < hi` (or `>=`/`<=`) → `IndexRange`
  - Otherwise: `Scan`

Integrated into `execute_select` (single-table no-JOIN path):
- Loads indexes from catalog before the scan.
- Dispatches on `AccessMethod`:
  - `Scan` → existing `TableEngine::scan_table` (unchanged)
  - `IndexLookup` → `BTree::lookup_in` → `TableEngine::read_row`
  - `IndexRange` → `BTree::range_in` → `TableEngine::read_row` for each hit

New `TableEngine::read_row(storage, columns, rid)` reads a single heap row by RID.

## New error types

In `axiomdb-core/src/error.rs`:
- `IndexAlreadyExists { name, table }` — SQLSTATE 42P07
- `IndexKeyTooLong { key_len, max }` — SQLSTATE 54000

(UniqueViolation already existed)

## Tests

- 11 new integration tests in `crates/axiomdb-sql/tests/integration_indexes.rs`
- 4 new unit tests in `axiomdb-catalog/src/schema.rs` (roundtrip, old-format compat)
- 10 unit tests in `axiomdb-sql/src/key_encoding.rs`
- 6 unit tests in `axiomdb-sql/src/planner.rs`

Total: 1135 tests pass (was 1124 before this phase).

## Deferred items

- `⚠️` Composite index planner (> 1 column WHERE predicate) — encoding supported,
  planner deferred to subfase 6.8
- `⚠️` Bloom filter per index — deferred to 6.4
- `⚠️` MVCC on secondary indexes — deferred to 6.14
- `⚠️` Index statistics (NDV, row counts) — deferred to 6.10

## 6.15 — Index corruption detection

`6.15` adds a startup-time logical integrity pass for secondary and primary
indexes. The new verifier lives in
`crates/axiomdb-sql/src/index_integrity.rs` and runs immediately after WAL
recovery in both:

- `crates/axiomdb-network/src/mysql/database.rs`
- `crates/axiomdb-embedded/src/lib.rs`

### What it does

For every catalog-visible table and index:

1. scan heap-visible rows under the committed snapshot
2. derive the exact expected index entries using the same key encoding and
   partial-index predicate semantics as normal DML maintenance
3. enumerate the actual B+ Tree entries from the catalog root
4. compare expected vs actual

If the tree is readable but divergent, AxiomDB:

- rebuilds a fresh index root from heap contents
- flushes the rebuilt pages
- rotates the catalog root inside a WAL-protected transaction
- defers free of the old tree pages until commit durability is confirmed

If the tree cannot be traversed safely, open fails with
`DbError::IndexIntegrityFailure` and the database never starts serving traffic.

### Code changes

- `crates/axiomdb-sql/src/index_integrity.rs`
  - `verify_and_repair_indexes_on_open(...)`
  - `IndexIntegrityReport`
  - `RebuiltIndex`
- `crates/axiomdb-sql/src/executor/ddl.rs`
  - `build_index_root_from_heap(...)`
- `crates/axiomdb-sql/src/executor/bulk_empty.rs`
  - `collect_btree_pages(...)`
  - `free_btree_pages(...)`
- `crates/axiomdb-core/src/error.rs`
  - `DbError::IndexIntegrityFailure`
- `crates/axiomdb-sql/src/lib.rs`
  - exports the verifier API for server/embedded callers

### Tests

- `crates/axiomdb-sql/tests/integration_index_integrity.rs`
  - rebuild missing unique-index entries
  - rebuild partial-index divergence
  - fail open for unreadable roots
  - verify rebuilt indexes remain durable across reopen
- `crates/axiomdb-network/tests/integration_open_integrity.rs`
  - server open fails on unreadable index root
- `crates/axiomdb-embedded/tests/integration.rs`
  - embedded open fails on unreadable index root

### Important invariant discovered during implementation

The rebuilt B+ Tree pages are written directly into storage, not through WAL.
That means the correct ordering is:

1. build fresh tree pages
2. `storage.flush()`
3. commit catalog root swap

Without that ordering, WAL recovery could make the new root visible before the
rebuilt pages were durable.

### Deferred

- SQL `REINDEX` remains deferred to `19.15` / `37.44`
- broader user-facing integrity surfaces (`CHECK TABLE`, `PRAGMA integrity_check`
  style commands) remain deferred to later diagnostics phases

## 6.16 — Primary-key SELECT access path

`6.16` closes the planner blind spot that still treated PRIMARY KEY lookups as
heap scans for single-table `SELECT`.

The change lives in `crates/axiomdb-sql/src/planner.rs`:

- `find_index_on_col(...)` now accepts `allow_primary`
- `plan_select(...)` and `plan_select_ctx(...)` enable PRIMARY KEY eligibility
- `WHERE pk = literal` bypasses the small-table / NDV `stats_cost_gate`
- PK ranges reuse the existing `IndexRange` path
- the collation guard is preserved: non-binary session collation still rejects
  text-key index access, even for PRIMARY KEY indexes

### What this fixes

The executor already knew how to run `IndexLookup` and `IndexRange`, and PK
B+Trees were already populated on `INSERT` since `6.9`. The missing piece was
planner-side: `idx.is_primary` was still excluded, so:

```sql
SELECT * FROM bench_users WHERE id = 42;
```

could not reach the PK B+Tree at all.

### Validation

- planner unit tests:
  - PK equality → `IndexLookup`
  - PK equality bypasses `stats_cost_gate`
  - PK range → `IndexRange`
  - non-binary collation still rejects text PK index access
- SQL integration:
  - `SELECT id, name FROM users WHERE id = 2` on a table with only
    `PRIMARY KEY (id)`
- MySQL wire smoke:
  - PK equality lookup without any secondary index
  - PK range lookup without any secondary index
- targeted local benchmark:
  - `python3 benches/comparison/local_bench.py --scenario select_pk --rows 5000 --table`
  - MariaDB `12.7K lookups/s`
  - MySQL `13.4K lookups/s`
  - AxiomDB `11.1K lookups/s`

### Deferred

- composite PK/prefix planner work remains separate
- wire/materialization overhead after the PK planner path is active remains
  tracked separately in the performance debt list

## 6.17 — Indexed UPDATE candidate fast path

`6.17` fixes the row-discovery side of indexed `UPDATE`.

Before this subphase, both `execute_update_ctx` and `execute_update` started by
calling `TableEngine::scan_table(...)` and only then applied the `WHERE`
filter. That meant `UPDATE ... WHERE id BETWEEN ...` still paid O(n) heap
discovery even after `5.20` had already improved the physical rewrite path.

### What changed

In `crates/axiomdb-sql/src/planner.rs`:

- `plan_update_candidates(...)`
- `plan_update_candidates_ctx(...)`

These mirror the indexed DELETE candidate planner:

- no `stats_cost_gate`
- no `IndexOnlyScan`
- PRIMARY KEY, UNIQUE, secondary, and eligible partial indexes allowed
- same non-binary text-collation guard as `DELETE`

In `crates/axiomdb-sql/src/executor/update.rs`:

- indexed predicates now go through planner-selected `IndexLookup` /
  `IndexRange`
- candidate `RecordId`s are materialized before any heap or index mutation
- fetched rows still re-evaluate the full original `WHERE`
- the surviving rows then flow into the unchanged `5.20` stable-RID / fallback
  write path

### Why this scope matters

`6.17` does not redesign UPDATE writes. That remains owned by `5.20`.

The separation is intentional:

- `6.17` solves candidate discovery
- `5.20` remains the source of truth for physical heap/index update semantics

That keeps the indexed UPDATE speedup from weakening rollback, FK checks, or
index-correctness invariants.

### Validation

- planner unit tests:
  - PK equality → indexed UPDATE access
  - PK range → indexed UPDATE access
  - text-key PK rejected under non-binary collation
- SQL integration:
  - PK range update changes only the targeted rows
  - secondary-index equality update changes only the targeted row
- MySQL wire smoke:
  - PK-range UPDATE on a PK-only table
  - secondary-index equality UPDATE
- targeted local benchmark:
  - `python3 benches/comparison/local_bench.py --scenario update_range --rows 5000 --table`
  - MariaDB `618K rows/s`
  - MySQL `291K rows/s`
  - AxiomDB `85.2K rows/s`

### Deferred

- the remaining `update_range` gap is no longer discovery-side; it now sits in
  the apply path after rows are found

## 6.20 — UPDATE apply fast path

`6.20` closes the apply-side debt left open after `6.17`.

The benchmark that was still behind is narrower than it first looked:

- `local_bench.py --scenario update_range` uses only `PRIMARY KEY (id)` by default
- the workload updates `score`, not `id`
- the statement runs inside one explicit transaction

So the remaining gap was not planner discovery anymore. It was the write path
after candidate rows had already been found.

### What changed

In `crates/axiomdb-sql/src/executor/delete.rs` and `crates/axiomdb-sql/src/table.rs`:

- `IndexLookup` / `IndexRange` candidate materialization now uses
  `TableEngine::read_rows_batch(...)` instead of one `read_row(...)` per RID
- batched heap reads preserve candidate order while reading each page once

In `crates/axiomdb-sql/src/executor/update.rs`:

- UPDATE now partitions matched rows into:
  - matched but no-op rows
  - physically changed rows
- no-op rows keep current affected-row semantics but skip heap and index work
- both ctx and non-ctx UPDATE paths now share the same statement-level
  "can any index be affected at all?" bailout

In `crates/axiomdb-sql/src/table.rs` and `crates/axiomdb-wal/src/txn.rs`:

- stable-RID rewrites now accumulate `UpdateInPlace` images per statement
- WAL emission uses `reserve_lsns(...) + write_batch(...)` through
  `record_update_in_place_batch(...)`
- undo / rollback / recovery semantics stay byte-for-byte compatible with the
  existing `UpdateInPlace` format

In `crates/axiomdb-sql/src/index_maintenance.rs`:

- UPDATE no longer reinserts new index keys row-by-row
- each affected index now does:
  - grouped delete pass
  - grouped insert pass
  - one final root persistence write

### Validation

- storage unit tests:
  - batch row reads preserve RID order and read same-page batches once
- WAL unit tests:
  - batched `UpdateInPlace` entries remain parseable and recovery-compatible
- SQL integration:
  - no-op UPDATE keeps matched-row count while leaving data unchanged
  - PK-range candidate path still updates only matching rows
  - secondary-index candidate path still updates only matching rows
  - grouped UPDATE index maintenance still removes old PK/secondary keys correctly
- MySQL wire smoke:
  - no-op PK-range UPDATE preserves matched-row count
  - PK-range UPDATE changes only targeted rows
- targeted local benchmark:
  - `python3 benches/comparison/local_bench.py --scenario update_range --rows 50000 --range-rows 5000 --table --engines axiomdb`
  - MariaDB `618K rows/s`
  - MySQL `291K rows/s`
  - AxiomDB `369.9K rows/s`

### Result

`6.20` moves `update_range` from an apply-path bottleneck to a remaining
MariaDB-vs-AxiomDB gap. AxiomDB now beats the documented MySQL local result on
the default PK-only benchmark without changing SQL-visible UPDATE semantics.

## 6.18 — Indexed multi-row INSERT batch path

`6.18` closes the immediate multi-row `INSERT ... VALUES (...), (... )`
performance gap for indexed tables.

Before this subphase, the executor only took the batch heap path when the table
had no secondary indexes. A table with just `PRIMARY KEY (id)` still fell back
to per-row heap + index maintenance for a multi-row VALUES statement.

### What changed

In `crates/axiomdb-sql/src/executor/staging.rs`:

- extracted shared physical apply helpers:
  - `apply_insert_batch(...)`
  - `apply_insert_batch_with_ctx(...)`
- staged transactional INSERT flush (`5.21`) now reuses the same grouped
  heap/index apply layer as immediate multi-row INSERT

In `crates/axiomdb-sql/src/executor/insert.rs`:

- immediate `InsertSource::Values` now materializes a full batch when more than
  one row is present
- both ctx and non-ctx paths route that batch through the shared grouped apply
  helper even when PRIMARY KEY or secondary indexes exist
- single-row INSERT behavior remains unchanged

### Important semantic boundary

The new immediate path intentionally does **not** reuse the staged
`committed_empty` bulk-load optimization from `5.21`.

That optimization is only safe after enqueue-time uniqueness validation using
the staging path's `unique_seen` set. Immediate multi-row INSERT must still
detect duplicate PRIMARY KEY / UNIQUE values inside the same SQL statement
without exposing partial writes.

### Validation

- SQL integration:
  - multi-row INSERT on a PK-only table preserves all rows
  - duplicate UNIQUE key inside one multi-row statement errors correctly
  - partial UNIQUE index mixed-membership case remains correct
- MySQL wire smoke:
  - PK-only multi-row INSERT
  - duplicate UNIQUE inside the same statement
  - failed UNIQUE batch leaves no partial committed rows
- targeted local benchmark:
  - `python3 benches/comparison/local_bench.py --scenario insert_multi_values --rows 5000 --table`
  - MariaDB `160,581 rows/s`
  - MySQL `259,854 rows/s`
  - AxiomDB `321,002 rows/s`

### Deferred

- autocommit INSERT throughput remains a separate problem from immediate
  multi-row VALUES on indexed tables


### 6.19 WAL Fsync Pipeline

Leader-based fsync coalescing inspired by MariaDB's `group_commit_lock`.

#### What was built

- `FsyncPipeline` (`axiomdb-wal/src/fsync_pipeline.rs`): shared state machine
  tracking `flushed_lsn`, `leader_active`, `pending_lsn`, and a waiter queue.
  Three outcomes from `acquire(lsn, txn_id)`: `Expired` (already fsynced),
  `Acquired` (become leader), `Queued(rx)` (await leader's confirmation).
- `WalSyncMethod` + `WalDurabilityPolicy` (`axiomdb-storage/src/config.rs`):
  configurable sync strategy and per-commit durability contract.
- `TxnManager::deferred_commit_mode`: DML `commit()` appends to BufWriter
  without inline fsync; `take_pending_deferred_commit()` hands the txn_id to
  the pipeline leader.
- `Database::take_commit_rx()` (`axiomdb-network/src/mysql/database.rs`):
  bridges SQL execution to pipeline acquire / leader fsync / follower await.
- Handler `await_commit_rx`: awaits the oneshot after releasing the DB lock.
- Old `CommitCoordinator` and timer-based group commit (`3.19`) removed.

#### Tests

- 9 unit tests in `fsync_pipeline.rs` (Expired, Acquired, Queued, release_ok,
  release_err, disk_full, monotonic flushed_lsn, leader release/re-acquire).
- 6 integration tests in `tests/integration_fsync_pipeline.rs`:
  single commit visibility, two sequential leaders, follower piggyback,
  crash recovery, batch wakeup (3 followers), MVCC visibility invariant.
- Wire smoke in `tools/wire-test.py` [6.19]: two-connection autocommit
  correctness, visibility, and usability after pipeline fsync path.

#### Known gap

Single-connection request-response throughput on macOS APFS is ~224 ops/s —
identical to the pre-pipeline baseline. The pipelining benefit (piggyback on
an in-flight fsync) requires the next INSERT to arrive while the previous
fsync is still running. In the current MySQL handler model the OK packet is
sent only after fsync, so sequential clients cannot overlap. Multi-connection
concurrent batching works correctly. Linux `fdatasync` would yield ~5–10K ops/s.
