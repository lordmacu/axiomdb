# Spec: 40.5 — Lock Manager: Row-Level Locks

## What to build (not how)

A sharded, row-level lock manager that enables multiple transactions to operate on
different rows concurrently while serializing access to the same row. This is the
core concurrency control mechanism — without it, concurrent writers corrupt data.

## Research findings

### InnoDB lock architecture (primary reference)
- **Lock structure (`lock_t`)**: packed `type_mode` (32 bits) encodes mode + flags + wait
  state in a single field. Record locks use a **bitmap** (1 bit per heap_no on page) —
  one lock struct covers multiple rows on the same page.
- **Hash table sharding**: `page_id.fold() % n_partitions` → per-shard RwLock. Only the
  relevant shard is locked during acquisition — other shards uncontested.
- **Collision chain ordering**: granted locks appear before waiting locks. This enables
  fast conflict checking (scan chain until first waiter → everything after is also waiting).
- **Conflict matrix**: 5×5 (IS, IX, S, X, AUTO_INC). S/X is the core; IS/IX enable
  hierarchical locking (intention locks at table level, real locks at row level).
- **Implicit locks via trx_id**: If a record's `DB_TRX_ID` matches an active transaction,
  it's implicitly X-locked — no explicit lock struct needed. Saves memory for common case.

### PostgreSQL lock manager (reference for advanced features)
- **LOCKTAG system**: generic (table, page, tuple, transaction, object). Each lock object
  identified by a tag → hashed into partitioned lock table.
- **8 lock modes**: AccessShare through AccessExclusive. More granular than InnoDB.
- **Fast-path locks**: Per-backend array for uncontested weak locks — avoids shared hash
  table entirely for 80%+ of SELECT locks.
- **PROCLOCK**: per-process-per-lock junction table. Tracks which modes each backend holds.
- **Wait queue**: doubly-circular list on LOCK struct. FIFO, but rearrangeable by deadlock
  detector (soft deadlock resolution via topological sort).

### Design choice for AxiomDB: InnoDB-inspired with PostgreSQL enhancements
- **5 lock modes** (InnoDB): IS, IX, S, X, AUTO_INC. Simpler than PostgreSQL's 8.
- **Sharded hash table** (both): reduces contention.
- **Per-page bitmap** (InnoDB): efficient for clustered index (many rows per page).
- **Explicit wait queue** (PostgreSQL): cleaner than InnoDB's implicit chain ordering.
- **No fast-path initially**: add in 40.12 optimization phase if needed.

## Lock modes and conflict matrix

```
          IS    IX    S     X     AI
IS        ✓     ✓     ✓     ✗     ✓
IX        ✓     ✓     ✗     ✗     ✓
S         ✓     ✗     ✓     ✗     ✗
X         ✗     ✗     ✗     ✗     ✗
AI        ✓     ✓     ✗     ✗     ✗
```

- **IS (Intention Shared)**: table-level — "I intend to read rows"
- **IX (Intention Exclusive)**: table-level — "I intend to modify rows"
- **S (Shared)**: row-level — "I'm reading this row" (multiple S compatible)
- **X (Exclusive)**: row-level — "I'm modifying this row" (blocks everything)
- **AI (Auto-Increment)**: table-level — brief lock during ID generation

### Additional lock flags (InnoDB-inspired)
- **GAP**: lock on the gap between rows (prevents phantom inserts)
- **REC_NOT_GAP**: lock only on the record, not the gap
- **INSERT_INTENTION**: special gap lock for pending inserts (compatible with other insert intentions)

## Data structures

### LockMode enum
```rust
#[repr(u8)]
pub enum LockMode {
    IntentionShared = 0,
    IntentionExclusive = 1,
    Shared = 2,
    Exclusive = 3,
    AutoIncrement = 4,
}
```

### LockFlags bitfield
```rust
bitflags! {
    pub struct LockFlags: u16 {
        const WAITING         = 0x0001;
        const GAP             = 0x0002;
        const REC_NOT_GAP     = 0x0004;
        const INSERT_INTENTION = 0x0008;
        const TABLE_LOCK      = 0x0010;
    }
}
```

### LockEntry (one per lock grant/wait)
```rust
pub struct LockEntry {
    pub txn_id: TxnId,
    pub mode: LockMode,
    pub flags: LockFlags,
    pub requested_at: Instant,
    // For record locks: bitmap of locked heap_nos on this page
    pub rec_bitmap: Option<Vec<u64>>,  // 1 bit per slot, 64 slots per u64
    // For table locks: table_id
    pub table_id: Option<u32>,
}
```

### LockQueue (per lockable object)
```rust
pub struct LockQueue {
    pub granted: Vec<LockEntry>,    // locks currently held
    pub waiting: VecDeque<LockWaiter>,  // FIFO queue of waiters
}

pub struct LockWaiter {
    pub entry: LockEntry,
    pub notify: Arc<Notify>,  // tokio::sync::Notify to wake waiter
}
```

