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

## Subfase 40.3 — StorageEngine Interior Mutability

### What was implemented

Changed the `StorageEngine` trait so all mutating methods (`write_page`, `alloc_page`,
`free_page`, `flush`, `set_current_snapshot`) take `&self` instead of `&mut self`.
Mutable state is now managed with interior mutability inside each implementation.
This is the architectural unlock for concurrent writer support (phases 40.4–40.12).

### Key components

**`crates/axiomdb-storage/src/page_lock.rs`** (new file)
- `PageLockTable`: 64-shard per-page `RwLock` table inspired by InnoDB's `block_lock`.
- Shard selection: `page_id % 64` (power-of-2 fast modulo).
- Lazy lock creation per page; locks never removed (bounded by total pages).
- `read(page_id)` → shared guard; `write(page_id)` → exclusive guard.
- Shard `RwLock` held only during `HashMap` lookup; page `RwLock` held during I/O.

**`crates/axiomdb-storage/src/engine.rs`**
- Trait updated: `Send + Sync`, all methods `&self`.
- Two new optional methods: `set_current_snapshot(&self, ...)` and `deferred_free_count()`.

**`crates/axiomdb-storage/src/mmap.rs`** (MmapStorage)
- `freelist: Mutex<FreeList>` — allocation/free hold the mutex only during bitmap scan.
- `freelist_dirty: AtomicBool` — set atomically on alloc/free.
- `deferred_frees: Mutex<Vec<(TxnId, Vec<u64>)>>` — epoch-tagged deferred free queue.
- `current_snapshot_id: AtomicU64` — updated per-statement without any lock.
- `page_locks: PageLockTable` — per-page exclusive locks for `write_page`.
- `grow_lock: Mutex<()>` — prevents concurrent file extension.
- Lock ordering: `page_lock < freelist_mutex < grow_lock` — no deadlock possible.

**`crates/axiomdb-storage/src/memory.rs`** (MemoryStorage)
- `inner: RwLock<MemoryStorageInner>` — single lock for the test storage.
- All `StorageEngine` methods acquire read or write locks on demand.

**~190 call sites across 25 files**
- Pattern: `storage: &mut dyn StorageEngine` → `storage: &dyn StorageEngine`.
- No logic changes — purely mechanical signature update.
- Files updated: `axiomdb-index/src/tree.rs`, `axiomdb-wal/src/txn.rs`,
  `axiomdb-wal/src/recovery.rs`, `axiomdb-catalog/src/bootstrap.rs`,
  `axiomdb-sql/src/executor/*.rs`, `axiomdb-sql/src/fk_enforcement.rs`,
  `axiomdb-sql/src/index_maintenance.rs`, and all integration tests.

### Pre-existing bug fixed

**Clustered secondary stale entries on re-insert after MVCC delete**:
When a clustered row is deleted (MVCC-mark only, secondary entries left in place per
the deferred-cleanup design), re-inserting the same row failed with `DuplicateKey`
because `BTree::insert_in` found the stale physical secondary entry. Fixed in
`ClusteredSecondaryLayout::insert_row`: now calls `BTree::delete_in` with the same
physical key before `BTree::insert_in`, cleaning up any stale entry. For non-unique
indexes the physical key includes the PK suffix (globally unique per row), so any
pre-existing entry is guaranteed to be dead.

### Benchmarks (storage.rs)

| Benchmark | Result | Verdict |
|---|---|---|
| memory/alloc/alloc_page | 1837 ns/iter | ✅ |
| memory/write_read/write_page | 2031 ns/iter | ✅ |
| memory/write_read/read_page | 1308 ns/iter | ✅ |
| memory/sequential/read_sequential/1000 | 547 µs/iter | ✅ |

The `RwLock` overhead in `MemoryStorage` is below measurement noise — uncontended
lock acquisition is a single atomic CAS (~5 ns on modern hardware).

### Tests

- All 268 `axiomdb-storage` tests pass (4 suites).
- All 16 `integration_delete_apply` tests pass (including the previously failing
  `test_delete_where_secondary_index_maintains_all_indexes`).
- Wire protocol: 311/311 assertions pass.

## Subfase 40.4 — Concurrent WAL Writer

### What was implemented

Replaced `WalWriter` (single `BufWriter<File>` + plain `u64 next_lsn`) with
`ConcurrentWalWriter` — a thread-safe, group-commit WAL writer where all methods
take `&self`. Multiple transactions can submit WAL entries simultaneously without
serializing on a mutex for LSN reservation.

### Key components

**`crates/axiomdb-wal/src/concurrent_writer.rs`** (new file)

- `WriteQueue`: `Vec<(u64, Vec<u8>)>` — pending `(base_lsn, serialized_bytes)` entries.
  `drain_sorted()` drains and sorts by `base_lsn` before handing off to the writer.
