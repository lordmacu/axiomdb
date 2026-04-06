# Plan: 40.4b — Per-Connection Transaction State

## Files to create / modify

| File | Change |
|---|---|
| `crates/axiomdb-core/src/traits.rs` | Add `active_ids: Arc<HashSet<TxnId>>` to `TransactionSnapshot` |
| `crates/axiomdb-storage/src/heap.rs` | Update `RowHeader::is_visible()` for `active_ids` |
| `crates/axiomdb-storage/src/clustered_tree.rs` | Update `active_snapshot()` helper in tests |
| `crates/axiomdb-wal/src/txn.rs` | Major refactor: extract ConnectionTxn, refactor TxnManager |
| `crates/axiomdb-wal/src/lib.rs` | Re-export `ConnectionTxn`, `ExecutionContext` |
| `crates/axiomdb-sql/src/executor/mod.rs` | Executor signature sweep |
| `crates/axiomdb-sql/src/executor/insert.rs` | Signature sweep |
| `crates/axiomdb-sql/src/executor/update.rs` | Signature sweep |
| `crates/axiomdb-sql/src/executor/delete.rs` | Signature sweep |
| `crates/axiomdb-sql/src/executor/ddl.rs` | Signature sweep |
| `crates/axiomdb-sql/src/executor/bulk_empty.rs` | Signature sweep |
| `crates/axiomdb-sql/src/executor/staging.rs` | Signature sweep |
| `crates/axiomdb-sql/src/executor/shared.rs` | Signature sweep |
| `crates/axiomdb-sql/src/executor/select.rs` | Snapshot usage update |
| `crates/axiomdb-sql/src/executor/aggregate.rs` | Snapshot usage update |
| `crates/axiomdb-sql/src/table.rs` | Signature sweep |
| `crates/axiomdb-sql/src/vacuum.rs` | Signature sweep |
| `crates/axiomdb-sql/src/fk_enforcement.rs` | Signature sweep |
| `crates/axiomdb-sql/src/index_integrity.rs` | Signature sweep |
| `crates/axiomdb-sql/src/index_maintenance.rs` | Signature sweep |
| `crates/axiomdb-sql/src/lib.rs` | Update `execute_with_ctx` signature |
| `crates/axiomdb-catalog/src/writer.rs` | TxnManager → ConnectionTxn |
| `crates/axiomdb-catalog/src/reader.rs` | Snapshot update |
| `crates/axiomdb-network/src/mysql/database.rs` | Build ExecutionContext, store conn_txn |
| `crates/axiomdb-network/src/mysql/handler.rs` | Per-connection `Option<ConnectionTxn>` |
| `crates/axiomdb-embedded/src/lib.rs` | `Db` gains `conn_txn: Option<ConnectionTxn>` |

## Algorithm / Data structures

### Step 1: TransactionSnapshot update

```rust
// axiomdb-core/src/traits.rs — BEFORE
pub struct TransactionSnapshot {
    pub snapshot_id: TxnId,
    pub current_txn_id: TxnId,
}

// AFTER
use std::collections::HashSet;
use std::sync::Arc;

pub struct TransactionSnapshot {
    pub snapshot_id: TxnId,
    pub current_txn_id: TxnId,
    /// Txn IDs that were in-flight when this snapshot was taken.
    /// A row created by any of these is NOT visible (uncommitted).
    /// Empty for plain committed() snapshots (backward compatible).
    pub active_ids: Arc<HashSet<TxnId>>,
}

impl TransactionSnapshot {
    pub fn committed(max_committed: TxnId) -> Self {
        Self {
            snapshot_id: max_committed.saturating_add(1),
            current_txn_id: 0,
            active_ids: Arc::new(HashSet::new()),
        }
    }
    pub fn active(txn_id: TxnId, max_committed_at_start: TxnId) -> Self {
        Self {
            snapshot_id: max_committed_at_start.saturating_add(1),
            current_txn_id: txn_id,
            active_ids: Arc::new(HashSet::new()),  // populated by TxnManager in 40.4b
        }
    }
}
```

