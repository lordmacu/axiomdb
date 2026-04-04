# Spec: 40.10 — Database Lock Redesign

## What to build (not how)

Replace `Arc<RwLock<Database>>` with `Arc<SharedDatabase>` — a structure where
each subsystem has its own independent synchronization. No global lock serializing
all writes. This is where subfases 40.1-40.9 converge into a production architecture.

## Research findings

### InnoDB/MySQL: No single global lock
- **Global singletons** (`buf_pool`, `lock_sys`, `log_sys`, `trx_sys`) are separate
  objects, each with their own internal latches
- **Per-connection `THD`** holds: session variables, security context, temp tables,
  `ha_data[]` with InnoDB's `trx_t` per-connection transaction
- **No centralized `RwLock<Database>`** — each query accesses subsystems independently
- **DDL serialized** via metadata lock (MDL) — separate from row-level data locks
- Connection handler calls `ha_innobase::write_row()` which accesses `buf_pool` (global),
  `lock_sys` (global), `log_sys` (global) — each with its own fine-grained latch

### PostgreSQL: Process model with shared memory
- **`ProcGlobal`** in shared memory: global array of `PGPROC` per backend
- **Per-backend**: `MyProc` (PGPROC pointer), `MyDatabaseId`, `MyRoleId`, local memory
- **Shared structures**: buffer pool (with per-buffer locks), WAL (with insertion locks),
  lock table (with per-partition locks), catalog caches (with invalidation)
- **NO single lock**: each backend accesses shared structures via their own fine-grained
  synchronization. `LWLock` (lightweight lock) used for brief critical sections.
- **DDL**: `AccessExclusiveLock` on relation — blocks all DML on that table (not on others)

### Current AxiomDB bottleneck
```rust
// handler.rs: ALL writes go through this single exclusive lock
let mut guard = db.write().await;  // blocks ALL other connections
guard.execute_query(sql, ...)?;
```

**Impact**: 10 clients doing INSERT → 10× latency, 1× throughput. This is the
fundamental blocker that Phase 40 eliminates.

## Current Database struct (what it holds today)

```rust
pub struct Database {
    pub storage: MmapStorage,              // page I/O
    pub txn: TxnManager,                   // transactions + WAL
    pub bloom: BloomRegistry,              // bloom filters for indexes
    pub pipeline: FsyncPipeline,           // WAL group commit
    pub schema_version: Arc<AtomicU64>,    // already atomic ✓
    pub status: Arc<StatusRegistry>,       // already atomic ✓
    pub runtime_mode: Arc<AtomicU8>,       // already atomic ✓
    pub snapshot_registry: Arc<SnapshotRegistry>,  // already shared ✓
}
```

**Problem fields** (require exclusive `&mut self`):
- `storage: MmapStorage` → becomes `Arc<MmapStorage>` with interior mutability (40.3)
- `txn: TxnManager` → becomes `Arc<TxnCoordinator>` with per-connection txn (40.1+40.2)
- `bloom: BloomRegistry` → wrap in `Arc<RwLock<BloomRegistry>>`
- `pipeline: FsyncPipeline` → integrated into ConcurrentWalWriter (40.4)

## New architecture

### SharedDatabase (global, one per server)

```rust
/// Shared database state — each subsystem independently synchronized.
/// No global RwLock. Multiple connections access subsystems concurrently.
pub struct SharedDatabase {
    // ── Storage (interior mutability from 40.3) ──
    pub storage: Arc<MmapStorage>,

    // ── Transaction coordinator (atomic from 40.1) ──
    pub txn_coord: Arc<TxnCoordinator>,

    // ── WAL (concurrent from 40.4) ──
    pub wal: Arc<ConcurrentWalWriter>,

    // ── Lock manager (row-level from 40.5) ──
    pub lock_mgr: Arc<LockManager>,

    // ── Page allocator (thread-safe from 40.9) ──
    pub allocator: Arc<GlobalPageAllocator>,

    // ── Catalog (DDL serialized, DML concurrent) ──
    pub catalog_lock: Arc<tokio::sync::RwLock<()>>,
    // Read mode: DML (multiple concurrent). Write mode: DDL (exclusive).

    // ── Bloom filters (lightweight lock) ──
    pub bloom: Arc<RwLock<BloomRegistry>>,

    // ── Already-atomic metadata ──
    pub schema_version: Arc<AtomicU64>,
    pub status: Arc<StatusRegistry>,
    pub runtime_mode: Arc<AtomicU8>,
    pub snapshot_registry: Arc<SnapshotRegistry>,
}
```

