# Storage Engine

The storage engine is the lowest user-accessible layer in AxiomDB. It manages raw
16-kilobyte pages on disk or in memory, provides a freelist for page allocation, and
exposes a simple trait that all higher layers depend on.

---

## The StorageEngine Trait

```rust
pub trait StorageEngine: Send {
    fn read_page(&self, page_id: u64) -> Result<PageRef, DbError>;
    fn write_page(&mut self, page_id: u64, page: &Page) -> Result<(), DbError>;
    fn alloc_page(&mut self, page_type: PageType) -> Result<u64, DbError>;
    fn free_page(&mut self, page_id: u64) -> Result<(), DbError>;
    fn flush(&mut self) -> Result<(), DbError>;
    fn page_count(&self) -> u64;
}
```

`read_page` returns an owned `PageRef` — a heap-allocated copy of the 16 KB page data.
This is a deliberate change from the original `&Page` borrow: owned pages survive mmap
remaps (during `grow()`) and page reuse (after `free_page`), which is essential for
concurrent read/write access. The copy cost is ~0.5 us from L2/L3 cache — the same cost
PostgreSQL pays when copying a page from the buffer pool into backend-local memory.

---

## Page Format

Every page is exactly `PAGE_SIZE = 16,384` bytes (16 KB). The first `HEADER_SIZE = 64`
bytes are the page header; the remaining `PAGE_BODY_SIZE = 16,320` bytes are the body.

### Page Header — 64 bytes

```text
Offset   Size   Field            Description
──────── ────── ──────────────── ──────────────────────────────────────
     0      4   magic            0xAXIOM_DB — identifies valid pages
     4      1   page_type        PageType enum (see below)
     5      3   _pad             alignment padding
     8      4   checksum         CRC32c of bytes [12..PAGE_SIZE)
    12      8   page_id          This page's own ID (self-identifying)
    20      8   lsn              Log Sequence Number of last write
    28      2   free_start       First free byte offset in the body (for heap pages)
    30      2   free_end         Last free byte offset in the body (for heap pages)
    32     32   _reserved        Future use (flags, epoch, etc.)
Total:    64 bytes
```

The CRC32c checksum covers all bytes from offset 12 to the end of the page (4 bytes
for the checksum field itself are excluded). On every `read_page`, AxiomDB verifies
the checksum and returns `DbError::ChecksumMismatch` if it fails.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — CRC32c Instead of Double-Write Buffer</span>
Traditional engines (InnoDB) use a double-write buffer to detect partial page writes caused by a crash mid-flush. AxiomDB instead uses per-page CRC32c checksums: if a page's checksum fails on read, the WAL is replayed to reconstruct the correct state. Same crash-safety guarantee — zero write amplification.
</div>
</div>

### Page Types

```rust
pub enum PageType {
    Meta     = 1,  // page 0: database header + catalog root
    Data     = 2,  // heap pages holding table rows
    Index    = 3,  // B+ Tree internal and leaf nodes
    Overflow = 4,  // continuation pages for large values (TOAST)
    Free     = 5,  // pages in the freelist (tracked but unused)
}
```

---

## MmapStorage — Memory-Mapped File

`MmapStorage` uses a hybrid I/O model inspired by SQLite: **read-only mmap for reads,
`pwrite()` for writes**. The mmap is opened with `memmap2::Mmap` (not `MmapMut`),
making it structurally impossible to write through the mapped region.

```
Physical file (axiomdb.db):
┌──────────┬──────────┬──────────┬──────────┬──────────┐
│  Page 0  │  Page 1  │  Page 2  │  Page 3  │  ...     │
│ (Meta)   │ (Data)   │ (Index)  │ (Data)   │          │
└──────────┴──────────┴──────────┴──────────┴──────────┘
     ↑           ↑                     ↓
     │           └── read_page(1): copy 16KB from mmap → owned PageRef
     └── mmap (read-only, MAP_SHARED)
                                  write_page(3): pwrite() to file descriptor
```

### Read path: mmap + PageRef copy

`read_page(page_id)` computes `mmap_ptr + page_id * 16384`, copies 16 KB into a
heap-allocated `PageRef`, verifies the CRC32c checksum, and returns the owned copy.
The copy cost (~0.5 us from L2/L3 cache) is the same price PostgreSQL pays when
copying a buffer pool page into backend-local memory.

### Write path: pwrite() to file descriptor

