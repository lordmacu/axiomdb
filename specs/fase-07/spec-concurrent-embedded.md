# Spec: concurrent-embedded — SharedDb + Connection types

Phase: 7 (deferred) — Concurrent embedded connections
Task: SharedDb + Connection (Task 1 of sprint concurrent-embedded)
Status: approved

## Context

`axiomdb-embedded` currently exposes a single-connection `Db` struct that wraps
all engine state in one object. `AsyncDb` wraps it in `Arc<Mutex<Db>>`, serializing
every operation — reads and writes alike — through a global lock. This is worse than
SQLite in WAL mode, which allows N concurrent readers with one writer.

The wire server (`axiomdb-network`) already solved this with `SharedDatabase`:
`MmapStorage`, `TxnManager`, and `BloomRegistry` are all `Send + Sync` and use
interior mutability. Per-connection state (`SessionContext`, `SchemaCache`) lives
in each connection handler independently.

This task brings the same model to the embedded API by adding `SharedDb` and
`Connection` types without touching `Db` (backward compat preserved).

## Goal

Add `SharedDb` (cloneable shared engine handle) and `Connection` (per-connection
execution context) to `axiomdb-embedded` so that multiple connections can execute
read queries simultaneously without any global lock.

## Non-goals

- Not adding fsync pipeline or group commit (embedded uses synchronous commits)
- Not adding `SHOW PROCESSLIST` / connection registry (wire-server concern)
- Not adding row-level lock manager (out of scope, Phase 40.11)
- Not adding `SnapshotRegistry` / epoch-based page reclamation (Phase 7.8)
- Not changing `Db` at all — backward compat is absolute
- Not changing `AsyncDb` — deprecated implicitly but not removed
- Not adding connection pooling — `SharedDb::connect()` creates one connection

## Behavior

### Public API

```rust
// --- axiomdb_embedded public additions ---

/// Shared database engine handle.
///
/// Wraps `MmapStorage`, `TxnManager`, and `BloomRegistry` in an `Arc` so
/// multiple `Connection`s can share the same engine without any outer lock.
/// Clone is cheap (Arc refcount bump).
///
/// ## Concurrency
/// - N concurrent read queries: fully parallel (no global lock).
/// - DDL serializes against concurrent DML via an internal `catalog_lock`.
/// - Two DML writers on different rows: parallel (per-page page locks in storage).
/// - Two DML writers on the same row: serialized by the page lock.
#[derive(Clone)]
pub struct SharedDb {
    inner: Arc<SharedDbInner>,
}

impl SharedDb {
    /// Opens or creates a database at `path`.
    /// Creates the file and initializes the catalog if it does not exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError>;

    /// Opens or creates an ephemeral in-memory database.
    /// Data is discarded when the last `SharedDb` clone is dropped.
    pub fn open_memory() -> Result<Self, DbError>;

    /// Opens a database from a DSN string (`file:path` or `:memory:`).
    pub fn open_dsn(dsn: impl Into<String>) -> Result<Self, DbError>;

    /// Creates a new connection to this database.
    /// Each `Connection` has independent session state and schema cache.
    /// Cheap — no I/O, no locking.
    pub fn connect(&self) -> Connection;
}

/// Per-connection execution context.
///
/// Holds per-connection state (`SessionContext`, `SchemaCache`) and a shared
/// reference to the engine (`Arc<SharedDbInner>`). Not `Clone` — each call
/// to `SharedDb::connect()` produces an independent connection.
///
/// `Connection` is `Send` — it can be moved to a different thread.
pub struct Connection {
    inner: Arc<SharedDbInner>,
    session: SessionContext,
    schema_cache: SchemaCache,
    schema_version_seen: u64,
}

impl Connection {
    /// Executes a SQL DML/DDL statement. Returns rows affected.
    pub fn execute(&mut self, sql: &str) -> Result<u64, DbError>;

    /// Executes a SQL SELECT. Returns rows as `Vec<Row>`.
    pub fn query(&mut self, sql: &str) -> Result<Vec<Row>, DbError>;

    /// Executes a SQL SELECT. Returns column names and rows.
    pub fn query_with_columns(
        &mut self,
        sql: &str,
    ) -> Result<(Vec<String>, Vec<Row>), DbError>;

    /// Begins an explicit transaction. Subsequent execute/query calls are
    /// part of this transaction until commit() or rollback().
    pub fn begin(&mut self) -> Result<(), DbError>;

    /// Commits the active explicit transaction.
    pub fn commit(&mut self) -> Result<(), DbError>;

    /// Rolls back the active explicit transaction.
    pub fn rollback(&mut self) -> Result<(), DbError>;

    /// Returns `true` if inside an explicit transaction.
    pub fn in_transaction(&self) -> bool;
}
```

### Internal structure

```rust
struct SharedDbInner {
    storage: MmapStorage,
    txn: TxnManager,
    bloom: BloomRegistry,
    /// Monotonic counter incremented after every successful DDL.
    /// Connections compare their local `schema_version_seen` to detect
    /// stale `SchemaCache` and call `invalidate()` before the next statement.
    schema_version: AtomicU64,
    /// Serializes DDL against concurrent DML (tokio-free: std RwLock).
    /// DDL takes write lock; DML and SELECTs take read lock only for
    /// schema resolution, not for execution.
    catalog_lock: std::sync::RwLock<()>,
    /// Temp dir for `:memory:` databases. Kept alive until all clones drop.
    _tmpdir: Option<tempfile::TempDir>,
}
```

### Semantics

**`SharedDb::connect()`**
- Precondition: `SharedDb` is open (not degraded).
- Postcondition: returns a `Connection` with a fresh `SessionContext::default()` and `SchemaCache::default()`.
- Invariant: all connections share the same `storage`, `txn`, `bloom`, `schema_version`.
- Cost: one `Arc::clone()`. No I/O, no lock.