### ConnectionState (per connection)

```rust
/// Per-connection state — owned by each connection handler task.
/// Not shared between connections.
pub struct ConnectionState {
    /// Reference to shared database (immutable Arc, no lock).
    pub shared: Arc<SharedDatabase>,

    /// Per-connection session (variables, caches, stats).
    pub session: SessionContext,

    /// Active transaction (from 40.2, None when in autocommit idle).
    pub active_txn: Option<ConnectionTxn>,

    /// Per-transaction page allocation batch (from 40.9).
    pub page_batch: LocalPageBatch,

    /// Schema cache (per-connection, invalidated on DDL).
    pub schema_cache: SchemaCache,
}
```

### Connection handler (no global lock)

```rust
pub async fn handle_connection(
    shared: Arc<SharedDatabase>,
    stream: TcpStream,
) {
    let mut conn = ConnectionState::new(Arc::clone(&shared));

    loop {
        let query = read_query(&mut stream).await?;

        // NO GLOBAL LOCK. Each query accesses subsystems independently.
        let result = execute_connection_query(&mut conn, &query).await?;

        send_result(&mut stream, &result).await?;
    }
}
```

### Query execution (no global lock)

```rust
fn execute_connection_query(
    conn: &mut ConnectionState,
    sql: &str,
) -> Result<QueryResult, DbError> {
    let shared = &conn.shared;

    // 1. Parse (pure, no lock)
    let stmt = parse(sql)?;

    // 2. Analyze (reads catalog — shared lock if DDL-sensitive)
    let analyzed = if is_ddl(&stmt) {
        let _ddl_guard = shared.catalog_lock.write().await;
        analyze_and_execute_ddl(shared, conn, stmt)?
    } else {
        // DML: acquire catalog READ lock (compatible with other DML)
        let _cat_guard = shared.catalog_lock.read().await;
        analyze_and_execute_dml(shared, conn, stmt)?
    };

    Ok(analyzed)
}
```

### DML execution (concurrent)

```rust
fn analyze_and_execute_dml(
    shared: &SharedDatabase,
    conn: &mut ConnectionState,
    stmt: Stmt,
) -> Result<QueryResult, DbError> {
    // 1. Snapshot (lock-free atomic load from TxnCoordinator)
    let snap = shared.txn_coord.snapshot();

    // 2. Autocommit wrapper (or use existing explicit txn)
    let txn = conn.active_txn.get_or_insert_with(|| {
        shared.txn_coord.begin()
    });

    // 3. Execute DML
    //    - Storage access: shared.storage (interior mutability, page locks)
    //    - Row locks: shared.lock_mgr.acquire(...)
    //    - WAL: shared.wal.submit_entry(...)
    //    - Page alloc: conn.page_batch + shared.allocator
    let result = executor::execute_with_ctx(
        &*shared.storage,       // &dyn StorageEngine (&self)
        txn,                    // &mut ConnectionTxn
        &shared.lock_mgr,      // &LockManager
        &shared.wal,            // &ConcurrentWalWriter
        &mut conn.page_batch,   // &mut LocalPageBatch
        &shared.bloom.read(),   // &BloomRegistry
        &mut conn.session,       // &mut SessionContext
        stmt,
    )?;

    // 4. Commit if autocommit
    if conn.session.autocommit && conn.active_txn.is_some() {
        let txn = conn.active_txn.take().unwrap();
        shared.txn_coord.commit(txn, &shared.wal)?;
        commit_page_batch(&mut conn.page_batch, &shared.allocator);
    }

    Ok(result)
}
```

### DDL execution (serialized)