### Step 2: RowHeader::is_visible() update

```rust
// axiomdb-storage/src/heap.rs
pub fn is_visible(&self, snap: &TransactionSnapshot) -> bool {
    // Row was created by current txn (read-your-own-writes)
    let created_by_self = snap.current_txn_id != 0
        && self.txn_id_created == snap.current_txn_id;

    // Row was committed before this snapshot AND not in-flight at snapshot time
    let created_committed = self.txn_id_created < snap.snapshot_id
        && !snap.active_ids.contains(&self.txn_id_created);

    let created_visible = created_by_self || created_committed;
    if !created_visible {
        return false;
    }

    // Row not deleted
    if self.txn_id_deleted == 0 {
        return true;
    }
    // Deleted by current txn (we deleted it, not visible to us)
    if snap.current_txn_id != 0 && self.txn_id_deleted == snap.current_txn_id {
        return false;
    }
    // Deletion not visible: deleted after snapshot OR deleter still in-flight
    self.txn_id_deleted >= snap.snapshot_id
        || snap.active_ids.contains(&self.txn_id_deleted)
}
```

**Compatibility**: `TransactionSnapshot::committed(n)` has empty `active_ids` →
`active_ids.contains()` always false → existing behavior preserved for all unit tests
that use `committed()` directly.

### Step 3: ConnectionTxn struct

```rust
// axiomdb-wal/src/txn.rs — new public struct (replaces private ActiveTxn)
pub struct ConnectionTxn {
    pub txn_id: TxnId,
    pub snapshot_id_at_begin: u64,
    pub isolation_level: IsolationLevel,
    pub undo_ops: Vec<UndoOp>,
    pub deferred_free_pages: Vec<u64>,
    pub savepoints: Vec<Savepoint>,
    pub clustered_roots: HashMap<u32, u64>,
    /// Frozen active-txn set at BEGIN (for RR/Serializable isolation).
    /// None for READ COMMITTED (fresh snapshot per statement).
    pub(crate) active_ids_at_begin: Option<Arc<HashSet<TxnId>>>,
    /// Reusable WAL scratch buffer (per-connection, zero contention).
    pub(crate) wal_scratch: Vec<u8>,
    /// Copied from TxnManager at BEGIN time (server-wide config).
    pub(crate) deferred_commit_mode: bool,
    /// Set by commit() in deferred mode. Taken by take_pending_deferred_commit().
    pub(crate) pending_deferred_txn_id: Option<TxnId>,
}

impl ConnectionTxn {
    pub fn txn_id(&self) -> TxnId { self.txn_id }

    pub fn take_pending_deferred_commit(&mut self) -> Option<TxnId> {
        self.pending_deferred_txn_id.take()
    }
}
```

### Step 4: ExecutionContext struct

```rust
// axiomdb-wal/src/txn.rs (or a new exec_ctx.rs)
pub struct ExecutionContext<'a> {
    pub storage: &'a dyn StorageEngine,
    pub coord:   &'a TxnManager,
    pub wal:     &'a ConcurrentWalWriter,
    pub bloom:   &'a BloomRegistry,
}
```

> **Note**: `ExecutionContext` imports `BloomRegistry` from `axiomdb-sql`. To avoid
> a crate dependency cycle (`axiomdb-wal` → `axiomdb-sql`), `ExecutionContext` must
> live in `axiomdb-sql`, not `axiomdb-wal`. Define it in `axiomdb-sql/src/exec_ctx.rs`
> and import `TxnManager` + `ConcurrentWalWriter` from `axiomdb-wal`.

### Step 5: TxnManager changes

