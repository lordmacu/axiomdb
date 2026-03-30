# Plan: Production-safe concurrent storage layer (Phase 7.4a)

## Inspiration and research

This plan draws from all four databases in `research/`:

| Technique | Source | What we take |
|---|---|---|
| mmap for reads + pwrite for writes | **SQLite** `os_unix.c:5714-5738` | Exact model: `unixFetch` returns mmap pointer, writes go through `pwrite()` |
| Reference counting on mmap pointers | **SQLite** `os_unix.c:5733` `nFetchOut++` | Block remap while readers hold page refs |
| Assert `nFetchOut==0` before remap | **SQLite** `os_unix.c:5596` | Hard safety invariant for grow/remap |
| Buffer pin (refcount per backend) | **PostgreSQL** `bufmgr.c:101-131` | Concept of page pinning — pages can't be evicted/freed while pinned |
| Block reference counting | **DuckDB** `single_file_block_manager.cpp:1034` | `IncreaseBlockReferenceCount` prevents reuse |
| pwrite for page writes | **All four** (PG, InnoDB, DuckDB, SQLite) | Universal pattern — no production DB writes pages via mmap |

## Files to create/modify

- `crates/axiomdb-storage/src/engine.rs` — change `read_page(&self) -> Result<&Page>` to `read_page(&self) -> Result<PageRef>` where `PageRef` is an owned copy or a guard
- `crates/axiomdb-storage/src/page_ref.rs` — new: `PageRef` type that owns page data (16KB stack or `Box<Page>`)
- `crates/axiomdb-storage/src/mmap.rs` — rewrite to use read-only `Mmap` + `pwrite()` for writes + `AtomicUsize` ref counter for remap safety
- `crates/axiomdb-storage/src/memory.rs` — update `MemoryStorage::read_page` to return `PageRef`
- `crates/axiomdb-storage/src/heap.rs` — update all `read_page` callers to work with `PageRef`
- `crates/axiomdb-storage/src/heap_chain.rs` — same
- `crates/axiomdb-storage/src/freelist.rs` — add deferred-free queue
- `crates/axiomdb-storage/src/lib.rs` — export new types
- `crates/axiomdb-index/src/tree.rs` — update B-Tree page reads
- `crates/axiomdb-index/src/iter.rs` — update range iterator
- `crates/axiomdb-catalog/src/reader.rs` — update catalog page reads
- All callers of `StorageEngine::read_page` across the workspace

## Algorithm / Data structure

### PageRef — owned page data

```text
enum PageRef {
    Owned(Box<Page>),     // heap-allocated copy, safe to hold indefinitely
}

impl Deref for PageRef {
    type Target = Page;   // transparent access to &Page
}
```

Why `Box<Page>` instead of `&Page`:
- No lifetime tied to the mmap — safe across remap
- No reference counting needed at the read_page level
- 16KB allocation per page read is acceptable (mmap read + memcpy is ~1µs)
- PostgreSQL does the same: `ReadBuffer` copies page into shared buffer pool

Alternative considered: RAII guard with AtomicUsize counter (SQLite model).
Rejected because `Box<Page>` is simpler, Rust-idiomatic, and eliminates the
remap problem entirely — if the page is copied, the mmap can remap freely.

### MmapStorage rewrite

```text
struct MmapStorage {
    file: File,                     // open file handle for pwrite
    mmap: Mmap,                     // READ-ONLY mapping (not MmapMut)
    mmap_len: AtomicU64,            // current mmap size (for bounds check)
    freelist: FreeList,
    deferred_free: Vec<(u64, u64)>, // (page_id, freed_at_snapshot_id)
    dirty: DirtyTracker,
}

read_page(&self, page_id) -> Result<PageRef>:
    // bounds check against mmap_len
    // copy 16KB from mmap into Box<Page>
    // verify checksum
    // return PageRef::Owned(box_page)

write_page(&mut self, page_id, page) -> Result<()>:
    // pwrite(file.as_raw_fd(), page.as_bytes(), offset)
    // mmap automatically reflects the change (MAP_SHARED)
    // dirty.mark(page_id)

alloc_page(&mut self, page_type) -> Result<u64>:
    // try deferred_free queue first (if safe to reuse)
    // else try freelist
    // else grow file + remap mmap

grow(&mut self, extra_pages) -> Result<u64>:
    // file.set_len(new_size)
    // drop old Mmap
    // create new Mmap (read-only)
    // no risk: all readers hold PageRef copies, not &Page refs

flush(&mut self) -> Result<()>:
    // file.sync_all() — fsync on file descriptor
    // NOT msync on mmap (mmap is read-only)

free_page(&mut self, page_id, current_snapshot) -> Result<()>:
    // push (page_id, current_snapshot) to deferred_free
    // do NOT add to freelist immediately
```

