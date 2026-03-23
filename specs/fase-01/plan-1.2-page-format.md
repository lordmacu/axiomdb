# Plan: 1.2 — Page Format

## Files to create/modify
- `crates/nexusdb-storage/src/page.rs` — Page, PageHeader, PageType
- `crates/nexusdb-storage/src/lib.rs` — re-export module
- `crates/nexusdb-storage/Cargo.toml` — add crc32c, bytemuck

## Dependencies to add
```toml
# nexusdb-storage/Cargo.toml
crc32c   = "0.6"        # CRC32c with hardware acceleration (SSE4.2 / ARM CRC)
bytemuck = { version = "1", features = ["derive"] }
nexusdb-core = { workspace = true }
```

## Data structures

```rust
pub const PAGE_SIZE: usize = 16_384;
pub const HEADER_SIZE: usize = 64;
pub const PAGE_MAGIC: u64 = 0x4E455855_53444200;

#[repr(u8)]
pub enum PageType { Meta=0, Data=1, Index=2, Overflow=3, Free=4 }

#[repr(C)]
struct PageHeader {
    magic:      u64,   // 8
    page_type:  u8,    // 1
    flags:      u8,    // 1
    item_count: u16,   // 2
    checksum:   u32,   // 4  → total 16
    page_id:    u64,   // 8  → 24
    lsn:        u64,   // 8  → 32
    free_start: u16,   // 2  → 34
    free_end:   u16,   // 2  → 36
    _reserved:  [u8;28], // 28 → 64
}

#[repr(C, align(64))]
pub struct Page {
    data: [u8; PAGE_SIZE],
}
```

## Public API

```rust
impl Page {
    pub fn new(page_type: PageType, page_id: u64) -> Self
    pub fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Result<Self, DbError>  // verifies magic+checksum
    pub fn header(&self) -> &PageHeader
    pub fn header_mut(&mut self) -> &mut PageHeader
    pub fn body(&self) -> &[u8]         // bytes[HEADER_SIZE..]
    pub fn body_mut(&mut self) -> &mut [u8]
    pub fn verify_checksum(&self) -> Result<(), DbError>
    pub fn update_checksum(&mut self)   // call before flush
}
```

## Implementation phases
1. Add deps in Cargo.toml
2. Define constants and PageType
3. Define PageHeader with `#[repr(C)]`, verify size in const assert
4. Define Page with `#[repr(C, align(64))]`, verify size/align in const assert
5. Implement `Page::new` — initialize header, compute checksum
6. Implement `Page::from_bytes` — verify magic, verify checksum
7. Implement `verify_checksum` and `update_checksum`
8. Unit tests

## Tests
- `test_page_size_and_alignment` — const asserts at runtime
- `test_new_page_valid` — new page passes verify_checksum
- `test_checksum_detects_corruption` — mutate 1 byte → error
- `test_invalid_magic` — incorrect magic → error
- `test_from_bytes_roundtrip` — new → as_bytes → from_bytes → OK

## Antipatterns to avoid
- DO NOT use `std::mem::transmute` directly — use bytemuck or raw pointers with SAFETY
- DO NOT expose PageHeader as pub outside the crate — access only through Page methods
- DO NOT compute checksum over the header (would include the checksum field itself) — only over body

## Risks
- `bytemuck::Pod` requires no hidden padding → verify with `size_of` asserts
- CRC32c vs CRC32: make sure to use the `crc32c` crate (not `crc32fast`)