```rust
pub struct TxnManager {
    wal: ConcurrentWalWriter,
    next_txn_id: u64,                                // stays u64 (under &mut self)
    max_committed: AtomicU64,                        // was u64
    // active: Option<ActiveTxn>,                   // REMOVED
    active_set: RwLock<HashSet<TxnId>>,              // NEW — PostgreSQL ProcArray
    lowest_active_id: AtomicU64,                     // NEW — DuckDB GC horizon
    committed_free_batches: Vec<(TxnId, Vec<u64>)>,
    durability_policy: WalDurabilityPolicy,
    deferred_commit_mode: bool,                      // server-wide config; copied to ConnTxn
    last_clustered_roots: HashMap<u32, u64>,
}
```

**Key method signatures after refactoring:**

```rust
// begin — returns ConnectionTxn (not stored in self)
pub fn begin_with_isolation(&mut self, iso: IsolationLevel) -> Result<ConnectionTxn, DbError> {
    let txn_id = self.next_txn_id;
    self.next_txn_id += 1;
    // overflow check...

    // Write WAL BEGIN (no scratch buffer needed — begin() uses wal.append() directly)
    let mut entry = WalEntry::new(0, txn_id, EntryType::Begin, ...);
    self.wal.append(&mut entry)?;

    // Snapshot: read max_committed + active_set atomically
    let (snapshot_id_at_begin, active_ids_at_begin) = {
        let set = self.active_set.read().unwrap();
        let mc = self.max_committed.load(Ordering::Acquire);
        // Add self to active_set before releasing lock
        // Wait: we need WRITE lock for this. Use write lock from the start.
    };
    // Revised: use write lock once for both registration and snapshot capture
    let (snapshot_id_at_begin, active_ids_at_begin) = {
        let mut set = self.active_set.write().unwrap();
        set.insert(txn_id);
        // Update lowest_active_id
        let prev = self.lowest_active_id.load(Ordering::Relaxed);
        if prev == 0 || txn_id < prev {
            self.lowest_active_id.store(txn_id, Ordering::Relaxed);
        }
        let mc = self.max_committed.load(Ordering::Acquire);
        let active_ids = if iso.uses_frozen_snapshot() {
            Some(Arc::new(set.clone()))  // freeze set AFTER adding self (correct: self won't see own rows via active_ids)
            // Actually: self should NOT be in active_ids_at_begin for the frozen snapshot
            // because the snapshot is for reads of OTHER txns' data, not own data.
            // Self's own writes are handled by current_txn_id check.
            // So: capture set BEFORE inserting self.
        } else {
            None
        };
        (mc + 1, active_ids)
    };
    // For RR: capture active_set BEFORE adding self to it
    // Revised: insert first, then build snapshot EXCLUDING self
    ...

    Ok(ConnectionTxn {
        txn_id,
        snapshot_id_at_begin,
        isolation_level: iso,
        undo_ops: Vec::new(),
        deferred_free_pages: Vec::new(),
        savepoints: Vec::new(),
        clustered_roots: self.last_clustered_roots.clone(),
        active_ids_at_begin,
        wal_scratch: Vec::with_capacity(256),
        deferred_commit_mode: self.deferred_commit_mode,
        pending_deferred_txn_id: None,
    })
}

// commit — takes ConnectionTxn by value (dropped after commit)
pub fn commit(&mut self, mut conn_txn: ConnectionTxn) -> Result<(), DbError> {
    let txn_id = conn_txn.txn_id;

    // WAL COMMIT entry (uses conn_txn.wal_scratch)
    let mut entry = WalEntry::new(0, txn_id, EntryType::Commit, ...);
    self.wal.append_with_buf(&mut entry, &mut conn_txn.wal_scratch)?;

    // Read-only check (no undo_ops → no fsync needed)
    let is_read_only = conn_txn.undo_ops.is_empty();
    if is_read_only {
        self.wal.flush_no_sync()?;
    } else {
        match self.durability_policy { ... }
    }

    // ATOMICALLY: advance max_committed + remove from active_set
    {
        let mut set = self.active_set.write().unwrap();
        self.max_committed.store(txn_id, Ordering::Release);
        set.remove(&txn_id);
        let new_lowest = set.iter().copied().min().unwrap_or(0);
        self.lowest_active_id.store(new_lowest, Ordering::Relaxed);
    }

    self.last_clustered_roots = conn_txn.clustered_roots;
    if !conn_txn.deferred_free_pages.is_empty() {
        self.committed_free_batches.push((txn_id, conn_txn.deferred_free_pages));
    }
    Ok(())
    // conn_txn dropped here
}

// rollback — takes ConnectionTxn by value
pub fn rollback(&mut self, mut conn_txn: ConnectionTxn,
                storage: &dyn StorageEngine) -> Result<(), DbError> {
    // Apply undo_ops in reverse (same logic as before, but reading conn_txn.undo_ops)
    for op in conn_txn.undo_ops.drain(..).rev() { ... }

    // Remove from active_set (no max_committed advance on rollback)
    {
        let mut set = self.active_set.write().unwrap();
        set.remove(&conn_txn.txn_id);
        let new_lowest = set.iter().copied().min().unwrap_or(0);
        self.lowest_active_id.store(new_lowest, Ordering::Relaxed);
    }
    // conn_txn dropped; last_clustered_roots NOT updated (rollback = no state change)
    Ok(())
}

// snapshot — &self (AtomicU64 + RwLock read)
pub fn snapshot(&self) -> TransactionSnapshot {
    let set = self.active_set.read().unwrap();
    let mc = self.max_committed.load(Ordering::Acquire);
    TransactionSnapshot {
        snapshot_id: mc + 1,
        current_txn_id: 0,
        active_ids: Arc::new(set.clone()),
    }
}

// active_snapshot — &self + &ConnectionTxn (no longer fallible)
pub fn active_snapshot(&self, conn_txn: &ConnectionTxn) -> TransactionSnapshot {
    if conn_txn.isolation_level.uses_frozen_snapshot() {
        TransactionSnapshot {
            snapshot_id: conn_txn.snapshot_id_at_begin,
            current_txn_id: conn_txn.txn_id,
            active_ids: conn_txn.active_ids_at_begin.clone()
                .unwrap_or_else(|| Arc::new(HashSet::new())),
        }
    } else {
        // READ COMMITTED: fresh snapshot
        let set = self.active_set.read().unwrap();
        let mc = self.max_committed.load(Ordering::Acquire);
        TransactionSnapshot {
            snapshot_id: mc + 1,
            current_txn_id: conn_txn.txn_id,
            active_ids: Arc::new(set.clone()),
        }
    }
}

// record_insert — &self (only needs self.wal) + conn_txn: &mut ConnectionTxn
pub fn record_insert(&self, conn_txn: &mut ConnectionTxn,
                     table_id: u32, key: &[u8], value: &[u8],
                     page_id: u64, slot_id: u16) -> Result<(), DbError> {
    let txn_id = conn_txn.txn_id;
    // encode physical loc...
    let mut entry = WalEntry::new(0, txn_id, EntryType::Insert, table_id, ...);
    self.wal.append_with_buf(&mut entry, &mut conn_txn.wal_scratch)?;
    conn_txn.undo_ops.push(UndoOp::UndoInsert { page_id, slot_id });
    Ok(())
}
// Same pattern for all 13 other record_* methods.

// autocommit — &mut self + closure taking (&Self, &mut ConnectionTxn)
pub fn autocommit<F, T>(&mut self, storage: &dyn StorageEngine, f: F) -> Result<T, DbError>
where
    F: FnOnce(&Self, &mut ConnectionTxn) -> Result<T, DbError>,
{
    let mut conn = self.begin()?;
    match f(self, &mut conn) {
        Ok(v) => { self.commit(conn)?; Ok(v) }
        Err(e) => { let _ = self.rollback(conn, storage); Err(e) }
    }
}
```

