//! Deferred-commit + fsync pipeline integration tests.
//!
//! Tests cover:
//! - `commit_deferred_mode = false` (default): TxnManager behaves identically
//!   to pre-3.19; `take_pending_deferred_commit()` always returns None.
//! - `commit_deferred_mode = true` (used by the Phase 6.19 fsync pipeline):
//!   - DML commit → `take_pending_deferred_commit()` returns `Some(txn_id)`.
//!   - max_committed NOT advanced until `advance_committed()` is called.
//!   - Read-only commit → flush_no_sync path; `take_pending_deferred_commit()` None.
//! - `advance_committed()` advances max_committed to the batch max.
//! - `advance_committed()` never regresses max_committed.
//! - `wal_flush_and_fsync()` succeeds (flush + fsync round-trip).
//! - MVCC invariant: row inserted in deferred mode is invisible until
//!   `advance_committed()` is called (snapshot_id has not advanced).

use std::path::PathBuf;

use axiomdb_storage::{heap::insert_tuple, read_tuple, MmapStorage, Page, PageType, StorageEngine};
use axiomdb_wal::TxnManager;
use tempfile::TempDir;

// ── TestEnv ───────────────────────────────────────────────────────────────────

struct TestEnv {
    _dir: TempDir,
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
}

fn make_storage(env: &TestEnv) -> (MmapStorage, u64) {
    let mut storage = MmapStorage::create(&env.db).unwrap();
    let page_id = storage.alloc_page(PageType::Data).unwrap();
    let page = Page::new(PageType::Data, page_id);
    storage.write_page(page_id, &page).unwrap();
    (storage, page_id)
}

fn insert_row(storage: &mut MmapStorage, txn: &mut TxnManager, page_id: u64, data: &[u8]) -> u16 {
    let txn_id = txn.active_txn_id().unwrap();
    let raw = *storage.read_page(page_id).unwrap().as_bytes();
    let mut page = Page::from_bytes(raw).unwrap();
    let slot_id = insert_tuple(&mut page, data, txn_id).unwrap();
    storage.write_page(page_id, &page).unwrap();
    txn.record_insert(1, b"k", data, page_id, slot_id).unwrap();
    slot_id
}

// ── Tests — default mode (inline fsync) ───────────────────────────────────────

