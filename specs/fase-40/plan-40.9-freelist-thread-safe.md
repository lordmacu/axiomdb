# Plan: 40.9 — FreeList Thread-Safety

## Files to create/modify

| File | Change |
|---|---|
| `crates/axiomdb-storage/src/page_alloc.rs` | **NEW**: GlobalPageAllocator + LocalPageBatch |
| `crates/axiomdb-storage/src/freelist.rs` | Rename to FreeListBitmap (internal, wrapped by GlobalPageAllocator) |
| `crates/axiomdb-storage/src/mmap.rs` | Use GlobalPageAllocator instead of raw FreeList |
| `crates/axiomdb-storage/src/memory.rs` | Same for MemoryStorage |
| `crates/axiomdb-storage/src/lib.rs` | Export new module |
| `crates/axiomdb-wal/src/txn.rs` | LocalPageBatch in ConnectionTxn |

## Implementation phases

### Phase 1: LocalPageBatch struct
```rust
pub struct LocalPageBatch {
    available: Vec<u64>,   // pre-allocated, ready for use
    freed: Vec<u64>,       // freed by this txn, pending recycle
}

impl LocalPageBatch {
    pub fn alloc(&mut self) -> Option<u64> {
        self.available.pop()  // O(1), zero locks
    }

    pub fn free(&mut self, page_id: u64) {
        self.freed.push(page_id);  // O(1), zero locks
    }

    pub fn is_empty(&self) -> bool {
        self.available.is_empty()
    }

    /// On commit: return freed pages to global recycle queue.
    pub fn drain_freed(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.freed)
    }

    /// On rollback: return available (unused pre-allocated) to global bitmap.
    pub fn drain_available(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.available)
    }

    /// Refill available from a batch of page_ids.
    pub fn refill(&mut self, pages: Vec<u64>) {
        self.available = pages;
    }
}
```

### Phase 2: GlobalPageAllocator struct
```rust
pub struct GlobalPageAllocator {
    bitmap: Mutex<FreeListBitmap>,
    recycle_queue: Mutex<VecDeque<u64>>,
    total_pages: AtomicU64,
    grow_lock: Mutex<()>,
}

impl GlobalPageAllocator {
    /// Allocate a batch of up to `count` pages from recycle queue + bitmap.
    pub fn alloc_batch(&self, count: usize) -> Result<Vec<u64>, DbError> {
        let mut result = Vec::with_capacity(count);

        // 1. Drain from recycle queue first (preferred — already initialized)
        {
            let mut recycle = self.recycle_queue.lock().unwrap();
            while result.len() < count {
                match recycle.pop_front() {
                    Some(pid) => result.push(pid),
                    None => break,
                }
            }
        }

        // 2. Scan bitmap for remaining needed
        if result.len() < count {
            let mut bitmap = self.bitmap.lock().unwrap();
            while result.len() < count {
                match bitmap.alloc() {
                    Some(pid) => result.push(pid),
                    None => break,  // need file growth
                }
            }
        }

        if result.is_empty() {
            // 3. File growth needed
            self.grow()?;
            return self.alloc_batch(count);  // retry after growth
        }

        Ok(result)
    }

    /// Return freed pages to recycle queue (called on commit).
    pub fn recycle_pages(&self, pages: Vec<u64>) {
        let mut recycle = self.recycle_queue.lock().unwrap();
        recycle.extend(pages);
    }

    /// Return unused pre-allocated pages to bitmap (called on rollback).
    pub fn return_pages(&self, pages: Vec<u64>) {
        let mut bitmap = self.bitmap.lock().unwrap();
        for pid in pages {
            bitmap.free(pid);
        }
    }

    fn grow(&self) -> Result<(), DbError> {
        let _grow_guard = self.grow_lock.lock().unwrap();
        // ... extend file, extend bitmap ...
        Ok(())
    }
}
```

### Phase 3: StorageEngine alloc_page integration
```rust
// In MmapStorage (after 40.3 interior mutability):
impl StorageEngine for MmapStorage {
    fn alloc_page(&self, page_type: PageType) -> Result<u64, DbError> {
        // Note: LocalPageBatch is NOT here — it's per-transaction.
        // StorageEngine.alloc_page() always goes through global.
        // The executor wraps this with LocalPageBatch.
        let pages = self.allocator.alloc_batch(1)?;
        let page_id = pages[0];
        // ... initialize page ...
        Ok(page_id)
    }
}
```