### Step 6: BEGIN — active_ids_at_begin capture ordering

Critical ordering in `begin()` for the frozen RR snapshot:
```
1. Acquire active_set WRITE lock
2. Capture mc = max_committed.load(Acquire)
3. Capture active_ids_copy = active_set.clone()   ← BEFORE inserting self
4. Insert txn_id into active_set
5. Update lowest_active_id
6. Release write lock
7. active_ids_at_begin = Some(Arc::new(active_ids_copy))  ← excludes self (correct)
```

Self must NOT be in `active_ids_at_begin` — own writes are visible via `current_txn_id`,
not via the `active_ids` check. If self were in the set, own inserts would be invisible.

### Step 7: Executor signature pattern

All executor functions follow this uniform pattern:
```rust
// BEFORE
fn execute_xxx_ctx(
    stmt: XxxStmt,
    storage: &dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError>

// AFTER
fn execute_xxx_ctx(
    stmt: XxxStmt,
    exec_ctx: &ExecutionContext,     // storage, coord, wal, bloom
    conn_txn: &mut ConnectionTxn,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError>
```

Inside the body, replace:
- `storage` → `exec_ctx.storage`
- `bloom` → `exec_ctx.bloom`
- `txn.record_insert(...)` → `exec_ctx.coord.record_insert(conn_txn, ...)`
- `txn.active_snapshot()` → `exec_ctx.coord.active_snapshot(conn_txn)`
- `txn.snapshot()` → `exec_ctx.coord.snapshot()`
- `txn.begin_txn_id()` or `txn.active_txn_id()` → `conn_txn.txn_id()`
- `txn.clustered_root(id)` → `exec_ctx.coord.clustered_root(Some(conn_txn), id)`