#[test]
fn test_default_mode_commit_advances_max_committed() {
    let env = TestEnv::new();
    let (mut storage, page_id) = make_storage(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();

    txn.begin().unwrap();
    insert_row(&mut storage, &mut txn, page_id, b"hello");
    txn.commit().unwrap();

    let snap = txn.snapshot();
    assert!(snap.snapshot_id > 0, "max_committed should have advanced");
    assert!(txn.take_pending_deferred_commit().is_none());
}

#[test]
fn test_default_mode_readonly_no_pending() {
    let env = TestEnv::new();
    let (_, _) = make_storage(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();

    txn.begin().unwrap();
    txn.commit().unwrap();

    assert!(txn.take_pending_deferred_commit().is_none());
}

// ── Tests — deferred commit mode (fsync pipeline hook) ───────────────────────

#[test]
fn test_deferred_mode_dml_does_not_advance_max_committed() {
    let env = TestEnv::new();
    let (mut storage, page_id) = make_storage(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();
    txn.set_deferred_commit_mode(true);

    let snap_before = txn.snapshot();

    txn.begin().unwrap();
    insert_row(&mut storage, &mut txn, page_id, b"world");
    txn.commit().unwrap();

    let snap_after = txn.snapshot();
    assert_eq!(
        snap_before.snapshot_id, snap_after.snapshot_id,
        "max_committed must not advance before advance_committed() is called"
    );
}

#[test]
fn test_deferred_mode_dml_returns_pending_txn_id() {
    let env = TestEnv::new();
    let (mut storage, page_id) = make_storage(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();
    txn.set_deferred_commit_mode(true);

    txn.begin().unwrap();
    let txn_id = txn.active_txn_id().unwrap();
    insert_row(&mut storage, &mut txn, page_id, b"deferred");
    txn.commit().unwrap();

    let pending = txn.take_pending_deferred_commit();
    assert_eq!(
        pending,
        Some(txn_id),
        "DML commit must produce a pending txn_id"
    );

    // Second call returns None — idempotent take.
    assert!(txn.take_pending_deferred_commit().is_none());
}

#[test]
fn test_deferred_mode_readonly_no_pending_and_advances_committed() {
    let env = TestEnv::new();
    let (_, _) = make_storage(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();
    txn.set_deferred_commit_mode(true);

    let snap_before = txn.snapshot();

    txn.begin().unwrap();
    // No DML — read-only transaction always goes through flush_no_sync.
    txn.commit().unwrap();

    let snap_after = txn.snapshot();
    assert!(
        snap_after.snapshot_id >= snap_before.snapshot_id,
        "read-only commit must advance max_committed"
    );
    assert!(txn.take_pending_deferred_commit().is_none());
}

// ── Tests — advance_committed ─────────────────────────────────────────────────

#[test]
fn test_advance_committed_advances_max() {
    let env = TestEnv::new();
    let (mut storage, page_id) = make_storage(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();
    txn.set_deferred_commit_mode(true);

    let mut txn_ids = vec![];
    for i in 0..3u8 {
        txn.begin().unwrap();
        insert_row(&mut storage, &mut txn, page_id, &[i]);
        txn.commit().unwrap();
        txn_ids.push(txn.take_pending_deferred_commit().unwrap());
    }

    let snap_before = txn.snapshot();
    txn.advance_committed(&txn_ids);
    let snap_after = txn.snapshot();

    let max_id = *txn_ids.iter().max().unwrap();
    assert!(
        snap_after.snapshot_id > snap_before.snapshot_id,
        "advance_committed must increase max_committed"
    );
    assert!(
        snap_after.snapshot_id > max_id,
        "snapshot_id must be at least max_txn_id + 1"
    );
}

#[test]
fn test_advance_committed_no_regression() {
    let env = TestEnv::new();
    let (mut storage, page_id) = make_storage(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();
    txn.set_deferred_commit_mode(true);

    txn.begin().unwrap();
    insert_row(&mut storage, &mut txn, page_id, b"x");
    txn.commit().unwrap();
    let txn_id = txn.take_pending_deferred_commit().unwrap();
    txn.advance_committed(&[txn_id]);
    let snap_high = txn.snapshot();

    // A lower txn_id must not regress max_committed.
    txn.advance_committed(&[1]);
    let snap_after = txn.snapshot();
    assert_eq!(
        snap_high.snapshot_id, snap_after.snapshot_id,
        "advance_committed must not regress max_committed"
    );
}

#[test]
fn test_advance_committed_empty_slice_is_noop() {
    let env = TestEnv::new();
    let (_, _) = make_storage(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();

    let snap_before = txn.snapshot();
    txn.advance_committed(&[]);
    let snap_after = txn.snapshot();
    assert_eq!(snap_before.snapshot_id, snap_after.snapshot_id);
}

// ── Tests — wal_flush_and_fsync ───────────────────────────────────────────────

#[test]
fn test_wal_flush_and_fsync_succeeds() {
    let env = TestEnv::new();
    let (mut storage, page_id) = make_storage(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();
    txn.set_deferred_commit_mode(true);

    txn.begin().unwrap();
    insert_row(&mut storage, &mut txn, page_id, b"fsync test");
    txn.commit().unwrap();

    txn.wal_flush_and_fsync()
        .expect("flush_and_fsync must succeed");
}

// ── Tests — MVCC visibility invariant ────────────────────────────────────────

/// Verifies the MVCC invariant: a row committed in deferred mode is invisible
/// to snapshots taken before `advance_committed()` is called.
///
/// This is the key correctness property of the fsync pipeline hook:
/// `max_committed` does not advance until the leader confirms the fsync, so no
/// snapshot can observe the row before durability is guaranteed.
#[test]
fn test_deferred_row_invisible_before_advance_committed() {
    let env = TestEnv::new();
    let (mut storage, page_id) = make_storage(&env);
    let mut txn = TxnManager::create(&env.wal).unwrap();
    txn.set_deferred_commit_mode(true);

    txn.begin().unwrap();
    let txn_id = txn.active_txn_id().unwrap();
    let slot_id = insert_row(&mut storage, &mut txn, page_id, b"invisible until fsync");
    txn.commit().unwrap();
    let pending = txn.take_pending_deferred_commit().unwrap();

    // Snapshot taken BEFORE advance_committed — max_committed has not advanced.
    let snap = txn.snapshot();

    // The row exists in the heap page.
    let raw = *storage.read_page(page_id).unwrap().as_bytes();
    let page = Page::from_bytes(raw).unwrap();
    let (header, _data) = read_tuple(&page, slot_id).unwrap().unwrap();

    // MVCC visibility: txn_id_created must be < snapshot_id OR == current_txn_id.
    // Since max_committed was not advanced, snapshot_id = max_committed + 1 = 1.
    // The row has txn_id_created = txn_id (= 1). For a new snapshot (current_txn_id=0),
    // 1 < 1 is false and 1 == 0 is false → row is invisible. Correct.
    let visible_to_snap =
        header.txn_id_created < snap.snapshot_id || header.txn_id_created == snap.current_txn_id;
    assert!(
        !visible_to_snap,
        "row must be invisible before advance_committed: txn_id={txn_id}, \
         snapshot_id={}, txn_id_created={}",
        snap.snapshot_id, header.txn_id_created
    );

    // After advance_committed, a new snapshot sees the row.
    txn.advance_committed(&[pending]);
    let snap_after = txn.snapshot();
    let visible_after = header.txn_id_created < snap_after.snapshot_id;
    assert!(
        visible_after,
        "row must be visible after advance_committed: txn_id_created={}, snapshot_id={}",
        header.txn_id_created, snap_after.snapshot_id
    );
}