### Deferred free queue

```text
deferred_free: Vec<(page_id: u64, freed_at: u64)>

release_deferred_frees(min_active_snapshot: u64):
    for (page_id, freed_at) in deferred_free:
        if freed_at < min_active_snapshot:
            freelist.free(page_id)
    retain only entries where freed_at >= min_active_snapshot
```

For Phase 7.4a, `min_active_snapshot` is simply `max_committed` (single writer,
no concurrent readers yet). The full epoch tracking comes in Phase 7.4/7.8
when concurrent readers exist.

## Implementation phases

1. **Create `PageRef` type** and change `StorageEngine::read_page` signature.
   Update `MemoryStorage` first (simpler). Compile and fix all callers
   across the workspace. This is the largest mechanical change.

2. **Rewrite `MmapStorage`** to use read-only `Mmap` + `pwrite()`.
   - Open mmap as `Mmap` (not `MmapMut`)
   - `write_page` uses `pwrite()` via `std::os::unix::fs::FileExt::write_at`
   - `flush` uses `file.sync_all()` instead of `mmap.flush()`
   - `grow` drops old `Mmap`, creates new `Mmap` (safe because no `&Page` refs exist)

3. **Add deferred free queue** to `MmapStorage`.
   - `free_page` pushes to deferred queue instead of freelist
   - `alloc_page` checks deferred queue first (pages where freed_at < min_active_snapshot)
   - Initial `min_active_snapshot = max_committed` (single writer)

4. **Update all callers** — heap.rs, heap_chain.rs, tree.rs, iter.rs, reader.rs,
   integrity checker, etc. Most changes are mechanical: `let page = storage.read_page(id)?;`
   now gives a `PageRef` that derefs to `&Page`.

5. **Tests**: concurrent read+write test, remap safety test, freed page reuse test.

## Tests to write

- unit: `PageRef` deref to `&Page` works correctly
- unit: `pwrite` + mmap read returns correct data
- unit: `grow` after `pwrite` reflects new pages in mmap
- unit: deferred free queue holds pages until safe to reuse
- integration: concurrent `read_page` + `write_page` on MmapStorage — no checksum errors
- integration: `grow` during active reads — no segfault (PageRef is a copy)
- integration: all existing tests pass with new `PageRef` return type

## Anti-patterns to avoid

- Do NOT keep `MmapMut` — the mmap MUST be read-only to prevent accidental writes.
- Do NOT return `&Page` from `read_page` — the lifetime is unsafe across remap.
- Do NOT add per-page RwLock — MVCC handles visibility, not locking.
- Do NOT implement a buffer pool yet — direct mmap read + copy is fast enough and simpler.
- Do NOT use `msync` for flush — `fsync` on the file descriptor is correct for pwrite.
- Do NOT immediately reuse freed pages — defer until safe.

## Risks

- **Performance regression from page copies**: copying 16KB per `read_page` adds ~0.5-1µs per page. For scan-heavy workloads this could be measurable. Mitigation: benchmark before/after; the copy is from mmap (hot in L2/L3 cache) so it's a fast memcpy. PostgreSQL copies pages into its buffer pool too.
- **pwrite atomicity not guaranteed on all filesystems**: ext4/XFS with journaling provide 4KB-aligned pwrite atomicity. 16KB pages span 4 filesystem blocks, so a crash mid-pwrite could leave a torn page. Mitigation: AxiomDB's WAL already provides crash recovery — a torn page is detected by checksum on recovery and the WAL replay fixes it.
- **Deferred free queue growth**: if a long-running reader holds a very old snapshot, the deferred free queue grows unbounded. Mitigation: Phase 7.11 (vacuum) will cap this; for now, single-writer means snapshots advance quickly.
- **`Box<Page>` allocation pressure**: 16KB per read_page call creates heap allocation pressure for scan workloads. Mitigation: consider a thread-local page pool or arena allocator in a follow-up if benchmarks show regression.