### Step 8: Database::execute_query (network layer) new flow

```rust
// database.rs — build ExecutionContext and thread conn_txn
fn execute_query(
    &mut self,
    sql: &str,
    handler_state: &mut HandlerState,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let exec_ctx = ExecutionContext {
        storage: &self.storage,
        coord: &self.txn,
        wal: self.txn.wal(),
        bloom: &self.bloom,
    };

    // Autocommit path: create/destroy ConnectionTxn inline
    if is_autocommit(sql, handler_state) {
        let mut conn_txn = self.txn.begin()?;
        let result = execute_stmt(sql, &exec_ctx, &mut conn_txn, ctx);
        match result {
            Ok(r) => { self.txn.commit(conn_txn)?; Ok(r) }
            Err(e) => { let _ = self.txn.rollback(conn_txn, &self.storage); Err(e) }
        }
    } else {
        // Explicit transaction: conn_txn lives in handler_state
        let conn_txn = handler_state.conn_txn.as_mut().unwrap();
        execute_stmt(sql, &exec_ctx, conn_txn, ctx)
    }
}
```

`HandlerState` (or equivalent per-connection state in handler.rs) gains:
```rust
pub conn_txn: Option<ConnectionTxn>,
```

BEGIN in handler: `handler_state.conn_txn = Some(self.db.txn.begin()?)`
COMMIT: `self.db.txn.commit(handler_state.conn_txn.take().unwrap())?`
ROLLBACK: `self.db.txn.rollback(handler_state.conn_txn.take().unwrap(), &self.db.storage)?`

## Implementation phases

### Phase 1 — Core types (axiomdb-core + axiomdb-storage)

1. `axiomdb-core/src/traits.rs`: add `active_ids: Arc<HashSet<TxnId>>` to
   `TransactionSnapshot`. Update `committed()` and `active()` constructors.
   Add imports: `use std::collections::HashSet; use std::sync::Arc;`
2. `axiomdb-storage/src/heap.rs`: update `RowHeader::is_visible()` per algorithm above.
3. `axiomdb-storage/src/clustered_tree.rs`: update test helpers (`active_snapshot()`,
   `committed_snapshot()`) — they just call `TransactionSnapshot::active/committed()`,
   so no logic change needed, only add `active_ids: Arc::new(HashSet::new())` if direct
   struct literals exist.
4. Run `cargo test -p axiomdb-storage` — must pass before proceeding.

### Phase 2 — ConnectionTxn + ExecutionContext (axiomdb-wal + axiomdb-sql)