`write_page(page_id, page)` calls `pwrite()` on the underlying file descriptor at
offset `page_id * 16384`. The mmap (MAP_SHARED) automatically reflects the change
on subsequent reads. `pwrite()` of a 16 KB aligned page is atomic on all modern
journaling filesystems (ext4, XFS, APFS, ZFS), eliminating torn pages for concurrent
readers.

### Flush: fsync on file descriptor

`flush()` calls `file.sync_all()` (which maps to `fsync()`) on the file descriptor.
This ensures all `pwrite()` data is durable. No `msync` is needed since writes do not
go through the mmap.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Read-Only Mmap + pwrite (SQLite Model)</span>
No production database uses mmap for writes. PostgreSQL uses pwrite + buffer pool,
InnoDB uses pwrite + doublewrite buffer, DuckDB uses pwrite exclusively, and SQLite
uses mmap for reads + pwrite for writes. AxiomDB follows the SQLite model: mmap gives
zero-copy reads from the OS page cache, while pwrite provides coherent page writes visible through the mmap. Note that a
16 KB pwrite is NOT crash-atomic on 4 KB-block filesystems (ext4, APFS, XFS) — a
crash mid-write leaves a torn page. AxiomDB detects torn pages via CRC32c checksums
on recovery; full WAL page-image redo (3.8c) will provide the repair path.
</div>
</div>

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">No Double-Buffer Overhead</span>
MySQL InnoDB keeps every hot page in RAM twice — once in the OS page cache, once in
the InnoDB buffer pool. AxiomDB's mmap approach uses the OS page cache directly. For a
working set that fits in RAM, this roughly halves the memory footprint of the storage
layer.
</div>
</div>

**Trade-offs:**
- We cannot control which pages stay hot in memory (the OS uses LRU).
- On 32-bit systems, the address space limits the maximum database size.
  On 64-bit, the address space is effectively unlimited.
- `PageRef` copies add ~0.5 us per page read vs. direct pointer access, but this
  eliminates use-after-free risks from mmap remap and page reuse.

### Deferred Page Free Queue

When `free_page(page_id)` is called, the page does **not** return to the freelist
immediately. Instead it enters an epoch-tagged queue: `deferred_frees: Vec<(page_id,
freed_at_snapshot)>`. Each entry records the snapshot epoch at which the page became
unreachable. `release_deferred_frees(oldest_active_snapshot)` only releases pages
whose `freed_at_snapshot <= oldest_active_snapshot` — pages freed more recently remain
queued because a concurrent reader might still hold a snapshot that references them.

Under the current `Arc<RwLock<Database>>` architecture, `flush()` passes `u64::MAX`
(release all) because the writer holds exclusive access and no readers are active.
When snapshot slot tracking is added (Phase 7.8), the actual oldest active snapshot
will be used instead. The queue is capped at 4096 entries with a tracing warning
to detect snapshot leaks.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Deferred Frees — Simplified Epoch Reclamation</span>
PostgreSQL uses buffer pins to prevent eviction while a backend reads a page. DuckDB
uses block reference counts. AxiomDB's deferred free queue achieves the same safety
with less complexity: freed pages are quarantined until all concurrent readers that
could reference them have completed.
</div>
</div>

---

## Dirty Page Tracking and Targeted Flush

`MmapStorage` tracks every page written since the last `flush()` in a
`PageDirtyTracker` (an in-memory `HashSet<u64>`). On `flush()`, instead of
calling `mmap.flush()` (which issues `msync` over the entire file), AxiomDB
coalesces the dirty page IDs into contiguous runs and issues one `flush_range`
call per run.

### Coalescing algorithm

`PageDirtyTracker::contiguous_runs()` sorts the dirty IDs and merges adjacent
IDs into `(start_page, run_length)` pairs:

```rust
// Dirty pages: {2, 3, 5, 6, 7}  →  runs: [(2, 2), (5, 3)]
// Byte ranges: [(2*16384, 32768), (5*16384, 49152)]
```

The merge is O(n log n) on the number of dirty pages and produces the minimum
number of `msync` syscalls for any given dirty set.

### Freelist integration

When the freelist changes (`alloc_page`, `free_page`), `freelist_dirty` is set.
On `flush()`, the freelist bitmap is serialized into page 1 first, and page 1 is
added to the effective flush set even if it was not already in the dirty tracker.
Only after **all** targeted flushes succeed are `freelist_dirty` and the dirty
tracker cleared. A partial failure leaves both intact so the next `flush()` can
retry safely.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Sub-file msync</span>
SQLite and PostgreSQL issue `fsync` over the entire data file on every checkpoint or WAL sync. AxiomDB's targeted `flush_range` (backed by `msync(MS_SYNC)`) touches only the pages that actually changed. On workloads where a small fraction of pages are written per checkpoint, this reduces I/O proportionally to the dirty-page ratio.
</div>
</div>

