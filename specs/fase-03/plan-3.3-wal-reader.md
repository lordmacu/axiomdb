# Plan: 3.3 — WalReader

## Files to create/modify

- `crates/axiomdb-wal/src/reader.rs` — WalReader, ForwardIter, BackwardIter
- `crates/axiomdb-wal/src/lib.rs` — add `mod reader; pub use reader::WalReader;`
- `crates/axiomdb-wal/tests/integration_wal_reader.rs` — integration tests

## Data structures

```rust
/// WAL file reader. Stateless — opens a File per scan.
pub struct WalReader {
    path: PathBuf,
}

/// Forward iterator — BufReader to amortize syscalls.
pub struct ForwardIter {
    reader: BufReader<File>,
    from_lsn: u64,
    done: bool,    // true after the first error — the iterator ends
}

/// Backward iterator — direct seekable File (seeks invalidate BufReader).
pub struct BackwardIter {
    file: File,
    cursor: u64,   // position of the start of the next entry to read (going backwards)
    done: bool,
}
```

## Algorithm

### `WalReader::open(path)`

```
1. File::open(path) → if it fails, map to DbError::Io
2. Read 16 header bytes → if < 16 bytes, DbError::WalInvalidHeader
3. Verify magic + version → if invalid, DbError::WalInvalidHeader
4. Return WalReader { path: path.to_path_buf() }
```

Note: we open the file only to verify the header. We do not keep the handle.

### `WalReader::scan_forward(from_lsn)`

```
1. File::open(path)
2. Verify header (already verified in open, but may have been corrupted)
3. Seek to WAL_HEADER_SIZE
4. BufReader::new(file) with 64KB capacity
5. Return ForwardIter { reader, from_lsn, done: false }
```

### `ForwardIter::next()`

```
1. If done → return None
2. Read 4 bytes (entry_len): if EOF → return None; if < 4 bytes → Err(Truncated), done=true
3. entry_len = u32::from_le_bytes(...)
4. Read remaining (entry_len - 4) bytes → if < expected → Err(Truncated), done=true
5. Build full slice (4 + rest) and call WalEntry::from_bytes()
6. If Err → done=true, return Some(Err(...))
7. If Ok((entry, _)):
   - If entry.lsn < from_lsn → continue (next iteration, do not return)
   - If entry.lsn >= from_lsn → return Some(Ok(entry))
```

Optimization: instead of reading 4 + N bytes in two operations, use a pre-allocated buffer.
First read the 4 bytes, then `read_exact` for the remaining `entry_len - 4`.

### `WalReader::scan_backward()`

```
1. File::open(path)
2. Verify header
3. file_len = file.seek(End(0))
4. If file_len == WAL_HEADER_SIZE → no entries, cursor = WAL_HEADER_SIZE
5. Return BackwardIter { file, cursor: file_len, done: false }
```

### `BackwardIter::next()`

```
1. If done → return None
2. If cursor <= WAL_HEADER_SIZE → return None  (reached the beginning)
3. If cursor - WAL_HEADER_SIZE < 4 → Err(Truncated), done=true
4. file.seek(cursor - 4)
5. Read 4 bytes → entry_len_2 (= length of the entry ending at cursor)
6. If cursor < entry_len_2 → Err(Truncated), done=true
7. entry_start = cursor - entry_len_2
8. If entry_start < WAL_HEADER_SIZE → Err(Truncated), done=true
9. file.seek(entry_start)
10. Read entry_len_2 bytes → buf
11. WalEntry::from_bytes(&buf) → if Err → done=true, return Some(Err(...))
12. cursor = entry_start
13. return Some(Ok(entry))
```

## Implementation phases

1. Create `src/reader.rs` with `WalReader`, `ForwardIter`, `BackwardIter`
2. Export from `src/lib.rs`
3. Write integration tests in `tests/integration_wal_reader.rs`

## Tests to write

### Unit tests (in reader.rs)

- `test_open_valid_wal` — open() on a valid (empty) WAL → Ok
- `test_open_invalid_magic` — open() on file with incorrect magic → Err(WalInvalidHeader)
- `test_open_nonexistent` — open() on non-existent path → Err(Io)
- `test_forward_empty_wal` — WAL with header only → forward returns None immediately
- `test_backward_empty_wal` — same for backward

### Integration tests (`tests/integration_wal_reader.rs`)

- `test_forward_all_entries` — write N entries with writer, read with forward from LSN 0
- `test_forward_from_lsn` — skip the first K entries, verify entries are received from LSN K+1
- `test_forward_stops_on_truncation` — write entries, truncate file to middle of last entry → forward returns N-1 entries + Err at the end
- `test_backward_all_entries` — verify reverse LSN order
- `test_backward_matches_forward_reversed` — backward must be the exact reverse of forward
- `test_forward_crc_corruption` — bit flip in entry payload → Err(WalChecksumMismatch)

## Anti-patterns to avoid

- **DO NOT** read the entire file into RAM in `open()` — the scan must be lazy/streaming
- **DO NOT** use `BufReader` in `BackwardIter` — seeks invalidate the internal buffer
- **DO NOT** share a `File` handle between `ForwardIter` and `BackwardIter` — each opens its own
- **DO NOT** `unwrap()` in `src/reader.rs` — everything handles `Result`
- **DO NOT** return `Iterator<Item = WalEntry>` without the `Result` — corruption is a real case

## Risks

- **Corrupt entry_len_2 in backward scan** → detected because `WalEntry::from_bytes()` verifies
  the CRC and also verifies that `entry_len_2 == entry_len` → returns `Err` → iterator ends
- **read_exact in ForwardIter may block on slow hardware** → acceptable, we use synchronous `File`
- **file_len changes between open() and scan** → for recovery, the WAL is not written concurrently
  with the read (recovery happens before opening the engine) → not a real case
