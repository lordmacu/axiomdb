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

## Subfase 40.8 — B-tree Latch Coupling

### What was implemented

A hybrid optimistic / pessimistic latch coupling protocol for both the
**index B-tree** (`axiomdb-index/src/tree_*.rs`) and the **clustered B-tree**
(`axiomdb-storage/src/clustered_tree/*.rs`). All page latches come from the
shared `PageLockTable` introduced in 40.3.

The protocol has three layers:

1. **Read descent (`lookup`, `range`, `leftmost_leaf`)** — S-latch coupling
   with `try_read` on the child and a restart-on-contention loop. At most
   two S-latches are held simultaneously, briefly during the parent → child
   handover.
2. **Optimistic write fast path (`insert`, `delete`)** — single X-latch on
   the leaf if the leaf can absorb the operation in place
   (`num_keys < threshold` for insert, `num_keys > MIN_KEYS_LEAF` for
   delete). Internals are not X-latched.
3. **Pessimistic write descent with early X-latch release** — when the
   optimistic path fails, the writer restarts in pessimistic mode and
   X-latches every level. At each recursive level, the parent latch is
   **dropped before** the recursive call when the immediate child is
   "safe" (cannot propagate a structural change upward), so concurrent
   readers can resume on the cleared internal page.

### Key components

**`crates/axiomdb-index/src/tree_insert.rs`**

- `child_is_safe_for_insert(child_pid, fillfactor)` — returns `true` when
  the child has room to absorb one new entry without splitting:
  `num_keys < fill_threshold(ORDER_LEAF, fillfactor)` for a leaf,
  `num_keys < ORDER_INTERNAL` for an internal node.
- `insert_subtree` now drops the parent X-latch before recursing into a
  safe child. The recursive call is then guaranteed to return
  `InsertResult::Ok(child_pid)`, so the parent never needs an update.

**`crates/axiomdb-index/src/tree_delete.rs`**

- `child_is_safe_for_delete(child_pid)` — returns `true` only when the
  immediate child is a **leaf** with strictly more than `MIN_KEYS_LEAF`
  keys. Internal children stay pessimistic because the index B-tree's
  internal-rebalance routines (`rotate_right` / `rotate_left` /
  `merge_children`) still allocate a fresh parent pid via Copy-on-Write,
  which means even an internal child with plenty of keys can return a
  different pid to its grandparent when a deeper underflow propagates a
  rebalance up to it.
- `delete_subtree` drops the parent X-latch before recursing into a safe
  leaf child. Leaves always rewrite in place (`write_leaf_same_pid`), so
  their pid is stable.

**`crates/axiomdb-storage/src/clustered_tree/page_utils.rs`**

- `child_is_safe_for_insert(storage, child_pid, key, row_data_len)` —
  byte-budget check with 2× headroom over `cell_footprint` (leaf) /
  `separator_footprint(key.len() + 64)` (internal). The 2× margin covers
  the fact that the propagated separator may be slightly larger than the
  inserted key (the median promotion picks among existing leaf keys).
- `child_is_safe_for_delete(storage, child_pid)` — for leaf children only,
  requires `used > capacity / 2`. The clustered tree's underfull rule is
  `used < capacity / 4`, so this leaves a comfortable margin even after
  losing one cell. Internal children stay pessimistic by the same
  reasoning as the index tree (sub-tree introspection would be required
  to rule out a deeper rebalance).

**`crates/axiomdb-storage/src/clustered_tree/btree_insert.rs`**

- `insert_subtree` peeks the immediate child while holding the parent
  X-latch, then drops the parent latch on safe descent. The clustered
  tree never changes a page's pid on split (the old page stays as the
  left half), so a safe child always returns `InsertResult::Inserted`.

**`crates/axiomdb-storage/src/clustered_tree/delete.rs`**

- `delete_physical_subtree` drops the parent latch on safe descent. After
  the recursive call returns, the parent latch is **re-acquired only**
  when the child reports `min_changed && child_idx > 0` (separator
  repair). If the new separator no longer fits in the parent page, the
  function surfaces `underfull = true` so the grandparent's existing
  rebalance path can handle the propagation.

### Latch ordering

- Tree descent: parent before child (standard latch coupling).
- Sibling pairs (rotate / merge): always `min(left_pid, right_pid)` first,
  then the other — same ascending block-id order InnoDB uses.
