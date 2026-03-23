# Spec: 1.5 — Free List

## What to build
Bitmap-based free list persisted in page 1 of the file. Alloc in O(1) using
trailing_zeros over 64-bit words. Free in O(1). Automatically grows the
storage when space is exhausted.

## Capacity
- 1 bitmap page covers `(PAGE_SIZE - HEADER_SIZE) * 8 = 130,560 pages`
- 130,560 pages × 16KB = ~2GB of data covered by a single bitmap
- Sufficient for all project phases without multi-page support

## Bitmap layout (body of page 1)
```
bits: 0 = USED, 1 = FREE
word 0 → pages 0-63
word 1 → pages 64-127
...
Bit ordering: LSB-first within each u64
```

## Initial state on create
- Pages 0 and 1 = USED (meta and bitmap, permanently reserved)
- Pages 2..total_pages = FREE

## Public API

### FreeList (pure bitmap, no storage dependency)
- `FreeList::new(total_pages, reserved: &[u64])` → initializes bitmap
- `FreeList::from_bytes(bytes, total_pages)` → deserializes from page body
- `to_bytes(&self, buf: &mut [u8])` → serializes for writing to page
- `alloc(&mut self) → Option<u64>` → first free page, marks it used
- `free(&mut self, page_id) → Result<()>` → marks free
- `total_pages(&self) → u64`
- `grow(&mut self, new_total)` → extends bitmap, new pages = FREE

### MmapStorage (new methods)
- `grow(&mut self, extra_pages: u64) → Result<u64>` → extends file and remaps, returns first new page_id
- `set_page_count(&mut self, count: u64) → Result<()>` → updates meta without re-reading (fixed offset)

### MemoryStorage (new methods)
- `free_page(&mut self, page_id: u64) → Result<()>` → returns page to the pool

## Acceptance criteria
- [ ] `alloc` returns unique and increasing page_ids starting from 2
- [ ] `free` + `alloc` → reuses the freed page_id (lowest page first)
- [ ] bitmap serializes/deserializes without loss
- [ ] MmapStorage grows automatically when pages are exhausted
- [ ] grow persists to disk (reopen sees the new page_count)
- [ ] Pages 0 and 1 are never returned by alloc
- [ ] double-free returns an error

## Out of scope
- Multi-bitmap (more than 130,560 pages — future phases)
- StorageEngine trait (1.6)
