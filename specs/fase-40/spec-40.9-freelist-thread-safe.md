# Spec: 40.9 — FreeList Thread-Safety

## What to build (not how)

Reduce page allocation contention for concurrent transactions via a two-tier
allocation system.

> **Status after 40.3 (done):** Tier 2 — the global `Mutex<FreeList>` — is already
> implemented in MmapStorage: `freelist: Mutex<FreeList>`, `grow_lock: Mutex<()>`,
> `freelist_dirty: AtomicBool`. Every allocation now serializes on a brief Mutex
> acquisition rather than requiring `&mut StorageEngine`.
>
> **What remains for 40.9:** Tier 1 — the per-transaction `LocalPageBatch` that
> eliminates the global Mutex for 99% of allocations by batching 64 pages per refill.
> This subfase adds `LocalPageBatch` to `ConnectionTxn` (40.4b) and wires the fast
> path through the executor.

The solution is a **two-tier allocation system** (DuckDB-inspired): per-transaction
local batch + global lock-free replenishment. Tier 2 (global Mutex<FreeList>) is
already done in 40.3. This spec covers Tier 1 only.

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

## Implementation contract (locked in /spec-task on 2026-04-09)

The high-level design above is preserved. The contract was strengthened
during brainstorm by reading two reference systems in `research/`:

### Research synthesis (what we borrow / reject / adapt)

#### PostgreSQL `RelationAddBlocks` (`research/postgresql/src/backend/access/heap/hio.c:236-360`)

**Borrowed:**
- **Adaptive batch sizing** — `extend_by_pages += extend_by_pages × waiter_count` so the next refill grabs more pages when the freelist is contended. AxiomDB starts at `BATCH_ALLOC_SIZE = 64` and caps at `MAX_BATCH_SIZE = 256`.
- **Sticky bulk mode** — the `bistate->already_extended_by` field is mirrored as `LocalPageBatch::last_refill_size` so a connection in bulk-INSERT mode reuses the same batch size on every refill (avoids file-extension thrash that hurts ext4/xfs allocation patterns).
- **Steal protection** — when committing, leftover `LocalPageBatch.available` pages go back to the **bitmap**, not the global recycle queue, so other connections can't grab them. The pages we genuinely freed during the txn (`LocalPageBatch.freed`) go to the recycle queue for fast reuse. PostgreSQL's "only put leftover pages in the FSM, keep mine private" idea, adapted.
- **Sequential locality** — `mdzeroextend()` always extends the file in contiguous chunks. AxiomDB adds `FreeList::alloc_batch_sequential(n)` which scans words for runs of `n` free bits before falling back to non-contiguous fill, so refilled batches are OS-prefetch friendly.

**Rejected:**
- PostgreSQL's `extension lock` is a separate `LWLock` from the buffer mapping locks. We reuse `MmapStorage::freelist: Mutex<FreeList>` plus a new `extension_waiters: AtomicU32` counter. Adding a dedicated `LWLock` is overkill for the current single-mutex storage layer.
- PostgreSQL puts unused pages into the durable FSM. AxiomDB keeps the recycle queue **in memory** (see DuckDB note below).

#### DuckDB `SingleFileBlockManager` (`research/duckdb/src/storage/single_file_block_manager.cpp:829-893`)

**Borrowed:**
- The **5-state model** as a mental framework. AxiomDB collapses it to 3 explicit states because we don't need DuckDB's checkpoint-iteration semantics:
  - `bitmap FREE` ≡ DuckDB `free_list`
  - `bitmap USED + recycle_queue` ≡ DuckDB `free_blocks_in_use` + `modified_blocks`
  - `bitmap USED + in-use` ≡ DuckDB `newly_used_blocks` + `multi_use_blocks`
- The **`max_block++` always-fresh** path (when refilling from a grown file segment, just hand out sequential IDs without consulting the bitmap).
- The **single-mutex-per-batch** acquisition pattern: hold the freelist `Mutex` once per batch, not once per page.

**Rejected:**
- DuckDB's `set<block_id_t>` data structure for the free list. AxiomDB's bitmap is much more compact (1 bit/page vs ~24 bytes/page for a `set<u64>`) and the on-disk format is a bitmap already, so we'd pay extra to convert.
- DuckDB's claim of "thread-local 128-page batch" referenced in the original spec. **It does not exist in `single_file_block_manager.cpp`** — DuckDB uses a single mutex. The thread-local idea was a misattribution. We adopt it anyway because the per-`ConnectionTxn` `LocalPageBatch` is a strictly better fit for AxiomDB's per-connection executor model than what DuckDB actually does.

