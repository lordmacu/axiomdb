# Phase 40 — Concurrent Writers (Multi-Writer Engine)

## Motivation

AxiomDB currently enforces **single-writer**: one `Arc<RwLock<Database>>` means only
one connection can write at a time. All others queue behind the exclusive lock.

```
10 clients doing INSERT simultaneously:
  Client 1:  [====write====]
  Client 2:                  [====write====]
  Client 3:                                  [====write====]
  ...
  Client 10:                                                              [====write====]
  Total: 10× latency, 1× throughput
```

With concurrent writers:
```
  Client 1:  [====write====]
  Client 2:  [====write====]
  Client 3:  [====write====]
  ...
  Client 10: [====write====]
  Total: 1× latency, 10× throughput
```

**Impact**: This is the difference between a toy database and a production database.
MySQL/PostgreSQL support hundreds of concurrent writers. AxiomDB supports exactly 1.

## Blast radius

**184 function signatures** across 25 files take `&mut dyn StorageEngine`.
Every level of the stack enforces single-writer:

1. `Database` — `Arc<RwLock<Database>>` (global exclusive lock for writes)
2. `TxnManager` — `active: Option<ActiveTxn>` (1 transaction at a time)
3. `WalWriter` — single `next_lsn` counter, mutable BufWriter
4. `StorageEngine` — `write_page(&mut self)`, `alloc_page(&mut self)`
5. `FreeList` — `alloc(&mut self)`, `free(&mut self)`
6. `HeapChain` — assumes exclusive page access
7. `BTree` — assumes exclusive node access during insert/delete

## Architecture target

**InnoDB-inspired hybrid**: Row-level locking + MVCC snapshots + group commit WAL.

Not full PostgreSQL partitioned locks (overkill for AxiomDB's scale).
Not SQLite's single-writer (that's what we have now).

### Key design decisions

