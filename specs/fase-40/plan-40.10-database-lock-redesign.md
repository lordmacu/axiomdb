# Plan: 40.10 — Database Lock Redesign

## Files to create/modify

| File | Change |
|---|---|
| `crates/axiomdb-network/src/mysql/shared_db.rs` | **NEW**: SharedDatabase struct |
| `crates/axiomdb-network/src/mysql/connection.rs` | **NEW**: ConnectionState struct |
| `crates/axiomdb-network/src/mysql/database.rs` | Refactor: Database::open returns Arc<SharedDatabase> |
| `crates/axiomdb-network/src/mysql/handler.rs` | Replace Arc<RwLock<Database>> with Arc<SharedDatabase> |
| `crates/axiomdb-server/src/main.rs` | Update server startup to create SharedDatabase |
| `crates/axiomdb-network/src/mysql/mod.rs` | Export new modules |
| `crates/axiomdb-network/tests/*.rs` | Adapt all integration tests |

## Implementation phases

### Phase 1: SharedDatabase struct (new file)
- Define SharedDatabase with all Arc-wrapped subsystems
- `SharedDatabase::open(path, config) -> Result<Arc<Self>>` factory method
- Initialize: storage (40.3), txn_coord (40.1), wal (40.4), lock_mgr (40.5),
  allocator (40.9), catalog_lock, bloom, status, schema_version
- All fields are `Arc<T>` — SharedDatabase itself is `Send + Sync`

### Phase 2: ConnectionState struct (new file)
- Per-connection: `shared: Arc<SharedDatabase>`, `session: SessionContext`,
  `active_txn: Option<ConnectionTxn>`, `page_batch: LocalPageBatch`,
  `schema_cache: SchemaCache`
- `ConnectionState::new(shared: Arc<SharedDatabase>) -> Self`
- Methods: `begin_txn()`, `commit_txn()`, `rollback_txn()`, `autocommit()`

### Phase 3: Handler refactoring
Replace the write/read dispatch:

```rust
// BEFORE (handler.rs):
if is_read_only {
    let guard = db.read().await;      // shared lock
    guard.execute_read_query(...)
} else {
    let mut guard = db.write().await;  // EXCLUSIVE lock
    guard.execute_query(...)
}

// AFTER:
// No lock acquisition at all!
conn.execute_query(sql)?;
// ConnectionState::execute_query calls shared subsystems directly
```

### Phase 4: Catalog lock for DDL
- `SharedDatabase.catalog_lock: Arc<tokio::sync::RwLock<()>>`
- DML acquires `.read()` (compatible with other DML)
- DDL acquires `.write()` (exclusive — blocks DML)
- Only DDL statements (CREATE/DROP/ALTER TABLE, CREATE/DROP INDEX) acquire write
- All other statements acquire read (briefly, just during catalog resolution)

### Phase 5: Autocommit integration
```rust
impl ConnectionState {
    fn autocommit<F, T>(&mut self, f: F) -> Result<T>
    where F: FnOnce(&mut Self) -> Result<T>
    {
        let txn = self.shared.txn_coord.begin()?;
        self.active_txn = Some(txn);
        match f(self) {
            Ok(result) => {
                let txn = self.active_txn.take().unwrap();
                self.shared.txn_coord.commit(txn, &self.shared.wal)?;
                commit_page_batch(&mut self.page_batch, &self.shared.allocator);
                Ok(result)
            }
            Err(e) => {
                let txn = self.active_txn.take().unwrap();
                self.shared.txn_coord.rollback(txn, &*self.shared.storage)?;
                rollback_page_batch(&mut self.page_batch, &self.shared.allocator);
                Err(e)
            }
        }
    }
}
```

### Phase 6: Deferred commit / fsync pipeline integration
- `take_commit_rx()` logic moves to ConnectionState
- Uses shared ConcurrentWalWriter (40.4) for group commit
- FsyncPipeline integrated into ConcurrentWalWriter (no separate struct)

### Phase 7: Remove old Database struct
- All functionality absorbed by SharedDatabase + ConnectionState
- Delete `Database` struct
- Update all references

### Phase 8: Integration tests
- 2 connections: both INSERT concurrently → both succeed
- 2 connections: SELECT during INSERT → SELECT sees consistent MVCC snapshot
- DDL during DML: CREATE TABLE blocks until ongoing INSERTs finish
- Autocommit: each statement is independent transaction
- Explicit txn: BEGIN → INSERT → INSERT → COMMIT across one connection
- Schema cache: DDL bumps version → other connections invalidate cache

## Tests to write

1. **SharedDatabase::open**: opens database, all subsystems initialized
2. **ConnectionState::new**: creates per-connection state with shared reference
3. **Concurrent INSERT (2 connections)**: both insert into same table → both succeed
4. **Concurrent INSERT + SELECT**: reader gets MVCC-consistent snapshot
5. **DDL serialization**: CREATE TABLE waits for active DML transactions
6. **Autocommit**: insert → commit → insert → commit (per-statement)
7. **Explicit transaction**: BEGIN → 3 inserts → COMMIT → all visible
8. **Rollback**: BEGIN → insert → ROLLBACK → data not visible
9. **Schema cache invalidation**: DDL bumps version → DML re-reads catalog
10. **Wire protocol**: 2 pymysql connections × concurrent INSERT → no errors
11. **Stress**: 8 tokio tasks × mixed DML → no corruption, no deadlock

## Anti-patterns to avoid

- DO NOT keep any `Arc<RwLock<Database>>` — the entire point is removing it
- DO NOT hold catalog_lock.read() across entire DML execution — acquire briefly
  for catalog resolution, release before page access
- DO NOT put ConnectionTxn in SharedDatabase — it's per-connection state
- DO NOT use `Mutex<Database>` as a "simpler alternative" — same bottleneck
- DO NOT skip MemoryStorage tests — integration tests use in-memory storage

## Risks

- **Largest API change**: handler.rs, all integration tests, server main.rs all change.
  Mitigation: do it after all subsystems (40.1-40.9) are in place.
- **Catalog lock granularity**: catalog_lock covers ALL tables. If DDL on table A blocks
  DML on table B, that's unnecessary. Mitigation: acceptable for first version. Per-table
  catalog locks can be added later.
- **Tokio runtime**: handler uses async. catalog_lock must be tokio::sync::RwLock (not std).
  Mutex-based subsystems should use `parking_lot::Mutex` (sync, not async).
  DON'T mix tokio::sync::Mutex with parking_lot::Mutex carelessly.
