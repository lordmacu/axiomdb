# Spec: 40.3 — StorageEngine Interior Mutability

## What to build (not how)

Change the `StorageEngine` trait from `write_page(&mut self)` to `write_page(&self)` with
**per-page RwLocks** and **interior mutability** for mutable state (FreeList, dirty tracker,
deferred frees). This is the architectural unlock that enables all concurrent writer
subfases that follow.

This is NOT a shortcut wrapper around a global Mutex. Each page gets its own lock.
Two transactions writing different pages proceed in **true parallel** — zero contention.

## Research findings

### InnoDB Buffer Pool (the gold standard)
- **`buf_page_get_gen()`** acquires per-page `block_lock` (SX-exclusive lock, 3 modes: S/SX/X)
- **Hash-partitioned** lookup table: `page_id.fold() % N_PARTITIONS` → shard RwLock
- **Atomic reference counting** per page (`buf_page_t::fix()` = atomic increment)
- **NO global &mut**: page content lock is interior to `buf_block_t`, NOT in buffer pool struct
- **Cache-line aligned**: each `buf_block_t` aligned to `CPU_LEVEL1_DCACHE_LINESIZE` to avoid false sharing

### PostgreSQL Buffer Manager
- **`BufferDesc`** per buffer: atomic `state` field packs refcount (18 bits) + lock mode (20 bits) + flags
- **`LockBuffer(BUFFER_LOCK_EXCLUSIVE)`** acquires per-buffer content lock via atomic CAS
- **`MarkBufferDirty()`** sets dirty flag via atomic CAS on state field — no lock needed
- **Separate pin/content-lock**: pin prevents eviction, content lock controls read/write access
- **NO &mut self**: `ReadBuffer()` and `MarkBufferDirty()` use `&self` equivalent

### Key pattern (both databases)
```
Global buffer pool: &self (shared, immutable structure)
  └── Per-page control block: owns RwLock + atomic state
       └── Page content: protected by per-page RwLock
```

## Current AxiomDB architecture (what changes)

### StorageEngine trait today
```rust
pub trait StorageEngine: Send + Sync {
    fn read_page(&self, page_id: u64) -> Result<PageRef, DbError>;        // ✅ already &self
    fn write_page(&mut self, page_id: u64, page: &Page) -> Result<(), DbError>;  // ❌ &mut self
    fn alloc_page(&mut self, page_type: PageType) -> Result<u64, DbError>;       // ❌ &mut self
    fn free_page(&mut self, page_id: u64) -> Result<(), DbError>;                // ❌ &mut self
    fn flush(&mut self) -> Result<(), DbError>;                                  // ❌ &mut self
    fn page_count(&self) -> u64;                                                 // ✅ already &self
    fn prefetch_hint(&self, page_id: u64, count: u32) {}                         // ✅ already &self
    fn set_current_snapshot(&mut self, snapshot_id: u64) {}                      // ❌ &mut self
}
```

### MmapStorage mutable state today
| Field | Type | Why mutable | Solution |
|---|---|---|---|
| `freelist` | `FreeList` | alloc/free modify bitmap | `Mutex<FreeList>` |
| `freelist_dirty` | `bool` | set on alloc/free | `AtomicBool` |
| `dirty` | `PageDirtyTracker` | mark pages dirty | Already atomic ✅ |
| `deferred_frees` | `Vec<(TxnId, Vec<u64>)>` | accumulate freed pages | `Mutex<Vec<...>>` |
| `current_snapshot_id` | `u64` | set per-statement | `AtomicU64` |
| `file` (pwrite) | `File` | write page bytes | Already thread-safe (pwrite) ✅ |
| `mmap` | `Mmap` | read-only mapping | Already &self ✅ |

### Blast radius
**~190 function signatures** across 25 files take `&mut dyn StorageEngine`.
All must change to `&dyn StorageEngine`. This is mechanical but must be thorough.

## New structures

### PageLockTable (InnoDB-inspired, sharded)

```rust
/// Per-page RwLock table, sharded to minimize contention on the shard lookup.
/// InnoDB uses CPU_LEVEL1_DCACHE_LINESIZE-aligned blocks; we use 64 shards
/// (one per potential CPU core) to ensure no false sharing.
pub struct PageLockTable {
    shards: Box<[RwLock<HashMap<u64, Arc<RwLock<()>>>>]>,  // 64 shards
}
```

**Shard selection**: `page_id % 64` (power of 2 for fast modulo).

**Lock modes per page:**
- `read(page_id)` → shared lock (multiple concurrent readers)
- `write(page_id)` → exclusive lock (one writer, blocks readers)

**Lock lifecycle:**
- Lock created lazily on first access to a page
- Locks are NOT removed (bounded by total pages in database)
- Shard RwLock held only during HashMap lookup (microseconds)
- Page RwLock held during actual page I/O (milliseconds)

### Updated MmapStorage

```rust
pub struct MmapStorage {
    mmap: Mmap,                                    // read-only, shared
    file: File,                                    // pwrite is thread-safe
    freelist: Mutex<FreeList>,                     // protected allocation
    freelist_dirty: AtomicBool,                    // atomic flag
    dirty: PageDirtyTracker,                       // already atomic
    deferred_frees: Mutex<Vec<(TxnId, Vec<u64>)>>, // protected queue
    current_snapshot_id: AtomicU64,                // atomic
    page_locks: PageLockTable,                     // NEW: per-page RwLocks
    doublewrite: Mutex<DoublewriteBuffer>,         // protect doublewrite
    page_count: AtomicU64,                         // atomic (for grow)
    config: DbConfig,                              // immutable after init
}
```

