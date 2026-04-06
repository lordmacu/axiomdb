# Spec: 40.4b — Per-Connection Transaction State

## What to build (not how)

Move the active transaction state out of the global `TxnManager` into per-connection
ownership. Each connection gets its own `ConnectionTxn` that holds its undo log,
snapshot, isolation level, WAL scratch buffer, and savepoints. The `TxnManager` becomes
a lightweight coordinator that only tracks: atomic `max_committed`, the active-transaction
set for snapshot visibility, the WAL writer, and shared config.

Simultaneously introduce `ExecutionContext<'a>` to bundle all shared read-only subsystems.
This is the **one and only** executor signature sweep — in 40.11 only a new field is added
to `ExecutionContext`; no further signature changes are needed.

> **Status of prerequisites:**
> - 40.3 (StorageEngine `&self` interior mutability) — ✅ DONE
> - 40.4 (ConcurrentWalWriter) — ✅ DONE

## Research findings

### InnoDB (`trx_t` + `trx_sys_t`)
- **Per-connection**: `trx_t` allocated from pool per MySQL thread via `THD::ha_data[]`.
  Holds `id`, `state`, `read_view`, `isolation_level`, `undo_no`, `rsegs` (rollback
  segments), `start_time`. Its own `mutex` protects state transitions — no global lock.
- **Global**: `trx_sys_t.m_max_trx_id` (atomic counter), `rw_trx_hash` (lock-free hash
  of active R/W transactions). `snapshot_ids()` spins on a **version counter** until
  `rw_trx_hash_version == m_max_trx_id` — ensures all assigned IDs are visible in the
  hash before a snapshot reads it.
- **Registration race**: `register_rw()` first inserts into `rw_trx_hash`, then bumps
  version counter (RELEASE barrier). `snapshot_ids()` reads version first (ACQUIRE).
  This guarantees snapshots never see a partial state.

### PostgreSQL (`PGPROC` + `ProcArray`)
- **Per-backend**: `PGPROC` has `xid` (current XID), `xmin` (oldest active at snapshot
  start), `subxids` cache (up to 64 sub-XIDs). Lives in shared memory, indexed by
  `pgxactoff` into dense `ProcGlobal` arrays.
- **Global**: `ProcArrayStruct` — dense array of backend indexes protected by
  `ProcArrayLock` (shared LWLock). `GetSnapshotData()` holds shared lock for the entire
  snapshot construction iteration.
- **Commit atomicity**: `latestCompletedXid` advances AND backend is removed from
  `ProcArray` **under the same lock**. A snapshot cannot observe a txn as both committed
  and still in-flight simultaneously.

### DuckDB model (`DuckTransaction` + `DuckTransactionManager`)
- **Per-connection**: `DuckTransaction` has `start_time: transaction_t` (snapshot
  timestamp assigned at BEGIN), `transaction_id: transaction_t` (unique ID),
  `commit_id: transaction_t` (set only after COMMIT), `UndoBuffer undo_buffer`
  (per-connection undo log), `LocalStorage storage` (uncommitted appends).
- **Visibility**: `chunk.insert_id <= txn.start_time && chunk.delete_id > txn.start_time`.
  This works because DuckDB stores the **commit timestamp** in the row header — once
  committed, `insert_id` is the commit time, not the begin time.
- **Active set**: `vector<DuckTransaction*> active_transactions` + `lowest_active_id`
  and `lowest_active_start` for garbage collection. When a txn commits, it is removed;
  the `lowest_active_id` helps vacuum know which old versions are still needed.
- **`ActiveTransactionState` optimization**: DuckDB tracks whether other transactions
  exist (`OTHER_TRANSACTIONS` vs `NO_OTHER_TRANSACTIONS`) to skip cleanup overhead
  when the database is effectively single-writer.

### Why AxiomDB needs `active_ids` (not just `start_time`)

DuckDB avoids an `active_ids` set because it writes the **commit timestamp** into
the row header. AxiomDB writes the **begin txn_id** (`txn_id_created`) instead.
This difference is critical:

```
DuckDB row: insert_id = commit_time  → visibility: insert_id <= snapshot.start_time ✅
AxiomDB row: txn_id_created = begin_id → visibility: txn_id_created < snapshot_id
             where snapshot_id = max_committed + 1
```

Without `active_ids`, the AxiomDB check `txn_id_created < snapshot_id` is **broken**
for multi-writer: if txn 3 (id=3) is still running and txn 4 commits, a new snapshot
gets `snapshot_id = 5`. It would see txn 3's uncommitted rows (3 < 5). The `active_ids`
set is the correct fix — visibility requires BOTH `txn_id_created <= max_committed`
AND `txn_id_created NOT IN active_ids`.

Changing to commit-timestamp semantics would require storing `commit_id` per row
(DuckDB doubles storage per row for this) — not worth it. The `active_ids` approach
adds a single `Arc<HashSet<TxnId>>` per snapshot instead.

### Critical rule for AxiomDB (InnoDB + PostgreSQL agree)
```
RULE: max_committed must advance AND active_set must remove the txn atomically.
```
A snapshot constructed between these two operations would incorrectly see the txn as
committed AND still active — violating the visibility invariant.

**Implementation**: acquire `active_set` write lock → advance `max_committed` →
remove `txn_id` → release. Snapshot construction: acquire `active_set` read lock →
read `max_committed` → clone set → release.

## New structures

### Per-connection (owned by the connection handler)

```rust
/// All state for one active transaction. Owned by the connection that called BEGIN.
/// Returned by TxnManager::begin(), passed back to commit() / rollback().
pub struct ConnectionTxn {
    /// Assigned TxnId for this transaction.
    pub txn_id: TxnId,
    /// max_committed at BEGIN + 1. Frozen snapshot for RR/Serializable isolation.
    pub snapshot_id_at_begin: u64,
    /// Isolation level — controls active_snapshot() behavior.
    pub isolation_level: IsolationLevel,
    /// Undo operations in chronological order; applied last-to-first on rollback.
    pub undo_ops: Vec<UndoOp>,
    /// Pages to free after WAL commit is confirmed durable.
    pub deferred_free_pages: Vec<u64>,
    /// Savepoints: index into undo_ops + deferred_free_pages at savepoint creation.
    pub savepoints: Vec<Savepoint>,
    /// Latest clustered B-tree root per table touched by this txn.
    /// Tracks root changes due to splits/merges within the txn for correct undo.
    pub clustered_roots: HashMap<u32, u64>,
    /// Reusable WAL entry serialization buffer (per-connection — no contention).
    /// Capacity grows to the largest entry seen, retained across operations.
    pub(crate) wal_scratch: Vec<u8>,
    /// When true, commit() skips inline fsync and stores txn_id for pipeline pickup.
    pub(crate) deferred_commit_mode: bool,
    /// Set by commit() in deferred mode. Taken by take_pending_deferred_commit().
    pub(crate) pending_deferred_txn_id: Option<TxnId>,
}
```

### Shared execution context (passed to every executor function)

