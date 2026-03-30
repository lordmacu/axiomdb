# Spec: Production-safe concurrent storage layer (Phase 7.4a)

## Reviewed first

- `crates/axiomdb-storage/src/mmap.rs` — `MmapMut`, `read_page_from_mmap` returns `&Page` into mmap, `grow()` remaps unconditionally, `write_page` uses `copy_from_slice` on mmap
- `crates/axiomdb-storage/src/engine.rs` — `StorageEngine` trait: `read_page(&self) -> Result<&Page>`
- `crates/axiomdb-storage/src/page.rs` — `PAGE_SIZE = 16384`, `verify_checksum()`
- `research/sqlite/src/os_unix.c` — `unixFetch` returns pointer + increments `nFetchOut`; `unixUnfetch` decrements; `unixRemapfile` asserts `nFetchOut==0` before remap; `unixMapfile` no-ops if `nFetchOut>0`
- `research/sqlite/src/pager.c` — `getPageMMap` acquires mmap page with refcount; `pagerAcquireMapPage` wraps raw pointer in `PgHdr` with `nRef`
- `research/sqlite/src/wal.c` — readers record `mxFrame` snapshot; checkpoint cannot overwrite frames ≤ any reader's `mxFrame`; `aReadMark[]` array tracks reader positions
- `research/postgres/src/backend/storage/buffer/bufmgr.c` — `PrivateRefCountData` pin per backend; page cannot be evicted while pinned
- `research/mariadb-server/storage/innobase/include/buf0dblwr.h` — doublewrite buffer for torn page recovery
- `research/duckdb/src/storage/single_file_block_manager.cpp` — `ChecksumAndWrite` uses pwrite not mmap; `IncreaseBlockReferenceCount` prevents reuse

## Research synthesis

### The universal rule: no production DB uses mmap for writes

| Database | Read I/O | Write I/O | Torn page protection | Reader isolation |
|---|---|---|---|---|
| PostgreSQL | `pread()` + buffer pool | `pwrite()` + fsync | WAL replay | Buffer pin (refcount per backend) |
| InnoDB | Buffer pool (not mmap) | `os_file_write()` + fsync | Doublewrite buffer | Per-page RwLock latch |
| DuckDB | `FileHandle::Read()` (pread) | `FileHandle::Write()` (pwrite) | Checksum + WAL | Block refcount |
| SQLite | **mmap for reads** + pread fallback | `pwrite()` only | WAL frame checksums | `nFetchOut` refcount on mmap pointers |

SQLite is the closest model: **mmap for reads, pwrite for writes, refcount to prevent remap while readers hold pointers.**

### Three critical problems in AxiomDB today

**Problem 1: Torn pages (16KB non-atomic mmap writes)**

`write_page` uses `self.mmap[offset..].copy_from_slice(page.as_bytes())` which is a 16KB memcpy to the mmap region. This is NOT atomic — a concurrent reader calling `read_page` can see a half-written page (first 8KB new, last 8KB old). The checksum will fail, but the reader gets an error instead of correct data.

No production DB writes pages via mmap. PostgreSQL, InnoDB, DuckDB, and SQLite all use `pwrite()` for page writes.

**Problem 2: Remap invalidates reader pointers**

`grow()` calls `MmapMut::map_mut(&self.file)` which unmaps the old region and maps a new one. Any `&Page` reference returned by `read_page` that points into the old mmap becomes a dangling pointer — use-after-free / segfault.

SQLite solves this with `nFetchOut`: a counter of outstanding mmap references. `unixRemapfile` asserts `nFetchOut==0` before remapping. If readers hold references, remap is blocked.

**Problem 3: Freed page reuse while readers hold references**

When `free_page(page_id)` is called, `alloc_page()` can immediately reuse that page_id. A reader holding a `&Page` reference to the old content will now read the new (unrelated) page content — silent data corruption.

PostgreSQL solves this with buffer pins. DuckDB uses block reference counts. SQLite uses WAL frame snapshots. AxiomDB has none of these — Phase 7.8 (epoch-based reclamation) is deferred.

### AxiomDB-first decision

Fix all three problems using a hybrid approach inspired by all four databases:

1. **Write via `pwrite()`, read via mmap** (SQLite model)
   - `write_page` uses `pwrite()` to the file, not `copy_from_slice` to the mmap
   - After `pwrite()`, the mmap automatically reflects the change (MAP_SHARED)
   - `pwrite()` of a 16KB aligned page is atomic on all modern filesystems with journaling (ext4, XFS, APFS, ZFS)
   - Eliminates torn pages for concurrent readers

2. **Page guard with refcount** (SQLite `nFetchOut` model)
   - `read_page` returns a `PageGuard` RAII wrapper instead of raw `&Page`
   - `PageGuard` increments an `AtomicUsize` counter on creation, decrements on drop
   - `grow()` spins/waits until counter reaches 0 before remapping
   - Eliminates remap-during-read segfaults

3. **Deferred page free with epoch tracking** (simplified PostgreSQL pin model)
   - Freed pages are NOT immediately available for reuse
   - Pages move to a "pending free" list with the snapshot_id at which they were freed
   - `alloc_page` only reuses a freed page when no active reader has a snapshot older than the free epoch
   - Requires tracking the oldest active reader snapshot (min_active_snapshot)
   - Eliminates use-after-free from page reuse

## What to build (not how)

Make `MmapStorage` safe for concurrent read/write access so that:

- **Writes use `pwrite()`** to the underlying file descriptor instead of writing to the mmap region directly. The mmap remains read-only for page access.

- **`read_page` returns a guard** that prevents file remap while the guard is alive. Multiple guards can exist simultaneously (readers don't block each other). File growth waits for all guards to be dropped.

- **Freed pages are not immediately reusable.** A freed page enters a deferred-free queue. Pages are released for reuse only when no reader snapshot predates the free operation.

- **The mmap is opened read-only** (`Mmap` instead of `MmapMut`) since writes go through `pwrite()`. This makes it structurally impossible to accidentally write via mmap.

- **`StorageEngine` trait changes**: `read_page` returns an owned `PageRef` (or `PageGuard`) instead of `&Page`. This changes the lifetime model from borrowed to owned, which is necessary for concurrent access.

## Inputs / Outputs

- Input: concurrent read and write operations on the same MmapStorage
- Output:
  - `read_page` never returns torn/partial page data
  - `read_page` never segfaults due to remap
  - `read_page` never returns data from a freed+reallocated page
  - `write_page` is durable after `flush()` (pwrite + fsync)
  - All existing single-threaded tests pass without regression
- Errors:
  - No new error types. Existing `PageNotFound`, `ChecksumMismatch` remain.

## Acceptance criteria

- [ ] `write_page` uses `pwrite()` to file descriptor, not mmap `copy_from_slice`.
- [ ] mmap is opened as read-only (`Mmap` not `MmapMut`) for page reads.
- [ ] `read_page` returns an owned page type (not a borrow into the mmap).
- [ ] Outstanding page refs prevent `grow()` from remapping (verified by test).
- [ ] `grow()` waits for all page refs to be dropped before remapping.
- [ ] Freed pages enter a deferred-free queue, not the immediate free list.
- [ ] Deferred-free pages are released only when no reader snapshot is older.
- [ ] `flush()` calls `fsync()` on the file descriptor (not `msync` on mmap).
- [ ] Concurrent read+write test passes without checksum errors or segfaults.
- [ ] All existing single-threaded tests pass without regression.
- [ ] `MemoryStorage` updated to return owned pages (trait change).

## Out of scope

- Buffer pool / page cache (pages read directly from mmap, no LRU)
- Doublewrite buffer (pwrite atomicity is sufficient on modern FS)
- Multi-writer support (single writer Mutex remains)
- Per-page locking (MVCC handles visibility)

## Dependencies

- Phase 7.1: MVCC visibility rules (implemented — provides snapshot_id for epoch tracking)
- Phase 2.6: B-Tree CoW (implemented — writes go to new pages, old pages safe for readers)

## ⚠️ DEFERRED

- Full epoch-based reclamation with min_active_snapshot tracking → can be simplified initially to "never reuse freed pages during a transaction" and refined in Phase 7.8
- `madvise(MADV_SEQUENTIAL)` for scan prefetch → Phase 11.9
- Direct I/O bypass (`O_DIRECT`) for write path → future performance subphase
