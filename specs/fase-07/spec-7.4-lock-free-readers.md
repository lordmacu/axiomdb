# Spec: Lock-free readers with CoW (Phase 7.4)

## Reviewed first

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-storage/src/engine.rs` — `StorageEngine` trait: `read_page(&self)`, `write_page(&mut self)`
- `crates/axiomdb-storage/src/mmap.rs` — `MmapStorage::read_page` is pure pointer arithmetic on mmap, no mutable state
- `crates/axiomdb-wal/src/txn.rs` — `snapshot(&self)`, `active_snapshot(&self)` vs `begin/commit/rollback(&mut self)`
- `crates/axiomdb-catalog/src/reader.rs` — `CatalogReader::new(&dyn StorageEngine)` — takes shared ref
- `crates/axiomdb-sql/src/executor/mod.rs` — `execute(&mut dyn StorageEngine, &mut TxnManager)` — takes mutable even for SELECT
- `crates/axiomdb-network/src/mysql/database.rs` — `Database::execute_query(&mut self)` — exclusive for everything
- `crates/axiomdb-network/src/mysql/handler.rs` — `Arc<Mutex<Database>>` — global exclusive lock
- `research/postgres/src/backend/storage/lmgr/lwlock.c` — wait-free shared lock via atomic CAS on per-page LWLocks
- `research/mariadb-server/storage/innobase/include/rw_lock.h` — per-page atomic RwLock with shared counter + writer sentinel bit
- `research/duckdb/src/include/duckdb/transaction/duck_transaction_manager.hpp` — no per-page locks, MVCC-only visibility, separate undo buffer

## Research synthesis

### PostgreSQL

Uses **per-page lightweight locks** (LWLock) with **wait-free shared lock
acquisition** via atomic compare-exchange. Each backend pins pages locally
and holds shared buffer locks only during tuple visibility checks. Writers
acquire exclusive page locks but never force readers to release. The
per-page granularity means a writer modifying page 42 does not block a
reader scanning page 100.

Key design from `lwlock.c:30-45`: replaced spinlock-protected counters with
a single atomic variable to eliminate contention on frequently-shared locks.

### InnoDB

Uses **per-page RwLocks** (`rw_lock` in `rw_lock.h`) wrapped in
**mini-transactions** (MTR). Readers increment a shared counter atomically;
writers set a sentinel bit (`WRITER = 1U << 31`). The shared counter allows
wait-free reader acquisition when no writer holds the lock. Mini-transactions
scope lock lifetime to individual page accesses, not entire queries.

### DuckDB

Uses **no per-page locks**. Correctness handled entirely by MVCC timestamps
and undo buffers. Writers store old versions; readers access old versions
via `start_time` comparison. Only table-level locks exist for checkpoints
and vacuum. The transaction manager uses a `mutex` for `begin`/`commit` but
readers need no coordination at all.

### AxiomDB-first decision

AxiomDB should follow a **DuckDB-inspired architecture** with one
AxiomDB-specific optimization: because `MmapStorage::read_page(&self)` is
already lock-free (pure pointer arithmetic on the mmap), readers don't even
need an undo buffer to see old versions — the mmap and B-Tree CoW already
provide this.

The architecture splits `Database` into:

- **`SharedState`** (`Arc`, no lock): read-only infrastructure accessible by
  any number of concurrent readers without coordination.
- **`WriterState`** (`Mutex`, one writer): mutable infrastructure for WAL,
  heap writes, index maintenance.

Readers create a snapshot from an `AtomicU64` (max_committed), read pages
via mmap (`&self`), and resolve visibility via `RowHeader::is_visible()`.
No lock of any kind is ever taken by a reader.

This is strictly better than PostgreSQL/InnoDB (no per-page locks needed)
and equivalent to DuckDB's MVCC-only model, but simpler because AxiomDB's
B-Tree CoW eliminates the need for undo buffers on index pages.

## What to build (not how)

Split the server's `Database` into shared (read) and exclusive (write)
components so that:

- **Read-only operations** (SELECT, SHOW, system variable queries) execute
  without acquiring any lock. They read `max_committed` from an atomic,
  create a snapshot, and scan pages via `&self` on `MmapStorage`.

- **Write operations** (INSERT, UPDATE, DELETE, DDL, BEGIN/COMMIT/ROLLBACK)
  acquire an exclusive `Mutex` on the writer state. Only one writer at a time.

- **Readers never wait for writers.** A SELECT that starts before an INSERT
  completes sees the pre-INSERT snapshot. A SELECT that starts after the
  INSERT commits sees the post-INSERT data.

- **Writers wait only for the writer mutex**, not for readers. Because mmap
  pages are CoW and MVCC visibility is snapshot-based, a writer can modify
  pages while readers are still scanning them — the readers see the old
  page versions via their snapshot.

The SQL executor gains a read-only entry point that takes `&dyn StorageEngine`
(shared ref) instead of `&mut dyn StorageEngine`. The existing write path
remains unchanged.

## Inputs / Outputs

- Input: concurrent MySQL client connections issuing SELECT and DML queries
- Output:
  - Multiple SELECT queries execute simultaneously with zero coordination
  - SELECT never blocks INSERT/UPDATE/DELETE
  - INSERT/UPDATE/DELETE never blocks SELECT
  - INSERT/UPDATE/DELETE blocks other writes (single-writer via Mutex)
  - Snapshot consistency: each query sees a consistent point-in-time view
- Errors:
  - No new error types

## Use cases

1. Lock-free concurrent reads:
   - 16 connections each run `SELECT * FROM users` simultaneously
   - All 16 execute without any lock, using the same mmap pages
   - Throughput scales linearly with connection count

2. Reader unblocked by writer:
   - Connection A: starts slow `SELECT * FROM large_table` (takes 500ms)
   - Connection B: `INSERT INTO large_table VALUES (...)` (takes 1ms)
   - B acquires writer Mutex, inserts, commits, releases Mutex — **without
     waiting for A** to finish its SELECT
   - A sees pre-insert data (its snapshot was taken before the insert)

3. Writer unblocked by reader:
   - Connection A: starts slow `SELECT * FROM large_table`
   - Connection B: `INSERT INTO other_table VALUES (...)`
   - B does NOT wait for A — there is no shared lock to contend

4. Autocommit write path:
   - `INSERT INTO t VALUES (1)` → acquire writer Mutex → begin → insert →
     commit → advance `max_committed` atomic → release Mutex
   - Between statements, readers can create snapshots freely

5. Explicit transaction:
   - `BEGIN` → acquire writer Mutex → begin txn → release Mutex
   - `SELECT ...` → read lock-free (uses active_snapshot)
   - `INSERT ...` → acquire writer Mutex → insert → release Mutex
   - `COMMIT` → acquire writer Mutex → commit → release Mutex
   - Each statement in the txn acquires/releases the Mutex independently

## Acceptance criteria

- [ ] `Database` split into `SharedState` (Arc, no lock) + `WriterState` (Mutex).
- [ ] `max_committed` is `AtomicU64` in `SharedState`, readable without lock.
- [ ] `MmapStorage` accessible via `Arc` in `SharedState` for concurrent reads.
- [ ] Read-only executor path takes `&dyn StorageEngine` (shared ref).
- [ ] Multiple concurrent SELECT queries execute without any lock (verified by test).
- [ ] SELECT does not block INSERT and INSERT does not block SELECT (verified by test).
- [ ] Writer Mutex only contended between writers, never by readers.
- [ ] `schema_version` remains `Arc<AtomicU64>` (already lock-free).
- [ ] Explicit transactions acquire writer Mutex per-statement, not for entire txn.
- [ ] Existing tests pass without regression.

## Out of scope

- Per-page locking (PostgreSQL/InnoDB model — unnecessary with mmap CoW)
- Multi-writer concurrency (Phase 7.5 refines writer serialization)
- `MemoryStorage` thread safety (test-only; real server uses MmapStorage)
- Row-level locking (`SELECT FOR UPDATE` — Phase 13)
- Undo buffers for index pages (B-Tree CoW already provides this)

## Dependencies

- Phase 7.1: MVCC visibility rules (implemented)
- Phase 2.6: B-Tree Copy-on-Write (implemented)
- Phase 3.4: `RowHeader::is_visible()` (implemented)

## ⚠️ DEFERRED

- True per-statement writer Mutex release inside explicit transactions requires
  careful handling of `ActiveTxn` state ownership. Initial implementation may
  hold writer Mutex for the entire explicit transaction duration, upgrading to
  per-statement release in a follow-up.
- Reader starvation of writers: unlikely with Tokio Mutex (FIFO fairness) but
  not formally prevented.
