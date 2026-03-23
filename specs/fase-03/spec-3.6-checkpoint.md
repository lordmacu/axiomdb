# Spec: 3.6 — WAL Checkpoint

## What to build (not how)

A `Checkpointer` that flushes all modified data pages to disk, writes a
`Checkpoint` WAL entry, fsyncs the WAL, and records the checkpoint LSN in
the database meta page. After a successful checkpoint, crash recovery only
needs to replay WAL entries written after the checkpoint LSN.

## Critical invariant: ordering

```
SAFE order (must always be respected):
  1. storage.flush()          ← pages land on disk
  2. wal.append(Checkpoint)   ← WAL records that pages are on disk
  3. wal.commit()             ← WAL entry is durable (fsync)
  4. write checkpoint_lsn → meta page → storage.flush() (meta page only)

UNSAFE (NEVER do this):
  WAL Checkpoint before storage.flush() → crash recovery trusts checkpoint
  but pages were never written → data loss
```

Violating this ordering is a silent data-loss bug. Tests must verify it.

## Checkpoint LSN storage — meta page body

Page 0 (PageType::Meta) body currently uses the first 16 bytes for
`DbFileMeta { magic: u64, page_count: u64 }`. We extend it:

```
body[0..8]   magic: u64         (existing)
body[8..16]  page_count: u64    (existing)
body[16..24] checkpoint_lsn: u64  ← NEW: LSN of last successful checkpoint (0 = none)
```

The checkpoint_lsn is written AFTER the WAL Checkpoint entry is fsynced.
On database open, reading body[16..24] gives the LSN from which crash
recovery must replay.

## Inputs / Outputs

### `Checkpointer::new(db_path)`
- Input: path to the `.db` file (reads/writes meta page)
- Output: `Result<Checkpointer, DbError>`

### `checkpoint(storage, wal_writer) -> Result<u64, DbError>`
- Input: mutable access to storage and WAL writer
- Output: checkpoint LSN (the LSN assigned to the Checkpoint WAL entry)
- Steps:
  1. `storage.flush()` — msync all dirty pages
  2. Append `WalEntry { entry_type: Checkpoint, ... }` to WAL (buffered)
  3. `wal_writer.commit()` — fsync WAL (Checkpoint entry is now durable)
  4. Write checkpoint_lsn to meta page body[16..24]
  5. Flush meta page to disk (targeted write_page + flush)
- Error: any I/O error from steps 1-5 propagated as `DbError::Io`

### `last_checkpoint_lsn(storage) -> Result<u64, DbError>`
- Input: read-only storage access
- Output: LSN of last successful checkpoint (0 if never checkpointed)
- Reads body[16..24] of page 0

### Integration with TxnManager
- `TxnManager` exposes `wal_mut() -> &mut WalWriter` for the Checkpointer
- Checkpoint is triggered explicitly (caller decides when)
- Auto-checkpoint by WAL size belongs to 3.7

## Use cases

1. **Fresh DB, no checkpoint**: `last_checkpoint_lsn` returns 0.
   Crash recovery (3.8) replays the entire WAL. ✓

2. **Normal checkpoint**: after several commits, caller triggers checkpoint.
   All committed pages land on disk. `checkpoint_lsn = N`. Next crash
   recovery starts replay from LSN N+1 (skips already-flushed data). ✓

3. **Checkpoint after empty WAL**: WAL has only the header, no entries.
   `checkpoint()` writes and fsyncs a Checkpoint entry (LSN=1).
   `last_checkpoint_lsn` returns 1. ✓

4. **Crash between storage.flush() and WAL fsync**: On reopen,
   `last_checkpoint_lsn` returns the previous checkpoint LSN (or 0).
   Crash recovery replays from the old LSN. Pages written in the failed
   checkpoint are safely on disk but WAL replay will re-apply them
   idempotently. ✓ (correctness guaranteed by idempotent replay in 3.8)

5. **Crash between WAL fsync and meta page write**: The Checkpoint WAL
   entry exists but `last_checkpoint_lsn` is stale. On reopen, recovery
   can fall back to scanning the WAL backward for the last Checkpoint entry
   as a secondary source. ✓ (3.8 handles this; 3.6 just needs to document it)

6. **ENOSPC on WAL append (3.6b)**: `io::Error` with `ErrorKind::OutOfMemory`
   or `StorageFull` is mapped to `DbError::StorageFull`. The Checkpointer
   propagates this to the caller, which is responsible for rollback. ✓

7. **Repeated checkpoints**: each call advances `checkpoint_lsn` monotonically.
   The old LSN in the meta page is atomically replaced by the new one. ✓

## Acceptance criteria

- [ ] `checkpoint()` calls `storage.flush()` BEFORE `wal.commit()` — verified by test ordering
- [ ] `last_checkpoint_lsn()` returns 0 on a fresh database
- [ ] After `checkpoint()`, `last_checkpoint_lsn()` returns the checkpoint LSN
- [ ] Checkpoint LSN is monotonically increasing across multiple checkpoints
- [ ] After simulated crash (drop + reopen), `last_checkpoint_lsn()` returns the last committed checkpoint LSN
- [ ] `WalEntry` for Checkpoint has `entry_type == EntryType::Checkpoint`
- [ ] Checkpoint WAL entry is readable via `WalReader::scan_forward`
- [ ] `checkpoint()` with MemoryStorage succeeds (flush is no-op, for unit tests)
- [ ] ENOSPC: `DbError::StorageFull` is the error type when disk is full (3.6b)
- [ ] No `unwrap()` in src/

## Out of scope

- WAL truncation after checkpoint — 3.7
- Auto-checkpoint by time or WAL size — 3.7
- Crash recovery replay — 3.8
- Granular dirty page tracking — 3.13
- Checkpoint during active transaction — undefined behavior, caller must not do this

## Dependencies

- `axiomdb-storage`: `StorageEngine::flush()`, `Page`, `MmapStorage` (already exist)
- `axiomdb-wal`: `WalWriter`, `WalEntry`, `EntryType::Checkpoint` (already exist)
- `axiomdb-core`: `DbError` (already exists)
- `TxnManager::wal_mut()` accessor — needs to be added to expose `&mut WalWriter`
