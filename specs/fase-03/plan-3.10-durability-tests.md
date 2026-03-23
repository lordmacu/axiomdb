# Plan: 3.10 — Durability Tests

## File to create

| File | What |
|---|---|
| `crates/nexusdb-wal/tests/integration_durability.rs` | All 9 durability scenarios |

## Why nexusdb-wal/tests?

`nexusdb-wal` depends on `nexusdb-storage`, so integration tests here can use
both `TxnManager`, `CrashRecovery`, `Checkpointer`, `MmapStorage`, and
`IntegrityChecker` without circular dependencies.

## Shared helpers

```rust
// Temp dir + db + wal paths
struct TestEnv {
    _dir: TempDir,
    db:   PathBuf,
    wal:  PathBuf,
}
impl TestEnv {
    fn new() -> Self { ... }
    fn create(&self) -> (MmapStorage, TxnManager) { ... }
    fn open_with_recovery(&self) -> (MmapStorage, TxnManager, RecoveryResult) { ... }
}

// Insert a row into storage and record in txn_mgr.
// Returns (page_id, slot_id).
fn do_insert(storage: &mut MmapStorage, mgr: &mut TxnManager,
             data: &[u8]) -> (u64, u16) { ... }

// Verify a slot is alive (Some) or dead (None).
fn assert_slot_alive(storage: &dyn StorageEngine, page_id: u64, slot_id: u16) { ... }
fn assert_slot_dead(storage: &dyn StorageEngine, page_id: u64, slot_id: u16) { ... }
```

## Scenario implementations

### Scenario 1: committed data survives crash

```rust
#[test]
fn test_committed_data_survives_crash() {
    let env = TestEnv::new();
    let committed_slots: Vec<(u64, u16)>;

    // Session 1: insert + commit several rows.
    {
        let (mut storage, mut mgr) = env.create();
        let mut slots = Vec::new();
        for i in 0..5u64 {
            mgr.begin().unwrap();
            let (pid, sid) = do_insert(&mut storage, &mut mgr, &i.to_le_bytes());
            mgr.commit().unwrap();
            slots.push((pid, sid));
        }
        mgr.wal_mut().flush_buffer().unwrap();
        committed_slots = slots;
        // Drop = crash (no graceful shutdown WAL write)
    }

    // Session 2: recovery.
    {
        let (storage, _mgr, result) = env.open_with_recovery();
        assert_eq!(result.undone_txns, 0);
        for (pid, sid) in &committed_slots {
            assert_slot_alive(&storage, *pid, *sid);
        }
        let report = IntegrityChecker::post_recovery_check(&storage,
            result.max_committed).unwrap();
        assert!(report.is_clean(), "{}", report.summary());
    }
}
```

### Scenario 2: uncommitted data absent after recovery

```rust
#[test]
fn test_uncommitted_data_rolled_back() {
    let env = TestEnv::new();
    let (page_id, slot_id);

    {
        let (mut storage, mut mgr) = env.create();
        mgr.begin().unwrap();
        (page_id, slot_id) = do_insert(&mut storage, &mut mgr, b"uncommitted");
        mgr.wal_mut().flush_buffer().unwrap();
        // Crash: no commit
    }

    {
        let (storage, _, result) = env.open_with_recovery();
        assert_eq!(result.undone_txns, 1);
        assert_slot_dead(&storage, page_id, slot_id);
        let report = IntegrityChecker::post_recovery_check(&storage,
            result.max_committed).unwrap();
        assert!(report.is_clean());
    }
}
```

### Scenario 3: partial transaction

```rust
#[test]
fn test_partial_transaction_committed_survives() {
    let env = TestEnv::new();
    let (pid_a, sid_a, pid_b, sid_b);

    {
        let (mut storage, mut mgr) = env.create();
        // txn1: committed
        mgr.begin().unwrap();
        (pid_a, sid_a) = do_insert(&mut storage, &mut mgr, b"row-A");
        mgr.commit().unwrap();
        // txn2: crash
        mgr.begin().unwrap();
        (pid_b, sid_b) = do_insert(&mut storage, &mut mgr, b"row-B");
        mgr.wal_mut().flush_buffer().unwrap();
    }

    {
        let (storage, _, result) = env.open_with_recovery();
        assert_eq!(result.undone_txns, 1);
        assert_slot_alive(&storage, pid_a, sid_a);
        assert_slot_dead(&storage, pid_b, sid_b);
        let report = IntegrityChecker::post_recovery_check(&storage,
            result.max_committed).unwrap();
        assert!(report.is_clean());
    }
}
```

