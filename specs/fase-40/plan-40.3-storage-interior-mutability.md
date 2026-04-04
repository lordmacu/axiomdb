# Plan: 40.3 — StorageEngine Interior Mutability

## Files to create/modify

| File | Change |
|---|---|
| `crates/axiomdb-storage/src/engine.rs` | Change trait: all `&mut self` → `&self` |
| `crates/axiomdb-storage/src/page_lock.rs` | **NEW**: PageLockTable (64-shard per-page RwLock) |
| `crates/axiomdb-storage/src/mmap.rs` | Interior mutability: Mutex<FreeList>, AtomicBool, PageLockTable |
| `crates/axiomdb-storage/src/memory.rs` | Same pattern for MemoryStorage |
| `crates/axiomdb-storage/src/lib.rs` | Export new module |
| `crates/axiomdb-storage/src/freelist.rs` | No changes (Mutex wraps externally) |
| `crates/axiomdb-storage/src/heap.rs` | `&mut dyn StorageEngine` → `&dyn StorageEngine` |
| `crates/axiomdb-storage/src/heap_chain.rs` | ~43 signatures: `&mut` → `&` |
| `crates/axiomdb-index/src/tree.rs` | ~25 signatures: `&mut` → `&` |
| `crates/axiomdb-wal/src/txn.rs` | ~11 signatures involving storage |
| `crates/axiomdb-wal/src/checkpoint.rs` | Storage signatures |
| `crates/axiomdb-wal/src/recovery.rs` | Storage signatures |
| `crates/axiomdb-catalog/src/bootstrap.rs` | Storage signatures |
| `crates/axiomdb-sql/src/executor/*.rs` | ~100 signatures |
| `crates/axiomdb-sql/src/table.rs` | Storage signatures |
| `crates/axiomdb-sql/src/index_maintenance.rs` | Storage signatures |
| `crates/axiomdb-sql/src/fk_enforcement.rs` | Storage signatures |
| `crates/axiomdb-sql/src/vacuum.rs` | Storage signatures |
| All test files | `&mut` → `&` on storage refs |

## Implementation phases

### Phase 1: PageLockTable (new file)
Create `crates/axiomdb-storage/src/page_lock.rs`:
- 64-shard structure (power of 2 for fast modulo)
- Each shard: `parking_lot::RwLock<HashMap<u64, Arc<parking_lot::RwLock<()>>>>`
  (or std::sync::RwLock if parking_lot not available)
- `read(page_id)` → shared guard
- `write(page_id)` → exclusive guard
- Lazy lock creation per page
- Unit tests: concurrent read/write on same/different pages

### Phase 2: MmapStorage interior mutability
Wrap mutable fields:
- `freelist: FreeList` → `freelist: Mutex<FreeList>`
- `freelist_dirty: bool` → `freelist_dirty: AtomicBool`
- `deferred_frees: Vec<...>` → `deferred_frees: Mutex<Vec<...>>`
- `current_snapshot_id: u64` → `current_snapshot_id: AtomicU64`
- Add `page_locks: PageLockTable`
- Add `grow_lock: Mutex<()>` (prevents concurrent file extension)

### Phase 3: Change StorageEngine trait
All `&mut self` → `&self`. This breaks compilation across all crates.

### Phase 4: Fix MmapStorage impl
Update all method implementations to use interior mutability:
- write_page: acquire page lock, pwrite, mark dirty
- alloc_page: mutex lock freelist, alloc, page lock, init page
- free_page: mutex lock freelist, free
- flush: mutex lock freelist (briefly), sync file

### Phase 5: Fix MemoryStorage impl
Same pattern for test storage:
- `pages: RwLock<BTreeMap<u64, Page>>`
- write_page: write lock on pages map
- alloc_page: write lock, insert new page

### Phase 6: Fix all 190 call sites
Systematic across all crates (largest phase):
1. `crates/axiomdb-storage/` — heap.rs, heap_chain.rs (~43 sites)
2. `crates/axiomdb-index/` — tree.rs (~25 sites)
3. `crates/axiomdb-wal/` — txn.rs, checkpoint.rs, recovery.rs (~11 sites)
4. `crates/axiomdb-catalog/` — bootstrap.rs (~5 sites)
5. `crates/axiomdb-sql/` — executor/*.rs, table.rs, etc (~100 sites)
6. Test files

Pattern: `storage: &mut dyn StorageEngine` → `storage: &dyn StorageEngine`
No logic changes — just remove `mut`.

### Phase 7: Verify
- `cargo test --workspace` — all tests pass
- `cargo clippy --workspace -- -D warnings` — no warnings
- Stress test: 4 threads writing to MmapStorage concurrently

## Lock ordering (deadlock prevention)

```
RULE: page_lock < freelist_mutex < grow_lock

write_page:  acquires page_lock only       → safe
alloc_page:  acquires freelist → page_lock → safe (freelist < page_lock)
free_page:   acquires freelist only        → safe
flush:       acquires freelist only        → safe
grow:        acquires grow_lock → freelist → safe (grow < freelist)

NO function acquires page_lock then freelist → no cycle possible.
```

## Tests to write

1. **PageLockTable unit tests:**
   - Concurrent shared reads (4 threads, same page) → all succeed
   - Exclusive write blocks shared reads (2 threads, same page)
   - Different pages don't block each other
   - 1000 pages, random concurrent access → no panic

2. **MmapStorage concurrency tests:**
   - 4 threads × 100 write_page to different pages → all succeed, data intact
   - 2 threads write same page → serialized, last write wins
   - alloc_page under contention → no duplicate page IDs
   - flush while write in progress → no corruption

3. **All existing tests pass unchanged** (most critical validation)

## Anti-patterns to avoid

- DO NOT wrap entire MmapStorage in a single Mutex — defeats the purpose
- DO NOT hold page lock during freelist operations — lock ordering violation
- DO NOT hold freelist mutex during pwrite — unnecessary contention
- DO NOT use `unsafe` for interior mutability — use Mutex/AtomicU64/RwLock
- DO NOT change any DML/query logic — this phase is ONLY about storage access patterns
- DO NOT skip MemoryStorage update — tests use MemoryStorage, must behave identically

## Risks

- **190 call sites**: Largest mechanical change in the project. Mitigation: do it crate by
  crate, compile after each crate. Start with axiomdb-storage (most isolated), then index,
  then wal, then catalog, then sql.
- **Mutex poisoning**: If a thread panics while holding Mutex, subsequent acquires fail.
  Mitigation: use `parking_lot::Mutex` (no poisoning) or handle PoisonError.
- **RwLock fairness**: std::sync::RwLock has no fairness guarantee (writers can starve).
  Mitigation: use `parking_lot::RwLock` (write-preferring) for page locks.
