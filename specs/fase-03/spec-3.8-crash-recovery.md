# Spec: 3.8 — Crash Recovery State Machine

## What to build (not how)

A `CrashRecovery` module that detects and repairs inconsistencies left by
an abrupt process termination (SIGKILL, OOM, segfault). After recovery, the
database is in the same state as if all in-progress transactions had been
explicitly rolled back.

## Scope and explicit limitation

**In scope (Phase 3.8):**
- Detect in-progress transactions (Begin without Commit or Rollback in WAL)
- Undo their heap changes: mark inserted slots dead, restore deleted slots
- Restore `max_committed` from WAL scan

**Deferred (Phase 3.8b — power failure redo):**
- Redo committed transactions whose data pages were not fsynced before crash
- Requires: per-page `lsn` tracking + `restore_tuple` + full redo scan
- Power failure is rare in embedded mode; process crash is the common case

## Critical gap fixed: physical location in WAL entries

The WAL entry currently does NOT contain `page_id` or `slot_id` in its binary
format. This information is only in the TxnManager's in-memory undo log, which
is lost on crash.

**Fix**: encode `(page_id: u64, slot_id: u16)` = 10 bytes at the start of
the variable fields in WAL entries:

```
Insert: new_value = [page_id:8 LE][slot_id:2 LE][actual row bytes...]
Delete: old_value = [page_id:8 LE][slot_id:2 LE][original row bytes...]
Update: old_value = [old_page_id:8][old_slot:2][old row bytes...]
        new_value = [new_page_id:8][new_slot:2][new row bytes...]
```

This is a convention on top of the existing binary format (no WAL version bump
needed — the `new_value` and `old_value` fields are already variable-length bytes).

`record_insert`, `record_delete`, `record_update` in `TxnManager` are updated
to prepend these 10 bytes. A constant `PHYSICAL_LOC_LEN = 10` documents this.

## Recovery algorithm

```
State: Ready → Recovering(ScanningWal) → Recovering(UndoingInProgress)
     → Recovering(Verifying) → Ready
```

### Step 1 — Check if recovery is needed

```
needed = WAL scan finds any Begin without matching Commit or Rollback,
         starting from last_checkpoint_lsn
```

If not needed: return immediately (fast path for normal startup).

### Step 2 — Full WAL scan from checkpoint_lsn

Scan forward from `checkpoint_lsn`, building:
- `committed: HashSet<TxnId>` — txn_ids with Commit entry
- `rolled_back: HashSet<TxnId>` — txn_ids with Rollback entry
- `in_progress: HashMap<TxnId, Vec<RecoveryOp>>` — Begin without Commit/Rollback

`RecoveryOp`:
```rust
enum RecoveryOp {
    Insert { page_id: u64, slot_id: u16 },  // undo = mark_slot_dead
    Delete { page_id: u64, slot_id: u16 },  // undo = clear_deletion
}
```

### Step 3 — Undo in-progress transactions

For each in-progress txn, apply undo ops in **reverse order** (last op first):
- `RecoveryOp::Insert` → `mark_slot_dead(storage, page_id, slot_id)`
- `RecoveryOp::Delete` → `clear_deletion(storage, page_id, slot_id)`

After all undo ops: `storage.flush()` to ensure the corrections are durable.

### Step 4 — Advance max_committed

`max_committed = max txn_id in committed set` (from WAL scan).
This is returned in `RecoveryResult` for the caller to initialize `TxnManager`.

### Step 5 — Verifying (lightweight)

