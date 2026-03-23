# Spec: 3.5 — BEGIN / COMMIT / ROLLBACK (TxnManager)

## What to build (not how)

A `TxnManager` that coordinates the transaction lifecycle: assigns TxnIds,
records DML operations to the WAL, fsyncs on commit, and applies undo
operations on rollback. Handles autocommit mode transparently.

Single-writer constraint: at most one explicit transaction active at a time.
Concurrent readers are served via `TransactionSnapshot` without locking.

## Inputs / Outputs

### `TxnManager::create(wal_path)` / `TxnManager::open(wal_path)`
- `create`: fresh WAL file, max_committed = 0, next_txn_id = 1
- `open`: existing WAL, scans forward to find max committed txn_id
- Output: `Result<TxnManager, DbError>`

### `begin() -> Result<TxnId, DbError>`
- Starts a new explicit transaction
- Assigns next monotonic TxnId
- Writes Begin WAL entry (buffered, not fsynced)
- Error: `TransactionAlreadyActive` if one is already in progress

### `record_insert(table_id, key, value, page_id, slot_id) -> Result<(), DbError>`
### `record_delete(table_id, key, old_value, page_id, slot_id) -> Result<(), DbError>`
### `record_update(table_id, key, old_value, new_value, page_id, old_slot, new_slot) -> Result<(), DbError>`
- Called AFTER the heap+index change has been applied to storage
- Writes buffered WAL entry (not fsynced)
- Pushes undo operation to the in-memory undo log
- Error: `NoActiveTransaction` if called without begin()

### `commit() -> Result<(), DbError>`
- Writes Commit WAL entry, flushes BufWriter, fsyncs
- Advances `max_committed` to the current txn_id
- Clears active transaction state
- Error: `NoActiveTransaction`

### `rollback(storage: &mut dyn StorageEngine) -> Result<(), DbError>`
- Writes Rollback WAL entry (informational; no fsync needed)
- Applies undo operations in reverse order:
  - `UndoInsert { page_id, slot_id }` → zero out the slot (mark dead)
  - `UndoDelete { page_id, slot_id }` → clear txn_id_deleted (set to 0)
- Clears active transaction state
- Does NOT advance max_committed (rolled-back txn is invisible to all snapshots)
- Error: `NoActiveTransaction`

### `autocommit(storage, f) -> Result<T, DbError>`
- Convenience wrapper: `begin → f(txn_id) → commit (on Ok) / rollback (on Err)`
- Allows callers to write one-operation transactions without manual begin/commit
- On closure error: rollback is called automatically before propagating the error

### `snapshot() -> TransactionSnapshot`
- Returns a snapshot of committed data only (`snapshot_id = max_committed + 1`, `current_txn_id = 0`)
- Safe to call at any time; does not require an active transaction
- Used for read operations outside explicit transactions

### `active_snapshot() -> Result<TransactionSnapshot, DbError>`
- Returns snapshot for the active transaction (reads committed data + own writes)
- `snapshot_id = max_committed_at_begin + 1`, `current_txn_id = active_txn_id`
- Error: `NoActiveTransaction`

### `max_committed() -> TxnId`
- Returns the TxnId of the last committed transaction (0 if none)

## Use cases

1. **Explicit transaction**: `begin → record_insert → record_delete → commit`
   → WAL has [Begin, Insert, Delete, Commit], all fsynced at commit ✓

2. **Autocommit INSERT**: `autocommit(storage, |txn_id| { ... record_insert ... })`
   → Single fsync for the combined Begin+Insert+Commit ✓

3. **Rollback INSERT**: `begin → record_insert → rollback(storage)`
   → WAL has [Begin, Insert, Rollback] (no fsync); slot marked dead ✓
   → max_committed unchanged; future snapshots cannot see the row ✓

4. **Rollback DELETE**: `begin → record_delete → rollback(storage)`
   → txn_id_deleted cleared in RowHeader; row visible again ✓

5. **Error mid-transaction**: `begin → record_insert → error → rollback`
   → Undo applies cleanly; storage left in pre-transaction state ✓

6. **Concurrent reader during transaction**: `snapshot()` returns max_committed
   → Uncommitted inserts (txn_id_created > max_committed) not visible ✓

7. **Reopen after clean shutdown**: `open(wal_path)` scans WAL
   → max_committed restored from last Commit entry's txn_id ✓

8. **Double begin**: `begin() → begin()` → `TransactionAlreadyActive` error ✓

9. **Commit without begin**: `commit()` → `NoActiveTransaction` error ✓

## Error semantics (subfase 3.5c)

- **Constraint violation (e.g. UniqueViolation)**: caller rollbacks the statement
  that caused it. The transaction can continue with the next statement. In autocommit
  mode, the single statement is rolled back automatically.
- **Storage I/O error during DML**: forces full transaction rollback (data may be
  partially written; rollback restores heap consistency; WAL has Rollback entry).
- **Fsync failure on commit**: the transaction is lost (WAL not durable); caller
  receives error; database state is pre-transaction (undo already applied on WAL
  not reaching disk — crash recovery handles this via absent Commit entry).

## Acceptance criteria

- [ ] `TxnManager::create` creates a valid WAL, `max_committed() == 0`
- [ ] `begin() → commit()` advances `max_committed` to the assigned TxnId
- [ ] `begin() → rollback(storage)` does NOT advance `max_committed`
- [ ] `record_insert` after `rollback` leaves the slot dead (offset=0, length=0)
- [ ] `record_delete` after `rollback` restores `txn_id_deleted = 0` in the RowHeader
- [ ] WAL entries written in correct order: [Begin, DML*, Commit/Rollback]
- [ ] WAL is fsynced on commit; NOT fsynced on rollback
- [ ] `snapshot()` returns `snapshot_id = max_committed + 1, current_txn_id = 0`
- [ ] `active_snapshot()` returns `current_txn_id = active_txn_id`
- [ ] `autocommit` commits on Ok, rollbacks on Err
- [ ] Double `begin()` returns `TransactionAlreadyActive`
- [ ] `commit()` / `rollback()` without `begin()` returns `NoActiveTransaction`
- [ ] `open()` correctly recovers `max_committed` from an existing WAL
- [ ] All 9 use cases above verified by tests
- [ ] No `unwrap()` in `src/`; all `unsafe` has `SAFETY:` comments

## Out of scope

- Crash recovery replay (3.8) — TxnManager only records, does not replay on open
- Savepoints — deferred
- Two-phase commit — deferred
- Concurrent writers — Phase 7
- Autocommit flag (SET autocommit=0) — deferred to SQL executor phase

## Dependencies

- `axiomdb-wal`: WalWriter, WalEntry, EntryType (already exist)
- `axiomdb-storage`: StorageEngine, Page, heap::mark_slot_dead, heap::clear_deletion (new helpers needed)
- `axiomdb-core`: TxnId, TransactionSnapshot, DbError (already exist)
- New DbError variants: `TransactionAlreadyActive`, `NoActiveTransaction`