- Static API writers serialize at a synthetic tree-level X-latch keyed by
  the address of the `AtomicU64` root, so per-page latches never deadlock
  between static API callers.

### Tests

- **`crates/axiomdb-index/tests/integration_btree.rs`**:
  - `test_concurrent_insert_in_eight_threads_all_keys_reachable` — 8
    threads × 10K writes (= 80K total). Matches the spec stress scenario.
  - `test_btree_early_release_mixed_workload` — 20K inserts followed by
    every-third-key deletes. Exercises the early-release branch on both
    insert and delete in the same run.
  - `test_concurrent_readers_during_inserts_no_lost_keys` — 4 readers + 4
    writers on a pre-populated tree where the root never changes during
    the run.
- **`crates/axiomdb-storage/src/clustered_tree/tests_insert.rs`**:
  - `many_inserts_with_safe_descent_keep_tree_consistent` — 2K inserts
    grow the tree past one internal level and verify the leaf chain is
    sorted, all keys reachable.
- **`crates/axiomdb-storage/src/clustered_tree/tests_delete.rs`**:
  - `delete_physical_through_safe_descent_keeps_tree_consistent` — 128
    fat-row inserts followed by a delete in the middle of a populated
    leaf, verifying the safe-descent branch produces the same result as
    the pessimistic path.

### Validation

- `cargo test --workspace`: **2506 passed, 9 ignored, 0 failed**
- `cargo clippy --workspace --lib --bins -- -D warnings`: clean
- `cargo fmt --check`: clean

### Known limitations (deferred to 40.10)

- Concurrent root republication for `BTree::insert_in` / `delete_in` across
  root splits is not yet retry-safe. The 8-thread × 10K stress test runs
  writers-only; reader/writer interaction is exercised on a pre-populated
  tree where the root never changes.
- Internal-children early X-latch release for the index B-tree **delete**
  path requires the internal rebalance routines to keep their parent pid
  in place (no CoW). That refactor is the prerequisite for full
  internal-children early release on delete.

## Subfase 40.9 — FreeList Tier-1 (per-connection LocalPageBatch)

### What was implemented

A two-tier page allocation system that eliminates 99% of global `Mutex<FreeList>`
acquisitions. Tier 2 (the global Mutex) was already implemented in 40.3. This
subfase adds Tier 1: per-connection `LocalPageBatch` that caches pre-allocated
page IDs locally, so most allocations are O(1) with zero locks.

### Key components

**`crates/axiomdb-storage/src/local_page_batch.rs`** (new file)
- `LocalPageBatch`: per-connection batch with `available: VecDeque<u64>`,
  `current_type: Option<PageType>`, `freed: Vec<u64>`, `last_refill_size: usize`.
- `pop_or_refill(storage, ty)`: fast path pops from local `available` (O(1), zero
  contention); slow path refills from global allocator with adaptive sizing.
- `take_for_commit()` → `(available, freed)`: drains batch for COMMIT. Available
  pages go back to bitmap (steal protection); freed pages go to recycle queue.
- `take_for_rollback()` → `available`: drains batch for ROLLBACK. Available pages
  go back to bitmap; freed pages dropped (rollback restores the rows → pages in use).
- `batch_alloc_page(storage, batch, ty)`: canonical entry point that delegates to
  `pop_or_refill` when batch is `Some`, falls back to `storage.alloc_page` when `None`.
- `BATCH_ALLOC_SIZE = 64` (InnoDB extent size), `MAX_BATCH_SIZE = 256` (cap).

**`crates/axiomdb-storage/src/engine.rs`** (StorageEngine trait)
- `alloc_page_batch(n, page_type) -> Vec<u64>`: default impl loops `alloc_page`.
- `free_page_batch(&[u64])`: default impl loops `free_page`.
- `extension_waiters() -> u32`: returns 0 by default; MmapStorage overrides.
- `recycle_page(page_id)`: pushes to recycle queue; default impl calls `free_page`.

**`crates/axiomdb-storage/src/freelist.rs`**
- `alloc_batch_sequential(n) -> Vec<u64>`: scans bitmap words for a contiguous run
  of `n` free bits; falls back to non-contiguous per-bit allocation if no run found.
