# Plan: 40.5 — Lock Manager: Row-Level Locks

## Files to create/modify

| File | Change |
|---|---|
| `crates/axiomdb-lock/` | **NEW CRATE**: dedicated lock manager crate |
| `crates/axiomdb-lock/src/lib.rs` | LockManager, LockMode, LockFlags exports |
| `crates/axiomdb-lock/src/mode.rs` | LockMode enum, conflict matrix, compatibility checks |
| `crates/axiomdb-lock/src/entry.rs` | LockEntry, LockQueue, LockWaiter structs |
| `crates/axiomdb-lock/src/manager.rs` | LockManager with sharded hash table |
| `crates/axiomdb-lock/src/bitmap.rs` | Per-page record lock bitmap |
| `crates/axiomdb-lock/src/implicit.rs` | Implicit lock detection via RowHeader.txn_id |
| `Cargo.toml` (workspace) | Add axiomdb-lock to workspace members |

## Why a new crate?

The lock manager is a cross-cutting concern used by storage, executor, and WAL.
Putting it in axiomdb-storage creates circular dependencies (storage ↔ sql).
A dedicated crate avoids this and follows the existing pattern (axiomdb-core, axiomdb-types).

## Implementation phases

### Phase 1: LockMode + conflict matrix
- Define `LockMode` enum (IS, IX, S, X, AI)
- Define `LockFlags` bitfield (WAITING, GAP, REC_NOT_GAP, INSERT_INTENTION, TABLE_LOCK)
- Implement `is_compatible(held: LockMode, requested: LockMode) -> bool`
- Implement `conflicts_with_any(granted: &[LockEntry], request: &LockEntry) -> Option<TxnId>`
- Unit tests: all 25 cells of the 5×5 matrix

### Phase 2: LockEntry + LockQueue
- Define LockEntry (txn_id, mode, flags, timestamp, bitmap/table_id)
- Define LockQueue (granted Vec + waiting VecDeque)
- Define LockWaiter (entry + Arc<Notify> for async wake)
- Implement `try_grant_waiters()` — after lock release, scan waiting queue

### Phase 3: Per-page record bitmap
- `RecordBitmap`: Vec<u64> (1 bit per slot, expandable)
- `set_bit(heap_no)`, `clear_bit(heap_no)`, `test_bit(heap_no)`
- `has_conflict(other_bitmap)` — AND two bitmaps for overlap check
- Space: ~16 bytes per page (128 slots typical)

### Phase 4: LockManager (sharded hash table)
- 64 shards for record locks (page_id % 64)
- 16 shards for table locks (table_id % 16)
- Each shard: `RwLock<HashMap<key, LockQueue>>`
- `acquire_record_lock(txn_id, page_id, heap_no, mode, flags) -> LockResult`
- `acquire_table_lock(txn_id, table_id, mode) -> LockResult`
- `release_all_locks(txn_id)` — bulk release on COMMIT/ROLLBACK
- `LockResult`: `Granted`, `Waited(notify)`, `Deadlock`

### Phase 5: Implicit lock detection
- Check `RowHeader.txn_id_created` against active transaction set
- If active → convert to explicit lock (create LockEntry for that txn)
- Then queue the new request as normal
- Saves memory for uncontended INSERT rows

### Phase 6: Per-transaction lock tracking
- Each ConnectionTxn (from 40.2) holds `held_locks: Vec<LockRef>`
- LockRef = (shard_id, page_id or table_id, heap_no)
- On COMMIT/ROLLBACK: iterate held_locks, release each

### Phase 7: Integration points (stubs)
- Executor INSERT: acquire IX(table) + X(row) before heap insert
- Executor SELECT: acquire IS(table) + S(row) before heap read
- Executor UPDATE: acquire IX(table) + X(row) before heap modify
- Executor DELETE: acquire IX(table) + X(row) before heap delete
- (Actual executor integration is 40.11, but stubs validate the API)

## Tests to write

1. **Conflict matrix**: all 25 mode pairs → correct grant/wait decision
2. **Two txns, different rows**: both granted (no conflict)
3. **Two txns, same row, S+S**: both granted (shared compatible)
4. **Two txns, same row, S+X**: X waits for S
5. **Two txns, same row, X+X**: second X waits
6. **Lock release grants waiter**: release S → waiting X granted
7. **FIFO ordering**: 3 waiters → granted in request order
8. **Table intention locks**: IX+IX compatible, IX+X(table) conflict
9. **Bitmap operations**: set/clear/test bits, multi-row locking
10. **Implicit lock conversion**: detect implicit X, convert, queue waiter
11. **Bulk release**: 100 locks released on commit → all waiters granted
12. **Stress test**: 8 threads × 1000 locks on random pages → no corruption

## Anti-patterns to avoid

- DO NOT use a global Mutex for the entire lock manager — defeats purpose
- DO NOT hold shard RwLock during waiter notification — release first, then notify
- DO NOT store lock entries by value in multiple places — use indices/references
- DO NOT implement deadlock detection here — that's 40.6 (clean separation)
- DO NOT lock ordering: never acquire shard A then shard B (acquire one at a time)

## Risks

- **Memory growth**: one LockEntry per held lock. 1000 txns × 100 locks = 100K entries.
  Each entry ~64 bytes → ~6.4 MB. Acceptable.
- **Shard contention**: hot pages (e.g., table tail for sequential INSERTs) all hash to
  same shard. Mitigation: shard by page_id distributes unless all inserts go to same page.
  For same-page contention, the lock acquisition is brief (~1-10µs).
- **Waiter notification**: tokio::Notify may have overhead. Alternative: std::sync::Condvar
  if not using async. Decide based on network handler architecture (currently async Tokio).
