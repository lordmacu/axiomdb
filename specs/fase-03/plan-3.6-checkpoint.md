# Plan: 3.6 — WAL Checkpoint

## Files to create / modify

| File | Action | What |
|---|---|---|
| `crates/axiomdb-storage/src/meta.rs` | create | `MetaPage` reader/writer — checkpoint_lsn at body[16..24] |
| `crates/axiomdb-storage/src/lib.rs` | modify | `pub mod meta; pub use meta::...` |
| `crates/axiomdb-wal/src/checkpoint.rs` | create | `Checkpointer` struct + `checkpoint()` + `last_checkpoint_lsn()` |
| `crates/axiomdb-wal/src/lib.rs` | modify | `pub mod checkpoint; pub use checkpoint::Checkpointer;` |
| `crates/axiomdb-wal/src/txn.rs` | modify | Add `wal_mut() -> &mut WalWriter` accessor |

## Step 1 — MetaPage helpers (axiomdb-storage/src/meta.rs)

Constants and functions for reading/writing the meta page body:

```rust
/// Page-absolute offset of the checkpoint LSN in the meta page body.
/// body[16..24] = checkpoint_lsn: u64 LE
pub const CHECKPOINT_LSN_BODY_OFFSET: usize = 16;

/// Returns the checkpoint LSN stored in the meta page (page 0).
/// Returns 0 if the meta page has never been checkpointed.
pub fn read_checkpoint_lsn(storage: &dyn StorageEngine) -> Result<u64, DbError> {
    let page = storage.read_page(0)?;
    let body = page.body();
    if body.len() < CHECKPOINT_LSN_BODY_OFFSET + 8 {
        return Ok(0);
    }
    Ok(u64::from_le_bytes([
        body[16], body[17], body[18], body[19],
        body[20], body[21], body[22], body[23],
    ]))
}

/// Writes `lsn` into the checkpoint_lsn field of the meta page (page 0).
/// Caller must call storage.flush() afterward to ensure durability.
pub fn write_checkpoint_lsn(
    storage: &mut dyn StorageEngine,
    lsn: u64,
) -> Result<(), DbError> {
    // Read → modify → write (StorageEngine doesn't have read_page_mut).
    let bytes = *storage.read_page(0)?.as_bytes();
    let mut page = Page::from_bytes(bytes)?;
    let off = HEADER_SIZE + CHECKPOINT_LSN_BODY_OFFSET;
    page.as_bytes_mut()[off..off + 8].copy_from_slice(&lsn.to_le_bytes());
    page.update_checksum();
    storage.write_page(0, &page)?;
    Ok(())
}
```

Note: `HEADER_SIZE + CHECKPOINT_LSN_BODY_OFFSET` because body[] is `data[HEADER_SIZE..]`
but `as_bytes_mut()` returns the full page. Be precise about offset arithmetic.

## Step 2 — TxnManager::wal_mut()

Add to `crates/axiomdb-wal/src/txn.rs`:

```rust
/// Mutable access to the underlying WalWriter.
/// Used by Checkpointer to append the Checkpoint entry and fsync.
pub fn wal_mut(&mut self) -> &mut WalWriter {
    &mut self.wal
}
```

## Step 3 — Checkpointer (axiomdb-wal/src/checkpoint.rs)

```rust
pub struct Checkpointer;  // stateless — all state in storage and wal
```

No state needed: checkpoint_lsn lives in the meta page (durable), and the
WAL writer already knows the current LSN. Stateless = no sync issues.

### checkpoint()

```rust
/// Executes a full checkpoint:
///
/// 1. Flush all storage pages to disk (msync)
/// 2. Write a Checkpoint WAL entry (buffered)
/// 3. fsync the WAL (makes the Checkpoint entry durable)
/// 4. Write the checkpoint LSN to the meta page
/// 5. Flush the meta page to disk
///
/// Returns the LSN of the Checkpoint WAL entry.
///
/// ## Safety ordering
/// Step 1 MUST complete before step 3. If the process crashes between
/// steps 1 and 3, the pages are on disk but the WAL has no Checkpoint
/// entry — crash recovery replays from the previous checkpoint. Correct.
/// If the process crashes between 3 and 5, `last_checkpoint_lsn()` returns
/// the old LSN. Crash recovery scans backward to find the Checkpoint entry
/// as fallback (implemented in 3.8). Correct.
pub fn checkpoint(
    storage: &mut dyn StorageEngine,
    wal: &mut WalWriter,
) -> Result<u64, DbError> {
    // Step 1: flush all pages to disk BEFORE recording the checkpoint.
    storage.flush()?;

    // Step 2: write Checkpoint WAL entry (txn_id=0, no payload).
    let mut entry = WalEntry::new(
        0, 0, EntryType::Checkpoint, 0, vec![], vec![], vec![],
    );
    let checkpoint_lsn = wal.append(&mut entry)?;

    // Step 3: fsync WAL — Checkpoint entry is now durable.
    wal.commit()?;

    // Step 4: record checkpoint_lsn in the meta page.
    write_checkpoint_lsn(storage, checkpoint_lsn)?;

    // Step 5: flush meta page to disk.
    storage.flush()?;

    Ok(checkpoint_lsn)
}
```

