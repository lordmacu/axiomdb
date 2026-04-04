# Plan: 40.2 — Per-Connection Transaction State

## Files to create/modify

| File | Change |
|---|---|
| `crates/axiomdb-wal/src/txn.rs` | Refactor TxnManager: remove `active`, add `active_set`, new begin/commit/rollback signatures |
| `crates/axiomdb-wal/src/lib.rs` | Export new `ConnectionTxn` type |
| `crates/axiomdb-network/src/mysql/handler.rs` | Store `ConnectionTxn` per connection |
| `crates/axiomdb-network/src/mysql/database.rs` | Update execute_query to pass ConnectionTxn |
| `crates/axiomdb-sql/src/session.rs` | Add `active_txn: Option<ConnectionTxn>` to SessionContext |
| `crates/axiomdb-sql/src/executor/mod.rs` | Update execute() signature to accept ConnectionTxn |
| `crates/axiomdb-sql/src/executor/*.rs` | All DML executors pass ConnectionTxn through |

## Implementation phases

1. **Define ConnectionTxn struct** in axiomdb-wal/src/txn.rs
   - Move ActiveTxn fields into public ConnectionTxn
   - Add active_set: RwLock<HashSet<TxnId>> to TxnManager

2. **Refactor TxnManager API**
   - `begin() → Result<ConnectionTxn, DbError>` (returns, doesn't store)
   - `commit(conn_txn: ConnectionTxn) → Result<(), DbError>` (takes ownership)
   - `rollback(conn_txn: ConnectionTxn, storage) → Result<(), DbError>`
   - `autocommit(f: FnOnce(&mut ConnectionTxn) → Result<T>)` wrapper

3. **Update SessionContext** to hold `Option<ConnectionTxn>`
   - Explicit txn: stored between BEGIN and COMMIT/ROLLBACK
   - Autocommit: created and destroyed within single statement

4. **Update executor signatures** to receive `&mut ConnectionTxn` instead of `&mut TxnManager`
   - This is ~50 signatures (less than 40.3's 184 because many only need storage)

5. **Update network handler** to manage ConnectionTxn lifecycle
   - On BEGIN: call coordinator.begin(), store in session
   - On COMMIT: take from session, call coordinator.commit()
   - On ROLLBACK: take from session, call coordinator.rollback()

6. **Update all WAL recording methods**
   - record_insert, record_delete, etc. move to ConnectionTxn (they write to WAL + add undo)
   - ConnectionTxn holds reference to shared WalWriter (via Arc)

## Tests to write

- Two ConnectionTxn instances exist simultaneously (basic API test)
- BEGIN → INSERT → COMMIT lifecycle with ConnectionTxn
- BEGIN → INSERT → ROLLBACK applies undo_ops from ConnectionTxn
- Autocommit wrapper creates ephemeral ConnectionTxn
- active_set correctly tracks in-flight transactions
- Snapshot sees committed but not active transactions

## Anti-patterns to avoid

- DO NOT make ConnectionTxn Clone — it owns undo_ops and must be consumed on commit/rollback
- DO NOT lock active_set during the entire transaction — only during register/unregister
- DO NOT change StorageEngine signatures yet — that's 40.3

## Risks

- **Large refactor surface**: ~50 executor signatures change. Mitigation: systematic
  find-and-replace, all tests must pass.
- **Undo ops ownership**: ConnectionTxn must survive across multiple executor calls within
  one transaction. Must ensure it's not dropped prematurely.
- **Autocommit performance**: Creating/destroying ConnectionTxn per statement adds allocation.
  Mitigation: ConnectionTxn is small (~100 bytes + Vec), allocation is negligible vs I/O.