After undo, scan the WAL once more to verify that all in-progress txn_ids
are now absent from live heap slots. Log a warning if any anomaly is found
(but do not fail — we've already applied the corrections).

In Phase 3.8, verification is a best-effort debug assertion, not a hard blocker.

## Inputs / Outputs

### `CrashRecovery::is_needed(storage, wal_path) -> Result<bool, DbError>`
- Fast check: scan WAL for any Begin without Commit/Rollback
- Returns false immediately if WAL has no entries (fresh database)

### `CrashRecovery::recover(storage, wal_path) -> Result<RecoveryResult, DbError>`
- Full recovery: scan + undo + flush
- Idempotent: safe to call on an already-consistent database
- Returns: `RecoveryResult { max_committed, undone_txns, checkpoint_lsn }`

### Integration with TxnManager::open()
Add `TxnManager::open_with_recovery(storage, wal_path)` or update `open()`:
- Run `CrashRecovery::recover(storage, wal_path)`
- Use `RecoveryResult.max_committed` to initialize `max_committed`
- Return the TxnManager ready for use

## RecoveryState

```rust
pub enum RecoveryState {
    Ready,
    ScanningWal { progress: u64 },
    UndoingInProgress { remaining: u32 },
    Verifying,
}
```

Embedded in `CrashRecovery` for observability (caller can query current phase).
In Phase 3, this is informational only — recovery is synchronous and blocking.

## Use cases

1. **Clean shutdown (no recovery needed)**: WAL has matching Begin+Commit for
   all transactions. `is_needed` returns false. Fast path. ✓

2. **Crashed with one in-progress INSERT**: WAL has Begin+Insert, no Commit.
   Recovery: `mark_slot_dead(page_id, slot_id)`. Database as if INSERT never happened. ✓

3. **Crashed with one in-progress DELETE**: WAL has Begin+Delete, no Commit.
   Recovery: `clear_deletion(page_id, slot_id)`. Deleted row restored. ✓

4. **Crashed with one in-progress UPDATE**: WAL has Begin+Update (= Delete+Insert),
   no Commit. Recovery in reverse: undo Insert (kill new slot), undo Delete (restore old). ✓

5. **Multiple operations in one in-progress transaction**: undo all in reverse order. ✓

6. **Recovery after rotation**: checkpoint_lsn > 0. Recovery only scans WAL
   from checkpoint_lsn — committed data before checkpoint is already on disk. ✓

7. **Recovery called on already-consistent DB**: all undo operations are
   idempotent (mark_slot_dead on already-dead slot → `AlreadyDeleted` → ignored).
   `clear_deletion` on slot with `txn_id_deleted==0` → no-op. ✓

8. **Fresh database, empty WAL**: `is_needed` returns false immediately. ✓

## Acceptance criteria

- [ ] `PHYSICAL_LOC_LEN = 10` constant defined and used in record_* methods
- [ ] `record_insert` prepends [page_id:8][slot_id:2] to new_value
- [ ] `record_delete` prepends [page_id:8][slot_id:2] to old_value
- [ ] `record_update` prepends physical locations to both old_value and new_value
- [ ] `CrashRecovery::is_needed` returns false for a cleanly-closed database
- [ ] Use case 2: crashed INSERT slot is dead after recovery
- [ ] Use case 3: crashed DELETE row is live (txn_id_deleted=0) after recovery
- [ ] Use case 4: crashed UPDATE — new slot dead, old slot restored
- [ ] Use case 7: recovery is idempotent (safe to call twice)
- [ ] Use case 8: empty WAL → no crash, no panic
- [ ] `max_committed` in RecoveryResult matches the last Commit entry in WAL
- [ ] After recovery, `storage.flush()` called once to make corrections durable
- [ ] `TxnManager::open_with_recovery` initializes max_committed from WAL scan
- [ ] All existing tests still pass (record_* API change must be backward-compatible)
- [ ] No `unwrap()` in src/

## Out of scope (deferred to 3.8b)

- Redo of committed transactions after power failure
- Per-page `lsn` tracking and `restore_tuple`
- Partial page write detection
- Recovery from WAL corruption (CRC errors in WAL during recovery scan)
- Post-recovery integrity check (index vs heap) — Phase 3.9

## Dependencies

- 3.5 TxnManager (`record_insert/delete/update`, `undo log`) — updating these
- 3.6 Checkpointer (`last_checkpoint_lsn`) — used as recovery start point
- 3.7 WAL rotation (`WalWriter::open` with `start_lsn`) — WAL may be rotated
- heap.rs: `mark_slot_dead`, `clear_deletion` — used by undo
- `WalReader::scan_forward` — WAL traversal
