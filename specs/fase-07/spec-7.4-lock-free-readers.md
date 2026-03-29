# Spec: Lock-free readers with CoW (Phase 7.4)

## Reviewed first

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-network/src/mysql/database.rs` — `Arc<Mutex<Database>>`, `execute_query`
- `crates/axiomdb-network/src/mysql/handler.rs` — per-connection handler, lock acquisition
- `crates/axiomdb-storage/src/mmap.rs` — `MmapStorage`, `read_page(&self)`, `write_page(&mut self)`
- `crates/axiomdb-storage/src/memory.rs` — `MemoryStorage`
- `crates/axiomdb-storage/src/engine.rs` — `StorageEngine` trait
- `crates/axiomdb-wal/src/txn.rs` — `TxnManager`, `snapshot(&self)`, `begin(&mut self)`
- `crates/axiomdb-sql/src/executor/mod.rs` — `execute_with_ctx`
- `research/postgres/src/backend/storage/lmgr/lwlock.c` — wait-free shared lock via atomic CAS
- `research/mariadb-server/storage/innobase/include/rw_lock.h` — per-page atomic RwLock
- `research/duckdb/src/include/duckdb/transaction/duck_transaction_manager.hpp` — MVCC-only, no per-page locks

## Research synthesis

### PostgreSQL

Uses **per-page lightweight locks** (LWLock) with **wait-free shared lock
acquisition** via atomic compare-exchange. Readers hold shared buffer pins
during visibility checks. Writers acquire exclusive page locks. This provides
fine-grained concurrency but requires complex buffer pool management and
pin tracking per backend.

### InnoDB

Uses **per-page RwLocks** (`rw_lock` in `rw_lock.h`) with atomic counters:
readers increment a shared counter, writers set a sentinel bit. Wrapped in
**mini-transactions** (MTR) that batch page accesses. Similar granularity to
PostgreSQL.

### DuckDB

Uses **no per-page locks at all**. Readers and writers access pages concurrently.
Correctness is guaranteed by MVCC timestamps — readers see old versions via
undo buffers, not via locking. Only table-level locks exist for checkpoints
and vacuum. This is the simplest model and works because DuckDB's MVCC
ensures readers always see a consistent snapshot without coordination.

### AxiomDB-first decision

AxiomDB should follow the **DuckDB model**: MVCC for correctness, minimal
locking for coordination. The reasons:

1. AxiomDB already has DuckDB-style timestamp MVCC (O(1) visibility checks,
   no active txn ID arrays).
2. `MmapStorage.read_page()` is `&self` — mmap is inherently concurrent-read-safe
   via the OS page cache.
3. B-Tree Copy-on-Write means old tree roots remain valid for old snapshots.
4. HeapChain reads use `RowHeader::is_visible()` which is a pure function of
   the snapshot — no locking needed.

The concrete change: replace `Arc<tokio::sync::Mutex<Database>>` with
`Arc<tokio::sync::RwLock<Database>>`. Read-only queries acquire the read lock
(multiple concurrent), write queries acquire the write lock (exclusive).

This avoids the complexity of PostgreSQL/InnoDB per-page locks while
delivering the same result: readers never block writers, multiple readers
can execute concurrently.

## What to build (not how)

Replace the global exclusive mutex on `Database` with a read-write lock so
that:

- **Read-only SQL statements** (SELECT without side effects) execute under a
  **shared read lock**. Multiple readers can proceed concurrently.
- **Write SQL statements** (INSERT, UPDATE, DELETE, DDL) execute under an
  **exclusive write lock**, serializing writes as before.
- **Snapshot creation** (`TxnManager::snapshot()`) works under the read lock
  because it only reads `max_committed`.
- **Transaction state** for explicit transactions is owned by the connection,
  not shared. Each connection's `BEGIN`/`COMMIT`/`ROLLBACK` still acquires
  the write lock for the duration of each statement, then releases it.

The MVCC visibility rules from Phase 7.1 guarantee correctness: readers see a
consistent snapshot regardless of concurrent writes because old row versions
remain accessible until vacuum.

## Inputs / Outputs

- Input: concurrent MySQL client connections issuing SELECT and DML queries
- Output:
  - Multiple SELECT queries execute simultaneously without blocking each other
  - SELECT does not block INSERT/UPDATE/DELETE
  - INSERT/UPDATE/DELETE blocks other writes (single-writer serialization)
  - INSERT/UPDATE/DELETE waits for in-flight readers to finish (write lock
    waits for read lock holders to release)
- Errors:
  - No new error types. Existing transaction and lock errors remain.

## Use cases

1. Two concurrent SELECT queries on the same table:
   - Connection A: `SELECT * FROM users WHERE active = 1`
   - Connection B: `SELECT COUNT(*) FROM users`
   - Both execute simultaneously, neither blocks the other.

2. SELECT during INSERT:
   - Connection A: starts `SELECT * FROM large_table` (slow scan)
   - Connection B: `INSERT INTO large_table VALUES (...)`
   - Connection B waits for A's read lock to release, then inserts.
   - Connection A sees a consistent snapshot (pre-insert data).

3. Multiple readers, one writer:
   - 16 connections reading simultaneously.
   - 1 connection writing.
   - Writer waits for current readers to finish, then writes.
   - New readers queue behind the writer until it finishes.

4. Autocommit writes release lock between statements:
   - Connection A: `INSERT INTO t VALUES (1)` — acquires write lock, inserts,
     commits, releases write lock.
   - Between A's statements, readers can proceed.

## Acceptance criteria

- [ ] `Database` is wrapped in `Arc<tokio::sync::RwLock<Database>>` instead of `Arc<tokio::sync::Mutex<Database>>`.
- [ ] Read-only queries acquire read lock (`RwLock::read()`).
- [ ] Write queries acquire write lock (`RwLock::write()`).
- [ ] Multiple concurrent SELECT queries do not block each other (verified by test).
- [ ] SELECT does not block concurrent INSERT (verified by test).
- [ ] INSERT blocks concurrent INSERT (single-writer preserved).
- [ ] `TxnManager::snapshot()` works correctly under read lock.
- [ ] Existing single-connection tests pass without regression.
- [ ] Wire protocol tests pass without regression.

## Out of scope

- Per-table or per-page locking (PostgreSQL/InnoDB model)
- Lock-free writes (multi-writer — Phase 7.5+ will refine writer serialization)
- Row-level locking (`SELECT FOR UPDATE` — Phase 13)
- Deadlock detection (Phase 7.10)
- Reader starvation prevention (writers waiting indefinitely for readers)

## Dependencies

- Phase 7.1: MVCC visibility rules (already implemented)
- Phase 3.4: `RowHeader::is_visible()` (already implemented)
- Phase 2.6: B-Tree Copy-on-Write (already implemented)

## ⚠️ DEFERRED

- `MemoryStorage` thread safety — currently uses `HashMap`, not safe for concurrent access. Tests use single-threaded execution. Real concurrent tests require `MmapStorage` or a thread-safe memory storage.
- Writer starvation: if readers continuously hold the read lock, a writer may wait indefinitely. Tokio's `RwLock` is writer-preferring by default, which mitigates this.
- Per-connection transaction state isolation: explicit transactions (`BEGIN`/`COMMIT`) currently store state in `TxnManager` which is inside `Database`. For true per-connection transactions, `TxnManager` would need to be per-connection or the active transaction state would need to move out.
