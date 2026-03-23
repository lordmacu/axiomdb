# Spec: 1.6 — Trait StorageEngine

## What to build
`StorageEngine` trait that unifies MmapStorage and MemoryStorage under a
swappable interface. Allows all code in future phases (B-tree, WAL, SQL)
to work with any storage backend without concrete coupling.

## Where it lives
`axiomdb-storage::engine` — NOT in axiomdb-core, because it uses `Page` and `PageType`
which live in axiomdb-storage. Placing it in axiomdb-core would create a dep cycle.

axiomdb-core keeps: `RecordId`, `PageId`, `TxnId`, `Index` trait (abstract, no Page).

## Trait API

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

## FreeList integration in MmapStorage
- `MmapStorage` embeds `FreeList` (loaded from page 1 on open/create)
- `alloc_page`: uses freelist, grows if full
- `free_page`: marks in freelist, persists page 1
- Page 1 is always of type `Free` (bitmap page)

## Acceptance criteria
- [ ] `MmapStorage` implements `StorageEngine`
- [ ] `MemoryStorage` implements `StorageEngine`
- [ ] `Box<dyn StorageEngine>` compiles and works correctly
- [ ] `MmapStorage::alloc_page` returns unique non-overlapping page_ids
- [ ] `MmapStorage::free_page` + `alloc_page` reuses the page_id
- [ ] FreeList of MmapStorage persists across reopen
- [ ] axiomdb-core::traits::StorageEngine removed (replaced by the new one)

## Out of scope
- `Sync` (interior mutability, locks — Phase 7 MVCC)
- Encryption, compression