```rust
/// Immutable shared subsystems bundled for executor calls.
///
/// Introduced in 40.4b so that executor signatures change exactly once.
/// In 40.11, `lock_mgr: &'a LockManager` is added — zero other changes needed.
pub struct ExecutionContext<'a> {
    pub storage: &'a dyn StorageEngine,
    pub coord:   &'a TxnManager,       // renamed TxnCoordinator in 40.11
    pub wal:     &'a ConcurrentWalWriter,
    pub bloom:   &'a BloomRegistry,
}
```

### Global coordinator (stays in Database, shared via &-ref)

Changes to `TxnManager` — fields removed, fields changed, new fields:

**Removed:**
- `active: Option<ActiveTxn>` — moves to `ConnectionTxn` (owned by connection)
- `wal_scratch: Vec<u8>` — moves to `ConnectionTxn`
- `deferred_commit_mode: bool` — moves to `ConnectionTxn`
- `pending_deferred_txn_id: Option<TxnId>` — moves to `ConnectionTxn`

**Changed:**
- `max_committed: u64` → `max_committed: AtomicU64`
  (reason: `snapshot()` must be `&self` when multiple connections call it concurrently)

**Stays (unchanged):**
- `wal: ConcurrentWalWriter`
- `next_txn_id: u64` (still under `&mut TxnManager` — becomes `AtomicU64` in 40.10)
- `committed_free_batches: Vec<(TxnId, Vec<u64>)>` (shared across all connections)
- `durability_policy: WalDurabilityPolicy`
- `last_clustered_roots: HashMap<u32, u64>` (seeds new `ConnectionTxn` at BEGIN)

**Added:**
- `active_set: RwLock<HashSet<TxnId>>`
  (PostgreSQL ProcArray pattern — tracks in-flight txns for snapshot visibility)
- `lowest_active_id: AtomicU64`
  (DuckDB-inspired — minimum txn_id across all active transactions; used by vacuum
   to know which old row versions can be safely reclaimed; updated on BEGIN and COMMIT)

## API changes

### TxnManager lifecycle methods

```rust
// BEFORE:
pub fn begin(&mut self) -> Result<TxnId, DbError>
pub fn begin_with_isolation(&mut self, iso: IsolationLevel) -> Result<TxnId, DbError>
pub fn commit(&mut self) -> Result<(), DbError>
pub fn rollback(&mut self, storage: &mut dyn StorageEngine) -> Result<(), DbError>
pub fn autocommit<F,T>(&mut self, storage: &mut dyn StorageEngine, f: F) -> Result<T, DbError>

// AFTER:
pub fn begin(&mut self) -> Result<ConnectionTxn, DbError>
pub fn begin_with_isolation(&mut self, iso: IsolationLevel) -> Result<ConnectionTxn, DbError>
pub fn commit(&mut self, conn_txn: ConnectionTxn) -> Result<(), DbError>
pub fn rollback(&mut self, conn_txn: ConnectionTxn, storage: &dyn StorageEngine) -> Result<(), DbError>
pub fn autocommit<F,T>(&mut self, storage: &dyn StorageEngine, f: F) -> Result<T, DbError>
where F: FnOnce(&mut ConnectionTxn, &ExecutionContext) -> Result<T, DbError>
```

### TxnManager snapshot methods

```rust
// BEFORE:
pub fn snapshot(&self) -> TransactionSnapshot          // reads self.max_committed
pub fn active_snapshot(&self) -> Result<TransactionSnapshot, DbError>  // reads self.active

// AFTER:
pub fn snapshot(&self) -> TransactionSnapshot          // reads max_committed.load(Acquire)
pub fn active_snapshot(&self, conn_txn: &ConnectionTxn) -> TransactionSnapshot
// (no longer fallible — conn_txn is guaranteed non-null by type system)
```

### TxnManager record_* methods

All 14 `record_*` methods take `conn_txn: &mut ConnectionTxn` instead of reading
`self.active`:

```rust
// BEFORE:
pub fn record_insert(&mut self, table_id, key, value, page_id, slot_id) -> Result<(), DbError>
//   internally: self.active.as_mut().ok_or(NoActiveTransaction)?

// AFTER:
pub fn record_insert(&self, conn_txn: &mut ConnectionTxn,
                     table_id, key, value, page_id, slot_id) -> Result<(), DbError>
//   uses conn_txn.undo_ops and conn_txn.wal_scratch directly
//   TxnManager ref is &self (only needs wal: &ConcurrentWalWriter)
```

Same pattern for: `record_insert_batch`, `record_delete`, `record_delete_batch`,
`record_update`, `record_update_in_place`, `record_update_in_place_batch`,
`record_clustered_insert`, `record_clustered_delete_mark`,
`record_clustered_delete_mark_lightweight`, `record_clustered_delete_mark_batch`,
`record_clustered_field_patch_batch`, `record_clustered_update_batch`,
`record_clustered_update`.

### TxnManager savepoint methods

```rust
// BEFORE:
pub fn savepoint(&self) -> Savepoint
pub fn rollback_to_savepoint(&mut self, sp, storage) -> Result<(), DbError>

