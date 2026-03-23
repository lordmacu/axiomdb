# Spec: 3.10 — Durability Tests

## What to build (not how)

Integration tests that verify the end-to-end durability guarantees of the
storage + WAL + crash recovery stack using real disk I/O (MmapStorage).
Each test exercises a specific failure scenario, reopens the database, and
asserts the correct post-recovery state.

## Test scenarios

### 1 — Committed data survives crash
Write several transactions, commit all, simulate process crash (close without
explicit shutdown), reopen with `TxnManager::open_with_recovery`.
All committed rows must be readable. `undone_txns = 0`.

### 2 — Uncommitted data absent after recovery
Write an INSERT and crash before COMMIT. Reopen with recovery.
The inserted slot must be dead. `undone_txns = 1`.
`IntegrityChecker::post_recovery_check` must report clean.

### 3 — Partial transaction: some ops committed, one crashed
Txn1: insert row A → commit. Txn2: insert row B → crash (no commit).
After recovery: row A alive, row B dead. `undone_txns = 1`.

### 4 — Truncated WAL (partial entry at end)
Write WAL entries, then truncate the file mid-entry using `file.set_len()`.
Reopen with `TxnManager::open_with_recovery`: must NOT panic or error.
Valid entries before truncation point must still be processed.
The truncated (partial) entry at the end must be silently ignored.

### 5 — WAL rotation then crash
Commit txn1, rotate WAL (checkpoint + new WAL file), crash mid-txn2.
Recovery from the rotated (post-rotation) WAL: undoes txn2.
Txn1 data must be present (was checkpointed before rotation).

### 6 — Multiple crash + recovery cycles (idempotency)
Crash → recover → crash again → recover again → data consistent.
After second recovery: same state as after first recovery.
No panic, no error, `is_clean() == true`.

### 7 — Post-recovery IntegrityChecker reports clean
After scenarios 2, 3, 5, and 6: run `IntegrityChecker::post_recovery_check`.
Must return `is_clean() == true` with no `UncommittedAliveRow` errors.

### 8 — Corrupt checkpoint_lsn (documented failure mode)
Write txn1 (commit), txn2 (no commit). Manually corrupt checkpoint_lsn to
a value > all WAL LSNs. Recovery: `scan_forward(corrupt_lsn)` finds no entries
→ treats as clean → txn2's heap data survives → IntegrityChecker detects
`UncommittedAliveRow`.
This test **documents the failure mode**: corrupt checkpoint = silent data
inconsistency. Crash recovery (3.8) is correct only if checkpoint_lsn is valid.
Test asserts: no panic, `undone_txns = 0`, integrity check finds the anomaly.

### 9 — Partial page write (CRC corruption) — documented limitation
Write a data page to disk. Corrupt some body bytes directly in the file.
Attempt `read_page` → receives `DbError::ChecksumMismatch`.
Test asserts: error is detectable, not silently ignored.
Documents that page corruption recovery requires WAL redo (Phase 3.8b).

## Acceptance criteria

- [ ] Scenario 1: all committed rows readable after crash+recovery
- [ ] Scenario 2: crashed INSERT slot is dead; `undone_txns = 1`
- [ ] Scenario 3: committed row A alive, crashed row B dead
- [ ] Scenario 4: truncated WAL → no panic, valid entries processed
- [ ] Scenario 5: post-rotation crash → txn2 undone, txn1 data present
- [ ] Scenario 6: two crash+recovery cycles → idempotent result
- [ ] Scenario 7: `IntegrityChecker::post_recovery_check` clean after scenarios 2,3,5,6
- [ ] Scenario 8: corrupt checkpoint_lsn → no panic, anomaly detectable by integrity check
- [ ] Scenario 9: corrupted page → `ChecksumMismatch` error, not silent
- [ ] All tests use real disk I/O (MmapStorage + tempfile)
- [ ] No `unwrap()` in production code paths exercised by tests

## Out of scope

- Index divergence post-crash — requires catalog (Phase 3.12+)
- WAL corruption mid-entry beyond end-of-file — CRC mismatch is caught by WalReader
- Power failure redo (pages lost from page cache) — Phase 3.8b
- Multi-process crash scenarios — Phase 7 (concurrency)

## Dependencies

- 3.6 `Checkpointer` — scenario 5 (rotation)
- 3.7 `TxnManager::rotate_wal` — scenario 5
- 3.8 `CrashRecovery`, `TxnManager::open_with_recovery` — all scenarios
- 3.9 `IntegrityChecker::post_recovery_check` — scenario 7
- `MmapStorage` — all (real disk I/O)
- `std::fs::OpenOptions` — scenario 4 (truncate), scenario 9 (corrupt)