### Disk-full error classification

Every durable I/O call in `flush()` (and in `create()`/`grow()`) passes its
`std::io::Error` through `classify_io()` before returning:

```rust
// axiomdb-core/src/error.rs
pub fn classify_io(err: std::io::Error, operation: &'static str) -> DbError {
    // ENOSPC (28) and EDQUOT (69/122) → DbError::DiskFull { operation }
    // All other errors → DbError::Io(err)
}
```

When a `DiskFull` error propagates out of `MmapStorage`, the server runtime
transitions to **read-only degraded mode** — all subsequent mutating statements
are rejected immediately without re-entering the storage layer.

### Invariants

- `flush()` returns `Ok(())` only after all dirty pages are durable.
- Dirty tracking is cleared only on success — never on failure.
- The freelist page (page 1) is always included when `freelist_dirty` is set,
  regardless of whether it appears in the tracker.
- `dirty_page_count()` always reflects the count since the last successful flush.
- `ENOSPC`/`EDQUOT` errors are always surfaced as `DbError::DiskFull`, never
  silently wrapped in `DbError::Io`.

---

## Verified Open — Corruption Detection at Startup

`MmapStorage::open()` validates every **allocated** page before making the
database available. The startup sequence is:

1. Map the file and verify page 0 (meta) — magic, version, page count.
2. Load the freelist from page 1 and verify its checksum.
3. Scan pages `2..page_count`, skipping any page the freelist marks as free.
   For each allocated page, call `read_page_from_mmap()` which re-computes
   the CRC32c of the body and compares it to the stored `header.checksum`.

```rust
for page_id in 2..page_count {
    if !freelist.is_free(page_id) {
        Self::read_page_from_mmap(&mmap, page_id)?;
    }
}
```

If any page fails, `open()` returns `DbError::ChecksumMismatch { page_id, expected, got }`
immediately. No connection is accepted and no `Db` handle is returned.

Free pages are skipped because they are never written by the storage engine
and therefore have no valid page header or checksum. Scanning them would
produce false positives on a freshly created or partially filled database.

### Recovery wiring

Both the network server (`Database::open`) and the embedded handle (`Db::open`)
route through `TxnManager::open_with_recovery()` on every reopen:

```rust
let (txn, _recovery) = TxnManager::open_with_recovery(&mut storage, &wal_path)?;
```

This ensures WAL replay runs before the first query is executed, even if the
only change in this subphase is the corruption scan. Bypassing
`open_with_recovery()` with the older `TxnManager::open()` was an oversight
that this subphase closes.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Scan only allocated pages</span>
Free pages contain no valid page header — `file.set_len()` zero-initializes them, giving `checksum = 0`. The CRC32c of an all-zero body is non-zero, so scanning free pages would produce a spurious `ChecksumMismatch` on every fresh or sparsely-used database. The freelist (already in memory by step 2) provides the allocation bitmap at zero extra I/O cost.
</div>
</div>

---

## MemoryStorage — In-Memory for Tests

`MemoryStorage` stores pages in a `Vec<Box<Page>>`. It implements the same
`StorageEngine` trait as `MmapStorage`. All unit tests for the B+ Tree, WAL,
and catalog use `MemoryStorage`, so they run without touching the filesystem.

```rust
let mut storage = MemoryStorage::new();
let id = storage.alloc_page(PageType::Data)?;
let mut page = Page::new(PageType::Data, id);
page.body_mut()[0] = 0xAB;
page.update_checksum();
storage.write_page(id, &page)?;
let read_back = storage.read_page(id)?;
assert_eq!(read_back.body()[0], 0xAB);
```

---

## FreeList — Page Allocation

The FreeList tracks which pages are free using a bitmap. The bitmap is stored in a
dedicated page (or pages, for large databases). Each bit corresponds to one page:
`1 = free`, `0 = in use`.

### Allocation

Scans left-to-right for the first `1` bit, clears it, and returns the page ID.

```
Bitmap: 1110 1101 ...
         ↑
         First free: page 0 (bit 0 = 1)
```

After allocation: `0110 1101 ...`

### Deallocation