- `WriterState`: `BufWriter<File>` + `logical_end: u64` + `reserved_end: u64` + sync method.
  `ensure_capacity(required_end)` — pre-allocates in `PREALLOC_CHUNK` increments.
  `write_entries(&[(u64, Vec<u8>)])` — writes all entries in one pass.
- `ConcurrentWalWriter`:
  - `next_lsn: AtomicU64` — lock-free LSN reservation via `fetch_add(1, Relaxed)`.
  - `flushed_lsn: AtomicU64` — highest LSN confirmed durable. Updated by the flush leader.
  - `queue: Mutex<WriteQueue>` — held only for a single `Vec::push` (~1 µs).
  - `writer: Mutex<WriterState>` — held by the group commit leader for the I/O duration.
- `Drop` implementation: drains the queue and flushes the `BufWriter` to the OS page cache
  on drop (best-effort, no fsync). This preserves crash-simulation semantics used in
  durability tests: `drop(mgr)` makes buffered entries recoverable without durability guarantee.

**Lock ordering (no deadlock possible)**:
```
RULE: writer_mutex < queue_mutex
flush_and_sync: acquires writer_mutex → queue_mutex (brief drain)
submit_entry:   acquires queue_mutex only
```

**`crates/axiomdb-wal/src/txn.rs`**
- `TxnManager.wal: WalWriter` → `wal: ConcurrentWalWriter`
- All constructors (`create`, `open`, `open_with_recovery`) use `ConcurrentWalWriter::create/open`.
- `write_batch` calls updated: `write_batch(&scratch)` → `write_batch(lsn_base, &scratch)`.
- `wal_mut() -> &ConcurrentWalWriter` (no longer `&mut`).
- `rotate_wal` uses `ConcurrentWalWriter::rotate_file` and `ConcurrentWalWriter::open`.

**`crates/axiomdb-wal/src/checkpoint.rs`**
- `Checkpointer::checkpoint(storage, wal: &ConcurrentWalWriter)` — dropped `&mut`.
  All methods on `ConcurrentWalWriter` take `&self`, so `&mut` is unnecessary.

**`crates/axiomdb-wal/src/lib.rs`**
- Added `mod concurrent_writer;` and `pub use concurrent_writer::ConcurrentWalWriter`.

### Group commit algorithm

```
flush_and_sync():
  1. Acquire writer_mutex  → become group commit leader
  2. Acquire queue_mutex briefly → drain_sorted() → release queue_mutex
  3. write_entries(sorted)  → BufWriter in one pass
  4. BufWriter::flush()     → OS page cache
  5. fsync / fdatasync      → durable on disk
  6. flushed_lsn.fetch_max(max_lsn, Release)
  7. Release writer_mutex
```

One fsync covers all entries submitted by any number of concurrent transactions
between two flush calls — identical to InnoDB's group commit model.

### Benchmarks

| Benchmark | Result | Notes |
|---|---|---|
| `wal/lsn_reserve_single` | **~2.0 ns/op** | Lock-free `AtomicU64::fetch_add` |
| `wal/lsn_reserve_batch_100` | **~2.0 ns/op** | Same atomic op regardless of N |
| `wal/sequential/1` | ~3.4 µs | Single append + flush to OS |
| `wal/sequential/1000` | ~2.1 ms / **469K entries/s** | Amortized flush overhead |
| `wal/concurrent/threads/1` | ~283 µs for 100 entries | Baseline |
| `wal/concurrent/threads/4` | ~931 µs for 400 entries | 4× entries in ~same wall time |

The concurrent benchmark uses `flush_no_sync` (no fsync) to isolate queue/write
overhead. The group-commit advantage is most visible when fsync cost (~3–5 ms) is
shared: 8 transactions share 1 fsync instead of paying 8 × 5 ms = 40 ms.

### Tests

- `crates/axiomdb-wal/src/concurrent_writer.rs` — 10 unit tests:
  - `test_create_and_open` — LSN = 0 on fresh writer
  - `test_single_append_and_recover` — entry readable via WalReader after commit
  - `test_batch_append_in_lsn_order` — N entries in correct LSN order in file
  - `test_append_with_buf_zero_alloc` — scratch-buffer path, 4 entries
  - `test_open_after_close_resumes_lsn` — `next_lsn > last_written` after reopen
  - `test_rotate_file` — after rotation, `reserve_lsn()` returns `start_lsn + 1`
  - `test_flush_no_sync_visible_to_reader` — entries readable after `flush_no_sync`
  - `test_concurrent_appends_all_lsns_present` — 4 threads × 50 = 200 entries, all LSNs present, no duplicates, file in order
  - `test_concurrent_batches_all_lsns_present` — 4 threads × 25-entry batches = 100 entries
  - `test_no_duplicate_lsns_under_contention` — 8 threads × 125 = 1000 entries
- All 115 `axiomdb-wal` lib tests pass (including recovery, checkpoint, txn suites).
- All 14 `integration_durability` tests pass.
