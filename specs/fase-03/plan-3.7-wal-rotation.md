# Plan: 3.7 — WAL Rotation

## Files to modify / create

| File | Action | What |
|---|---|---|
| `crates/nexusdb-wal/src/writer.rs` | modify | Header v2 (24B), start_lsn, WalWriter::rotate_file(), open() logic |
| `crates/nexusdb-wal/src/reader.rs` | modify | Seek past 24-byte header instead of 16-byte |
| `crates/nexusdb-wal/src/txn.rs` | modify | rotate_wal() + check_and_rotate() |
| `crates/nexusdb-wal/src/rotation.rs` | create | WalRotator convenience wrapper |
| `crates/nexusdb-wal/src/lib.rs` | modify | Export WalRotator, update WAL_HEADER_SIZE/VERSION |

## Step 1 — WAL header v2: writer.rs

### Constants

```rust
pub const WAL_HEADER_SIZE: usize = 24;   // was 16
pub const WAL_VERSION: u16 = 2;          // was 1

/// Byte offset of start_lsn within the WAL header.
const START_LSN_OFFSET: usize = 14;
```

### write_header (update)

```rust
fn write_header(file: &mut File, start_lsn: u64) -> Result<(), DbError> {
    let mut header = [0u8; WAL_HEADER_SIZE];
    header[0..8].copy_from_slice(&WAL_MAGIC.to_le_bytes());
    header[8..10].copy_from_slice(&WAL_VERSION.to_le_bytes());
    // bytes 10..14: reserved, zero
    header[START_LSN_OFFSET..START_LSN_OFFSET + 8]
        .copy_from_slice(&start_lsn.to_le_bytes());
    file.write_all(&header)?;
    Ok(())
}
```

Call sites: `create()` passes `start_lsn = 0`.

### read_and_verify_header (update)

Returns `start_lsn: u64` in addition to verifying magic/version:

```rust
fn read_and_verify_header(file: &mut File, path: &Path) -> Result<u64, DbError> {
    file.seek(SeekFrom::Start(0))?;
    let mut header = [0u8; WAL_HEADER_SIZE];
    file.read_exact(&mut header)
        .map_err(|_| DbError::WalInvalidHeader { path: ... })?;
    // verify magic and version (must be 2)
    // read start_lsn from bytes [14..22]
    let start_lsn = u64::from_le_bytes([...]);
    Ok(start_lsn)
}
```

### WalWriter::create (minor update)

```rust
pub fn create(path: &Path) -> Result<Self, DbError> {
    let mut file = File::create_new(path)?;
    write_header(&mut file, 0)?;  // start_lsn = 0
    file.sync_all()?;
    ...
    next_lsn: 1,
    offset: WAL_HEADER_SIZE as u64,
}
```

### WalWriter::open (update next_lsn logic)

```rust
pub fn open(path: &Path) -> Result<Self, DbError> {
    let mut file = OpenOptions::new().read(true).append(true).open(path)?;
    let start_lsn = read_and_verify_header(&mut file, path)?;
    let last_lsn = scan_last_lsn(&mut file)?;
    // If the file has entries, continue from them.
    // If empty (just rotated), start from the header's start_lsn.
    let next_lsn = last_lsn.max(start_lsn) + 1;
    let offset = file.seek(SeekFrom::End(0))?;
    Ok(Self { writer: BufWriter::..., next_lsn, offset })
}
```

### scan_last_lsn (update offset)

Change the seek to use the new `WAL_HEADER_SIZE`:

```rust
fn scan_last_lsn(file: &mut File) -> Result<u64, DbError> {
    file.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;
    ...  // rest unchanged
}
```

### WalWriter::rotate_file (new private function)

```rust
/// Truncates the WAL file at `path` to just the v2 header with `start_lsn`.
/// Does NOT update `self` — caller must reopen or replace the writer.
pub(crate) fn rotate_file(path: &Path, start_lsn: u64) -> Result<(), DbError> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.set_len(0)?;                    // truncate
    write_header(&mut file, start_lsn)?; // write new header
    file.sync_all()?;                    // fsync — header is durable
    Ok(())
}
```

## Step 2 — reader.rs: update header seek

`WalReader::open()` calls `verify_header()` which reads `WAL_HEADER_SIZE` bytes.
`ForwardIter` and `BackwardIter` seek to `WAL_HEADER_SIZE`. Both change automatically
since they use the constant — no manual changes needed in most cases. Verify
that all references to the literal `16` are replaced by `WAL_HEADER_SIZE`.

```rust
// Check: scan_forward seeks to:
reader.seek(SeekFrom::Start(WAL_HEADER_SIZE as u64))?;
// Check: BackwardIter's scan uses file_len - entries correctly.
```

## Step 3 — TxnManager: rotate_wal + check_and_rotate