5. `axiomdb-wal/src/txn.rs`:
   a. Add `ConnectionTxn` struct (public) replacing private `ActiveTxn`.
   b. Refactor `TxnManager`: remove `active`, add `active_set`, `lowest_active_id`,
      change `max_committed` to `AtomicU64`.
   c. Rewrite `begin_with_isolation()`, `commit()`, `rollback()`, `snapshot()`,
      `active_snapshot()`, `autocommit()` per algorithm above.
   d. Rewrite all 14 `record_*` methods to take `conn_txn: &mut ConnectionTxn`.
   e. Update `savepoint()`, `rollback_to_savepoint()`, `defer_free_pages()`,
      `clustered_root()` to take `conn_txn`.
   f. Update `advance_committed()` / `advance_committed_single()` — now uses
      `max_committed.store(...)`, takes `&self`.
   g. Update `rotate_wal()` — check `active_set` instead of `self.active`.
   h. Fix all unit tests in `txn.rs` to use new API.
   i. Re-export `ConnectionTxn` from `axiomdb-wal/src/lib.rs`.
6. `axiomdb-sql/src/exec_ctx.rs` (NEW FILE): define `ExecutionContext<'a>`.
   Add to `axiomdb-sql/src/lib.rs`: `pub mod exec_ctx; pub use exec_ctx::ExecutionContext;`
7. Run `cargo test -p axiomdb-wal` — must pass before proceeding.

### Phase 3 — Executor sweep (axiomdb-sql)

8. Start with `axiomdb-sql/src/executor/mod.rs` — change the top-level
   `execute_with_ctx()` entry point. Let compiler failures guide the rest.
9. Sweep in order (follow compiler errors):
   - `executor/insert.rs` → `executor/update.rs` → `executor/delete.rs`
   - `executor/ddl.rs` → `executor/bulk_empty.rs` → `executor/staging.rs`
   - `executor/shared.rs` → `executor/select.rs` → `executor/aggregate.rs`
   - `table.rs` → `vacuum.rs` → `fk_enforcement.rs`
   - `index_integrity.rs` → `index_maintenance.rs`
10. Update `axiomdb-catalog/src/writer.rs` and `reader.rs`.
11. Run `cargo test -p axiomdb-sql` — must pass before proceeding.

### Phase 4 — Network + Embedded

12. `axiomdb-network/src/mysql/database.rs`:
    - Add `conn_txn: Option<ConnectionTxn>` to `HandlerState` (or wherever
      per-connection state is stored — check handler.rs for the exact struct).
    - Update `execute_query` / `execute_stmt` to build `ExecutionContext` and
      thread `conn_txn` per algorithm above.
    - Update BEGIN/COMMIT/ROLLBACK handling in `handler.rs`.
13. `axiomdb-embedded/src/lib.rs`:
    - Add `conn_txn: Option<ConnectionTxn>` to `Db`.
    - Update `execute()`, `query()`, `begin()`, `commit()`, `rollback()`.
14. Run `cargo test -p axiomdb-network` and `cargo test -p axiomdb-embedded`.

### Phase 5 — Closing protocol

15. `cargo test --workspace` — must be clean.
16. `cargo clippy --workspace -- -D warnings` — must be clean.
17. `cargo fmt --check` — must be clean.
18. Wire test: update `tools/wire-test.py` with:
    - Explicit BEGIN / INSERT / COMMIT smoke test
    - ROLLBACK restores state
    - Concurrent (sequential) autocommit INSERTs still visible

## Tests to write

**Unit (axiomdb-wal/src/txn.rs):**
- `test_begin_returns_connection_txn` — begin() returns ConnectionTxn, not stored in TxnManager
- `test_commit_takes_connection_txn` — after commit(conn_txn), conn_txn is consumed
- `test_two_connections_independent_undo_logs` — two ConnectionTxns have separate undo_ops
- `test_snapshot_excludes_active_ids` — snapshot() active_ids includes in-flight txn
- `test_commit_atomic_max_committed_and_active_set` — max_committed advances AND active_set
  loses the txn ID in the same atomic operation (verify with a second snapshot taken after)