Sets the bit corresponding to `page_id` back to `1`. Returns
`DbError::DoubleFree` if the bit was already `1` (guard against bugs in the
caller).

### Invariants

- No page appears twice in the freelist.
- No page can be both allocated and in the freelist simultaneously.
- The freelist bitmap is itself stored in allocated pages (and tracked recursively
  during bootstrap).

---

## Heap Pages — Slotted Format

Table rows (heap tuples) are stored in `PageType::Data` pages using a slotted page
layout. The slot array grows from the start of the body; tuples grow from the end
toward the center.

```
Body (16,320 bytes):
┌─────────────────────────────────────────────────────────────┐
│ Slot[0] │ Slot[1] │ ... │ free space │ ... │ Tuple[1] │ Tuple[0] │
└──────────────────────────────────────────────────────────────┘
↑                           ↑           ↑
free_start              free area      free_end (decreases)
```

`free_start` points to the first unused byte after the last slot entry.
`free_end` points to the first byte of the last tuple written (counting from the
end of the body).

### SlotEntry — 4 bytes

```text
Offset  Size  Field
     0     2  offset   — byte offset of the tuple within the body (0 = empty slot)
     2     2  length   — total length of the tuple in bytes
```

A slot with `offset = 0` and `length = 0` is an empty (deleted) slot. Deleted slots
are reused when the page is compacted (VACUUM, planned Phase 9).

### RowHeader — 24 bytes

Every heap tuple begins with a RowHeader that stores MVCC visibility metadata:

```text
Offset  Size  Field
     0     8  xmin      — txn_id of the transaction that inserted this row
     8     8  xmax      — txn_id of the transaction that deleted/updated this row (0 = live)
    16     1  deleted   — 1 if this row has been logically deleted
    17     7  _pad      — alignment
Total: 24 bytes
```

After the RowHeader comes the null bitmap and the encoded column data (see [Row Codec](row-codec.md)).

### Null Bitmap in Heap Rows

The null bitmap is stored immediately after the `RowHeader`. It occupies
`ceil(n_cols / 8)` bytes. Bit `i` (zero-indexed) being `1` means column `i` is NULL.

```
5 columns → ceil(5/8) = 1 byte = 8 bits (bits 5-7 unused, always 0)
11 columns → ceil(11/8) = 2 bytes
```

---

## Page 0 — The Meta Page

Page 0 is the `PageType::Meta` page. It is written during database creation
(bootstrap) and read during `open()`. Its body contains:

```text
Offset  Size  Field
     0     8  format_version     — AxiomDB file format version
     8     8  catalog_root_page  — Page ID of the catalog root (axiom_tables B+ Tree root)
    16     8  freelist_root_page — Page ID of the freelist bitmap root
    24     8  next_txn_id        — Next transaction ID to assign
    32     8  checkpoint_lsn     — LSN of the last successful checkpoint
    40   rest _reserved          — Future extensions
```

On crash recovery, the `checkpoint_lsn` tells the WAL reader where to start replaying.
All WAL entries with LSN > `checkpoint_lsn` and belonging to committed transactions
are replayed.

---

## Batch Delete Operations

AxiomDB implements three optimizations for DELETE workloads that dramatically reduce
page I/O and CRC32c computation overhead.

### HeapChain::delete_batch()

`delete_batch()` accepts a slice of `(page_id, slot_id)` pairs and groups them by
`page_id` before touching any page. For each unique page it reads the page once,
marks all targeted slots dead in a single pass, then writes the page back once.

```
Naive per-row delete path (before delete_batch):
  for each of N rows:
    read_page(page_id)          ← 1 read
    mark slot dead              ← 1 mutation
    update_checksum(page)       ← 1 CRC32c over 16 KB
    write_page(page_id, page)   ← 1 write
  Total: 3N page operations

Batch path (delete_batch):
  group rows by page_id → P unique pages
  for each page:
    read_page(page_id)          ← 1 read
    mark all M slots dead       ← M mutations (M rows on this page)
    update_checksum(page)       ← 1 CRC32c (once per page, not per row)
    write_page(page_id, page)   ← 1 write
  Total: 2P page operations
```

At 200 rows/page, deleting 10,000 rows hits 50 pages. The naive path requires 30,000
page operations; `delete_batch()` requires 100.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">300× Fewer Page Operations Than InnoDB Per-Row Buffer Pool Hits</span>
MySQL InnoDB processes each DELETE row individually: it pins the page in the buffer pool, applies the undo log entry, updates the row's delete-mark, and releases the pin — once per row. For a 10K-row full-table DELETE, AxiomDB performs 100 page operations (read + write per page); InnoDB performs 10,000+ buffer pool pin/unpin cycles plus 10,000 undo log entries.
</div>
</div>

