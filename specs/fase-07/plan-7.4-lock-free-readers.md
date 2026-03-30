# Plan: Lock-free readers with CoW (Phase 7.4)

## Inspiration

This design combines insights from three databases studied in `research/`:

- **DuckDB** — the overall architecture: no per-page locks, pure MVCC for
  reader/writer isolation, timestamp-based snapshots with `AtomicU64`
- **PostgreSQL** — the wait-free reader principle: readers should NEVER wait
  for any lock to acquire, not even a shared lock
- **InnoDB** — the `rw_lock` pattern: split state into a shared counter
  (readers) and an exclusive sentinel (writers); but AxiomDB simplifies this
  to zero-lock readers since mmap already provides concurrent page access

The key AxiomDB advantage over all three: B-Tree CoW means index pages don't
need undo buffers (DuckDB) or per-page locks (PG/InnoDB) for concurrent
access. Old tree roots remain valid for any snapshot.

## Files to create/modify

- `crates/axiomdb-network/src/mysql/database.rs` — split `Database` into `SharedState` + `WriterState`; add `execute_read_query()` that takes `&SharedState` with no lock
- `crates/axiomdb-network/src/mysql/handler.rs` — replace `Arc<Mutex<Database>>` with `Arc<SharedState>` + `Arc<Mutex<WriterState>>`; classify queries and route to read vs write path
- `crates/axiomdb-network/src/mysql/mod.rs` — update server startup
- `crates/axiomdb-sql/src/executor/mod.rs` — add `execute_read_only()` entry point taking `&dyn StorageEngine` + `TransactionSnapshot`
- `crates/axiomdb-sql/src/executor/select.rs` — add `execute_select_readonly()` that works with `&dyn StorageEngine`
- `crates/axiomdb-network/tests/integration_concurrent_readers.rs` — concurrent reader test with real MmapStorage
- `tools/wire-test.py` — concurrent SELECT smoke test

## Algorithm / Data structure

Split architecture:

```text
SharedState (Arc, no lock, concurrent access):
  storage: MmapStorage              // read_page(&self) — lock-free via mmap
  max_committed: AtomicU64           // readers load this for snapshot creation
  schema_version: Arc<AtomicU64>     // already exists, lock-free
  status: Arc<StatusRegistry>        // already exists, atomic counters
  catalog_page_ids: CatalogPageIds   // immutable after open, safe to share

WriterState (Mutex, exclusive):
  txn: TxnManager                    // begin/commit/rollback need &mut self
  bloom: BloomRegistry               // add/remove need &mut self
  pipeline: FsyncPipeline            // commit coordination
```

Read path (lock-free):

```text
on read-only query (SELECT, SHOW, etc):
  // NO lock acquired at any point
  snap = TransactionSnapshot::committed(shared.max_committed.load(Relaxed))
  reader = CatalogReader::new(&shared.storage, snap)
  result = execute_read_only(stmt, &shared.storage, snap)
  send result to client
```

Write path (exclusive Mutex):

```text
on write query (INSERT, UPDATE, DELETE, DDL):
  guard = writer.lock().await         // exclusive
  result = execute_with_ctx(stmt, &mut shared.storage_mut(), &mut guard.txn, ...)
  if committed:
    shared.max_committed.store(new_val, Release)  // readers see new data
  drop(guard)
```

Write path needs `&mut MmapStorage` — since `MmapStorage` is in `SharedState`,
the writer needs a way to get `&mut` access. Options:

Option A: `UnsafeCell<MmapStorage>` in SharedState, writer proves exclusivity
via Mutex guard.

Option B: `MmapStorage` in WriterState, but share a read-only `Arc<Mmap>` handle.

Option C: MmapStorage split into `MmapReader` (Arc, &self) + `MmapWriter` (&mut self).

**Recommended: Option C** — cleanest Rust ownership:

```text
MmapReader (Clone, Send, Sync):
  mmap: Arc<Mmap>                    // shared read-only mapping
  page_count: AtomicU64              // updated by writer after alloc

MmapWriter:
  mmap: MmapMut                      // exclusive mutable mapping
  reader: MmapReader                  // for read-during-write
```

Both implement `StorageEngine::read_page(&self)` via the shared `Arc<Mmap>`.
Only `MmapWriter` implements `write_page(&mut self)`.

## Implementation phases

