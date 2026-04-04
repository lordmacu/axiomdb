# Spec: 40.2 — Per-Connection Transaction State

## What to build (not how)

Move the active transaction state (`ActiveTxn`) out of the global `TxnManager` into
per-connection ownership. Each connection gets its own `ActiveTxn` with its own undo
log, snapshot, and isolation level. The `TxnManager` becomes a lightweight coordinator
that only tracks: atomic ID counters (40.1), the WAL writer, and a registry of active
transactions for snapshot visibility.

## Research findings

### InnoDB model (chosen reference)
- **`struct trx_t`** allocated per MySQL connection (1:1 via `THD::ha_data[]`)
- Per-transaction state: `undo_no`, `lock` (trx_lock_t with wait queue), `read_view`,
  `isolation_level`, `state` (NOT_STARTED → ACTIVE → COMMITTED)
- Global `trx_sys` tracks: `m_max_trx_id` (atomic), `rw_trx_hash` (active set for snapshots)
- `trx_allocate_for_mysql()` creates trx_t; `trx_free_for_mysql()` destroys
- Per-transaction mutex (`trx->mutex`) protects state — NOT a global lock

### PostgreSQL model
- **`struct PGPROC`** in shared memory per backend process
- Transaction state stack: `TransactionStateData` with `transactionId`, `subTransactionId`,
  `state`, `nestingLevel` — supports savepoints as nested states
- **`ProcArray`** dense array of active backends for efficient snapshot construction
- Snapshot reads iterate ProcArray under shared `ProcArrayLock`

### Current AxiomDB architecture
- `TxnManager.active: Option<ActiveTxn>` — ONE global transaction
- `ActiveTxn` holds: `txn_id`, `snapshot_id_at_begin`, `isolation_level`,
  `undo_ops: Vec<UndoOp>`, `deferred_free_pages: Vec<u64>`
- `SessionContext` already exists per-connection with session variables, caches, stats
- Connection handler in `handler.rs` has per-connection `Session` struct

## Inputs / Outputs

- Input: `TxnManager` with `active: Option<ActiveTxn>` (global, single-writer)
- Output: `TxnManager` as coordinator + per-connection `ConnectionTxn` holding `ActiveTxn`
- Errors: `DbError::TransactionAlreadyActive` now per-connection (not global)

## New structures

### Per-connection (owned by connection handler)

```rust
/// Transaction state owned by each connection. Created on BEGIN, destroyed on COMMIT/ROLLBACK.
pub struct ConnectionTxn {
    pub txn_id: TxnId,
    pub snapshot: TransactionSnapshot,
    pub isolation_level: IsolationLevel,
    pub undo_ops: Vec<UndoOp>,
    pub deferred_free_pages: Vec<u64>,
    pub savepoints: Vec<Savepoint>,
}
```

### Global coordinator (shared via Arc)

```rust
/// Lightweight coordinator — no per-transaction state, only shared counters and WAL.
pub struct TxnCoordinator {
    pub wal: WalWriter,                              // WAL writer (still single for now, 40.4 makes concurrent)
    pub next_txn_id: AtomicU64,                      // from 40.1
    pub max_committed: AtomicU64,                    // from 40.1
    pub active_set: RwLock<HashSet<TxnId>>,          // for snapshot visibility
    pub durability_policy: WalDurabilityPolicy,
    pub committed_free_batches: Mutex<Vec<(TxnId, Vec<u64>)>>,
}
```

### Active transaction registry

The `active_set: RwLock<HashSet<TxnId>>` tracks which transactions are currently in-flight.
- `begin()` adds txn_id to the set (write lock, brief)
- `commit()` removes txn_id from the set (write lock, brief)
- `snapshot()` reads the set to determine visibility (read lock)

This is the PostgreSQL ProcArray pattern simplified for AxiomDB.

## Lifecycle changes

### BEGIN
```
BEFORE: TxnManager.begin() → creates ActiveTxn in self.active
AFTER:  TxnCoordinator.begin() → returns ConnectionTxn to caller
        Coordinator: fetch_add next_txn_id, add to active_set, write WAL BEGIN entry
        Connection: stores ConnectionTxn locally
```

### COMMIT
```
BEFORE: TxnManager.commit() → reads self.active, writes WAL, advances max_committed
AFTER:  TxnCoordinator.commit(conn_txn) → takes ConnectionTxn from connection
        Coordinator: writes WAL COMMIT, advances max_committed, removes from active_set
        Connection: ConnectionTxn dropped, undo_ops discarded
```

### ROLLBACK
```
BEFORE: TxnManager.rollback() → reads self.active, applies undo_ops
AFTER:  TxnCoordinator.rollback(conn_txn, storage) → takes ConnectionTxn
        Coordinator: applies undo_ops from conn_txn, writes WAL ROLLBACK, removes from active_set
        Connection: ConnectionTxn dropped
```

### SNAPSHOT (for reads)
```
BEFORE: TxnManager.snapshot() → reads max_committed
AFTER:  TxnCoordinator.snapshot() → reads max_committed atomically (no lock needed)
        For READ COMMITTED: fresh snapshot per statement
        For REPEATABLE READ: uses conn_txn.snapshot (frozen at BEGIN)
```

## Use cases

1. **Single connection (today's behavior):** Identical behavior. ConnectionTxn created
   on BEGIN, stored in handler, passed back on COMMIT/ROLLBACK. No functional change.

2. **Two connections with explicit transactions:**
   Connection A: `BEGIN; INSERT INTO t VALUES (1); -- holds ConnectionTxn A`
   Connection B: `BEGIN; INSERT INTO t VALUES (2); -- holds ConnectionTxn B`
   Both have independent undo logs. Both registered in active_set.
   A commits → max_committed advances. B commits → max_committed advances again.

3. **Snapshot visibility with concurrent transactions:**
   A begins (txn_id=5), B begins (txn_id=6).
   C takes snapshot → active_set = {5, 6}, max_committed = 4.
   C cannot see uncommitted rows from A or B.
   A commits → max_committed = 5, active_set = {6}.
   D takes snapshot → sees A's rows but not B's.

4. **Autocommit (most common):**
   `INSERT INTO t VALUES (1)` → coordinator creates ConnectionTxn, executes,
   commits, ConnectionTxn dropped. Same as today but ConnectionTxn is ephemeral.

## Acceptance criteria

- [ ] `ActiveTxn` removed from `TxnManager` — lives in per-connection state
- [ ] New `ConnectionTxn` struct with txn_id, snapshot, undo_ops, isolation
- [ ] `TxnCoordinator` (or refactored TxnManager) tracks active_set: `RwLock<HashSet<TxnId>>`
- [ ] `begin()` returns `ConnectionTxn` to caller (doesn't store globally)
- [ ] `commit()` takes `ConnectionTxn` as parameter (not from global state)
- [ ] `rollback()` takes `ConnectionTxn` and applies its undo_ops
- [ ] Snapshot creation considers active_set for visibility
- [ ] Multiple connections can hold independent `ConnectionTxn` simultaneously
- [ ] All existing tests adapted to new API
- [ ] Autocommit wrapper works with new API
- [ ] Wire protocol handler updated to store ConnectionTxn in session

## Out of scope

- Concurrent page access (that's 40.3 + 40.7)
- Concurrent WAL writes (that's 40.4)
- Row-level locking between transactions (that's 40.5)
- Deadlock detection (that's 40.6)
- The `Arc<RwLock<Database>>` still serializes DML execution (removed in 40.10)

## Dependencies

- 40.1 (Atomic TxnId) — next_txn_id and max_committed must be AtomicU64