- `free_batch(&[u64])`: single-pass batch deallocation under one lock acquisition.

**`crates/axiomdb-storage/src/mmap.rs`** (MmapStorage)
- `recycle_queue: crossbeam_queue::SegQueue<u64>` — lock-free MPMC queue for pages
  freed by committed transactions, ready for fast reuse.
- `extension_waiters: AtomicU32` — incremented before freelist mutex acquisition,
  decremented after; used by `pop_or_refill` to compute adaptive batch size.
- `alloc_page_batch`: drains recycle_queue first (lock-free), then falls back to
  `freelist.alloc_batch_sequential` under mutex, then to `do_grow` if exhausted.
- `free_page_batch`: single bitmap mutation pass under one `Mutex<FreeList>` lock.
- `release_deferred_frees`: now also drains recycle_queue back into bitmap so
  in-memory growth is bounded.

**`crates/axiomdb-wal/src/txn.rs`** (ConnectionTxn)
- `local_page_batch: LocalPageBatch` field initialized empty in `begin`.
- Commit drains: `freed` → `recycle_queue` via `storage.recycle_page()`;
  `available` → bitmap via `storage.free_page_batch()`.
- Rollback drains: `available` → bitmap; `freed` cleared (pages still in use).
- Savepoint records `freed_len_at_sp`; rollback-to-savepoint truncates.

### Hot-path wiring

All hot-path allocators now accept `Option<&mut LocalPageBatch>` and route through
`batch_alloc_page`:

| Crate | File | Functions |
|---|---|---|
| `axiomdb-storage` | `heap_chain_write.rs` | `insert`, `insert_with_hint` |
| `axiomdb-storage` | `clustered_tree/btree_insert.rs` | `split_leaf`, `split_internal`, `insert_subtree` |
| `axiomdb-storage` | `clustered_tree/mod.rs` | `insert` (root creation) |
| `axiomdb-storage` | `clustered_overflow.rs` | `write_chain` |
| `axiomdb-index` | `tree_insert.rs` | `insert_leaf`, `split_internal`, `alloc_root` |
| `axiomdb-index` | `tree_delete.rs` | `rotate_left`, `rotate_right`, `merge_children` |

DDL, catalog, vacuum, and recovery paths continue using `storage.alloc_page` directly
(no batch), explicitly verified.

### Adaptive batch sizing (PostgreSQL-inspired)

The refill size is computed as:

```
n = last_refill_size × max(1, extension_waiters.load())
```

Capped at `MAX_BATCH_SIZE = 256`. Once a connection sees contention, it pulls a bigger
batch and stays there ("sticky bulk mode", borrowed from PostgreSQL's `bistate`).

### Homogeneous batches (PostgreSQL BulkInsertState model)

`LocalPageBatch.current_type` tracks which `PageType` the batch was last refilled for.
A type mismatch on `pop_or_refill` drains the existing batch back to the bitmap and
refills with the new type.

### Benchmarks (storage.rs)

| Benchmark | Time | Throughput | Speedup vs baseline |
|---|---|---|---|
| single_mutex_1000 (baseline) | 36.5 ms | 27.4K elem/s | 1× |
| batch_local_1000 | 1.27 ms | 787.7K elem/s | **28.8×** |
| 8t_single_10k (8 threads, per-page Mutex) | 5.35 s | 187 elem/s | 1× |
| 8t_batch_10k (8 threads, LocalPageBatch) | 7.6 ms | 131.6K elem/s | **703×** |

Both results far exceed the ≥5× closing gate target.

### Tests

- `crates/axiomdb-storage/tests/integration_storage.rs`:
  - `test_local_page_batch_pop_or_refill_basic` — basic pop, refill on empty
  - `test_local_page_batch_type_mismatch_drain` — type mismatch drain + refill
  - `test_local_page_batch_commit_drain` — commit drain semantics (available→bitmap, freed→recycle)
  - `test_mmap_alloc_page_batch_concurrent_8_threads` — 8 threads × 10K alloc+free, all unique
  - `test_mmap_alloc_page_batch_recycle_queue` — recycle queue fold-back

### Validation

- `cargo test --workspace`: **2518 passed, 9 ignored, 0 failed**
- `cargo clippy --workspace -- -D warnings`: clean
- `cargo fmt --check`: clean
