# Plan: 11.2 — TOAST

## Files to create/modify

| File | What changes |
|------|-------------|
| `Cargo.toml` (workspace) | Add `lz4_flex` crate (pure Rust LZ4) |
| `crates/axiomdb-types/Cargo.toml` | Add `lz4_flex` dependency |
| `crates/axiomdb-types/src/codec.rs` | Add TOAST sentinel encode/decode for Text/Bytes |
| `crates/axiomdb-storage/src/heap_chain_write.rs` | Check row size, TOAST large values before insert |
| `crates/axiomdb-storage/src/heap_chain_scan.rs` | Decode TOAST pointers, read overflow chains |
| `crates/axiomdb-sql/src/table_write.rs` | Wire TOAST into INSERT path |
| `crates/axiomdb-sql/src/table.rs` | Wire TOAST decode into scan/read path |

## Algorithm

### Encode path (INSERT/UPDATE)
1. Encode row normally via `encode_row()`
2. If `encoded.len() > MAX_INLINE_ROW` (8000):
   a. Find largest Text/Bytes column
   b. Try LZ4 compress: if compressed < original, use compressed
   c. Write to overflow chain: `write_chain(storage, data)`
   d. Replace inline bytes with sentinel: `[0xFF, 0xFE, 0x??][page_id:8][raw_len:4]`
   e. Re-check size; repeat if still too large
3. Insert the (now smaller) row into heap page

### Decode path (SELECT)
1. Decode row via `decode_row()`
2. For each Text/Bytes field: check if u24 length == 0xFF_FFFE or 0xFF_FFFD
3. If sentinel: read `page_id` + `raw_len` from next 12 bytes
4. Call `read_chain(storage, page_id, raw_len)`
5. If 0xFF_FFFD: LZ4 decompress
6. Replace sentinel with actual value

### Delete path
1. Before deleting heap row: scan for TOAST sentinels
2. For each overflow pointer: `free_chain(storage, page_id)`
3. Then delete heap row normally

## Implementation phases

### Phase 1: LZ4 dependency + codec sentinels
1. Add `lz4_flex` to workspace Cargo.toml
2. In `codec.rs`: add constants `TOAST_SENTINEL = 0xFF_FFFE`, `TOAST_LZ4_SENTINEL = 0xFF_FFFD`
3. Add `encode_toast_pointer()` and `decode_toast_pointer()` helpers
4. Unit tests for sentinel round-trip

### Phase 2: Write path (TOAST on INSERT)
1. In `heap_chain_write.rs` or `table_write.rs`: after `encode_row()`, check size
2. If oversized: identify largest varlena, compress + externalize
3. Replace inline data with sentinel + pointer
4. Insert the reduced row

### Phase 3: Read path (de-TOAST on SELECT)
1. In `decode_row()` or a wrapper: detect sentinels
2. Call `read_chain()` + optional LZ4 decompress
3. Return reconstructed Value

### Phase 4: Delete/Update path
1. Before heap delete: scan row for TOAST pointers, free chains
2. UPDATE: old row → free old chains; new row → TOAST if needed

## Tests
- `test_toast_large_text_insert_select` — 100 KB text round-trip
- `test_toast_lz4_compression` — verify compressed < original for compressible data
- `test_toast_small_values_unchanged` — values < threshold NOT toasted
- `test_toast_delete_frees_overflow` — overflow pages freed on DELETE
- `test_toast_multiple_large_columns` — row with 2 large Text columns

## Anti-patterns
- DO NOT change the encoding for values < threshold (backward compat)
- DO NOT TOAST fixed-size types (Int, BigInt, etc.) — only Text/Bytes
- DO NOT use system liblz4 — use `lz4_flex` (pure Rust, no C dep)

## Risks
| Risk | Mitigation |
|------|-----------|
| Sentinel collision with real length | 0xFF_FFFE = 16,777,214 bytes — real values this size already overflow pages |
| LZ4 makes data larger | Compare compressed vs original; use uncompressed if larger |
| Overflow chain corruption | read_chain has loop detection; checksum per page |