```rust
/// Checkpoint + WAL rotation.
/// MUST be called with no active transaction.
pub fn rotate_wal(
    &mut self,
    storage: &mut dyn StorageEngine,
    wal_path: &Path,
) -> Result<u64, DbError> {
    if self.active.is_some() {
        return Err(DbError::TransactionAlreadyActive {
            txn_id: self.active.as_ref().unwrap().txn_id,
        });
    }

    // 1. Checkpoint: flushes pages, writes Checkpoint entry, fsyncs WAL.
    let checkpoint_lsn = Checkpointer::checkpoint(storage, &mut self.wal)?;

    // 2. Truncate the WAL file and write new header with start_lsn.
    WalWriter::rotate_file(wal_path, checkpoint_lsn)?;

    // 3. Reopen the WAL: next_lsn = checkpoint_lsn + 1.
    self.wal = WalWriter::open(wal_path)?;

    Ok(checkpoint_lsn)
}

/// Rotates the WAL only if its current size exceeds `max_wal_size` bytes.
/// Returns true if rotation occurred.
pub fn check_and_rotate(
    &mut self,
    storage: &mut dyn StorageEngine,
    wal_path: &Path,
    max_wal_size: u64,
) -> Result<bool, DbError> {
    if self.wal.file_offset() > max_wal_size {
        self.rotate_wal(storage, wal_path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}
```

## Step 4 — WalRotator (rotation.rs)

```rust
/// Convenience wrapper that holds the max_wal_size threshold.
pub struct WalRotator {
    /// Maximum WAL file size in bytes before auto-rotation (default: 64 MB).
    pub max_wal_size: u64,
}

impl WalRotator {
    pub const DEFAULT_MAX_WAL_SIZE: u64 = 64 * 1024 * 1024; // 64 MB

    pub fn new(max_wal_size: u64) -> Self {
        Self { max_wal_size }
    }

    /// Rotates the WAL if it exceeds the configured size.
    /// Convenience wrapper around TxnManager::check_and_rotate.
    pub fn check_and_rotate(
        &self,
        mgr: &mut TxnManager,
        storage: &mut dyn StorageEngine,
        wal_path: &Path,
    ) -> Result<bool, DbError> {
        mgr.check_and_rotate(storage, wal_path, self.max_wal_size)
    }
}
```

## Step 5 — Tests

**writer.rs (update existing tests)**:
- All tests creating WAL files need no changes — `create()` still works the same.
- Tests that check `WAL_HEADER_SIZE == 16` must be updated to `== 24`.
- Tests that check `WAL_VERSION == 1` must be updated to `== 2`.

**rotation.rs tests** (`#[cfg(test)]` using MmapStorage + tempfile):

```rust
// Basic rotation: LSN continues from checkpoint_lsn + 1
fn test_rotate_lsn_continues()

// Multiple rotations: LSN is monotonically increasing across all
fn test_multiple_rotations_lsn_monotonic()

// Reopen after rotation: open() reads start_lsn from header
fn test_reopen_after_rotation_correct_next_lsn()

// check_and_rotate returns false if WAL below threshold
fn test_check_and_rotate_below_threshold()

// check_and_rotate returns true and rotates if WAL above threshold
fn test_check_and_rotate_above_threshold()

// rotate_wal with active transaction returns TransactionAlreadyActive
fn test_rotate_with_active_txn_error()

// After rotation: WAL file is exactly 24 bytes (header only)
fn test_rotated_wal_file_size()

// After rotation: last_checkpoint_lsn in meta page updated
fn test_rotation_updates_checkpoint_lsn()

// WAL entries written after rotation have correct (continuing) LSNs
fn test_entries_after_rotation_correct_lsn()

// WalRotator: check_and_rotate with threshold that is 0 always rotates
fn test_wal_rotator_always_triggers()
```

## Anti-patterns to avoid

- **NO** hardcoding `16` anywhere — use `WAL_HEADER_SIZE` constant
- **NO** `next_lsn = last_lsn + 1` without checking `start_lsn` — breaks monotonicity
- **NO** rotation with active transaction — caller must commit or rollback first
- **NO** opening two WalWriters to the same path — writer must be replaced, not duplicated
- **NO** `unwrap()` in src/

## Risks

| Risk | Mitigation |
|---|---|
| v1 WAL files in existing tests break after header change | Update `WAL_HEADER_SIZE` and `WAL_VERSION` constants; all tests use `create()` which writes v2 — no v1 files will be created |
| `rotate_file` crashes mid-write (truncated header) | Recovery detects `WalInvalidHeader`, uses meta page checkpoint_lsn — documented in spec use case 7 |
| `next_lsn = last_lsn.max(start_lsn) + 1` off-by-one | Fresh WAL: max(0,0)+1=1 ✓; empty rotated: max(0, ckpt)+1=ckpt+1 ✓; has entries: max(last, ckpt)+1=last+1 ✓ |
| `WalWriter` `offset` field out of sync after rotate | `open()` seeks to End(0) and gets the real file offset ✓ |

## Implementation order

```
1. writer.rs: WAL_HEADER_SIZE=24, WAL_VERSION=2, write_header(start_lsn)
2. writer.rs: read_and_verify_header returns start_lsn
3. writer.rs: open() uses max(scan_last_lsn, start_lsn) + 1
4. writer.rs: scan_last_lsn seeks to new WAL_HEADER_SIZE
5. writer.rs: rotate_file(path, start_lsn)
6. reader.rs: verify seek uses WAL_HEADER_SIZE (likely already a constant)
7. txn.rs: rotate_wal() + check_and_rotate()
8. rotation.rs: WalRotator
9. lib.rs: export WalRotator
10. Tests: all 10 rotation tests + fix any broken existing tests
11. cargo test --workspace + clippy + fmt
```
