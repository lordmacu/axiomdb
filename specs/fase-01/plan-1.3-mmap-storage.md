# Plan: 1.3 — MmapStorage

## Files to create/modify
- `crates/axiomdb-storage/src/mmap.rs` — MmapStorage + DbFileMeta
- `crates/axiomdb-storage/src/lib.rs` — pub mod mmap
- `crates/axiomdb-storage/Cargo.toml` — add memmap2

## Dependency
```toml
memmap2 = "0.9"
```

## Structures

```rust
const DB_FILE_MAGIC: u64 = 0x4E455855_53444201; // "NEXUSDB\1"
const DB_VERSION: u32    = 1;
const GROW_PAGES: u64    = 64; // 1MB per chunk

#[repr(C)]
struct DbFileMeta {
    db_magic:   u64,
    version:    u32,
    _pad:       u32,
    page_count: u64,
    _reserved:  [u8; PAGE_SIZE - HEADER_SIZE - 24],
}

pub struct MmapStorage {
    mmap: MmapMut,
    file: std::fs::File,
}
```

## Implementation phases

1. Add `memmap2` to Cargo.toml
2. Define `DbFileMeta` with const assert `size_of == PAGE_SIZE - HEADER_SIZE`
3. Implement `MmapStorage::create(path)`
   - Create file, truncate to `GROW_PAGES * PAGE_SIZE`
   - Mmap the file
   - Write page 0: `Page::new(Meta, 0)` + DbFileMeta in body
   - flush
4. Implement `MmapStorage::open(path)`
   - Open existing file in read-write mode
   - Mmap
   - Verify page 0: magic + checksum
5. Implement `read_page(page_id) -> Result<&Page>`
   - Bounds check vs page_count from meta
   - Cast ptr to &Page (SAFETY documented)
   - verify_checksum
6. Implement `write_page(page_id, &Page)`
   - Bounds check
   - copy_from_slice bytes to the correct mmap offset
7. Implement `flush()` → `mmap.flush()`
8. Implement `page_count()` → read from meta
9. Private method `meta(&self) -> &DbFileMeta`
10. Tests

## SAFETY for read_page
```
// SAFETY: offset = page_id * PAGE_SIZE is within self.mmap (verified by
// bounds check). The mmap is aligned to OS page (≥4KB, multiple of 64).
// PAGE_SIZE=16384 is a multiple of 64, so each page is align(64).
// Page is repr(C, align(64)). No simultaneous mutable aliases exist because
// write_page takes &mut self.
let page = unsafe { &*(ptr as *const Page) };
```

## Tests
- `test_create_and_open` — create + drop + open → valid meta
- `test_read_write_roundtrip` — write page 5 → read → identical data
- `test_out_of_bounds_read` — page_id >= page_count → PageNotFound
- `test_flush_and_reopen` — write + flush + drop + open + read → data persisted
- `test_checksum_corruption_detected` — corrupt byte on disk → read_page Err

## Antipatterns to avoid
- DO NOT use MmapMut::map_anon — we need a real file
- DO NOT truncate the file after mmap without re-mapping
- DO NOT compute page_count from file size at read time — use the meta page
