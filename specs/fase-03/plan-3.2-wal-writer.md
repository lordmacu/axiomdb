# Plan: WalWriter (Subfase 3.2)

## Files to create/modify

| File | Action | What it does |
|---|---|---|
| `crates/nexusdb-core/src/error.rs` | Modify | Add `WalInvalidHeader` |
| `crates/nexusdb-wal/src/writer.rs` | Create | Complete `WalWriter` |
| `crates/nexusdb-wal/src/lib.rs` | Modify | Expose `writer` module |
| `crates/nexusdb-wal/tests/integration_wal_writer.rs` | Create | Integration tests |

---

## Constants

```rust
pub const WAL_MAGIC: u64   = 0x4E455855_53574100; // "NEXUSWAL\0"
pub const WAL_VERSION: u16 = 1;
pub const WAL_HEADER_SIZE: usize = 16;
```

---

## Algorithm `create(path)`

```
1. File::create_new(path)  — fails if already exists (do not overwrite existing WAL)
2. Write header (16 bytes):
   - magic    (8 bytes LE)
   - version  (2 bytes LE)
   - reserved (6 bytes zeros)
3. fsync the file
4. Wrap in BufWriter (64KB capacity — amortizes syscalls)
5. next_lsn = 1
```

## Algorithm `open(path)`

```
1. OpenOptions::new().read(true).append(true).open(path)
2. Read first 16 bytes
3. Verify magic == WAL_MAGIC        → WalInvalidHeader if not
4. Verify version == WAL_VERSION    → WalInvalidHeader if not
5. Scan entries to find the last valid LSN:
   - Read entry by entry from offset 16
   - Store the LSN of the last entry that parses without error
   - Stop at the first truncated or CRC-invalid entry
6. next_lsn = last_valid_lsn + 1  (or 1 if no entries)
7. Seek to end of file
8. Wrap in BufWriter
```

**Why scan in open():**
If the process died after writing partial entries, the file may end with corrupt bytes.
The scan finds the last complete and valid entry, positioning the writer right after it
to continue without corrupting the WAL.

## Algorithm `append(entry)`

```
1. entry.lsn = self.next_lsn
2. bytes = entry.to_bytes()
3. self.writer.write_all(&bytes)   — writes to BufWriter (RAM)
4. self.next_lsn += 1
5. self.offset += bytes.len() as u64
6. return Ok(assigned_lsn)
```

## Algorithm `commit()`

```
1. self.writer.flush()             — drain BufWriter to OS buffer
2. self.writer.get_ref().sync_all() — fsync: ensure the OS flushed it to disk
3. return Ok(())
```

---

## Struct

```rust
pub struct WalWriter {
    writer:   BufWriter<File>,
    next_lsn: u64,
    offset:   u64,   // byte position (includes header)
}
```

---

## Implementation phases

### Step 1 — Add WalInvalidHeader to DbError
```rust
#[error("invalid WAL file at '{path}': incorrect magic or version")]
WalInvalidHeader { path: String },
```

### Step 2 — Implement writer.rs
In order:
1. Constants `WAL_MAGIC`, `WAL_VERSION`, `WAL_HEADER_SIZE`
2. `fn write_header(file: &mut File) -> Result<(), DbError>`
3. `fn read_and_verify_header(file: &mut File, path: &Path) -> Result<(), DbError>`
4. `fn scan_last_lsn(file: &mut File) -> Result<u64, DbError>` — scans entries, returns last valid LSN
5. `WalWriter::create()`
6. `WalWriter::open()`
7. `WalWriter::append()`
8. `WalWriter::commit()`
9. `WalWriter::current_lsn()` and `WalWriter::file_offset()`
10. Unit tests `#[cfg(test)]`

### Step 3 — Update lib.rs

### Step 4 — Integration tests

---

## Tests to write

**Unit tests (writer.rs `#[cfg(test)]`):**
- `test_header_size_is_16` — verify that write_header writes exactly 16 bytes
- `test_lsn_starts_at_1` — first append returns LSN 1
- `test_lsn_increments` — N appends → LSNs 1..=N

**Integration tests (tests/):**
- `test_create_writes_header` — file has correct magic and version in bytes 0-15
- `test_open_rejects_invalid_magic` — incorrect magic → WalInvalidHeader
- `test_open_rejects_unknown_version` — version 999 → WalInvalidHeader
- `test_append_without_commit_not_durable` — append × N, drop without commit → reopen → entries absent
- `test_append_commit_durable` — append × N + commit → reopen with File::open and read bytes → entries present
- `test_open_continues_lsn` — create + append(×3) + commit + drop → open → append → LSN is 4
- `test_file_offset_grows` — file_offset() grows with each append
- `test_current_lsn_before_and_after` — current_lsn() == 0 before, == N after N appends
- `test_create_fails_if_exists` — create() on existing file → Io error
- `test_multiple_commits` — append + commit + append + commit → reopen → all entries present

---

## Anti-patterns to avoid

- **DO NOT truncate the file in `open()`** — crash recovery needs the existing content
- **DO NOT fsync on every `append()`** — would destroy throughput (target: 180k ops/s)
- **DO NOT use `unwrap()`** in production
- **DO NOT make unnecessary seeks** — `append(true)` in OpenOptions guarantees writes at the end

## Risks

| Risk | Mitigation |
|---|---|
| BufWriter does not flush on drop | Explicit `commit()` before drop. Drop of BufWriter does flush but NOT fsync — document this |
| scan_last_lsn slow on large WAL | In a future phase: checkpoints truncate the WAL. In Phase 3 the WAL is small |
| File::create_new not available in Rust < 1.77 | workspace uses rust-version = "1.80" ✓ |
