# Specs: 40.7 — 40.12 (Remaining Subfases)

---

## 40.7 — HeapChain Concurrent Access

### What to build
Make heap page operations safe for concurrent transactions by integrating with
the Lock Manager (40.5) and StorageEngine interior mutability (40.3).

### Key changes
- INSERT into heap: acquire IX(table) + X(row) via LockManager before page write
- DELETE from heap: acquire IX(table) + X(row) before setting txn_id_deleted
- SCAN (SELECT): acquire IS(table) + S(row) per visible row (or table-level S for full scan)
- Chain growth (new page allocation): acquire page-level exclusive lock during chain extension
- Concurrent inserts to different pages: fully parallel (different page locks)
- Concurrent inserts to same page: serialized by page X-lock from StorageEngine (40.3)

### Acceptance criteria
- [ ] Heap INSERT acquires row lock before page modification
- [ ] Heap DELETE acquires row lock before marking deleted
- [ ] Heap SCAN acquires table IS + row S for each visible row
- [ ] Chain growth atomic (alloc + link under exclusive lock)
- [ ] 4 threads × 1000 inserts to same table → no corruption
- [ ] 2 threads insert to different pages → parallel execution
- [ ] All existing heap tests pass

---

## 40.8 — B-Tree Latch Coupling

### What to build
Make B-tree index operations safe for concurrent transactions using top-down
latch coupling protocol (InnoDB/Lehman-Yao inspired).

### Latch coupling protocol
**Read path (search/range scan):**
1. Acquire S-latch on root page
2. Read root, find child
3. Acquire S-latch on child page
4. Release S-latch on root (coupling: hold child, release parent)
5. Repeat down to leaf
6. At leaf: read data under S-latch, release when done

**Write path (insert/delete) — optimistic:**
1. Acquire S-latch on root (optimistic: assume no split)
2. Descend with S-latch coupling to leaf
3. At leaf: upgrade S→X latch
4. If leaf has space → modify under X-latch → done
5. If split needed → release all, re-traverse pessimistically

**Write path — pessimistic (split needed):**
1. Acquire X-latch on root
2. Descend with X-latch coupling (hold parent X while acquiring child X)
3. At leaf: modify (split if needed)
4. Split propagates up — parent already X-latched
5. Release all X-latches bottom-up

### Key changes
- Each B-tree page gets a RwLock (from PageLockTable in 40.3)
- Insert: optimistic S→X upgrade at leaf; pessimistic X-coupling if split
- Delete: same pattern (optimistic first, pessimistic if merge)
- Range scan: S-latch coupling, release page latch before moving to next leaf

### Acceptance criteria
- [ ] B-tree search uses S-latch coupling (read path safe for concurrent writers)
- [ ] B-tree insert: optimistic S→X at leaf; pessimistic X-coupling for splits
- [ ] B-tree delete: optimistic S→X at leaf; pessimistic X-coupling for merges
- [ ] Two concurrent inserts to same index → both succeed, tree valid
- [ ] No tree corruption under concurrent splits
- [ ] Range scan works during concurrent inserts (no phantom reads within snapshot)
- [ ] Stress test: 8 threads × 1000 index inserts → tree structure valid

---

## 40.9 — FreeList Thread-Safety

### What to build
Make page allocation and deallocation thread-safe for concurrent transactions.

### Two options (choose based on contention profile)

**Option A: Mutex<FreeList> (simple, sufficient for most workloads)**
- Wrap existing FreeList in `Mutex<FreeList>`
- alloc() and free() acquire Mutex briefly (~1µs)
- Contention only under extreme alloc pressure (rare)

**Option B: Lock-free bitmap (maximum throughput)**
```rust
struct ConcurrentFreeList {
    words: Vec<AtomicU64>,
    total_pages: AtomicU64,
}

fn alloc(&self) -> Option<u64> {
    for (i, word) in self.words.iter().enumerate() {
        loop {
            let val = word.load(Relaxed);
            if val == u64::MAX { break; } // all bits set (all used)
            let bit = val.trailing_zeros() as u64; // first free bit
            let new_val = val | (1 << bit);
            if word.compare_exchange_weak(val, new_val, AcqRel, Relaxed).is_ok() {
                return Some(i as u64 * 64 + bit);
            }
            // CAS failed → retry (another thread allocated from same word)
        }
    }
    None // no free pages
}
```

### Acceptance criteria
- [ ] alloc_page() is thread-safe (no duplicate allocations)
- [ ] free_page() is thread-safe (no lost free pages)
- [ ] 8 threads allocating simultaneously → all get unique page_ids
- [ ] 8 threads freeing simultaneously → all pages correctly freed
- [ ] No ABA problem on concurrent alloc + free of same page

---

## 40.10 — Database Lock Redesign

### What to build
Replace `Arc<RwLock<Database>>` with `Arc<SharedDatabase>` that uses interior
mutability throughout. This is where everything from 40.1-40.9 comes together.