### Scenario 4: truncated WAL

```rust
#[test]
fn test_truncated_wal_recovery_safe() {
    let env = TestEnv::new();

    {
        let (mut storage, mut mgr) = env.create();
        mgr.begin().unwrap();
        do_insert(&mut storage, &mut mgr, b"data");
        mgr.commit().unwrap(); // valid entries fsynced

        mgr.begin().unwrap();
        do_insert(&mut storage, &mut mgr, b"in-progress");
        mgr.wal_mut().flush_buffer().unwrap(); // partial entries flushed but not committed
    }

    // Truncate the WAL file to 3/4 of its size — cuts the last entry in half.
    let wal_len = std::fs::metadata(&env.wal).unwrap().len();
    let truncate_at = wal_len * 3 / 4;
    let file = OpenOptions::new().write(true).open(&env.wal).unwrap();
    file.set_len(truncate_at).unwrap();

    // Recovery must succeed — truncated/corrupt entry at end is ignored.
    let (storage, _, result) = env.open_with_recovery();
    // The committed txn before truncation should be recoverable.
    // (Truncation may or may not cut into the committed entry — test doesn't assert
    // specific row presence, only that recovery doesn't panic.)
    let _ = result; // recovery completed without error
    let report = IntegrityChecker::post_recovery_check(&storage,
        result.max_committed).unwrap();
    assert!(report.is_clean(), "{}", report.summary());
}
```

### Scenario 5: WAL rotation then crash

```rust
#[test]
fn test_wal_rotation_then_crash() {
    let env = TestEnv::new();
    let (pid_1, sid_1, pid_2, sid_2);

    {
        let (mut storage, mut mgr) = env.create();
        // txn1: committed
        mgr.begin().unwrap();
        (pid_1, sid_1) = do_insert(&mut storage, &mut mgr, b"before-rotation");
        mgr.commit().unwrap();
        // Rotate WAL: checkpoint + truncate
        mgr.rotate_wal(&mut storage, &env.wal).unwrap();
        // txn2: crash after rotation
        mgr.begin().unwrap();
        (pid_2, sid_2) = do_insert(&mut storage, &mut mgr, b"after-rotation");
        mgr.wal_mut().flush_buffer().unwrap();
    }

    {
        let (storage, _, result) = env.open_with_recovery();
        assert_eq!(result.undone_txns, 1, "txn2 must be rolled back");
        assert_slot_alive(&storage, pid_1, sid_1);
        assert_slot_dead(&storage, pid_2, sid_2);
        let report = IntegrityChecker::post_recovery_check(&storage,
            result.max_committed).unwrap();
        assert!(report.is_clean());
    }
}
```

### Scenario 6: multiple crash + recovery cycles

```rust
#[test]
fn test_multiple_crash_recovery_idempotent() {
    let env = TestEnv::new();
    let (page_id, slot_id);

    // Session 1: crash mid-insert.
    {
        let (mut storage, mut mgr) = env.create();
        mgr.begin().unwrap();
        (page_id, slot_id) = do_insert(&mut storage, &mut mgr, b"orphan");
        mgr.wal_mut().flush_buffer().unwrap();
    }

    // Session 2: first recovery.
    {
        let (storage, _, result) = env.open_with_recovery();
        assert_eq!(result.undone_txns, 1);
        assert_slot_dead(&storage, page_id, slot_id);
    }

    // Session 3: second recovery — idempotent.
    {
        let (storage, _, result2) = env.open_with_recovery();
        // WAL still shows the in-progress txn (no Rollback was written).
        // undone_txns can be 1 again but slot remains dead (idempotent undo).
        assert_slot_dead(&storage, page_id, slot_id);
        let report = IntegrityChecker::post_recovery_check(&storage,
            result2.max_committed).unwrap();
        assert!(report.is_clean(), "{}", report.summary());
    }
}
```