### mark_deleted() vs delete_tuple() — Splitting Checksum Work

`heap::mark_deleted()` is an internal function that stamps the slot as dead without
recomputing the page checksum. `delete_tuple()` (the single-row public API) calls
`mark_deleted()` followed immediately by `update_checksum()` — behavior is unchanged
for callers.

The batch path calls `mark_deleted()` N times (once per slot on a given page), then
calls `update_checksum()` exactly once when all slots on that page are done.

```rust
// Single-row path (public, unchanged):
pub fn delete_tuple(page: &mut Page, slot_id: u16) -> Result<(), DbError> {
    mark_deleted(page, slot_id)?;   // stamp dead
    page.update_checksum();          // 1 CRC32c
    Ok(())
}

// Batch path (called by delete_batch for each page):
for &slot_id in slots_on_this_page {
    mark_deleted(page, slot_id)?;   // stamp dead, no checksum
}
page.update_checksum();             // 1 CRC32c for all N slots on this page
```

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Deferred Checksum in Batch Paths</span>
CRC32c over a 16 KB page costs roughly 4–8 µs on modern hardware. Calling it once per deleted slot instead of once per page wastes N-1 full-page hashes per batch. Splitting <code>mark_deleted</code> from <code>update_checksum</code> makes the cost O(P) in the number of pages, not O(N) in the number of rows. The same split was applied to <code>insert_batch</code> in Phase 3.17.
</div>
</div>

### scan_rids_visible()

`HeapChain::scan_rids_visible()` is a variant of `scan_visible()` that returns only
`(page_id, slot_id)` pairs — no row data is decoded or copied.

```rust
pub fn scan_rids_visible(
    &self,
    storage: &dyn StorageEngine,
    snapshot: &TransactionSnapshot,
    self_txn_id: u64,
) -> Result<Vec<(u64, u16)>, DbError>
```

This is used by DELETE without a WHERE clause and TRUNCATE TABLE: both operations
need to locate every live slot but neither needs to decode the row's column values.
Avoiding `Vec<u8>` allocation for each row's payload cuts memory allocation to near
zero for full-table deletes.

### HeapChain::clear_deletions_by_txn()

`clear_deletions_by_txn(txn_id)` is the undo helper for `WalEntry::Truncate`. It
scans the entire heap chain and, for every slot where `txn_id_deleted == txn_id`,
clears the deletion stamp (sets `txn_id_deleted = 0`, `deleted = 0`).

This is used during ROLLBACK and crash recovery when a `WalEntry::Truncate` must be
undone. The cost is O(P) page reads and writes for P pages in the chain — identical
to a full-table scan. Because recovery and rollback are infrequent relative to inserts
and deletes, this trade-off is acceptable (see WAL internals for the corresponding
`WalEntry::Truncate` design decision).

---

## All-Visible Page Flag (Optimization A)

### What it is

Bit 0 of `PageHeader.flags` (`PAGE_FLAG_ALL_VISIBLE = 0x01`). When set, it
asserts that every alive slot on the page was inserted by a **committed** transaction
and none have been deleted. Sequential scans can skip per-slot MVCC
`txn_id_deleted` tracking for those pages entirely.

Inspired by PostgreSQL's all-visible map (`src/backend/storage/heap/heapam.c:668`),
but implemented as an in-page bit rather than a separate VM file — a single
cache-line read suffices.

### API

```rust
pub const PAGE_FLAG_ALL_VISIBLE: u8 = 0x01;

impl Page {
    pub fn is_all_visible(&self) -> bool { ... }   // reads bit 0 of flags
    pub fn set_all_visible(&mut self) { ... }       // sets bit 0; caller updates checksum
    pub fn clear_all_visible(&mut self) { ... }     // clears bit 0; caller updates checksum
}
```

### Lazy-set during scan

`HeapChain::scan_visible()` sets the flag after verifying that all alive slots
on a page satisfy:
- `txn_id_created <= max_committed` (committed transaction)
- `txn_id_deleted == 0` (not deleted)

This is a one-time write per page per table lifetime. After the first slow-path
scan, every subsequent scan takes the fast path and skips per-slot checks.

### Clearing on delete

