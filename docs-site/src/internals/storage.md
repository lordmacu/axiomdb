# Storage Engine

The storage engine is the lowest user-accessible layer in AxiomDB. It manages raw
16-kilobyte pages on disk or in memory, provides a freelist for page allocation, and
exposes a simple trait that all higher layers depend on.

---

## The StorageEngine Trait

```rust
pub trait StorageEngine: Send {
    fn read_page(&self, page_id: u64) -> Result<&Page, DbError>;
    fn write_page(&mut self, page_id: u64, page: &Page) -> Result<(), DbError>;
    fn alloc_page(&mut self, page_type: PageType) -> Result<u64, DbError>;
    fn free_page(&mut self, page_id: u64) -> Result<(), DbError>;
    fn flush(&mut self) -> Result<(), DbError>;
    fn page_count(&self) -> u64;
}
```

`read_page` returns `&Page` tied to `&self`. While that reference exists, the borrow
checker prevents any `&mut self` method from being called. This invariant is correct:
you cannot modify a page while a reference to it is alive, and it requires no locks.

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

`MmapStorage` maps the entire `.db` file using `memmap2::MmapMut`. Each page is
accessible as `&Page` via a pointer into the mapped region.

```
Physical file (axiomdb.db):
┌──────────┬──────────┬──────────┬──────────┬──────────┐
│  Page 0  │  Page 1  │  Page 2  │  Page 3  │  ...     │
│ (Meta)   │ (Data)   │ (Index)  │ (Data)   │          │
└──────────┴──────────┴──────────┴──────────┴──────────┘
     ↑           ↑
     │           └── read_page(1) returns &page_cache[1..] as &Page
     └── mmap covers the entire file
```

**Why mmap is faster than a custom buffer pool:**

| Approach                | Who manages page cache | Extra copies |
|-------------------------|------------------------|--------------|
| mmap (AxiomDB)          | OS kernel              | 0            |
| Custom buffer pool (MySQL InnoDB) | Application + OS  | 1 (data in both) |

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">No Double-Buffer Overhead</span>
MySQL InnoDB keeps every hot page in RAM twice — once in the OS page cache, once in the InnoDB buffer pool. AxiomDB's mmap approach uses the OS page cache directly. For a working set that fits in RAM, this roughly halves the memory footprint of the storage layer.
</div>
</div>

With mmap, `read_page(42)` is a pointer arithmetic operation: `mmap_ptr + 42 * 16384`.
The OS handles readahead, eviction, and write-back (via `msync`). There is no
"copy from disk into buffer pool" step.

**Trade-offs:**
- We cannot control which pages stay hot in memory (the OS uses LRU).
- On 32-bit systems, the address space limits the maximum database size.
  On 64-bit, the address space is effectively unlimited.
- `msync` (for `flush`) blocks until pages are on disk — same guarantee as `fsync`.

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
