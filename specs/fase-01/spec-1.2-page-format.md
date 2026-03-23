# Spec: 1.2 — Page Format

## What to build
On-disk/in-memory page structure: `Page`, `PageHeader`, `PageType`.
Foundation of the entire storage engine — all future phases read/write pages.

## Inputs / Outputs
- Input: raw buffer `[u8; PAGE_SIZE]` from mmap or RAM
- Output: typed access to header + data without copying

## Constants
- `PAGE_SIZE = 16_384` (16KB)
- `HEADER_SIZE = 64` (1 cache line)
- `PAGE_MAGIC = 0x4158494F_4D444200u64` ("AXIOMDB\0")

## PageHeader layout (64 bytes, repr(C))
```
offset  size  field
0       8     magic: u64          — detect corruption / invalid file
8       1     page_type: u8       — PageType discriminant
9       1     flags: u8           — bits: dirty, pinned, compressed, ...
10      2     item_count: u16     — number of items in the page
12      4     checksum: u32       — CRC32c of bytes [64..PAGE_SIZE]
16      8     page_id: u64        — logical position in the file
24      8     lsn: u64            — Log Sequence Number of the last write
32      2     free_start: u16     — offset of the start of free space
34      2     free_end: u16       — offset of the end of free space
36      28    _reserved: [u8;28]  — padding up to 64 bytes
```

## PageType
```rust
Meta = 0      — page 0, file header
Data = 1      — table rows (heap)
Index = 2     — B+ Tree node
Overflow = 3  — data that does not fit in a single Data page
Free = 4      — page in the free list
```

## Use cases
1. Create new page in RAM → header initialized, checksum computed
2. Read page from disk → verify magic + checksum, error if invalid
3. Write data into page → update free_start, recompute checksum before flush

## Acceptance criteria
- [ ] `size_of::<PageHeader>() == 64`
- [ ] `size_of::<Page>() == PAGE_SIZE`
- [ ] `align_of::<Page>() == 64`
- [ ] `Page::new(PageType::Data, page_id)` produces a valid page
- [ ] `page.verify_checksum()` returns `Ok(())` on an intact page
- [ ] `page.verify_checksum()` returns `Err(DbError::ChecksumMismatch)` if 1 byte is corrupted
- [ ] `page.header().magic == PAGE_MAGIC`

## Out of scope
- Reading/writing from disk (that is 1.3)
- Row serialization inside the data area (later phases)

## Dependencies
- `axiomdb-core` with `DbError` (already exists)
- `crc32c` crate for hardware-accelerated checksum