**Higher-level integration (executor):**
```rust
// In executor, when inserting:
fn alloc_page_for_txn(
    local: &mut LocalPageBatch,
    global: &GlobalPageAllocator,
    storage: &dyn StorageEngine,
    page_type: PageType,
) -> Result<u64, DbError> {
    // Fast path: local batch
    if let Some(pid) = local.alloc() {
        return Ok(pid);
    }
    // Slow path: refill from global
    let batch = global.alloc_batch(BATCH_ALLOC_SIZE)?;
    local.refill(batch);
    local.alloc().ok_or(DbError::Other("alloc failed after refill".into()))
}
```

### Phase 4: Commit/rollback integration
```rust
// On COMMIT:
fn commit_page_batch(local: &mut LocalPageBatch, global: &GlobalPageAllocator) {
    // Freed pages → recycle queue (other txns can use them)
    let freed = local.drain_freed();
    if !freed.is_empty() {
        global.recycle_pages(freed);
    }
    // Unused pre-allocated pages → return to bitmap
    let unused = local.drain_available();
    if !unused.is_empty() {
        global.return_pages(unused);
    }
}

// On ROLLBACK:
fn rollback_page_batch(local: &mut LocalPageBatch, global: &GlobalPageAllocator) {
    // Unused pre-allocated → return to bitmap
    let unused = local.drain_available();
    if !unused.is_empty() {
        global.return_pages(unused);
    }
    // Freed pages → do NOT recycle (rollback undoes the frees)
    local.freed.clear();
}
```

### Phase 5: FreeListBitmap (rename existing)
- Rename `FreeList` → `FreeListBitmap` (internal detail, not public API)
- No logic changes — just wrapped by GlobalPageAllocator
- Add `alloc_batch(n: usize) -> Vec<u64>` convenience method
  (scan for N free bits at once, fewer iterations than N × alloc())

### Phase 6: MemoryStorage adaptation
Same pattern: GlobalPageAllocator wraps an in-memory bitmap.
Tests use MemoryStorage — must behave identically.

## Tests to write

1. **LocalPageBatch unit**: alloc/free cycle, drain_freed, drain_available, refill
2. **GlobalPageAllocator unit**: alloc_batch, recycle_pages, return_pages
3. **Recycle priority**: freed pages appear in next alloc_batch before bitmap scan
4. **No duplicates**: 8 threads × alloc_batch(64) → all 512 page_ids unique
5. **Rollback returns pages**: alloc batch → rollback → pages available again
6. **Commit recycles freed**: free pages → commit → next batch gets recycled pages
7. **File growth**: exhaust bitmap → alloc triggers grow → new pages available
8. **Concurrent growth**: 2 threads trigger grow simultaneously → only 1 grows (grow_lock)
9. **Stress**: 8 threads × 10K alloc+free cycles → bitmap consistent, no lost pages
10. **Benchmark**: 8 concurrent alloc_batch vs 8 concurrent single alloc → measure speedup

## Anti-patterns to avoid

- DO NOT use AtomicU64 CAS on bitmap words (research shows worse than Mutex at >4 threads)
- DO NOT hold bitmap Mutex during file I/O (grow acquires grow_lock first, then bitmap)
- DO NOT keep local batch across transaction boundary (must drain on commit/rollback)
- DO NOT skip recycle queue check (recycled pages are already initialized = faster)
- DO NOT allocate pages to local batch that exceed total_pages (check before returning)

## Risks

- **Batch waste**: if txn allocates 1 page but batch is 64, 63 pages are "reserved" until
  commit/rollback. Mitigation: other txns still allocate from bitmap (local batch is advisory,
  not exclusive). On commit, unused pages returned immediately.
- **Recycle queue growth**: if many txns free pages but few allocate, queue grows.
  Mitigation: bounded queue (max 10K entries), excess returned to bitmap.
- **Lock ordering with PageLockTable**: GlobalPageAllocator.bitmap Mutex must NEVER be held
  while acquiring a page X-latch. Order: page latch < bitmap Mutex (same as 40.7).