```rust
fn analyze_and_execute_ddl(
    shared: &SharedDatabase,
    conn: &mut ConnectionState,
    stmt: Stmt,
) -> Result<QueryResult, DbError> {
    // catalog_lock.write() already held by caller
    // This blocks ALL DML (they hold catalog read lock)
    // But DDL is rare — acceptable serialization

    // Execute DDL (CREATE TABLE, DROP TABLE, etc.)
    let result = executor::execute_ddl(
        &*shared.storage,
        &mut conn.session,
        stmt,
    )?;

    // Bump schema version (atomic) — invalidates all session caches
    shared.schema_version.fetch_add(1, Ordering::Release);

    Ok(result)
}
```

## Concurrency matrix (final state)

| Operation A | Operation B | Behavior | Mechanism |
|---|---|---|---|
| SELECT | SELECT | **Parallel** | Both read-only, MVCC snapshots |
| SELECT | INSERT | **Parallel** | MVCC isolation, different page locks |
| SELECT | UPDATE (diff row) | **Parallel** | MVCC isolation |
| SELECT | UPDATE (same row) | **Parallel** | Reader sees old version via MVCC |
| INSERT | INSERT (diff page) | **Parallel** | Different page X-latches |
| INSERT | INSERT (same page) | **Brief serial** | Same page X-latch (~1µs) |
| UPDATE | UPDATE (diff row) | **Parallel** | Different row X-locks |
| UPDATE | UPDATE (same row) | **Serialized** | Row X-lock, waiter queued |
| DELETE | SELECT (same row) | **Parallel** | Reader sees pre-delete via MVCC |
| DDL | Any DML | **DDL waits** | catalog_lock write waits for all DML reads |
| DDL | DDL | **Serialized** | catalog_lock write is exclusive |

## Migration path from old to new

### Step 1: Create SharedDatabase struct
- Wrap existing fields in Arc
- Add new subsystems (lock_mgr, allocator, wal)

### Step 2: Create ConnectionState struct
- Move session, schema_cache to per-connection
- Add active_txn, page_batch

### Step 3: Update handler.rs
- Replace `Arc<RwLock<Database>>` with `Arc<SharedDatabase>`
- Replace `db.write().await` with direct subsystem access
- Replace `db.read().await` with direct subsystem access (same path!)

### Step 4: Update Database::open
- Initialize all subsystems with Arc
- Return Arc<SharedDatabase> instead of Database

### Step 5: Remove Database struct
- All functionality moved to SharedDatabase + ConnectionState
- Old Database struct deleted

## Acceptance criteria

- [ ] `Arc<RwLock<Database>>` completely removed from codebase
- [ ] `SharedDatabase` struct with independently-synchronized subsystems
- [ ] `ConnectionState` per-connection with session, txn, page_batch
- [ ] DML queries do NOT acquire any global lock
- [ ] DDL queries acquire `catalog_lock` write (serializes only DDL with DML)
- [ ] SELECT never blocked by INSERT/UPDATE/DELETE
- [ ] Multiple INSERT to different tables: fully parallel
- [ ] Multiple INSERT to same table: parallel (different pages) or brief serial (same page)
- [ ] Autocommit: ConnectionTxn created and committed per statement
- [ ] Explicit txn: ConnectionTxn persists across BEGIN...COMMIT
- [ ] Schema cache invalidated on DDL (schema_version atomic bump)
- [ ] All existing tests adapted to new API
- [ ] Wire protocol smoke test: 2 concurrent connections both INSERT

## Out of scope

- Online DDL (ALTER TABLE without blocking DML) — future phase
- Connection pooling — application-level concern
- Statement-level parallelism (parallel scan within one query) — Phase 9

## Dependencies

- 40.1 (Atomic TxnId) — TxnCoordinator with atomic IDs
- 40.2 (Per-connection txn) — ConnectionTxn struct
- 40.3 (StorageEngine interior mutability) — Arc<MmapStorage> with &self
- 40.4 (Concurrent WAL) — Arc<ConcurrentWalWriter>
- 40.5 (Lock Manager) — Arc<LockManager>
- 40.9 (FreeList thread-safe) — Arc<GlobalPageAllocator>
