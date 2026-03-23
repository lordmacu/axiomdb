# Spec: 3.7 — WAL Rotation

## What to build (not how)

When the WAL file grows beyond a configurable `max_wal_size`, trigger a
checkpoint (3.6) and then truncate the WAL file — keeping only the new
header with `start_lsn = checkpoint_lsn`. This bounds WAL disk usage while
maintaining global LSN monotonicity across rotations.

## The LSN monotonicity problem

After rotation, the WAL file is empty (only a header). When the database
reopens, `WalWriter::open()` scans for the last entry and finds none, so it
would set `next_lsn = 1` — **breaking the global LSN sequence**.

**Fix**: extend the WAL file header with a `start_lsn: u64` field. On open,
`next_lsn = max(scan_last_lsn(), header.start_lsn) + 1`.

## WAL header change: v1 → v2

```
v1 (16 bytes — current):
  [0..8]   magic: u64         "AXIOMWAL\0"
  [8..10]  version: u16 = 1
  [10..16] reserved: [u8; 6]

v2 (24 bytes — new):
  [0..8]   magic: u64         "AXIOMWAL\0"  (unchanged)
  [8..10]  version: u16 = 2   (bumped)
  [10..14] _reserved: [u8; 4]
  [14..22] start_lsn: u64     0 for fresh WAL, checkpoint_lsn for rotated
```

`WAL_HEADER_SIZE` changes from 16 to 24. `WAL_VERSION` changes from 1 to 2.
Version 1 files are rejected with `DbError::WalInvalidHeader` — we are in
early development with no production users; clean break is correct.

## Rotation procedure (mandatory ordering)

```
1. Checkpoint: Checkpointer::checkpoint(storage, wal_writer)
   → checkpoint_lsn
2. Truncate the WAL file:
   a. Close / drop the current WalWriter
   b. Open the file in write mode and truncate to 0
   c. Write new v2 header with start_lsn = checkpoint_lsn
   d. fsync the new header
3. Create new WalWriter pointing to the same path
   → next_lsn = checkpoint_lsn + 1
4. TxnManager replaces its internal WalWriter
```

If the process crashes between steps 1 and 3, the Checkpoint entry is
durable (step 1 fsyncs) and the old WAL file is intact. Recovery uses
`last_checkpoint_lsn()` from meta page and replays from there. Safe. ✓

If the process crashes during step 2 (header truncated but not rewritten),
the WAL file is corrupt. Recovery detects `WalInvalidHeader` and falls back
to treating `last_checkpoint_lsn()` as the recovery point with an empty WAL
(no entries to replay — all data is already on disk from the checkpoint). ✓

## Inputs / Outputs

### `TxnManager::rotate_wal(storage, wal_path) -> Result<u64, DbError>`
- Prerequisite: no active transaction (`active == None`)
- Step 1: checkpoint → checkpoint_lsn
- Step 2: truncate WAL file + write v2 header with start_lsn = checkpoint_lsn
- Step 3: replace internal WalWriter with fresh one (next_lsn = checkpoint_lsn + 1)
- Returns: checkpoint_lsn
- Error: `TransactionAlreadyActive` if called mid-transaction

### `TxnManager::check_and_rotate(storage, wal_path, max_wal_size) -> Result<bool, DbError>`
- Checks: `wal_writer.file_offset() > max_wal_size`
- If true: calls `rotate_wal` and returns `Ok(true)`
- If false: no-op, returns `Ok(false)`

### `WalRotator` (optional convenience wrapper)
```rust
pub struct WalRotator {
    max_wal_size: u64,   // default: 64 MB
}
impl WalRotator {
    pub fn new(max_wal_size: u64) -> Self
    pub fn check_and_rotate(
        &self,
        mgr: &mut TxnManager,
        storage: &mut dyn StorageEngine,
        wal_path: &Path,
    ) -> Result<bool, DbError>
}
```

## Use cases

1. **WAL grows beyond max_wal_size**: `check_and_rotate` triggers checkpoint +
   truncation. WAL file shrinks to 24 bytes (header only). `last_checkpoint_lsn`
   updated. Next LSNs continue from checkpoint_lsn + 1. ✓

2. **Multiple rotations**: each rotation increases checkpoint_lsn. LSNs are
   globally monotonic across all rotations. ✓

3. **Fresh database**: `start_lsn = 0`, `next_lsn = 1`. Same as before. ✓

4. **Reopen after rotation**: `WalWriter::open()` reads `start_lsn` from
   header, `scan_last_lsn()` returns 0 (empty entries). `next_lsn = start_lsn + 1`. ✓

5. **Rotation mid-transaction**: returns `TransactionAlreadyActive`. Caller
   must commit or rollback before rotating. ✓

6. **WAL below threshold**: `check_and_rotate` returns `false`, no rotation. ✓

7. **Crash during header truncation**: WAL file corrupt → `WalInvalidHeader` on
   open. Recovery uses meta page checkpoint_lsn, treats WAL as empty (correct
   since all data was checkpointed). ✓

## Acceptance criteria

- [ ] `WAL_HEADER_SIZE == 24` after this change
- [ ] `WAL_VERSION == 2` after this change
- [ ] Fresh WAL: `start_lsn = 0`, `next_lsn` starts at 1
- [ ] Rotated WAL: `start_lsn = checkpoint_lsn`, `next_lsn = checkpoint_lsn + 1`
- [ ] After rotation, LSNs continue monotonically (no reset to 1)
- [ ] After `rotate_wal`, `last_checkpoint_lsn()` returns the new checkpoint_lsn
- [ ] `check_and_rotate` returns `false` when WAL size ≤ max_wal_size
- [ ] `check_and_rotate` returns `true` and rotates when WAL size > max_wal_size
- [ ] Reopen after rotation: `WalWriter::open()` sets correct `next_lsn`
- [ ] `rotate_wal` with active transaction returns `TransactionAlreadyActive`
- [ ] All v1 WAL files (existing tests) updated to use v2 format
- [ ] cargo test --workspace passes (including all existing WAL tests)
- [ ] No `unwrap()` in src/

## Out of scope

- WAL archiving (keep old WAL files for point-in-time recovery) — future
- Multiple WAL files (PostgreSQL-style segments) — future
- Async auto-rotation (background task) — Phase 7
- WAL compression — future

## Dependencies

- 3.6 (Checkpointer::checkpoint) — used inside rotate_wal ✓
- `WalWriter::file_offset()` — already exists ✓
- `std::fs::File::set_len(0)` — for WAL truncation
