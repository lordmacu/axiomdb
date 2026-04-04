# Spec: 40.9 — FreeList Thread-Safety

## What to build (not how)

Make page allocation and deallocation thread-safe for concurrent transactions.
The current FreeList uses `&mut self` — every allocation serializes on a global lock.
Under 10 concurrent writers, this becomes a 10× bottleneck. Under 100, it's 100×.

The solution is a **two-tier allocation system** (DuckDB-inspired): per-transaction
local batch + global lock-free replenishment. Most allocations happen with zero
contention; global lock acquired only every N allocations.

## Research findings

### InnoDB extent allocation (reference for hierarchy)
- **Three-level**: space header → extent descriptors (64 pages each) → individual pages
- **SX-latch on space header** protects extent-level allocation
- **Segment-private lists**: each table/index has its own free extent pool
- **FSP_FREE_LIMIT**: pages beyond this limit are implicitly free (lazy init)
- Contention reduced by allocating 64 pages per extent lock acquisition

### PostgreSQL FSM (reference for distributed search)
- **Free Space Map**: B-tree of free space categories, lock-free search
- **Per-connection caching**: `bistate->current_buf` tracks last-used page
- **Relation extension lock**: only serialized when adding NEW pages to file
- **Pre-allocation OUTSIDE lock**: victim buffers allocated before acquiring extension lock
  (reduces lock hold time by ~100×)

### DuckDB thread-local batching (chosen primary reference)
- **Thread-local batch**: 128 pre-allocated page IDs per thread, zero-lock allocation
- **Global concurrent queue**: lock-free MPMC queue (Moodycamel) for replenishment
- **Two queues**: `untouched` (never used) + `touched` (freed, ready for reuse)
- **Batch refill**: every 128 allocations, take a batch from global queue
- **Batch return**: every 256 frees, push batch back to global queue
- **Result**: ~128× less lock contention than per-allocation locking

### SQLite (baseline — worst case)
- Single database mutex covers all allocation — no concurrency at all.
- AxiomDB's current model is equivalent to this.

### Performance projections (from research)

| Concurrent txns | Current (Mutex per alloc) | With batching (128/batch) | Improvement |
|---|---|---|---|
| 1 | 2µs/alloc | 2µs/alloc | 1× (no difference) |
| 10 | 20µs/alloc (lock wait) | 2µs/alloc (local) | 10× |
| 100 | 102µs/alloc | 1.5µs/alloc | 68× |

## Design: Two-tier allocation

### Tier 1: Per-transaction local batch (zero contention)

```rust
/// Per-transaction page allocation batch. Stored in ConnectionTxn (from 40.2).
/// Most allocations come from here — no locks, no atomics, pure local state.
pub struct LocalPageBatch {
    /// Pre-allocated page IDs ready for immediate use.
    available: Vec<u64>,
    /// Pages freed by this transaction (returned to global on commit).
    freed: Vec<u64>,
}
```

- `alloc()`: pop from `available` → O(1), zero contention
- `free()`: push to `freed` → O(1), zero contention
- When `available` is empty → refill from global (Tier 2)
- On COMMIT: push `freed` pages to global queue
- On ROLLBACK: push `available` (unused pre-allocated) back to global

### Tier 2: Global concurrent allocator

```rust
/// Global page allocator shared across all transactions.
/// Protected by Mutex but held only during batch operations (~1µs per 64 pages).
pub struct GlobalPageAllocator {
    /// Bitmap of all pages (existing structure, wrapped in Mutex).
    bitmap: Mutex<FreeListBitmap>,
    /// Total pages in database file.
    total_pages: AtomicU64,
    /// Pages freed by committed transactions, ready for reuse.
    recycle_queue: Mutex<VecDeque<u64>>,
    /// File growth lock (separate from bitmap to minimize contention).
    grow_lock: Mutex<()>,
}
```

### Batch constants

```rust
/// Pages allocated per batch refill from global.
const BATCH_ALLOC_SIZE: usize = 64;  // InnoDB extent size
/// Freed pages accumulated before returning to global.
const BATCH_FREE_THRESHOLD: usize = 128;  // DuckDB threshold
```

## Allocation flow

### Fast path (99% of allocations)
```
Transaction calls alloc_page():
  1. Check LocalPageBatch.available
  2. If non-empty → pop page_id → return immediately (0 locks)
  3. Cost: ~10ns
```

### Slow path (every 64 allocations)
```
LocalPageBatch.available is empty:
  1. Acquire GlobalPageAllocator.bitmap Mutex
  2. Scan bitmap for 64 consecutive-ish free pages
  3. Mark all 64 as used in bitmap
  4. Release bitmap Mutex
  5. Fill LocalPageBatch.available with 64 page_ids
  6. Pop first page_id → return
  Cost: ~2µs (amortized: 2µs / 64 = 31ns per alloc)
```