### Updated MemoryStorage (for tests)

Same pattern: `pages: RwLock<HashMap<u64, Page>>` with per-page granularity.
Tests must work identically with both MmapStorage and MemoryStorage.

## Detailed behavior

### write_page(&self) — new implementation

```
1. Validate page_id < page_count (atomic load)
2. Acquire page_locks.write(page_id)         → exclusive per-page lock
3. Call pwrite(page_id, page)                → thread-safe syscall
4. dirty.mark(page_id)                       → atomic flag set
5. Release page_locks.write(page_id)         → other threads can now access this page
```

Two threads writing different pages: step 2 acquires DIFFERENT locks → full parallelism.
Two threads writing same page: step 2 serializes → correctness guaranteed.

### alloc_page(&self) — new implementation

```
1. Acquire freelist Mutex
2. freelist.alloc() → get page_id (or None → grow)
3. freelist_dirty.store(true, Relaxed)
4. Release freelist Mutex
5. Acquire page_locks.write(page_id)
6. Initialize page (pwrite)
7. dirty.mark(page_id)
8. Release page_locks.write(page_id)
9. Return page_id
```

Freelist Mutex held only during bitmap scan (~microseconds).
Page lock held only during page init write (~microseconds).
Both are brief — contention is minimal.

### free_page(&self) — new implementation

```
1. Acquire freelist Mutex
2. freelist.free(page_id)
3. freelist_dirty.store(true, Relaxed)
4. Release freelist Mutex
```

No page lock needed — just bitmap update.

### flush(&self) — new implementation

```
1. Acquire freelist Mutex (briefly, to serialize freelist write)
2. Write freelist pages if dirty
3. Release freelist Mutex
4. file.sync_all()  → thread-safe
```

### grow (when alloc finds no free pages)

```
1. Acquire grow Mutex (separate from freelist, prevents double-grow)
2. Extend file
3. Update mmap (re-map or extend)
4. Acquire freelist Mutex
5. freelist.grow(new_pages)
6. Release freelist Mutex
7. Update page_count (atomic store)
8. Release grow Mutex
```

## Concurrency guarantees

| Scenario | Behavior | Mechanism |
|---|---|---|
| 2 threads write different pages | **Parallel** | Different page locks |
| 2 threads write same page | **Serialized** | Same page lock (exclusive) |
| 1 thread writes, 1 thread reads same page | **Serialized** | Page RwLock (write blocks read) |
| 2 threads read same page | **Parallel** | Page RwLock (shared mode) |
| 2 threads alloc pages | **Serialized briefly** | Freelist Mutex (~1µs) |
| 1 thread writes, 1 thread allocs | **Parallel** | Different locks entirely |

## Use cases

1. **Two INSERTs into different tables:** Each inserts on different heap pages → full parallel.
2. **Two INSERTs into same table, different pages:** Same table but different tail pages → parallel.
3. **Two INSERTs into same table, same page:** Serialized by page lock. Second waits ~1ms.
4. **SELECT during INSERT:** Read and write on same page serialized. Different pages parallel.
5. **alloc_page during INSERT:** Freelist mutex held briefly. Page write proceeds after.

## Acceptance criteria

- [ ] `StorageEngine` trait: ALL methods use `&self` (zero `&mut self`)
- [ ] `MmapStorage`: interior mutability via Mutex/Atomic for all mutable state
- [ ] `MemoryStorage`: same pattern (for tests)
- [ ] `PageLockTable`: 64-shard per-page RwLock table
- [ ] write_page acquires per-page exclusive lock
- [ ] read_page acquires per-page shared lock (or returns owned copy without lock)
- [ ] alloc_page uses Mutex<FreeList> — held only during bitmap scan
- [ ] free_page uses Mutex<FreeList>
- [ ] flush uses minimal locking
- [ ] ~190 call sites updated: `&mut dyn StorageEngine` → `&dyn StorageEngine`
- [ ] All existing tests pass unchanged
- [ ] No deadlock between page locks and freelist mutex (lock ordering: page < freelist)
- [ ] Stress test: 4 threads × 1000 write_page calls → no corruption
- [ ] `cargo clippy -- -D warnings` clean across all crates

## Out of scope

- Changing how the executor calls storage (signatures only, logic unchanged)
- Actual concurrent DML execution (still serialized by Arc<RwLock<Database>> until 40.10)
- Buffer pool / page cache (future phase — currently read_page returns owned copy)
- Page-level lock escalation or timeout (that's part of 40.5-40.6)

## Dependencies

- 40.1 (Atomic TxnId) — for AtomicU64 patterns
- 40.2 (Per-connection txn) — for understanding what state is per-connection vs shared

## Risks

- **Blast radius**: 190 call sites is large. Mitigation: systematic find-and-replace.
  The change is purely `&mut dyn StorageEngine` → `&dyn StorageEngine` — no logic changes.
- **Lock ordering**: page lock → freelist mutex must never reverse.
  alloc_page acquires freelist first, then page lock. write_page only acquires page lock.
  No cycle possible.
- **Performance**: Mutex<FreeList> is a potential bottleneck under extreme alloc pressure.
  Mitigation: 40.9 replaces with lock-free bitmap. For now, mutex contention is microseconds.
- **MemoryStorage divergence**: Must keep MemoryStorage behavior identical to MmapStorage.
  Same locking patterns, same ordering guarantees.