### last_checkpoint_lsn()

```rust
/// Returns the LSN of the last successful checkpoint.
/// Returns 0 if the database has never been checkpointed.
pub fn last_checkpoint_lsn(storage: &dyn StorageEngine) -> Result<u64, DbError> {
    read_checkpoint_lsn(storage)
}
```

## Step 4 — Tests

**Unit tests in checkpoint.rs** (`#[cfg(test)]`):

```rust
// Fresh DB: last_checkpoint_lsn returns 0
fn test_fresh_db_no_checkpoint()

// Basic checkpoint: last_checkpoint_lsn returns the checkpoint LSN
fn test_checkpoint_stores_lsn()

// Multiple checkpoints: LSN is monotonically increasing
fn test_multiple_checkpoints_monotonic()

// Checkpoint WAL entry is readable via WalReader
fn test_checkpoint_entry_in_wal()

// MemoryStorage: checkpoint succeeds (flush is no-op)
fn test_checkpoint_with_memory_storage()

// Ordering: storage.flush called before wal.commit
// (verified via a mock or by checking that WAL entry LSN > 0 after)
fn test_ordering_storage_flushed_before_wal()

// After simulated crash (drop TxnManager, reopen), last_checkpoint_lsn
// returns the last committed checkpoint LSN
fn test_checkpoint_lsn_survives_reopen()  // uses MmapStorage + tempfile

// Checkpoint with no prior WAL entries (entry_type=Checkpoint, lsn=1)
fn test_checkpoint_empty_wal()
```

**Integration with TxnManager** (test in checkpoint.rs):
```rust
// begin → commit (txn 1) → checkpoint → verify lsn in meta page
fn test_checkpoint_after_txn_commit()
```

## Step 5 — ENOSPC handling (3.6b)

No new code needed — `DbError::Io(#[from] std::io::Error)` already maps
all I/O errors including ENOSPC. Add a test that verifies the error type:

```rust
// Simulate I/O error during storage.flush() → propagated as DbError::Io
fn test_storage_flush_error_propagated()
```

For production behavior: any error from `checkpoint()` leaves the database
in a consistent state (pages may or may not be on disk, but the WAL has no
durable Checkpoint entry → crash recovery will replay from the old checkpoint).
This is documented in the spec's use case 4.

## Anti-patterns to avoid

- **NO** calling `wal.commit()` before `storage.flush()` — this is the critical invariant
- **NO** updating the meta page before `wal.commit()` — if WAL fsync fails, meta page would have a wrong checkpoint_lsn
- **NO** state in `Checkpointer` — keeps it simple and avoids sync issues
- **NO** WAL truncation here — that's 3.7
- **NO** `unwrap()` in src/

## Risks

| Risk | Mitigation |
|---|---|
| Off-by-one in meta page offset (HEADER_SIZE + body_offset) | Compile-time assert + test reads back the written value |
| `storage.flush()` is a full msync (slow for large DBs) | Acceptable for Phase 3; granular dirty tracking is 3.13 |
| Meta page write fails after WAL fsync | Recovery (3.8) scans WAL backward as fallback — documented |
| MemoryStorage::flush() is a no-op — test won't exercise real fsync | Tests with MmapStorage verify real I/O; MemoryStorage tests verify logic |

## Implementation order

```
1. axiomdb-storage/src/meta.rs — read/write_checkpoint_lsn
2. axiomdb-storage/src/lib.rs — export
3. axiomdb-wal/src/txn.rs — wal_mut() accessor
4. axiomdb-wal/src/checkpoint.rs — Checkpointer::checkpoint() + last_checkpoint_lsn()
5. axiomdb-wal/src/lib.rs — export Checkpointer
6. Unit tests (MemoryStorage, ordering, monotonic)
7. Integration test (MmapStorage, reopen, lsn survives)
8. cargo test --workspace + clippy + fmt
```