**`Connection::query()`**
- Precondition: none (works autocommit or in explicit txn).
- Execution path: mirrors `SharedDatabase::execute_read_query` from the wire server.
  - Checks `schema_version` against `schema_version_seen`; calls `schema_cache.invalidate()` if stale.
  - Gets snapshot from `txn` (no lock — `AtomicU64::load`).
  - Calls `run_cached(sql, &storage, &txn, &bloom, &mut schema_cache, &mut session, read_only=true)`.
  - No catalog lock held during execution.
- Postcondition: returns rows visible as of a committed snapshot at call time.
- Concurrency: multiple `Connection::query()` calls proceed simultaneously — no global lock.

**`Connection::execute()`**
- Execution path: mirrors `SharedDatabase::execute_query`.
  - DDL: takes `catalog_lock` write guard for the duration of schema mutation.
  - DML: no catalog lock. Per-page locks in `MmapStorage` handle concurrent writers.
  - On success DDL: increments `schema_version` (via `fetch_add(1, Release)`).
- Postcondition: returns rows-affected count (0 for DDL).

**Schema cache invalidation**
- `schema_version_seen` is updated to `schema_version.load(Acquire)` after each statement.
- If `schema_version_seen < schema_version` at the start of a statement, call `schema_cache.invalidate()` before analyze. This ensures stale plans are never used after DDL on another connection.

**Autocommit**
- Same as `Db`: each `execute()`/`query()` is an implicit `BEGIN…COMMIT` unless inside an explicit transaction.

**Explicit transactions**
- `begin()` → `txn.begin()` → stores `ConnectionTxn` in `session.conn_txn`.
- `commit()` → `txn.commit(conn_txn)` → flushes WAL synchronously.
- `rollback()` → `txn.rollback(conn_txn, &storage)`.

### Error cases

| Condition | Expected error | Notes |
|---|---|---|
| `begin()` while already in txn | `DbError::Other("already in transaction")` | |
| `commit()` with no active txn | `DbError::Other("no active transaction")` | |
| `rollback()` with no active txn | `DbError::Other("no active transaction")` | |
| SQL parse error | `DbError::ParseError { .. }` | same as `Db` |
| Table not found | `DbError::TableNotFound { .. }` | same as `Db` |
| Disk full on write | `DbError::DiskFull { .. }` | no degraded mode (embedded stays usable for reads) |

## Edge cases

- [x] Two connections reading simultaneously — must not deadlock or serialize
- [x] One connection writing while another reads — reader sees pre-write snapshot (MVCC)
- [x] DDL on connection A invalidates SchemaCache on connection B — detected via `schema_version`
- [x] `SharedDb` dropped while `Connection` is alive — `Arc` keeps engine alive
- [x] `:memory:` database — `_tmpdir` kept alive by `Arc` until last clone drops
- [x] Explicit txn left open on `Connection` drop — rolled back in `Drop` impl
- [x] `connect()` on a `:memory:` `SharedDb` — same `Arc<SharedDbInner>`, same in-memory store

## Performance budget

| Operation | Target | Notes |
|---|---|---|
| `SharedDb::connect()` | < 1 µs | `Arc::clone()` only |
| `Connection::query()` (cached, no lock) | same as current `Db::query()` | no regression |
| N concurrent readers throughput | linear with N (up to CPU cores) | no shared mutex |

## Dependencies

- Depends on: `axiomdb-sql::run_cached`, `axiomdb-sql::execute_with_ctx_locked`, `axiomdb-sql::SchemaCache`, `axiomdb-sql::SessionContext`
- Depends on: `axiomdb-wal::TxnManager`, `axiomdb-storage::MmapStorage`, `axiomdb-sql::BloomRegistry`
- Blocks: Task 2 (AsyncSharedDb), Task 3 (concurrency tests), Task 4 (close)

## Open questions

*(all resolved)*

- [x] Is `BloomRegistry` `Send+Sync`? → Yes, uses `RwLock<HashMap>` internally.
- [x] Is `MmapStorage` `Send+Sync`? → Yes, `Send+Sync` via interior mutability since Phase 40.3.
- [x] Does `SchemaCache` need sharing? → No, per-connection. Invalidated via `schema_version`.
- [x] Does embedded need the fsync pipeline? → No, synchronous commits are fine for embedded.
- [x] What phase number? → Deferred Phase 7 feature, spec lives in `specs/fase-07/`.

## Done criteria

- [ ] `SharedDb` and `Connection` compile with `cargo build -p axiomdb-embedded`
- [ ] `SharedDb: Clone + Send + Sync`
- [ ] `Connection: Send` (not `Clone`)
- [ ] All public items have rustdoc
- [ ] `Connection` `Drop` impl rolls back any open explicit transaction
- [ ] Schema cache invalidation works across connections (DDL on conn A, SELECT on conn B sees new schema)
- [ ] `cargo nextest run -p axiomdb-embedded` passes (including existing `Db` tests)
- [ ] `cargo clippy -p axiomdb-embedded -- -D warnings` clean

## References

- Wire server pattern: `crates/axiomdb-network/src/mysql/shared_db.rs`
- Storage concurrency design: `crates/axiomdb-storage/src/engine.rs` (Phase 40.3 notes)
- BloomRegistry concurrency: `crates/axiomdb-sql/src/bloom.rs` (Phase 40.10 notes)
- TxnManager concurrency: `crates/axiomdb-wal/src/txn.rs` (lines 255-284)
- Original deferred note: `crates/axiomdb-embedded/src/lib.rs:100` "(future: Phase 7)"