### Free path
```
Transaction calls free_page(page_id):
  1. Push page_id to LocalPageBatch.freed (0 locks)
  2. Cost: ~10ns
```

### Commit path
```
Transaction commits:
  1. Acquire GlobalPageAllocator.recycle_queue Mutex
  2. Push all LocalPageBatch.freed pages to recycle_queue
  3. Release Mutex
  4. Return unused LocalPageBatch.available pages to bitmap
  Cost: ~1µs
```

### Rollback path
```
Transaction rolls back:
  1. Pages in LocalPageBatch.available were pre-allocated but unused
     → return to bitmap (release batch)
  2. Pages in LocalPageBatch.freed were freed by this txn
     → they're actually still in use (rollback undoes the free)
     → do NOT return to recycle_queue
  Cost: ~1µs
```

### File growth
```
No free pages in bitmap AND recycle_queue empty:
  1. Acquire grow_lock Mutex (separate from bitmap)
  2. Extend file by GROW_PAGES (default 1024)
  3. Acquire bitmap Mutex
  4. Extend bitmap to cover new pages
  5. Release bitmap Mutex
  6. Update total_pages (atomic store)
  7. Release grow_lock
  Cost: ~10ms (disk I/O, rare)
```

## Recycled page priority

When refilling a local batch:
1. **First**: drain from `recycle_queue` (freed by committed txns, already on disk)
2. **Second**: scan bitmap for free pages (may require disk zeroing)

Recycled pages are preferred because they're already initialized — no need
to zero-fill or extend the file.

## Concurrency guarantees

| Scenario | Behavior | Mechanism |
|---|---|---|
| 2 txns alloc simultaneously | **Parallel** (each from own local batch) | Per-txn LocalPageBatch |
| 10 txns alloc, all batches empty | **Brief serialization** on bitmap Mutex | Each refills 64 pages, ~2µs each |
| Alloc during free by another txn | **Parallel** | Free goes to local batch, alloc from different local batch |
| Alloc during file growth | **Serialized** on grow_lock | Rare event, <1% of operations |
| Rollback returns pre-allocated pages | **No contention** | Returns to bitmap under Mutex (brief) |
| 100 txns sustained INSERT | **~68× faster than current** | 100 local batches, global refill every 64 allocs |

## Use cases

1. **Single transaction bulk INSERT (10K rows):**
   Allocates ~60 heap pages. First alloc triggers batch of 64 from global.
   Remaining 59 allocs from local batch — zero locks. One bitmap Mutex acquisition total.

2. **10 concurrent transactions, each inserting 1K rows:**
   Each has own local batch. 10 independent batches. Global bitmap accessed
   ~10 times total (once per txn to refill). Virtually zero contention.

3. **Transaction rollback after allocating 5 pages:**
   5 pages were in local batch. On rollback, 5 pages returned to bitmap.
   Other transactions' batches unaffected.

4. **Free + realloc pattern (DELETE then INSERT):**
   DELETE frees pages to local `freed` list. On COMMIT, freed pages go to
   `recycle_queue`. Next INSERT's batch refill picks from recycle_queue first.

## Acceptance criteria

- [ ] `LocalPageBatch` struct with `available: Vec<u64>` and `freed: Vec<u64>`
- [ ] `GlobalPageAllocator` with `Mutex<FreeListBitmap>` + `recycle_queue`
- [ ] Batch allocation: 64 pages per refill from global bitmap
- [ ] Fast path: alloc from local batch in O(1) with zero locks
- [ ] Slow path: bitmap Mutex held only during batch scan (~2µs for 64 pages)
- [ ] Free path: push to local `freed` in O(1) with zero locks
- [ ] Commit: return `freed` to `recycle_queue`, unused `available` to bitmap
- [ ] Rollback: return `available` to bitmap, keep `freed` (rollback undoes frees)
- [ ] File growth: separate `grow_lock`, extend bitmap atomically
- [ ] Recycle priority: prefer `recycle_queue` over bitmap scan for refill
- [ ] No duplicate page allocations under concurrency
- [ ] No lost free pages under concurrency
- [ ] 8 threads allocating simultaneously → all get unique page_ids
- [ ] 8 threads freeing simultaneously → all pages correctly recycled
- [ ] Stress test: 8 threads × 10K alloc+free cycles → bitmap consistent
- [ ] Benchmark: 8 concurrent allocs → ~10× throughput vs current Mutex

## Out of scope

- Lock-free bitmap with AtomicU64 CAS (research shows diminishing returns >4 threads)
- Hierarchical extent allocation (InnoDB-style, future optimization)
- Per-table/per-index segment allocation pools
- NUMA-aware allocation

## Dependencies

- 40.2 (Per-connection txn) — LocalPageBatch lives in ConnectionTxn
- 40.3 (StorageEngine interior mutability) — GlobalPageAllocator replaces Mutex<FreeList>