1. **Split MmapStorage** into `MmapReader` + `MmapWriter`:
   - `MmapReader`: holds `Arc<Mmap>`, implements `read_page(&self)`, `page_count(&self)`, `prefetch_hint(&self)`
   - `MmapWriter`: holds `MmapMut` + `MmapReader`, implements full `StorageEngine` trait
   - `MmapReader` is `Clone + Send + Sync`
   - `MmapWriter::reader()` returns a cloned `MmapReader`
   - Existing `MmapStorage` becomes `MmapWriter` internally

2. **Add read-only executor path**:
   - `execute_read_only(stmt, storage: &dyn StorageEngine, snap: TransactionSnapshot)` — new function
   - Handles: SELECT, SHOW TABLES, SHOW COLUMNS, SHOW DATABASES, DESCRIBE
   - Does NOT handle: INSERT, UPDATE, DELETE, DDL, BEGIN/COMMIT/ROLLBACK
   - Returns `DbError::NotImplemented` for write statements

3. **Split Database into SharedState + WriterState**:
   - `SharedState`: `MmapReader`, `AtomicU64` (max_committed), `Arc<AtomicU64>` (schema_version), `CatalogPageIds`
   - `WriterState`: `MmapWriter`, `TxnManager`, `BloomRegistry`, `FsyncPipeline`
   - `SharedState` is `Arc<SharedState>` — cloned per connection
   - `WriterState` is `Arc<Mutex<WriterState>>` — one writer at a time

4. **Update handler** to route queries:
   - Parse SQL → classify read vs write
   - Read: call `execute_read_only()` with `&shared.reader` — no lock
   - Write: call `writer.lock() → execute_with_ctx()` — exclusive

5. **Update `max_committed` atomically** after each commit:
   - In WriterState commit path: `shared.max_committed.store(new, Release)`
   - Readers: `shared.max_committed.load(Acquire)` for snapshot

6. **Concurrent reader test**:
   - Spawn N tokio tasks, each connecting via TCP and running SELECT
   - Verify all complete without blocking
   - Measure throughput scaling

## Tests to write

- unit: `MmapReader` concurrent `read_page` from multiple threads
- unit: `execute_read_only()` returns correct results for SELECT
- unit: `execute_read_only()` rejects INSERT/UPDATE/DELETE
- integration: N concurrent SELECT queries complete without deadlock (MmapStorage)
- integration: SELECT during INSERT — both complete, SELECT sees pre-insert data
- integration: existing single-connection tests pass (no regression)
- wire: concurrent pymysql SELECT test

## Anti-patterns to avoid

- Do NOT use `RwLock<Database>` — this still makes readers wait for writers (write lock blocks new read locks in writer-preferring implementations).
- Do NOT share `TxnManager` between readers and writers — readers use `AtomicU64` for max_committed, writers own TxnManager exclusively.
- Do NOT let readers access `BloomRegistry` — bloom filters are write-path optimization only; readers skip bloom checks (conservative: might_exist returns true).
- Do NOT hold writer Mutex across entire explicit transactions initially — defer per-statement release to a follow-up if needed.
- Do NOT make `MmapWriter` implement `Clone` — it must remain exclusive.

## Risks

- **MmapStorage write during concurrent read**: A writer calls `write_page()` on a mmap page while a reader is scanning it. With mmap, the OS handles page-level atomicity for aligned writes, but partial writes could be visible. Mitigation: B-Tree CoW writes to NEW pages (old pages untouched), and heap inserts to new slots (old slots untouched). Only heap DELETE modifies existing data (stamps `txn_id_deleted`), and this is a single 8-byte aligned write which is atomic on x86/ARM.
- **page_count growth**: When writer calls `alloc_page()`, the file grows and `page_count` increases. Readers need the updated count to avoid reading beyond the file. Mitigation: `page_count` is `AtomicU64` in the shared `MmapReader`.
- **mmap remap on growth**: When the file grows, the mmap may need to be remapped. Concurrent readers holding pointers to the old mmap would get stale data or segfault. Mitigation: use `MAP_SHARED` which allows the OS to extend the mapping, or use a double-buffer approach where old mmap remains valid until all readers release it.
- **MemoryStorage not thread-safe**: Tests using `MemoryStorage` can't test concurrent access. Mitigation: concurrent tests use `MmapStorage` with tempdir.