### Approach: storage trait extension + lock-free recycle queue

- `StorageEngine` gains two new methods with default impls that fall back to
  the existing per-page `alloc_page` / `free_page`:

  ```rust
  fn alloc_page_batch(
      &self,
      n: usize,
      page_type: PageType,
  ) -> Result<Vec<u64>, DbError>;

  fn free_page_batch(&self, ids: &[u64]) -> Result<(), DbError>;
  ```

  `MmapStorage` overrides both. `MemoryStorage` keeps the default impls
  (fine for tests; no contention there).

- `alloc_page_batch` returns **uninitialized page IDs**. The caller is still
  responsible for `Page::new(page_type, pid) + write_page(pid, &p)` at the
  point of first use, exactly as today's `alloc_page` callers already do.

- `MmapStorage::alloc_page_batch` algorithm:

  ```text
  1. Drain up to N from recycle_queue (lock-free SegQueue pops).
  2. If still need more, lock freelist Mutex once and:
     a. Try alloc_batch_sequential(remaining)  → contiguous IDs preferred
     b. Fall back to alloc() per page if no contiguous run
  3. If freelist exhausted: drop freelist lock, take grow_lock,
     do_grow(GROW_PAGES), re-acquire freelist lock, retry from (2).
  4. Return Vec<u64> of size exactly N (or error).
  ```

  Adaptive sizing: the `n` argument is what the caller asks for. The caller
  (`pop_or_refill`) computes it as `LocalPageBatch.last_refill_size.max(BATCH_ALLOC_SIZE) × max(1, extension_waiters.load())` capped at `MAX_BATCH_SIZE`.

- `LocalPageBatch` lives in `ConnectionTxn` (`axiomdb-wal/src/txn.rs`):

  ```rust
  pub struct LocalPageBatch {
      /// Pre-allocated page IDs ready for immediate use.
      /// Homogeneous in `current_type` to match PostgreSQL's bistate model.
      pub(crate) available: VecDeque<u64>,
      /// Page type currently held in `available`. A type mismatch on
      /// pop_or_refill drains `available` back to the recycle_queue and
      /// refills with the new type.
      pub(crate) current_type: Option<PageType>,
      /// Pages freed by this transaction (returned at commit).
      pub(crate) freed: Vec<u64>,
      /// Last refill batch size, used for "sticky bulk mode" — once a
      /// connection refills a large batch, subsequent refills stay large.
      pub(crate) last_refill_size: usize,
  }
  ```

  Not `Send + Sync`. Owned by exactly one connection at a time.

- Hot-path storage helpers gain an `Option<&mut LocalPageBatch>` parameter
  and route through a small `pop_or_refill(storage, batch, ty)` helper:

  ```rust
  pub fn pop_or_refill(
      storage: &dyn StorageEngine,
      batch: Option<&mut LocalPageBatch>,
      ty: PageType,
  ) -> Result<u64, DbError>;
  ```

  When `batch` is `None`, `pop_or_refill` falls through to
  `storage.alloc_page(ty)` directly. When `batch` is `Some`:

  ```text
  1. If batch.current_type != Some(ty):
     a. Drain batch.available back to storage.free_page_batch (return to bitmap).
     b. Set batch.current_type = Some(ty), batch.last_refill_size = BATCH_ALLOC_SIZE.
  2. If batch.available is non-empty: pop_front, return.
  3. Compute n = batch.last_refill_size × max(1, storage.extension_waiters_load())
     capped at MAX_BATCH_SIZE.
  4. ids = storage.alloc_page_batch(n, ty)?;
  5. batch.last_refill_size = n;
  6. batch.available.extend(ids); pop_front, return.
  ```

### Six design points locked

1. **`recycle_queue` durability** — `recycle_queue: crossbeam_queue::SegQueue<u64>`
   in `MmapStorage`. Lock-free MPMC, ideal for many concurrent commits
   pushing and many concurrent refills popping. **In-memory only**. On
   crash, its contents are lost, but **no data is lost** — the on-disk
   freelist bitmap is the source of truth. `release_deferred_frees` /
   `flush()` periodically drains the queue back into the bitmap so the
   in-memory growth is bounded.

