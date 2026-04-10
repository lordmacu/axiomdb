# Spec: 11.2 — TOAST (The Oversized-Attribute Storage Technique)

## What to build (not how)

When a row's encoded size exceeds the page tuple limit (~16 KB), move the
largest Text/Bytes values to overflow pages and store an 8-byte pointer
inline. On read, follow the pointer transparently. Optional LZ4 compression
before overflow.

## Research findings

### Current AxiomDB limits
- PAGE_SIZE = 16,384. PageHeader = 64. RowHeader = 24. SlotEntry = 4.
- MAX_TUPLE_DATA ≈ 16,292 bytes per tuple.
- Text/Bytes encoded as `u24 length + payload` (max 16 MB inline in codec).
- No overflow: `HeapPageFull` error if row doesn't fit.

### Existing overflow infrastructure (clustered_overflow.rs)
- `write_chain(storage, data, batch)` → `Option<first_page_id>` ← **REUSABLE**
- `read_chain(storage, first_page_id, expected_len)` → `Vec<u8>` ← **REUSABLE**
- `free_chain(storage, first_page_id)` ← **REUSABLE**
- Overflow pages: linked list, 16,312 bytes payload per page.

### PostgreSQL TOAST (heaptoast.c)
- Threshold: ~4000 bytes (fits 4 tuples per page).
- 4 strategies: PLAIN, MAIN, EXTENDED, EXTERNAL.
- Compresses with pglz/LZ4, then externalizes to toast table.
- AxiomDB simplification: overflow pages instead of toast table.

## Design decision

### TOAST encoding (inline marker)
Current Text/Bytes encoding: `[u24 length][payload bytes]`.

New encoding with 1-byte discriminant:
```
0x00 [u24 length][payload bytes]       — inline (unchanged, ≤ threshold)
0x01 [u32 uncompressed_len][u64 page]  — overflow pointer (13 bytes total)
0x02 [u32 uncompressed_len][u64 page]  — overflow + LZ4 compressed
```

The discriminant replaces the first byte of the u24 length. For inline values,
the first byte of u24 is always 0x00 when length < 16 MB (the high byte of
u24 LE). **Backward compatible**: existing rows with inline values already
have 0x00 as the high byte of u24 LE length.

Wait — u24 is 3 bytes LE. For a value of length 100: `[0x64, 0x00, 0x00]`.
The third byte (high byte) is 0x00 for values < 65536. For values ≥ 65536
(64 KB), the high byte is non-zero.

**Simpler approach**: prepend a 1-byte tag BEFORE the existing encoding:
```
Tag 0x00: [u24 length][payload]           — inline (add 1 byte overhead)
Tag 0x01: [u64 page_id][u32 raw_len]     — overflow uncompressed (13 bytes)
Tag 0x02: [u64 page_id][u32 raw_len]     — overflow LZ4 compressed (13 bytes)
```

This breaks backward compat for existing data. **Not acceptable.**

**Final approach**: use the existing `u24 length` field with a sentinel value:
- If length == 0xFF_FFFE → next 12 bytes are overflow pointer (page_id:u64 + raw_len:u32)
- If length == 0xFF_FFFD → overflow + LZ4 compressed
- All other u24 values → inline payload as before
- This is backward compatible: existing values never have length == 0xFF_FFFE.

### Threshold
- TOAST when encoded row > MAX_INLINE_ROW (8,000 bytes).
- This allows 2 tuples per page (comfortable margin).
- PostgreSQL uses ~4000; we use 8000 because our page layout has less overhead.

### Compression
- LZ4 compression attempted for values > 256 bytes.
- If compressed size ≥ original, store uncompressed.
- LZ4 chosen for speed (PostgreSQL 14+ default).

## Inputs / Outputs
- Input: row with Text/Bytes values that exceed threshold
- Output: row fits in page; large values stored in overflow chains
- Transparent: SELECT decodes overflow automatically

## Acceptance criteria
- [ ] INSERT with >8 KB Text value succeeds (currently fails with HeapPageFull)
- [ ] SELECT returns the original value (round-trip correctness)
- [ ] DELETE frees overflow pages
- [ ] UPDATE replaces overflow chain
- [ ] LZ4 compression reduces storage for compressible data
- [ ] Existing rows (without TOAST) still decode correctly
- [ ] No performance regression for small rows (< threshold)

## Out of scope
- Per-column TOAST strategy (PLAIN/EXTENDED/EXTERNAL) — all Text/Bytes use EXTENDED
- TOAST for clustered tables (heap only)
- Content-addressed dedup (Phase 14.9)
- MIME type detection (Phase 11.2c)
- Reference counting (Phase 11.2d)

## Dependencies
- clustered_overflow.rs: write_chain, read_chain, free_chain (already exists)
- LZ4 crate (to be added)