| Decision | Choice | Rationale |
|---|---|---|
| Lock granularity | **Row-level** (per-page hash) | InnoDB model, proven at scale |
| Deadlock detection | **Eager (Brent's cycle)** | Simpler than PostgreSQL's soft-breaking |
| MVCC | **Per-txn snapshots** (already have RowHeader-based) | Extend existing model |
| WAL concurrency | **Atomic LSN + group buffer** | Like InnoDB's log_write_up_to |
| StorageEngine | **Interior mutability** (Arc-based) | Change trait once, 184 call sites systematic update |
| Transaction state | **Per-connection** (move to SessionContext) | TxnManager becomes global coordinator |

---

## Subfases

### 40.1 — Atomic Transaction ID & Snapshot (Foundation)

**What:** Make `next_txn_id` and `max_committed` atomic. Enable lock-free snapshot creation.

**Changes:**
- `TxnManager.next_txn_id` → `AtomicU64`
- `TxnManager.max_committed` → `AtomicU64`
- `TxnManager::snapshot()` → no `&mut self` needed, just atomic load
- `TxnManager::begin()` → atomic fetch_add for txn_id assignment

**Why first:** Everything else depends on correct atomic transaction IDs.
No functional change yet — single writer still enforced — but the primitives are ready.

**Acceptance criteria:**
- [ ] `next_txn_id` is `AtomicU64` with `fetch_add(1, SeqCst)` in `begin()`
- [ ] `max_committed` is `AtomicU64` with `store(txn_id, Release)` in `commit()`
- [ ] `snapshot()` reads `max_committed` with `load(Acquire)` — no &mut self
- [ ] All existing tests pass unchanged
- [ ] No performance regression (atomics are negligible cost)

### 40.2 — Per-Connection Transaction State

**What:** Move active transaction state out of global TxnManager into per-connection SessionContext.

**Current:** `TxnManager { active: Option<ActiveTxn> }` — 1 active txn globally.
**Target:** Each connection owns its `ActiveTxn`. TxnManager becomes a coordinator.

**New structure:**
```rust
// Per-connection (in SessionContext or connection handler)
struct ConnectionTxn {
    txn_id: TxnId,
    snapshot: TransactionSnapshot,
    undo_ops: Vec<UndoOp>,
    isolation: IsolationLevel,
}

// Global coordinator (shared)
struct TxnCoordinator {
    next_txn_id: AtomicU64,
    max_committed: AtomicU64,
    wal: Arc<WalWriter>,  // shared WAL
    active_txns: RwLock<HashSet<TxnId>>,  // for snapshot visibility
}
```

**Acceptance criteria:**
- [ ] `ActiveTxn` removed from `TxnManager` — lives in connection handler
- [ ] Multiple connections can call `begin()` independently
- [ ] Each connection has its own undo log
- [ ] `TxnManager` tracks active txn set for snapshot visibility
- [ ] Rollback uses per-connection undo log
- [ ] Existing tests adapted to new API

### 40.3 — StorageEngine Interior Mutability

**What:** Change `StorageEngine` trait to support concurrent access.

**Current:** `write_page(&mut self)` — requires exclusive mutable reference.
**Target:** `write_page(&self)` — interior mutability via page-level locks.

**Changes to MmapStorage:**
```rust
struct MmapStorage {
    file: File,                              // pwrite is thread-safe
    mmap: Mmap,                              // read-only, shared
    freelist: Mutex<FreeList>,               // locked during alloc/free
    dirty: PageDirtyTracker,                 // already atomic
    page_locks: PageLockTable,               // NEW: per-page RwLock
}
```

**PageLockTable:** Sharded hash map of `page_id → RwLock<()>`.
- Read operations acquire shared lock on page
- Write operations acquire exclusive lock on page
- Sharded by `page_id % N_SHARDS` to reduce contention

**Blast radius:** 184 call sites change from `&mut dyn StorageEngine` to `&dyn StorageEngine`.
Systematic find-and-replace. No logic changes — just remove `mut`.

**Acceptance criteria:**
- [ ] `StorageEngine::write_page(&self)` — no `&mut`
- [ ] `StorageEngine::alloc_page(&self)` — no `&mut`
- [ ] `MmapStorage` uses interior mutability (Mutex<FreeList>, page locks)
- [ ] `MemoryStorage` adapted similarly (for tests)
- [ ] All 184 call sites updated
- [ ] All existing tests pass
- [ ] Concurrent reads don't block each other
- [ ] Concurrent writes to different pages don't block each other

### 40.4 — Concurrent WAL Writer

**What:** Allow multiple transactions to write WAL entries simultaneously.

**Current:** Single `BufWriter<File>` with mutable `next_lsn`.
**Target:** Atomic LSN assignment + concurrent write buffer.

**Design (InnoDB-inspired):**
```rust
struct ConcurrentWal {
    next_lsn: AtomicU64,
    write_lock: Mutex<BufWriter<File>>,  // serializes actual I/O
    scratch_pool: Vec<Mutex<Vec<u8>>>,   // per-writer scratch buffers
}
```

1. Transaction calls `reserve_lsn()` → atomic `fetch_add` → gets LSN
2. Serializes entry into thread-local scratch buffer
3. Acquires `write_lock` to flush to BufWriter
4. Group commit: if multiple txns waiting, leader flushes all

**Acceptance criteria:**
- [ ] `reserve_lsn()` is lock-free (AtomicU64)
- [ ] WAL entry serialization doesn't hold the write lock
- [ ] Multiple transactions can serialize entries concurrently
- [ ] `write_lock` only held during actual I/O
- [ ] LSN ordering preserved in WAL file
- [ ] Group commit: single fsync covers multiple transactions
- [ ] Crash recovery still works (sequential LSN scan)

### 40.5 — Lock Manager: Row-Level Locks

**What:** Implement a lock manager that supports row-level locking.

**Design (InnoDB-inspired):**
```rust
struct LockManager {
    shards: Vec<RwLock<LockShard>>,  // sharded by page_id
}

struct LockShard {
    locks: HashMap<(u64, u16), LockEntry>,  // (page_id, slot_id) → lock
}

struct LockEntry {
    mode: LockMode,              // S, X, IS, IX
    holders: Vec<TxnId>,         // transactions holding this lock
    waiters: VecDeque<LockWaiter>,  // FIFO wait queue
}

enum LockMode { Shared, Exclusive, IntentionShared, IntentionExclusive }
```

**Lock acquisition flow:**
1. Hash `(page_id, slot_id)` → shard
2. Acquire shard RwLock (read mode for check, write for modify)
3. Check conflict matrix: does requested mode conflict with held modes?
4. If no conflict → grant immediately
5. If conflict → add to wait queue, block

**Acceptance criteria:**
- [ ] LockManager struct with sharded hash table
- [ ] Lock modes: S, X (row-level), IS, IX (table-level)
- [ ] Conflict matrix correctly implemented
- [ ] FIFO wait queue per lock
- [ ] Lock acquisition returns Grant or Wait
- [ ] Lock release wakes next compatible waiter
- [ ] Unit tests: 2 txns on same row, different rows, S/X conflicts

### 40.6 — Deadlock Detection

**What:** Detect and resolve deadlocks when two or more transactions wait for each other.

**Algorithm: Brent's Cycle Detection (InnoDB-style)**
```
When transaction T starts waiting:
  tortoise = T
  hare = T
  while hare can advance in wait-for graph:
    if tortoise == hare → DEADLOCK
    move hare forward (follow wait edge)
    every 2nd step, move tortoise forward
```

**Victim selection:** Abort the transaction with least work done (fewest undo ops).

**Acceptance criteria:**
- [ ] Wait-for graph maintained in LockManager
- [ ] Brent's cycle detection runs on lock wait
- [ ] Deadlock returns `DbError::Deadlock { victim_txn_id }`
- [ ] Victim transaction is rolled back automatically
- [ ] No false positives (phantom deadlocks)
- [ ] Tests: simple A↔B deadlock, 3-way cycle, no deadlock case

### 40.7 — HeapChain Concurrent Access

**What:** Make heap page operations safe for concurrent writers.

**Changes:**
- Insert: acquire page X-lock before modifying
- Delete: acquire page X-lock before marking deleted
- Scan: acquire page S-lock while reading
- Chain growth: atomic alloc_page + page link update

**Key invariant:** Two transactions can insert into the same table simultaneously
if they target different pages. Same-page inserts serialize via page X-lock.

**Acceptance criteria:**
- [ ] Heap insert acquires page X-lock
- [ ] Heap scan acquires page S-lock
- [ ] Chain growth is atomic (new page allocated and linked under lock)
- [ ] Two concurrent inserts to same table succeed
- [ ] Two concurrent inserts to same page serialize correctly
- [ ] No data corruption under concurrent access
- [ ] Stress test: 8 threads × 1000 inserts each

### 40.8 — B-Tree Concurrent Access (Latch Coupling)

**What:** Make B-tree operations safe for concurrent writers using latch coupling.

**Algorithm (top-down latch coupling):**
1. Acquire S-lock on root
2. Read root, find child
3. Acquire S-lock on child
4. Release S-lock on root (coupling)
5. Repeat to leaf
6. At leaf: upgrade to X-lock for modification
7. If split needed: re-traverse with X-locks (pessimistic)

**Acceptance criteria:**
- [ ] B-tree search uses S-lock coupling (read path)
- [ ] B-tree insert uses optimistic coupling (S down, X at leaf)
- [ ] B-tree split re-traverses with X-locks
- [ ] Two concurrent inserts to same index succeed
- [ ] No tree corruption under concurrent splits
- [ ] Stress test: 8 threads × 1000 index inserts each

### 40.9 — FreeList Thread-Safety

**What:** Make page allocation/deallocation thread-safe.

**Simplest approach:** Wrap FreeList in `Mutex<FreeList>`.

**Better approach:** Lock-free bitmap with `AtomicU64` words.
```rust
struct ConcurrentFreeList {
    words: Vec<AtomicU64>,  // each bit = 1 page
    total_pages: AtomicU64,
}

fn alloc(&self) -> Option<u64> {
    for (word_idx, word) in self.words.iter().enumerate() {
        let val = word.load(Relaxed);
        if val != u64::MAX {  // has free bits
            let bit = val.trailing_ones() as u64;
            if word.compare_exchange(val, val | (1 << bit), AcqRel, Relaxed).is_ok() {
                return Some(word_idx as u64 * 64 + bit);
            }
        }
    }
    None
}
```

**Acceptance criteria:**
- [ ] alloc_page() is thread-safe (Mutex or lock-free)
- [ ] free_page() is thread-safe
- [ ] No duplicate page allocations under concurrency
- [ ] No lost free pages
- [ ] Stress test: 8 threads allocating/freeing simultaneously

### 40.10 — Database Lock Redesign

**What:** Replace `Arc<RwLock<Database>>` with fine-grained architecture.

**Current:**
```rust
Arc<RwLock<Database>>  // One global lock for everything
```

**Target:**
```rust
struct SharedDatabase {
    storage: Arc<MmapStorage>,          // interior mutability (40.3)
    wal: Arc<ConcurrentWal>,            // concurrent WAL (40.4)
    txn_coord: Arc<TxnCoordinator>,     // atomic IDs (40.1)
    lock_mgr: Arc<LockManager>,         // row locks (40.5)
    catalog: Arc<RwLock<Catalog>>,       // DDL serialized, DML concurrent
    bloom: Arc<RwLock<BloomRegistry>>,   // lightweight
    status: Arc<StatusRegistry>,         // already atomic
}
```

Each connection gets:
```rust
struct ConnectionState {
    active_txn: Option<ConnectionTxn>,  // per-connection (40.2)
    session: SessionContext,
    shared: Arc<SharedDatabase>,         // shared reference
}
```

**Acceptance criteria:**
- [ ] No global write lock for DML
- [ ] Multiple INSERT/UPDATE/DELETE run concurrently
- [ ] DDL still serialized (via catalog RwLock)
- [ ] Read queries never block write queries
- [ ] Write queries only block on row-level conflicts
- [ ] Connection handler creates ConnectionState per connection

### 40.11 — Executor Refactoring

**What:** Update all 184 executor signatures from `&mut dyn StorageEngine` to `&dyn StorageEngine`.

This is mechanical but large. Systematic find-and-replace across:
- `executor/insert.rs`
- `executor/update.rs`
- `executor/delete.rs`
- `executor/ddl.rs`
- `table.rs`
- `index_maintenance.rs`
- `fk_enforcement.rs`
- `vacuum.rs`

**Also:** Update `execute_query` to work with `ConnectionState` instead of `&mut Database`.

**Acceptance criteria:**
- [ ] Zero `&mut dyn StorageEngine` in executor code
- [ ] All DML operations work through `&dyn StorageEngine`
- [ ] Lock acquisition happens at executor level (before page access)
- [ ] All existing tests pass with new API
- [ ] Wire protocol smoke test passes

### 40.12 — Integration Tests & Benchmarks

**What:** Validate concurrent writers end-to-end.

**Tests:**
1. 2 clients INSERT into same table simultaneously — both succeed
2. 2 clients UPDATE same row — one waits, then succeeds
3. 2 clients UPDATE different rows — both run concurrently
4. Deadlock scenario — one aborted, other succeeds
5. 10 clients mixed workload — no corruption
6. Long transaction + short transactions — isolation correct
7. Crash during concurrent writes — recovery correct

**Benchmarks:**
```bash
# Single writer (baseline)
python3 benches/comparison/local_bench.py --scenario insert_autocommit --rows 1000

# Multi-writer (new)
python3 benches/comparison/concurrent_bench.py --clients 8 --scenario insert --rows 1000
```

**Target:** 8 concurrent writers should achieve ~4-6× throughput of single writer.

**Acceptance criteria:**
- [ ] All concurrent test scenarios pass
- [ ] No data corruption under stress
- [ ] Measurable throughput improvement with multiple clients
- [ ] insert_autocommit benchmark improves from 🔴 0.62x
- [ ] Wire protocol smoke test with concurrent connections

## Dependencies

- Phase 39 (Clustered Index) should be complete first — the clustered B-tree
  needs its own latch protocol, easier to design when format is finalized.
- Phase 3 (WAL) ✅
- Phase 7 (MVCC basics) ✅

## Estimated effort

| Subfase | Complexity | Estimated time |
|---|---|---|
| 40.1 Atomic TxnId | low | 1 day |
| 40.2 Per-connection txn | high | 3-5 days |
| 40.3 StorageEngine interior mut | high | 3-5 days (184 call sites) |
| 40.4 Concurrent WAL | max | 5-7 days |
| 40.5 Lock Manager | max | 5-7 days |
| 40.6 Deadlock Detection | high | 3-4 days |
| 40.7 HeapChain concurrent | high | 3-5 days |
| 40.8 B-Tree latch coupling | max | 5-7 days |
| 40.9 FreeList thread-safe | medium | 1-2 days |
| 40.10 Database lock redesign | high | 3-5 days |
| 40.11 Executor refactoring | medium (mechanical) | 3-5 days |
| 40.12 Integration tests | medium | 3-5 days |
| **Total** | | **~40-60 days** |
