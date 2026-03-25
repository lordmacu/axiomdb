//! Durability integration tests — Phase 3.10.
//!
//! These tests exercise the full crash-recovery stack using real disk I/O
//! (MmapStorage + WAL files). Each scenario:
//!
//! 1. Opens a fresh database (Session 1).
//! 2. Performs operations and then simulates a crash by dropping without
//!    graceful shutdown (`flush_buffer()` ensures WAL entries reach the OS
//!    page cache without an fsync, matching the behaviour of a process kill).
//! 3. Reopens the database with `TxnManager::open_with_recovery` (Session 2+).
//! 4. Asserts the expected post-recovery state.
//!
//! ## Why axiomdb-wal tests?
//! Integration tests here can import both `axiomdb-wal` (TxnManager,
//! CrashRecovery) and `axiomdb-storage` (MmapStorage, IntegrityChecker)
//! without circular dependencies.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use axiomdb_core::error::DbError;
use axiomdb_storage::{
    heap::insert_tuple, read_tuple, IntegrityChecker, MmapStorage, Page, PageType, StorageEngine,
};
use axiomdb_wal::{CrashRecovery, RecoveryResult, TxnManager};
use tempfile::TempDir;

// ── TestEnv ───────────────────────────────────────────────────────────────────

/// Encapsulates temporary directory + db/wal paths for a single test.
struct TestEnv {
    _dir: TempDir, // keeps the directory alive for the test's duration
    pub db: PathBuf,
    pub wal: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create test tmp dir");
        let db = dir.path().join("test.db");
        let wal = dir.path().join("test.wal");
        Self { _dir: dir, db, wal }
    }

    /// Creates a fresh database and returns (storage, txn_manager).
    fn create(&self) -> (MmapStorage, TxnManager) {
        let storage = MmapStorage::create(&self.db).expect("create db");
        let mgr = TxnManager::create(&self.wal).expect("create wal");
        (storage, mgr)
    }

    /// Opens an existing database, runs crash recovery, returns the results.
    fn open_with_recovery(&self) -> (MmapStorage, TxnManager, RecoveryResult) {
        let mut storage = MmapStorage::open(&self.db).expect("open db");
        let (mgr, result) =
            TxnManager::open_with_recovery(&mut storage, &self.wal).expect("recovery");
        (storage, mgr, result)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Inserts `data` into a new heap page slot under the current transaction.
/// Returns (page_id, slot_id). Caller must have called `mgr.begin()` first.
fn do_insert(storage: &mut MmapStorage, mgr: &mut TxnManager, data: &[u8]) -> (u64, u16) {
    let page_id = storage.alloc_page(PageType::Data).expect("alloc page");
    let txn_id = mgr
        .active_txn_id()
        .expect("must call begin() before do_insert");

    let page_bytes = *storage.read_page(page_id).expect("read page").as_bytes();
    let mut page = Page::from_bytes(page_bytes).expect("parse page");
    let slot_id = insert_tuple(&mut page, data, txn_id).expect("insert tuple");
    storage.write_page(page_id, &page).expect("write page");

    mgr.record_insert(1, data, data, page_id, slot_id)
        .expect("record insert");

    (page_id, slot_id)
}

fn assert_slot_alive(storage: &dyn StorageEngine, page_id: u64, slot_id: u16) {
    let page = storage
        .read_page(page_id)
        .expect("read page for slot check");
    let result = read_tuple(page, slot_id).expect("read_tuple");
    assert!(
        result.is_some(),
        "slot ({page_id}, {slot_id}) must be alive but is dead"
    );
}

fn assert_slot_dead(storage: &dyn StorageEngine, page_id: u64, slot_id: u16) {
    let page = storage
        .read_page(page_id)
        .expect("read page for slot check");
    let result = read_tuple(page, slot_id).expect("read_tuple");
    assert!(
        result.is_none(),
        "slot ({page_id}, {slot_id}) must be dead but is alive"
    );
}

// ── Scenario 1: committed data survives crash ──────────────────────────────────

#[test]
fn test_committed_data_survives_crash() {
    let env = TestEnv::new();
    let mut committed_slots = Vec::new();

    // Session 1: insert and commit 3 rows.
    {
        let (mut storage, mut mgr) = env.create();
        for i in 0u64..3 {
            mgr.begin().unwrap();
            let (pid, sid) = do_insert(&mut storage, &mut mgr, &i.to_le_bytes());
            mgr.commit().unwrap();
            committed_slots.push((pid, sid));
        }
        // Simulate crash: flush WAL buffer to OS (no fsync), then drop.
        mgr.wal_mut().flush_buffer().unwrap();
        storage.flush().unwrap();
    }

    // Session 2: crash recovery must find nothing to undo.
    let (storage, _, result) = env.open_with_recovery();
    assert_eq!(result.undone_txns, 0, "no in-progress transactions");
    for (pid, sid) in &committed_slots {
        assert_slot_alive(&storage, *pid, *sid);
    }
    let report = IntegrityChecker::post_recovery_check(&storage, result.max_committed).unwrap();
    assert!(
        report.is_clean(),
        "post-recovery integrity: {}",
        report.summary()
    );
}

// ── Scenario 2: uncommitted data absent after recovery ────────────────────────

#[test]
fn test_uncommitted_data_rolled_back() {
    let env = TestEnv::new();
    let (page_id, slot_id);

    // Session 1: begin + insert, crash before commit.
    {
        let (mut storage, mut mgr) = env.create();
        mgr.begin().unwrap();
        (page_id, slot_id) = do_insert(&mut storage, &mut mgr, b"uncommitted-row");
        storage.flush().unwrap(); // page is on disk
        mgr.wal_mut().flush_buffer().unwrap(); // WAL in OS buffer
                                               // Crash: drop without commit
    }

    // Session 2: recovery must undo the insert.
    let (storage, _, result) = env.open_with_recovery();
    assert_eq!(
        result.undone_txns, 1,
        "one in-progress transaction must be undone"
    );
    assert_slot_dead(&storage, page_id, slot_id);

    let report = IntegrityChecker::post_recovery_check(&storage, result.max_committed).unwrap();
    assert!(
        report.is_clean(),
        "no uncommitted rows after recovery: {}",
        report.summary()
    );
}

// ── Scenario 3: partial transaction — committed row survives, crashed row dies ─

#[test]
fn test_partial_transaction_committed_survives_crashed_dies() {
    let env = TestEnv::new();
    let (pid_a, sid_a, pid_b, sid_b);

    {
        let (mut storage, mut mgr) = env.create();

        // txn 1: insert row A, commit.
        mgr.begin().unwrap();
        (pid_a, sid_a) = do_insert(&mut storage, &mut mgr, b"row-A-committed");
        mgr.commit().unwrap();

        // txn 2: insert row B, crash.
        mgr.begin().unwrap();
        (pid_b, sid_b) = do_insert(&mut storage, &mut mgr, b"row-B-crashed");
        storage.flush().unwrap();
        mgr.wal_mut().flush_buffer().unwrap();
        // Crash
    }

    let (storage, _, result) = env.open_with_recovery();
    assert_eq!(result.undone_txns, 1);
    assert_slot_alive(&storage, pid_a, sid_a);
    assert_slot_dead(&storage, pid_b, sid_b);

    let report = IntegrityChecker::post_recovery_check(&storage, result.max_committed).unwrap();
    assert!(report.is_clean(), "{}", report.summary());
}

// ── Scenario 4: truncated WAL (partial entry at end) ──────────────────────────

#[test]
fn test_truncated_wal_recovery_safe() {
    let env = TestEnv::new();

    // Session 1: commit txn1, start txn2, flush, crash.
    {
        let (mut storage, mut mgr) = env.create();

        mgr.begin().unwrap();
        do_insert(&mut storage, &mut mgr, b"committed-row");
        mgr.commit().unwrap(); // this fsyncs the WAL

        mgr.begin().unwrap();
        do_insert(&mut storage, &mut mgr, b"in-progress-row");
        storage.flush().unwrap();
        mgr.wal_mut().flush_buffer().unwrap(); // partial entries in OS buffer
    }

    // Truncate the WAL file to 3/4 of its size — cuts the last entry in half.
    let wal_size = std::fs::metadata(&env.wal).unwrap().len();
    let truncate_at = (wal_size * 3) / 4;
    {
        use std::io::Write;
        let file = OpenOptions::new().write(true).open(&env.wal).unwrap();
        file.set_len(truncate_at).unwrap();
    }

    // Recovery must NOT panic or error — WalReader stops at the truncated entry.
    let (storage, _, _result) = env.open_with_recovery();

    // Post-truncation: the database may have uncommitted rows if the truncation
    // cut into the WAL entries needed to undo an in-progress transaction.
    // This is expected and correct — we can only undo what we can read.
    // What must NOT happen: structural corruption (overlaps, bad offsets, etc.).
    let report = IntegrityChecker::post_recovery_check(&storage, 0).unwrap();
    // max_committed=0: skip MVCC checks entirely — focus only on structural integrity.
    // With max_committed=0, UncommittedAliveRow is never reported (MVCC checks skipped).
    assert!(
        report.is_clean(),
        "truncated WAL must not cause structural corruption: {}",
        report.summary()
    );
}

// ── Scenario 5: WAL rotation then crash ───────────────────────────────────────

#[test]
fn test_wal_rotation_then_crash_recovery() {
    let env = TestEnv::new();
    let (pid_before, sid_before, pid_after, sid_after);

    {
        let (mut storage, mut mgr) = env.create();

        // txn1: insert + commit.
        mgr.begin().unwrap();
        (pid_before, sid_before) = do_insert(&mut storage, &mut mgr, b"before-rotation");
        mgr.commit().unwrap();

        // Rotate WAL: checkpoint (pages → disk) + new WAL file.
        mgr.rotate_wal(&mut storage, &env.wal).unwrap();

        // txn2 in the new WAL segment: insert + crash.
        mgr.begin().unwrap();
        (pid_after, sid_after) = do_insert(&mut storage, &mut mgr, b"after-rotation");
        storage.flush().unwrap();
        mgr.wal_mut().flush_buffer().unwrap();
        // Crash
    }

    // Recovery reads the rotated WAL (starts from checkpoint_lsn).
    let (storage, _, result) = env.open_with_recovery();
    assert_eq!(
        result.undone_txns, 1,
        "post-rotation crashed txn must be undone"
    );
    assert_slot_alive(&storage, pid_before, sid_before);
    assert_slot_dead(&storage, pid_after, sid_after);

    let report = IntegrityChecker::post_recovery_check(&storage, result.max_committed).unwrap();
    assert!(report.is_clean(), "{}", report.summary());
}

// ── Scenario 6: multiple crash + recovery cycles (idempotency) ───────────────

#[test]
fn test_multiple_crash_recovery_cycles_idempotent() {
    let env = TestEnv::new();
    let (page_id, slot_id);

    // Session 1: crash mid-insert.
    {
        let (mut storage, mut mgr) = env.create();
        mgr.begin().unwrap();
        (page_id, slot_id) = do_insert(&mut storage, &mut mgr, b"orphan-row");
        storage.flush().unwrap();
        mgr.wal_mut().flush_buffer().unwrap();
    }

    // Session 2: first recovery.
    {
        let (storage, _, result) = env.open_with_recovery();
        assert_eq!(result.undone_txns, 1);
        assert_slot_dead(&storage, page_id, slot_id);
    }

    // Session 3: second recovery — idempotent (slot is already dead).
    {
        let (storage, _, _result) = env.open_with_recovery();
        // Slot must still be dead.
        assert_slot_dead(&storage, page_id, slot_id);

        let report = IntegrityChecker::post_recovery_check(&storage, 0).unwrap();
        // max_committed=0 because no transactions ever committed — skip MVCC checks.
        // The database must be structurally consistent.
        assert!(
            report.is_clean(),
            "second recovery must leave DB clean: {}",
            report.summary()
        );
    }
}

// ── Scenario 7: integrity check confirms clean after each scenario ────────────
// (Covered inline in scenarios 1-6 via post_recovery_check assertions.)

// ── Scenario 8: corrupt checkpoint_lsn — documented failure mode ─────────────

#[test]
fn test_corrupt_checkpoint_lsn_documented_failure_mode() {
    let env = TestEnv::new();

    // Session 1: insert (uncommitted) + flush page.
    {
        let (mut storage, mut mgr) = env.create();
        mgr.begin().unwrap();
        do_insert(&mut storage, &mut mgr, b"uncommitted");
        storage.flush().unwrap();
        mgr.wal_mut().flush_buffer().unwrap();
    }

    // Corrupt the checkpoint_lsn in the meta page to a huge value.
    {
        let mut storage = MmapStorage::open(&env.db).unwrap();
        axiomdb_storage::write_checkpoint_lsn(&mut storage, 999_999).unwrap();
        storage.flush().unwrap();
    }

    // Recovery: scan_forward(999999) finds no WAL entries → undone_txns=0.
    // The uncommitted row survives — this is the documented failure mode.
    let (storage, _, result) = env.open_with_recovery();
    assert_eq!(
        result.undone_txns, 0,
        "corrupt checkpoint causes recovery to skip WAL entries (known failure mode)"
    );

    // The integrity checker with max_committed=0 skips MVCC checks (no txn ever committed).
    // The test documents that corrupt checkpoint_lsn can cause silent inconsistency
    // when there are also uncommitted rows AND no committed transactions.
    // Full detection requires correct checkpoint_lsn + committed txns as reference.
    let report = IntegrityChecker::post_recovery_check(&storage, result.max_committed).unwrap();
    // With max_committed=0, MVCC checks are skipped → no UncommittedAliveRow reported.
    // This is the documented double-failure scenario.
    assert_eq!(
        report.errors.len(),
        0,
        "with max_committed=0, MVCC checks are skipped — this documents the limitation"
    );
}

// ── Scenario 9: partial page write (CRC corruption) — documented limitation ───

#[test]
fn test_partial_page_write_detected_as_checksum_mismatch() {
    let env = TestEnv::new();

    // Session 1: commit a row and flush page to disk.
    let page_id = {
        let (mut storage, mut mgr) = env.create();
        mgr.begin().unwrap();
        let (pid, _) = do_insert(&mut storage, &mut mgr, b"written-row");
        mgr.commit().unwrap();
        storage.flush().unwrap();
        pid
    };

    // Corrupt the page body directly in the .db file (simulates partial write).
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = OpenOptions::new().write(true).open(&env.db).unwrap();
        // PAGE_SIZE = 16384; skip to middle of the body.
        let corrupt_offset = page_id * 16_384 + 200;
        f.seek(SeekFrom::Start(corrupt_offset)).unwrap();
        f.write_all(&[0xFF_u8; 8]).unwrap();
        f.sync_all().unwrap();
    }

    // Since 3.8b, open() verifies all allocated pages on startup.
    // Corruption must be caught before the first query.
    let result = MmapStorage::open(&env.db);
    assert!(
        matches!(result, Err(DbError::ChecksumMismatch { .. })),
        "corrupted page must be detected as ChecksumMismatch on open, got: {:?}",
        result.err()
    );
    // Documents: full recovery from partial page write requires WAL redo (Phase 3.8b).
}
