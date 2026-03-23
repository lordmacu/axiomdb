# Spec: 1.4 — MemoryStorage

## What to build
In-RAM implementation of the storage engine. Same API as MmapStorage but with
no I/O whatsoever. Used in all unit tests for future phases.

## Inputs / Outputs
- `new()` → empty MemoryStorage (page 0 Meta initialized automatically)
- `read_page(page_id)` → `&Page`, verifies checksum
- `write_page(page_id, &Page)` → stores a copy in RAM
- `alloc_page_raw(page_type)` → u64 (new page_id) — no free list yet
- `flush()` → no-op (returns Ok(()))
- `page_count()` → u64

## Acceptance criteria
- [ ] `MemoryStorage::new()` returns storage with a valid page 0
- [ ] `write_page` + `read_page` roundtrip preserves exact data
- [ ] `read_page` on an unwritten page returns `Err(PageNotFound)`
- [ ] `alloc_page_raw` returns consecutive page_ids starting from 1
- [ ] `flush()` returns `Ok(())`
- [ ] Does not require any file on disk

## Out of scope
- Real free list (1.5)
- StorageEngine trait (1.6)

## Dependencies
- `axiomdb-storage::page` (already exists)