2. **Rollback contract for `available`** — pages drawn from
   `alloc_page_batch` are marked **USED** in the bitmap immediately. On
   rollback we call `storage.free_page_batch(local.available)` to flip
   them back to FREE. This keeps the on-disk invariant `bitmap == truth`
   intact at all times — a crash mid-rollback leaves a few extra pages
   marked USED until the next bitmap reconciliation.

3. **Commit contract for `available`** — same as rollback. Leftover
   pre-allocated pages that the txn never touched go **back to the
   bitmap**, NOT to the recycle queue. PostgreSQL's "steal protection"
   rule. The `freed` pages (which the txn actually used and then freed)
   go to the recycle queue.

4. **Refill order** — `MmapStorage::alloc_page_batch` drains
   `recycle_queue` **first** (lock-free, already-zeroed pages), then falls
   back to `freelist.alloc_batch_sequential(remaining)` for the rest. If
   both run dry, falls through to `do_grow(GROW_PAGES)` exactly like
   today's `alloc_page` and retries.

5. **Adaptive batch sizing** — `BATCH_ALLOC_SIZE = 64` (initial / minimum),
   `MAX_BATCH_SIZE = 256` (cap), `extension_waiters: AtomicU32`
   incremented before `freelist.lock()` and decremented after. The
   actual refill size is
   `last_refill_size × max(1, extension_waiters)`, capped at
   `MAX_BATCH_SIZE`. Once a connection sees contention, it pulls a
   bigger batch and stays there.

6. **Per-page-type homogeneity** — `LocalPageBatch.current_type` enforces
   that the batch is homogeneous. A type mismatch on alloc returns the
   existing batch and refills with the new type. This matches
   PostgreSQL's bistate model (per-relation = per-page-type implicitly).

### Page lifecycle invariant

```
                                    storage.alloc_page_batch(n, ty)
bitmap FREE ────────────────────────────────────────────► bitmap USED
                                                                 │
                                                                 ▼
                                              local.available (current_type=ty)
                                                                 │
                                  pop_or_refill ────────────────►│
                                                                 ▼
                                                      executor uses page
                                                                 │
                              ┌──────────────────────────────────┤
                              │ executor frees                   │ executor commits
                              ▼                                  │
                       local.freed                               │
                              │                                  │
                              │ commit                           │
                              ▼                                  │
                  recycle_queue (SegQueue)                       │
                              │                                  │
                              │ next refill                      │
                              └──────► local.available ──────────┘
                                       (next txn)

  rollback or shutdown ──► storage.free_page_batch(available + freed-from-rollback-undo)
                                                       │
                                                       ▼
                                                 bitmap FREE

  flush() / release_deferred_frees ──► drain SegQueue ──► bitmap FREE
```

The invariant: a page is in **exactly one** of `bitmap FREE`,
`bitmap USED + local.available`, `bitmap USED + in use`, `bitmap USED +
local.freed`, or `bitmap USED + recycle_queue`. There is no transient state
where a page is missing from all lists.

## Acceptance criteria

### Data structures

- [ ] `LocalPageBatch` struct in `axiomdb-wal::txn` with:
      `available: VecDeque<u64>`, `current_type: Option<PageType>`,
      `freed: Vec<u64>`, `last_refill_size: usize`
- [ ] `LocalPageBatch::take_for_commit()` and `take_for_rollback()` helpers
      so the txn manager doesn't reach into the fields directly
- [ ] `LocalPageBatch` field added to `ConnectionTxn`; initialized empty in
      `TxnManager::begin`, drained in `commit` / `rollback`
- [ ] `BATCH_ALLOC_SIZE = 64` and `MAX_BATCH_SIZE = 256` as `pub const`s

### Storage trait + MmapStorage

- [ ] `StorageEngine::alloc_page_batch(n, page_type) -> Vec<u64>` and
      `free_page_batch(&[u64])` trait methods with default impls that fall
      back to per-page `alloc_page` / `free_page`
- [ ] `MmapStorage::recycle_queue: crossbeam_queue::SegQueue<u64>` (lock-free)
- [ ] `MmapStorage::extension_waiters: AtomicU32` for adaptive sizing
- [ ] `MmapStorage::alloc_page_batch` algorithm:
      drain `recycle_queue` first → `freelist.alloc_batch_sequential` for
      the rest → fall through to `do_grow` if exhausted