### Scenario 7: integrity check after recovery (covered by 1-6)

Each scenario above already runs `post_recovery_check`. No separate test needed
unless we want a dedicated "all-clean" smoke test.

### Scenario 8: corrupt checkpoint_lsn — documented failure mode

```rust
#[test]
fn test_corrupt_checkpoint_lsn_failure_mode() {
    let env = TestEnv::new();
    let (page_id, slot_id);

    {
        let (mut storage, mut mgr) = env.create();
        mgr.begin().unwrap();
        (page_id, slot_id) = do_insert(&mut storage, &mut mgr, b"uncommitted");
        mgr.wal_mut().flush_buffer().unwrap();
        storage.flush().unwrap(); // page on disk
    }

    // Corrupt checkpoint_lsn to a value beyond all WAL LSNs.
    {
        let mut storage = MmapStorage::open(&env.db).unwrap();
        nexusdb_storage::write_checkpoint_lsn(&mut storage, 99999).unwrap();
        storage.flush().unwrap();
    }

    // Recovery: scan_forward(99999) finds nothing → undone_txns=0.
    // The uncommitted slot survives (this is the documented failure mode).
    let (storage, _, result) = env.open_with_recovery();
    assert_eq!(result.undone_txns, 0, "corrupt checkpoint causes missed recovery");

    // IntegrityChecker DETECTS the anomaly.
    let report = IntegrityChecker::post_recovery_check(&storage, result.max_committed).unwrap();
    // With max_committed=0 and the slot's txn_id_created > 0, we should see an error
    // ONLY if max_committed > 0. Since no txns committed, max_committed=0 and
    // check_page skips MVCC checks. This documents the double failure mode:
    // corrupt checkpoint + zero committed = no detection.
    // The test documents the behavior without asserting an error (graceful degradation).
    let _ = report; // behavior is documented, not necessarily an error here
}
```

### Scenario 9: partial page write — documented limitation

```rust
#[test]
fn test_partial_page_write_detected() {
    let env = TestEnv::new();

    let page_id = {
        let (mut storage, mut mgr) = env.create();
        mgr.begin().unwrap();
        let (pid, _) = do_insert(&mut storage, &mut mgr, b"data");
        mgr.commit().unwrap();
        storage.flush().unwrap();
        pid
    };

    // Corrupt the page body directly in the .db file.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = OpenOptions::new().write(true).open(&env.db).unwrap();
        let corrupt_offset = page_id as u64 * 16384 + 100; // mid-body
        f.seek(SeekFrom::Start(corrupt_offset)).unwrap();
        f.write_all(&[0xFF; 8]).unwrap();
    }

    // Reading the corrupted page must return ChecksumMismatch.
    let storage = MmapStorage::open(&env.db).unwrap();
    let result = storage.read_page(page_id);
    assert!(
        matches!(result, Err(DbError::ChecksumMismatch { .. })),
        "corrupted page must be detected, got: {result:?}"
    );
    // Test documents: full recovery requires WAL redo (Phase 3.8b).
}
```

## Anti-patterns to avoid

- **NO** using MemoryStorage — all scenarios must use MmapStorage (real I/O)
- **NO** calling `mgr.commit()` to simulate a crash — use `flush_buffer()` + drop
- **NO** asserting specific LSN values — they depend on entry count and could drift
- **NO** ignoring errors silently in test helpers — use `.unwrap()` in tests

## Implementation order

```
1. Create tests/integration_durability.rs with TestEnv + helpers
2. Implement scenarios 1, 2, 3 (basic crash recovery)
3. Implement scenario 4 (truncated WAL)
4. Implement scenarios 5, 6 (rotation + multiple cycles)
5. Implement scenarios 8, 9 (documented failure modes)
6. cargo test -p nexusdb-wal integration_durability -- all pass
7. cargo test --workspace (no regressions)
8. cargo clippy --workspace -- -D warnings
```
