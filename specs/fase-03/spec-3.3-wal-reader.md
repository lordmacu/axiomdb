# Spec: 3.3 — WalReader

## What to build

A WAL file reader that exposes two scan modes:

- **Forward**: from the start of the WAL (or from a specific LSN) towards the end,
  stopping at the first truncated/corrupt entry — correct behavior for crash recovery.
- **Backward**: from the last valid entry towards the start, using the `entry_len_2` trailer
  to navigate without reading the entire file — required for ROLLBACK.

The WalReader does not keep the file open between scans — each iterator opens its own
`File` handle. This eliminates shared mutable state and allows concurrent scans.

## Inputs / Outputs

- Input: `path: &Path` — path to the existing WAL file
- Output (forward): `impl Iterator<Item = Result<WalEntry, DbError>>`
- Output (backward): `impl Iterator<Item = Result<WalEntry, DbError>>`
- Construction errors: `DbError::WalInvalidHeader` if the header is invalid or the file does not exist

### Iterator behavior

**Forward:**
- Opens `File` in read mode, wrapped in `BufReader<File>` (64KB buffer)
- Verifies the magic header before starting to iterate
- Skips entries with `LSN < from_lsn` (linear scan from WAL_HEADER_SIZE)
- For each entry: parses, verifies CRC — if it fails, returns the error and the iterator ends
- Reaches EOF → iterator ends cleanly (`None`)
- Truncated or corrupt entry → the item is `Err(...)` and the iterator ends

**Backward:**
- Opens `File` in read mode (seekable, without BufReader — seeks invalidate the buffer)
- Verifies the magic header
- Initial position: `file_end` — 4 bytes → read `entry_len_2` → seek to `file_end - entry_len_2`
- Parses the complete entry (reads `entry_len_2` bytes)
- Moves cursor: `current_pos -= entry_len_2`
- Repeats until reaching `WAL_HEADER_SIZE` (start of the entries area)
- Any error → the item is `Err(...)` and the iterator ends

## Use cases

1. **Crash recovery (happy path)**: WAL with 100 all-valid entries → forward returns 100 entries
2. **Crash recovery with truncated tail**: WAL with 50 valid entries + partial bytes at the end → forward returns 50 entries and then `Err(WalEntryTruncated)`
3. **from_lsn skip**: forward with `from_lsn=51` skips the first 50 entries and returns only from LSN 51
4. **Full backward**: returns entries in decreasing LSN order (last → first)
5. **Empty WAL** (header only): forward and backward both end with `None` immediately
6. **Corrupt WAL in the middle**: entry 30 of 100 has a bad CRC → forward returns `Ok` for 1-29, `Err(WalChecksumMismatch)` at 30, end

## Acceptance criteria

- [ ] `WalReader::open()` verifies header and returns error on invalid file
- [ ] `scan_forward(0)` returns all WAL entries in increasing LSN order
- [ ] `scan_forward(N)` skips entries with `LSN < N` and returns from LSN N onward
- [ ] Forward stops at the first corrupt/truncated entry returning `Err`
- [ ] `scan_backward()` returns all entries in decreasing LSN order
- [ ] Backward stops at the first corrupt entry returning `Err`
- [ ] Empty WAL (header only): both iterators end cleanly with `None`
- [ ] Both iterators open their own file handle — no shared mutable state in `WalReader`
- [ ] Integration tests in `tests/integration_wal_reader.rs`
- [ ] No `unwrap()` in `src/reader.rs`

## Out of scope

- LSN indexing for O(1) seek — linear scan is sufficient for this phase
- Multi-segment WAL (rotation) — single file only
- Zero-copy with mmap — entries have variable payloads with owned `Vec<u8>`
- Thread-safe concurrent reading — the iterator is used in one thread at a time

## Dependencies

- `WalEntry::from_bytes()` — subfase 3.1 ✅
- `WalWriter` + constants `WAL_HEADER_SIZE`, `WAL_MAGIC`, `WAL_VERSION` — subfase 3.2 ✅
- `DbError::WalEntryTruncated`, `WalChecksumMismatch`, `WalInvalidHeader` — already in axiomdb-core ✅