- [ ] `MmapStorage::free_page_batch` performs a single bitmap mutation
      pass under one `Mutex<FreeList>` lock
- [ ] `release_deferred_frees` (or `flush()`) folds the `recycle_queue`
      back into the bitmap so in-memory growth is bounded
- [ ] `crossbeam-queue = "0.3"` added to workspace `Cargo.toml`
- [ ] `FreeList::alloc_batch_sequential(n) -> Option<(start, len)>` that
      finds a contiguous run of `n` free bits in O(words) and falls back
      to non-contiguous fill when no run exists

### Wiring through hot paths

- [ ] `pop_or_refill(storage, batch, ty)` helper in `axiomdb-wal::txn`
      that hides the optional-batch ergonomics, performs the type-mismatch
      drain, and computes the adaptive refill size
- [ ] Hot-path call sites updated to take `Option<&mut LocalPageBatch>`
      and route through `pop_or_refill`:
  - [ ] `axiomdb-storage::heap_chain_write::insert{,_with_hint}`
  - [ ] `axiomdb-storage::clustered_tree::btree_insert::{split_leaf,split_internal}`
  - [ ] `axiomdb-storage::clustered_tree::mod::insert` (root creation)
  - [ ] `axiomdb-storage::clustered_overflow::write_chain` (big-row spill)
  - [ ] `axiomdb-index::tree_insert::{insert_leaf,split_internal,alloc_root}`
  - [ ] `axiomdb-index::tree_delete::{rotate_left,rotate_right,merge_children}`
        (CoW internal rebalance still allocates new pages)
- [ ] `ExecutionContext::alloc_page(ty)` wrapper that forwards
      `Some(&mut conn_txn.local_page_batch)`; executor call sites use it
      instead of touching the storage trait directly
- [ ] DDL / catalog / vacuum / recovery paths keep using
      `storage.alloc_page` directly (no batch); explicitly verified

### Drain semantics

- [ ] **Commit** drains `local.freed` into `recycle_queue` in O(N) pushes
      on the lock-free `SegQueue`
- [ ] **Commit** also drains `local.available` (leftover prealloc) back
      to the bitmap via `free_page_batch` — **steal protection**
- [ ] **Rollback** calls `free_page_batch(local.available)` to return
      pre-allocated-but-unused pages to the bitmap, and **keeps**
      `local.freed` (the rollback undo restores the rows that were
      freed → the pages are still in use)

### Correctness invariants

- [ ] No duplicate page allocations under concurrency
- [ ] No lost free pages under concurrency
- [ ] Page lifecycle invariant (see "Page lifecycle invariant" diagram):
      a page is always in exactly one of bitmap-FREE, available, in-use,
      freed, or recycle_queue
- [ ] **8 threads × 10K alloc+free** stress test → bitmap consistent,
      every alloc'd ID unique, every freed ID accounted for
- [ ] **8 threads × 10K** of mixed page-type allocs (heap + index +
      clustered) → batches drain and refill correctly on type switch
- [ ] Crash + recovery test: after a simulated crash with non-empty
      `recycle_queue`, the bitmap is still the source of truth and the
      lost queue entries become available via the next bitmap scan

### Benchmark gates

- [ ] Benchmark: 8-thread concurrent INSERT throughput ≥ **5×** current
      single-Mutex baseline (target is 10× per spec but 5× is the closing
      gate floor; document the actual ratio)
- [ ] Single-thread INSERT throughput within **±2%** of current baseline
      (the new path must not regress single-writer performance)

### Closing gates

- [ ] `cargo test --workspace` clean
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] Wire-test smoke not required (no SQL-visible behavior change)

## Out of scope

- Lock-free bitmap with AtomicU64 CAS (research shows diminishing returns >4 threads)
- Hierarchical extent allocation (InnoDB-style, future optimization)
- Per-table/per-index segment allocation pools
- NUMA-aware allocation

## Dependencies

- 40.3 (StorageEngine interior mutability) — ✅ DONE: `Mutex<FreeList>` + `grow_lock` in MmapStorage
- 40.4b (Per-connection TxnState) — `LocalPageBatch` lives in `ConnectionTxn`