// AFTER:
pub fn savepoint(conn_txn: &ConnectionTxn) -> Savepoint
pub fn rollback_to_savepoint(&self, conn_txn: &mut ConnectionTxn,
                              sp: Savepoint, storage: &dyn StorageEngine) -> Result<(), DbError>
```

### TxnManager accessor methods

```rust
// BEFORE:
pub fn active_txn_id(&self) -> Option<TxnId>
pub fn clustered_root(&self, table_id: u32) -> Option<u64>

// AFTER:
// active_txn_id() — callers use conn_txn.txn_id directly (no need for TxnManager)
// clustered_root now takes conn_txn:
pub fn clustered_root(&self, conn_txn: Option<&ConnectionTxn>, table_id: u32) -> Option<u64>
// or: ConnectionTxn::clustered_root(&self, last_roots: &HashMap<u32,u64>) -> Option<u64>
```

### Executor signatures

Every executor function currently using `txn: &mut TxnManager` is changed to
`(exec_ctx: &ExecutionContext, conn_txn: &mut ConnectionTxn)`:

```rust
// BEFORE (~100 sites):
fn execute_insert_ctx(
    stmt: InsertStmt,
    storage: &dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError>

// AFTER (same pattern everywhere):
fn execute_insert_ctx(
    stmt: InsertStmt,
    exec_ctx: &ExecutionContext,     // storage + coord + wal + bloom
    conn_txn: &mut ConnectionTxn,   // undo log + snapshot + savepoints
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError>
```

Inside the body: `exec_ctx.storage`, `exec_ctx.coord.record_insert(conn_txn, ...)`,
`exec_ctx.bloom`, `exec_ctx.coord.active_snapshot(conn_txn)`.

### Wire handler (axiomdb-network)

`Session` stores `Option<ConnectionTxn>`. The handler holds `conn_txn` across query
calls when inside an explicit transaction:

```rust
// handler.rs — new per-connection state:
struct HandlerState {
    // existing: session: Session, schema_cache: SchemaCache, ...
    conn_txn: Option<ConnectionTxn>,   // Some(..) inside BEGIN..COMMIT
}
```

`execute_query(db, sql, handler_state)` flow:
1. Acquire `Arc<RwLock<Database>>` write lock → get `&mut Database`
2. Build `ExecutionContext { storage: &db.storage, coord: &db.txn, wal: &db.txn.wal, bloom: &db.bloom }`
3. For DML: if autocommit → `db.txn.begin()` → returns `ConnectionTxn` → execute → `db.txn.commit(conn_txn)`
4. For explicit txn: `handler_state.conn_txn` holds the `ConnectionTxn` across calls

### Embedded API (`axiomdb-embedded`)

`Db` struct stores `conn_txn: Option<ConnectionTxn>` for explicit transactions:

```rust
pub struct Db {
    storage: MmapStorage,
    txn: TxnManager,
    bloom: BloomRegistry,
    schema_cache: SchemaCache,
    session: SessionContext,
    conn_txn: Option<ConnectionTxn>,   // ← new field
    degraded: bool,
    error_msg: Option<CString>,
}
```

`Db::execute()` / `Db::query()`:
- Autocommit: `let mut conn = self.txn.begin()?; ... self.txn.commit(conn)?`
- Explicit txn: use/modify `self.conn_txn`

## Snapshot visibility algorithm

```
visibility(row_txn_id, snapshot):
  1. row_txn_id <= snapshot.max_committed   → written before snapshot
  2. row_txn_id NOT IN snapshot.active_ids  → not still in-flight
  Both must hold for the row to be visible.
```

**COMMIT atomicity** (prevents snapshot gap):
```
acquire active_set WRITE lock
  max_committed.store(txn_id, Release)
  active_set.remove(txn_id)
release WRITE lock
```

**SNAPSHOT construction**:
```
acquire active_set READ lock
  mc = max_committed.load(Acquire)
  ids = active_set.clone()
release READ lock
→ TransactionSnapshot { max_committed: mc, active_ids: ids }
```

**READ COMMITTED** (fresh per-statement):
```rust
fn active_snapshot(conn_txn: &ConnectionTxn) -> TransactionSnapshot {
    if conn_txn.isolation_level.uses_frozen_snapshot() {
        TransactionSnapshot { snapshot_id: conn_txn.snapshot_id_at_begin,
                              current_txn_id: conn_txn.txn_id }
    } else {
        let mut snap = coord.snapshot();    // fresh max_committed + active_ids
        snap.current_txn_id = conn_txn.txn_id;
        snap
    }
}
```

> **Note on TransactionSnapshot**: The current struct has `snapshot_id: u64` and
> `current_txn_id: TxnId`. After this subfase it gains `active_ids: Arc<HashSet<TxnId>>`
> for correct visibility with concurrent transactions. The existing `snapshot_id`
> becomes the effective `max_committed` at snapshot time.

## Field migration summary

| Field | Before (TxnManager) | After |
|---|---|---|
| `active.txn_id` | TxnManager | `ConnectionTxn.txn_id` |
| `active.snapshot_id_at_begin` | TxnManager | `ConnectionTxn.snapshot_id_at_begin` |
| `active.isolation_level` | TxnManager | `ConnectionTxn.isolation_level` |
| `active.undo_ops` | TxnManager | `ConnectionTxn.undo_ops` |
| `active.deferred_free_pages` | TxnManager | `ConnectionTxn.deferred_free_pages` |
| `active.savepoints` (via Savepoint) | TxnManager | `ConnectionTxn.savepoints` |
| `active.clustered_roots` | TxnManager | `ConnectionTxn.clustered_roots` |
| `wal_scratch` | TxnManager | `ConnectionTxn.wal_scratch` |
| `deferred_commit_mode` | TxnManager | `ConnectionTxn.deferred_commit_mode` |
| `pending_deferred_txn_id` | TxnManager | `ConnectionTxn.pending_deferred_txn_id` |
| `max_committed: u64` | TxnManager | `TxnManager.max_committed: AtomicU64` |
| `next_txn_id: u64` | TxnManager | stays (plain u64, under &mut self) |
| `committed_free_batches` | TxnManager | stays |
| `last_clustered_roots` | TxnManager | stays |
| `wal` | TxnManager | stays |
| `durability_policy` | TxnManager | stays |
| *(new)* `active_set` | — | `TxnManager: RwLock<HashSet<TxnId>>` |
| *(new)* `lowest_active_id` | — | `TxnManager: AtomicU64` (vacuum GC horizon) |

## Use cases

1. **Autocommit (most common path):**
   ```
   execute_query() {
     conn = txn.begin()?;                        // allocates ConnectionTxn
     exec_ctx = ExecutionContext { storage, coord: &txn, wal: &txn.wal, bloom };
     result = execute_insert_ctx(stmt, &exec_ctx, &mut conn, ctx)?;
     txn.commit(conn)?;                          // drops ConnectionTxn
   }
   ```

2. **Explicit transaction across multiple queries:**
   ```
   BEGIN  → conn = txn.begin()? → stored in handler_state.conn_txn
   INSERT → execute_insert_ctx(&exec_ctx, &mut handler_state.conn_txn.unwrap_mut(), ctx)
   UPDATE → execute_update_ctx(&exec_ctx, &mut handler_state.conn_txn.unwrap_mut(), ctx)
   COMMIT → txn.commit(handler_state.conn_txn.take()?)
   ```

3. **Snapshot visibility with concurrent transactions (future — correct data structure now):**
   ```
   txn A begins (id=5) → active_set = {5}
   txn B begins (id=6) → active_set = {5, 6}
   txn C takes snapshot → max_committed=4, active_ids={5,6}
   txn C reads row with txn_id=3 → visible (3 ≤ 4 AND 3 ∉ {5,6})
   txn C reads row with txn_id=5 → invisible (5 ∈ {5,6})
   txn A commits → max_committed=5, active_set={6}
   txn D takes snapshot → max_committed=5, active_ids={6}
   txn D reads row with txn_id=5 → visible (5 ≤ 5 AND 5 ∉ {6})
   ```

4. **SAVEPOINT within ConnectionTxn:**
   ```
   BEGIN → conn = txn.begin()?
   stmt1 → records in conn.undo_ops[0..3]
   SAVEPOINT s1 → sp = TxnManager::savepoint(&conn) = Savepoint { undo_len: 3, ... }
   stmt2 → records in conn.undo_ops[3..6]
   ROLLBACK TO s1 → txn.rollback_to_savepoint(&mut conn, sp, storage)
                     undoes undo_ops[3..6], conn.undo_ops truncated to [0..3]
   COMMIT → txn.commit(conn)
   ```

## Acceptance criteria

- [ ] `ActiveTxn` struct removed — all its fields live in `ConnectionTxn`
- [ ] `ConnectionTxn` is public, exported from `axiomdb-wal` crate
- [ ] `TxnManager.active: Option<ActiveTxn>` field removed
- [ ] `TxnManager.max_committed` is `AtomicU64`
- [ ] `TxnManager.active_set: RwLock<HashSet<TxnId>>` added
- [ ] `TxnManager.lowest_active_id: AtomicU64` added (vacuum GC horizon, DuckDB-inspired)
- [ ] `begin()` returns `ConnectionTxn` (not stored globally)
- [ ] `commit(conn_txn: ConnectionTxn)` takes ownership — `ConnectionTxn` dropped on commit
- [ ] `rollback(conn_txn, storage)` takes ownership — applies undo_ops from conn_txn
- [ ] COMMIT advances `max_committed` AND removes from `active_set` under same write lock
- [ ] SNAPSHOT reads `max_committed` AND `active_set` under same read lock
- [ ] `active_snapshot(conn_txn)` returns frozen (RR) or fresh (RC) snapshot correctly
- [ ] All 14 `record_*` methods take `conn_txn: &mut ConnectionTxn` instead of `&mut self`
- [ ] `ExecutionContext<'a>` struct introduced with `storage, coord, wal, bloom` fields
- [ ] All ~100 executor signatures changed from `txn: &mut TxnManager` to
     `(exec_ctx: &ExecutionContext, conn_txn: &mut ConnectionTxn)`
- [ ] `axiomdb-network` handler stores `Option<ConnectionTxn>` in per-connection state
- [ ] `axiomdb-embedded` `Db` struct stores `conn_txn: Option<ConnectionTxn>`
- [ ] `autocommit()` wrapper works: creates `ConnectionTxn`, commits or rolls back on error
- [ ] WAL rotation (`rotate_wal`) works with new API (no active conn_txn required)
- [ ] Crash recovery (`open_with_recovery`) works unchanged
- [ ] `TransactionSnapshot` extended with `active_ids: Arc<HashSet<TxnId>>` for correct MVCC
- [ ] All existing unit tests in `axiomdb-wal/src/txn.rs` pass
- [ ] All existing integration tests pass (`cargo test --workspace`)
- [ ] Wire protocol smoke test: explicit BEGIN / multi-statement txn / COMMIT via pymysql
- [ ] Wire protocol smoke test: ROLLBACK restores pre-BEGIN state
- [ ] Wire protocol smoke test: autocommit INSERT visible to subsequent SELECT

## Out of scope

- Concurrent DML execution — `Arc<RwLock<Database>>` still serializes (removed in 40.10)
- `LocalPageBatch` per connection — that is 40.9
- `LockManager` integration — that is 40.5 / 40.11
- `next_txn_id` → `AtomicU64` — that is 40.10 (when write lock is removed)
- Renaming `TxnManager` → `TxnCoordinator` — that is 40.11 (cleaner single rename commit)

## Dependencies

- 40.3 (StorageEngine `&self`) — ✅ DONE: `&dyn StorageEngine` throughout
- 40.4 (ConcurrentWalWriter) — ✅ DONE: `TxnManager.wal` is already `ConcurrentWalWriter`

## Notes on renumbering

This spec was originally numbered 40.2. In practice 40.2 was used for the Plan Cache
(OID-based statement plan cache). This per-connection txn work is tracked as **40.4b**
in progreso.md, and must be completed before 40.5 (LockManager) can be wired end-to-end.
The inline implementation of `next_txn_id` as an atomic (originally 40.1) is deferred
to 40.10 when the Database write lock is removed — no atomics needed before that point.
