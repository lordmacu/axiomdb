# Spec: WalWriter (Subfase 3.2)

## What to build (not how)

The `WalWriter` — component that writes entries to the WAL file in append-only fashion,
manages the global LSN, performs selective fsync on COMMIT, and maintains a magic header
for file integrity validation.

---

## Fixed design decisions

| Aspect | Decision | Reason |
|---|---|---|
| fsync | **Only on COMMIT** (and ROLLBACK) | Industry standard — intermediate entries in BufWriter, only the commit pays disk latency |
| File header | **16 magic bytes** at the start | Detect invalid file before parsing entries |
| LSN | **WalWriter owns** the AtomicU64 counter | LSN always monotonic, impossible to duplicate from the outside |
| Buffering | **BufWriter<File>** with flush on commit | Amortizes write syscalls, zero overhead on intermediate entries |
| Opening | **Create or reopen** an existing file | Crash recovery requires reopening the WAL without truncating it |

---

## WAL file header

```
Offset  Size    Field
     0       8  magic      — 0x4E455855_53574100 ("NEXUSWAL\0" in LE)
     8       2  version    — u16 LE, currently 1
    10       6  _reserved  — zeros, reserved for future flags
Total: 16 bytes
```

On create: write header + fsync.
On reopen: read and verify magic + version before appending.

---

## Public API

### `WalWriter::create(path: &Path) -> Result<Self, DbError>`
- Creates a new WAL file. Fails if it already exists.
- Writes the 16-byte header and performs fsync.
- Initializes `next_lsn = 1`.

### `WalWriter::open(path: &Path) -> Result<Self, DbError>`
- Opens an existing WAL file for continued writing.
- Verifies magic and version from the header.
- Seeks to the end of the file.
- Reads the LSN of the last entry to initialize `next_lsn = last_lsn + 1`.
- Fails if the file does not exist or the header is invalid.

### `WalWriter::append(&mut self, entry: &mut WalEntry) -> Result<u64, DbError>`
- Assigns the next LSN to the entry (`entry.lsn = self.next_lsn`).
- Serializes the entry with `WalEntry::to_bytes()`.
- Writes to the BufWriter (no fsync).
- Increments `next_lsn`.
- Returns the assigned LSN.
- **Does not fsync** — only writes to the in-RAM buffer.

### `WalWriter::commit(&mut self) -> Result<(), DbError>`
- Flushes the BufWriter to the OS.
- Fsyncs the file descriptor — guarantees durability on disk.
- Returns an error if fsync fails.

### `WalWriter::current_lsn(&self) -> u64`
- Returns the value of `next_lsn - 1` (last assigned LSN).
- `0` if no entry has been written yet.

### `WalWriter::file_offset(&self) -> u64`
- Returns the current byte position in the file (total bytes written).
- Used by the WalReader to know how far to read.

---

## Crash behavior

If the process dies between `append()` and `commit()`:
- Entries in the BufWriter are lost (they never reached disk).
- The WAL on disk remains in the state of the last successful `commit()`.
- The WalReader/crash recovery will ignore incomplete entries (CRC detects them).

This is the correct behavior: only entries with COMMIT on disk are durable.

---

## Inputs / Outputs

### `append`
- Input: `&mut WalEntry` (LSN is assigned here, hence `&mut`)
- Output: `Ok(assigned_lsn: u64)` or error
- Errors: `Io` if the write to the BufWriter fails

### `commit`
- Input: none
- Output: `Ok(())` or error
- Errors: `Io` if fsync fails

### `create` / `open`
- Errors:
  - `Io` — cannot create/open the file
  - `WalInvalidHeader { path }` — incorrect magic or version (only in `open`)

---

## Use cases

1. **Create new WAL**: `create()` → file with 16-byte header on disk
2. **Append without commit**: `append()` × N → entries in buffer, nothing on disk yet
3. **Commit**: `append()` × N → `commit()` → all entries on disk, durable
4. **Reopen existing WAL**: `open()` → verifies header → continues from correct next_lsn
5. **Crash between append and commit**: reopen → entries are not there (never reached disk)
6. **LSN always increasing**: two consecutive `append()` calls return LSNs n and n+1

---

## Acceptance criteria

- [ ] `WalWriter::create()` creates file with exactly 16 header bytes
- [ ] `WalWriter::open()` rejects file without correct magic → `WalInvalidHeader`
- [ ] `WalWriter::open()` rejects file with unknown version → `WalInvalidHeader`
- [ ] `append()` assigns monotonically increasing LSN (verify with N consecutive calls)
- [ ] `append()` without `commit()` → entries are NOT on disk (simulate by reopening)
- [ ] `append()` + `commit()` → entries ARE on disk (verify by reading the file)
- [ ] `current_lsn()` returns 0 before the first append, correct LSN after
- [ ] `file_offset()` grows with each append
- [ ] Reopening with `open()` → `next_lsn` continues from where it left off
- [ ] Zero `unwrap()` in production code
- [ ] Zero `unsafe`

---

## Out of scope

- WalReader (subfase 3.3)
- WAL rotation/truncation (future phase — checkpoint)
- Compression (future phase)
- Per-table WAL (discarded — WAL is global)
- Concurrent writing from multiple threads (Phase 7 with Mutex<WalWriter>)

---

## Dependencies

- `axiomdb-wal`: `WalEntry`, `EntryType`, `MIN_ENTRY_LEN` (subfase 3.1 ✅)
- `axiomdb-core`: `DbError` — add `WalInvalidHeader { path: String }`
- `std::fs`, `std::io::BufWriter` — no new external dependencies