### New architecture
```rust
pub struct SharedDatabase {
    pub storage: Arc<MmapStorage>,            // interior mutability (40.3)
    pub wal: Arc<ConcurrentWalWriter>,        // concurrent WAL (40.4)
    pub txn_coord: Arc<TxnCoordinator>,       // atomic IDs (40.1)
    pub lock_mgr: Arc<LockManager>,           // row locks (40.5)
    pub catalog: Arc<RwLock<CatalogState>>,    // DDL serialized
    pub bloom: Arc<RwLock<BloomRegistry>>,     // lightweight
    pub status: Arc<StatusRegistry>,           // already atomic
    pub schema_version: Arc<AtomicU64>,        // already atomic
}

pub struct ConnectionState {
    pub active_txn: Option<ConnectionTxn>,     // per-connection (40.2)
    pub session: SessionContext,
    pub shared: Arc<SharedDatabase>,
}
```

### Connection handler changes
- On new connection: create ConnectionState with Arc::clone(shared)
- On query: no global lock for DML — use LockManager for row-level control
- On DDL (CREATE/DROP): acquire catalog write lock (serializes DDL only)
- On SELECT: no write lock at all — pure reader

### Acceptance criteria
- [ ] No `Arc<RwLock<Database>>` — replaced with `Arc<SharedDatabase>`
- [ ] DML queries do NOT acquire global write lock
- [ ] DDL queries acquire catalog RwLock (write mode)
- [ ] Multiple INSERT/UPDATE/DELETE run concurrently
- [ ] SELECT never blocks and never blocked by DML
- [ ] Connection handler manages per-connection state

---

## 40.11 — Executor Refactoring

### What to build
Update all 184+ executor signatures from `&mut dyn StorageEngine` to
`&dyn StorageEngine`, and integrate lock acquisition at the DML level.

### Key patterns
```rust
// BEFORE
fn execute_insert(storage: &mut dyn StorageEngine, txn: &mut TxnManager, ...) { ... }

// AFTER
fn execute_insert(storage: &dyn StorageEngine, txn: &ConnectionTxn,
                  lock_mgr: &LockManager, ...) { ... }
```

### Lock acquisition points in executor
- INSERT: `lock_mgr.acquire(IX, table)` + `lock_mgr.acquire(X, row)` before heap insert
- UPDATE: `lock_mgr.acquire(IX, table)` + `lock_mgr.acquire(X, row)` before heap modify
- DELETE: `lock_mgr.acquire(IX, table)` + `lock_mgr.acquire(X, row)` before marking deleted
- SELECT: `lock_mgr.acquire(IS, table)` before scan (row S-locks optional for read committed)
- DDL: `lock_mgr.acquire(X, table)` before schema change

### Acceptance criteria
- [ ] Zero `&mut dyn StorageEngine` in executor code
- [ ] Zero `&mut TxnManager` in executor code (uses `&ConnectionTxn` or `&TxnCoordinator`)
- [ ] Lock acquisition integrated at statement level
- [ ] All existing tests adapted and passing
- [ ] Wire protocol smoke test passes with new executor signatures

---

## 40.12 — Integration Tests & Benchmarks

### What to build
End-to-end validation that concurrent writers work correctly and performantly.

### Test scenarios (minimum 15 tests)
1. 2 clients INSERT into same table simultaneously → both succeed
2. 2 clients UPDATE same row → one waits, then succeeds
3. 2 clients UPDATE different rows in same table → both run concurrently
4. 2 clients DELETE different rows → both succeed
5. Simple A↔B deadlock → one aborted, other succeeds, aborted can retry
6. 3-way deadlock cycle → one victim, others succeed
7. 10 clients mixed INSERT/UPDATE/DELETE → no corruption
8. Long transaction + short transactions → isolation correct (MVCC)
9. DDL (CREATE TABLE) during DML → DDL waits for DML to finish
10. DDL (DROP TABLE) during SELECT → DROP waits for SELECT to finish
11. Autocommit INSERT stress: 8 clients × 1000 rows → all inserted
12. Explicit transaction: BEGIN → multiple DML → COMMIT across 2 clients
13. ROLLBACK releases locks → waiting txn proceeds
14. Lock timeout: client waits > 50s → LockTimeout error
15. Crash during concurrent writes → recovery correct, no corruption

### Benchmarks
```bash
# Concurrent INSERT throughput
python3 benches/comparison/concurrent_bench.py --clients 1,2,4,8 --scenario insert --rows 10000

# Concurrent UPDATE contention
python3 benches/comparison/concurrent_bench.py --clients 1,2,4,8 --scenario update_same_table --rows 10000

# Mixed workload
python3 benches/comparison/concurrent_bench.py --clients 8 --scenario mixed --rows 10000
```

### Target metrics
- 8 concurrent writers: **4-6× throughput** vs single writer
- insert_autocommit: improve from 🔴 0.62x (group commit benefits from real concurrency)
- Deadlock rate: < 1% under random row ordering
- Lock wait time p99: < 100ms for non-deadlock cases

### Acceptance criteria
- [ ] All 15+ test scenarios pass
- [ ] No data corruption under concurrent stress
- [ ] Measurable throughput improvement with multiple clients
- [ ] Deadlocks detected and resolved correctly
- [ ] Crash recovery works after concurrent writes
- [ ] Wire protocol smoke test with concurrent connections
- [ ] Benchmark results documented in docs/fase-40.md
