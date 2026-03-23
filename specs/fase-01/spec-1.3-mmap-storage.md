# Spec: 1.3 — MmapStorage

## What to build
mmap-based storage engine: open/create a `.db` file, read and write
pages with direct zero-copy access to the mmap. No free list yet (that is 1.5).

## Inputs / Outputs
- `create(path)` → `MmapStorage` with page 0 (Meta) initialized
- `open(path)` → `MmapStorage` validating magic + checksum of page 0
- `read_page(page_id)` → `&Page` (zero-copy from mmap), verifies checksum
- `write_page(page_id, &Page)` → copies bytes to the mmap
- `flush()` → msync to guarantee durability
- `page_count()` → u64

## File layout
```
offset 0          : Page 0 — Meta (header + DbFileMeta in body)
offset PAGE_SIZE  : Page 1
offset 2*PAGE_SIZE: Page 2
...
```

## DbFileMeta (in body of page 0, offset 64 of the file)
```
offset  size  field
0       8     db_magic: u64    — 0x4E455855_53444201 ("NEXUSDB\1")
8       4     version: u32     — format version (1)
12      4     _pad: u32
16      8     page_count: u64  — total pages in the file
24      ...   reserved
```

## File growth
- Growth unit: **64 pages = 1MB** (reduces truncate syscalls)
- On create: file starts with 64 pre-allocated pages
- On write beyond current range: error `PageNotFound` (alloc is 1.5)

## Acceptance criteria
- [ ] `create` produces a valid file with page 0 of type Meta
- [ ] `open` on a file created with `create` works correctly
- [ ] `open` on a nonexistent file returns `Err(DbError::Io(...))`
- [ ] `read_page(0)` returns the Meta page with valid checksum
- [ ] `write_page` + `read_page` roundtrip preserves exact data
- [ ] `read_page` out of range returns `Err(DbError::PageNotFound)`
- [ ] `flush` completes without error
- [ ] Crash test: write → drop → reopen → read → same content

## Out of scope
- Page alloc/free (1.5)
- MemoryStorage (1.4)
- Concurrency (Phase 7)

## Dependencies
- `nexusdb-storage::page` (already exists)
- `memmap2` crate
