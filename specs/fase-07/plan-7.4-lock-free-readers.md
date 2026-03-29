# Plan: Lock-free readers with CoW (Phase 7.4)

## Files to create/modify

- `crates/axiomdb-network/src/mysql/handler.rs` — change `Arc<Mutex<Database>>` to `Arc<RwLock<Database>>`, split query handling into read vs write paths
- `crates/axiomdb-network/src/mysql/database.rs` — add `execute_read_query()` that takes `&self` (read lock) alongside existing `execute_query()` that takes `&mut self` (write lock); add `is_read_only_stmt()` classifier
- `crates/axiomdb-network/src/mysql/mod.rs` — update server startup to use `RwLock`
- `crates/axiomdb-sql/src/executor/mod.rs` — add `execute_read_only()` entry point that takes `&dyn StorageEngine` + `&TxnManager` (shared refs only)
- `crates/axiomdb-network/tests/integration_connection_lifecycle.rs` — add concurrent reader test
- `tools/wire-test.py` — smoke test for concurrent SELECTs

## Algorithm / Data structure

Statement classification:

```text
is_read_only(stmt) → bool:
    SELECT without subqueries that write → true
    SHOW TABLES / SHOW COLUMNS / SHOW DATABASES / SHOW STATUS → true
    SELECT @@variable → true (intercepted before executor)
    Everything else → false
```

Handler flow:

```text
on COM_QUERY(sql):
    parse(sql)
    if is_read_only(parsed):
        guard = db.read().await        // shared lock — concurrent
        snapshot = guard.txn.snapshot() // &self — read only
        result = execute_read(parsed, &guard.storage, snapshot)
        drop(guard)                    // release read lock
    else:
        guard = db.write().await       // exclusive lock — serialized
        result = guard.execute_query(sql, session, cache)
        drop(guard)                    // release write lock
    send result to client
```

Read path (new):

```text
Database::execute_read_query(&self, sql, session):
    // &self — multiple callers can hold this simultaneously
    parse → analyze → execute_read_only(stmt, &self.storage, self.txn.snapshot())
```

Write path (existing, unchanged):

```text
Database::execute_query(&mut self, sql, session):
    // &mut self — exclusive
    parse → analyze → execute_with_ctx(stmt, &mut storage, &mut txn, ...)
```

Key insight: `MmapStorage::read_page(&self)` already takes `&self`.
`TxnManager::snapshot(&self)` already takes `&self`. So the read path
only needs shared references.

## Implementation phases

1. Add `is_read_only_sql()` classifier in `database.rs` that checks the parsed
   statement type. Conservative: only SELECT without explicit txn, SHOW, and
   pure-read statements qualify.

2. Add `execute_read_query(&self, ...)` to `Database` that creates a snapshot
   and calls the analyzer + executor with `&self.storage` and the snapshot.
   This uses the existing `execute()` (non-ctx) path which already takes
   `&mut dyn StorageEngine` — but `MmapStorage` implements
   `StorageEngine` with `read_page(&self)`, so this needs a read-only
   storage wrapper or the trait needs a `&self` read path.

3. Change `Arc<Mutex<Database>>` to `Arc<RwLock<Database>>` in the handler.
   Read-only queries call `db.read().await`, write queries call `db.write().await`.

4. Handle edge case: connections with active explicit transactions (`BEGIN`)
   must always use the write lock (they may write at any point).

5. Add concurrent reader integration test: spawn N tokio tasks, each doing
   SELECT via pymysql or raw TCP, verify they all complete without blocking.

## Tests to write

- unit: `is_read_only_sql()` classifier for SELECT, SHOW, INSERT, UPDATE, DELETE, BEGIN
- integration: two concurrent SELECT queries complete without deadlock
- integration: SELECT during INSERT — INSERT waits, SELECT proceeds
- integration: existing single-connection tests pass (no regression)
- wire: pymysql concurrent SELECT test in wire-test.py

## Anti-patterns to avoid

- Do NOT make `StorageEngine::write_page` take `&self` — keep the `&mut self` requirement to prevent accidental writes during read path.
- Do NOT hold the write lock for the entire duration of an explicit transaction — acquire per-statement and release between statements (autocommit-like behavior for explicit txns too, with txn state preserved in session).
- Do NOT classify INSERT...SELECT as read-only even though it starts with a read.
- Do NOT allow concurrent writers — single-writer serialization is intentional until Phase 7.5.

## Risks

- `StorageEngine` trait requires `&mut self` for ALL methods including `read_page` in some implementations → mitigate by checking if `MmapStorage::read_page` can work with `&self` (it should, since mmap reads are thread-safe).
- `MemoryStorage` is not thread-safe for concurrent read/write → mitigate by noting this is a test-only storage; real server uses `MmapStorage`.
- Explicit transactions that start with SELECT then do INSERT would need to upgrade from read to write lock → mitigate by always using write lock when `in_explicit_txn` is true.
- Tokio's `RwLock` has different fairness guarantees than std `RwLock` → mitigate by using tokio's RwLock which is write-preferring (prevents writer starvation).