- `test_rollback_does_not_advance_max_committed` — max_committed stays after rollback
- `test_lowest_active_id_updated_on_begin_and_commit`
- `test_rr_snapshot_frozen_at_begin` — RR conn_txn.active_ids_at_begin captured correctly
- `test_rc_snapshot_fresh_per_call` — RC always returns fresh active_ids

**Integration (axiomdb-sql/tests/):**
- `integration_explicit_txn.rs` (new or extend existing):
  - `test_explicit_begin_commit_visible`
  - `test_explicit_begin_rollback_invisible`
  - `test_savepoint_within_connection_txn`
  - `test_two_sequential_explicit_txns` (A commits → B sees A's writes)

**Wire protocol (tools/wire-test.py):**
- `BEGIN; INSERT INTO t VALUES (1); COMMIT;` → `SELECT COUNT(*) FROM t` = 1
- `BEGIN; INSERT INTO t VALUES (2); ROLLBACK;` → count unchanged
- Autocommit INSERT → immediately visible in next query
- `SET autocommit = 0; INSERT ...; COMMIT;` → standard explicit txn

## Anti-patterns to avoid

- **DO NOT** change `is_visible()` before `TransactionSnapshot` has `active_ids` —
  the struct change must land first (Phase 1) or `is_visible()` will fail to compile.
- **DO NOT** update executor signatures before `ConnectionTxn` and `ExecutionContext`
  are fully defined and compiling — Phase 2 must complete before Phase 3 starts.
- **DO NOT** make `next_txn_id` an `AtomicU64` in this phase — it's still under
  `&mut TxnManager`. AtomicU64 here would be incorrect and wasteful; wait for 40.10.
- **DO NOT** put `ExecutionContext` in `axiomdb-wal` — it needs `BloomRegistry` from
  `axiomdb-sql`, which would create a dependency cycle. It lives in `axiomdb-sql`.
- **DO NOT** include `self` (the new ConnectionTxn's own txn_id) in `active_ids_at_begin` —
  own writes must be visible via `current_txn_id` check, not blocked by `active_ids`.
- **DO NOT** advance `max_committed` on ROLLBACK — only COMMIT advances it.
- **DO NOT** try to compile across crates mid-sweep — complete each phase's crate
  before moving to the next (follow the dependency order: core → storage → wal → sql → net).
- **DO NOT** use `Arc::clone` in hot paths (record_insert) — `wal_scratch` is per-connection
  Vec, no cloning needed. Only snapshot construction clones the active_ids set.
- **DO NOT** change `autocommit()` in WAL tests to require `ExecutionContext` —
  WAL-level tests don't have `BloomRegistry`. Use the simpler `fn(&Self, &mut ConnectionTxn)`
  closure form for `TxnManager::autocommit()`.

## Risks

| Risk | Mitigation |
|---|---|
| `is_visible()` behavior change breaks existing MVCC tests | Phase 1: run storage tests immediately after. Empty `active_ids` in `committed()` preserves old behavior exactly. |
| Borrow checker conflicts in `autocommit()` closure | The closure takes `&Self` (shared), not `&mut Self`. After `begin()` returns, mutable borrow ends. Shared borrow during closure, then mutable for `commit()`. This compiles. |
| `ExecutionContext` lifetime vs borrow of `Database` fields | `exec_ctx` has lifetime `'_` tied to the statement execution scope. Always let `exec_ctx` go out of scope before calling `self.txn.commit()` (different borrow). |
| ~100 executor signatures is large mechanical change — easy to miss one | Use compiler as the guide: start at `execute_with_ctx` entry point, follow errors down. Every error points to the next site. |
| Handler stores `Option<ConnectionTxn>` — must not be dropped on session end without rollback | Implement `Drop` for handler connection state that calls `rollback()` if `conn_txn.is_some()`. |
| `active_ids` Arc clone per snapshot is O(active connections) | Under single-writer (pre-40.10), active_set has 0 or 1 elements — clone is O(1). Negligible cost. |