### LockManager (sharded)
```rust
pub struct LockManager {
    /// Sharded by page_id for record locks, by table_id for table locks.
    record_shards: Box<[RwLock<HashMap<u64, LockQueue>>]>,  // page_id → queue
    table_shards: Box<[RwLock<HashMap<u32, LockQueue>>]>,   // table_id → queue
    num_shards: usize,  // 64 (power of 2)
}
```

## Lock lifecycle

### Acquire record lock

```
1. Compute shard: page_id % num_shards
2. Acquire shard write lock (RwLock::write)
3. Find or create LockQueue for this page_id
4. Check bitmap: is heap_no already locked by this txn in compatible mode?
   → Yes: return immediately (already held)
5. Scan granted list: any conflicting lock from another txn?
   → No conflict: add to granted list, set bitmap bit, return GRANTED
   → Conflict found: create LockWaiter, add to waiting queue
6. Release shard write lock
7. If waiting: await notify (with timeout = lock_wait_timeout)
8. On wake: check if granted or timed out
   → Granted: return GRANTED
   → Timeout: return DbError::LockTimeout
   → Deadlock: return DbError::Deadlock (set by deadlock detector)
```

### Release all locks for transaction (on COMMIT/ROLLBACK)

```
1. For each lock held by this txn (from per-txn lock list):
   a. Compute shard for the lock's page_id/table_id
   b. Acquire shard write lock
   c. Remove LockEntry from granted list
   d. Clear bitmap bits
   e. Try to grant waiting locks:
      - Scan waiting queue head → check conflict with remaining granted
      - If no conflict → move from waiting to granted, notify waiter
   f. Release shard write lock
```

### Table-level intention locks

Before acquiring any row lock, the transaction must hold the appropriate
table-level intention lock:
- Before S row lock → acquire IS on table
- Before X row lock → acquire IX on table
- DDL (DROP/ALTER) → acquire X on table (blocks all DML)

This enables detecting table-level conflicts without scanning all row locks.

## Implicit locks (InnoDB optimization)

For rows in the clustered index, the `RowHeader.txn_id_created` field acts as
an **implicit X-lock** if the transaction is still active:

```
function is_implicitly_locked(row_header, active_txns):
    if row_header.txn_id_created in active_txns:
        return true  // implicitly X-locked by the inserting txn
    return false
```

Before acquiring an explicit lock, check for implicit locks. If found, convert
to explicit (create LockEntry for the owning txn) then queue the new request.

This saves memory: most INSERT rows are never contended, so no lock struct needed.

## Use cases

1. **Two INSERTs, different rows:** Both acquire IX on table (compatible), then X on
   different pages/slots. No conflict → fully parallel.

2. **SELECT and UPDATE, same row:** SELECT acquires IS + S(row). UPDATE acquires IX + X(row).
   S and X conflict → UPDATE waits until SELECT commits.

3. **Two UPDATEs, same row:** Both acquire IX (compatible). Both try X on same row.
   First gets X. Second waits in queue. First commits → second granted.

4. **DDL (DROP TABLE) during DML:** DROP acquires X on table. Conflicts with IS/IX held
   by DML → DROP waits for all DML to finish.

5. **Gap lock prevents phantom:** Txn A scans range [10,20], acquires gap lock on gap
   after row 20. Txn B tries INSERT id=15 → insert intention lock conflicts with gap lock
   → B waits until A commits.

## Acceptance criteria

- [ ] LockManager struct with 64-shard hash table for record locks
- [ ] 5 lock modes: IS, IX, S, X, AI with correct conflict matrix
- [ ] Lock flags: WAITING, GAP, REC_NOT_GAP, INSERT_INTENTION, TABLE_LOCK
- [ ] Per-page bitmap for record locks (1 bit per heap_no)
- [ ] FIFO wait queue per lockable object with async notify
- [ ] Table-level intention locking (IS/IX before row S/X)
- [ ] Lock release grants to first compatible waiter in queue
- [ ] Per-transaction lock list for bulk release on COMMIT/ROLLBACK
- [ ] Implicit lock detection via RowHeader.txn_id_created
- [ ] Lock wait timeout (configurable, default 50s like InnoDB)
- [ ] Shard write lock held only during hash lookup + conflict check (~1-10µs)
- [ ] Unit tests: S/X conflict, IS/IX compatibility, gap lock, wait queue
- [ ] Stress test: 8 txns × 1000 row locks on random rows → no corruption

## Out of scope

- Deadlock detection (that's 40.6)
- Predicate locks / serializable snapshot isolation (future phase)
- Lock escalation (row → page → table)
- Lock compression for bulk operations

## Dependencies

- 40.1 (Atomic TxnId) — for active_txns set
- 40.2 (Per-connection txn) — each connection holds its own lock list
- 40.3 (StorageEngine interior mutability) — page locks vs row locks distinction