`heap::mark_deleted()` clears the flag **unconditionally** as its very first
mutation — before stamping `txn_id_deleted`. Both changes land in the same
`update_checksum()` + `write_page()` call. There is no window where the flag is
set while a slot is deleted.

### Read-only variant for catalog scans

`HeapChain::scan_visible_ro()` takes `&dyn StorageEngine` (immutable) and never
sets the flag. Used by `CatalogReader` and other callers that hold only a shared
reference. Catalog tables are small (a few pages) and not hot enough to warrant
the lazy-set write.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
After the first scan on a stable table, SELECT skips N per-slot MVCC comparisons
(4 u64 comparisons each) and replaces them with 1 bit-check per page.
At 200 rows/page, a 10K-row scan goes from 10,000 visibility checks to 50 flag reads.
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision</span>
In-page bit vs. separate visibility map file (PostgreSQL's approach): the in-page
bit requires no additional file I/O and is covered by the existing page checksum.
The trade-off is that clearing the flag on any delete requires a page write — the
same write already happening for the slot stamp, so no additional I/O is incurred.
</div>
</div>

---

## Sequential Scan Prefetch Hint (Optimization C)

### What it is

`StorageEngine::prefetch_hint(start_page_id, count)` — a hint method telling
the backend that pages starting at `start_page_id` will be read sequentially.
Implementations that do not support prefetch provide a default no-op.

Inspired by PostgreSQL's `read_stream.c` adaptive lookahead.

### API

```rust
// Default no-op in the trait — all existing backends compile unchanged
fn prefetch_hint(&self, start_page_id: u64, count: u64) {}
```

`MmapStorage` overrides this with `madvise(MADV_SEQUENTIAL)` on macOS and Linux:

```rust
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn prefetch_hint(&self, start_page_id: u64, count: u64) {
    // SAFETY: ptr derived from live MmapMut, offset < mmap_len verified,
    // clamped_len <= mmap_len - offset. madvise is a pure hint.
    let _ = unsafe { libc::madvise(ptr, clamped_len, libc::MADV_SEQUENTIAL) };
}
```

`count = 0` uses the backend default (`PREFETCH_DEFAULT_PAGES = 64`, 1 MB).

### Call sites

`HeapChain::scan_visible()`, `scan_rids_visible()`, and `delete_batch()` each
call `storage.prefetch_hint(root_page_id, 0)` once before their scan loop. This
tells the OS kernel to begin async read-ahead for the pages that follow,
overlapping disk I/O with CPU processing of the current page.

### When it helps

The hint has measurable impact on cold-cache workloads (data not in OS page
cache). On warm cache (mmap pages already faulted in), `madvise` is accepted
but the kernel takes no additional action — no performance regression.

---

## Lazy Column Decode (Optimization B)

### What it is

`decode_row_masked(bytes, schema, mask)` — a variant of `decode_row` that accepts
a boolean mask. When `mask[i] == false`, the column's wire bytes are skipped
(cursor advanced, no allocation) and `Value::Null` is placed in the output slot.

Inspired by PostgreSQL's selective column access in the executor.

### API

```rust
pub fn decode_row_masked(
    bytes: &[u8],
    schema: &[DataType],
    mask: &[bool],      // mask.len() must equal schema.len()
) -> Result<Vec<Value>, DbError>
```

For skipped columns:
- Fixed-length types (Bool=1B, Int/Date=4B, BigInt/Real/Timestamp=8B, Decimal=17B, Uuid=16B):
  `ensure_bytes` is called then `pos` advances — no allocation.
- Variable-length types (Text, Bytes): the 3-byte length prefix is read to advance
  `pos` by `3 + len` — the payload is never copied or parsed.
- NULL columns (bitmap bit set): no wire bytes, cursor unchanged regardless of mask.

### Column mask computation

The executor computes the mask via `collect_column_refs(expr, mask)`, which walks
the AST and marks every `Expr::Column { col_idx }` reference. It does not recurse
into subquery bodies (different row scope).

`SELECT *` (Wildcard/QualifiedWildcard) always produces `None` — `decode_row()`
is used directly with no overhead.

When all mask bits are `true`, `scan_table` also uses `decode_row()` directly.

### Where it applies

- `execute_select_ctx` (single-table SELECT): mask covers SELECT list + WHERE + ORDER BY + GROUP BY + HAVING
- `execute_delete_ctx` (DELETE with WHERE): mask covers the WHERE clause only (no-WHERE path uses `scan_rids_visible` — no decode at all)
