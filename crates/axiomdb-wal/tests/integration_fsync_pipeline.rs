//! Fsync pipeline integration tests — Phase 6.19.
//!
//! These tests exercise the `FsyncPipeline` state machine together with
//! `TxnManager` deferred commit mode, verifying:
//!
//! - End-to-end pipeline flow: deferred commit → acquire → fsync → release.
//! - Follower piggyback: commit queued behind a leader shares one fsync.
//! - Crash recovery: data committed via the pipeline survives process restart.
//! - Batch advance: `advance_committed` makes all pipelined rows visible at once.
//! - MVCC invariant: rows inserted in deferred mode are invisible until
//!   `advance_committed` is called.

use std::path::PathBuf;

use axiomdb_core::TransactionSnapshot;
use axiomdb_storage::{heap::insert_tuple, read_tuple, MmapStorage, Page, PageType, StorageEngine};
use axiomdb_wal::{AcquireResult, FsyncPipeline, TxnManager};
use tempfile::TempDir;

// ── TestEnv ───────────────────────────────────────────────────────────────────

struct TestEnv {
    _dir: TempDir,
    pub db: PathBuf,
    pub wal: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("test.db");
        let wal = dir.path().join("test.wal");
        Self { _dir: dir, db, wal }
    }

    fn create(&self) -> (MmapStorage, TxnManager) {
        let storage = MmapStorage::create(&self.db).expect("create db");
        let txn = TxnManager::create(&self.wal).expect("create wal");
        (storage, txn)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn do_insert(storage: &mut MmapStorage, txn: &mut TxnManager, data: &[u8]) -> (u64, u16) {
    let page_id = storage.alloc_page(PageType::Data).expect("alloc page");
    let txn_id = txn.active_txn_id().expect("must begin first");
    let raw = *storage.read_page(page_id).expect("read page").as_bytes();
    let mut page = Page::from_bytes(raw).expect("parse page");
    let slot_id = insert_tuple(&mut page, data, txn_id).expect("insert tuple");
    storage.write_page(page_id, &page).expect("write page");
    txn.record_insert(1, data, data, page_id, slot_id)
        .expect("record insert");
    (page_id, slot_id)
}

fn slot_visible(
    storage: &MmapStorage,
    page_id: u64,
    slot_id: u16,
    snap: TransactionSnapshot,
) -> bool {
    let page = storage.read_page(page_id).expect("read page");
    if let Some((header, _)) = read_tuple(&page, slot_id).expect("read_tuple") {
        header.txn_id_created < snap.snapshot_id || header.txn_id_created == snap.current_txn_id
    } else {
        false
    }
}

fn leader_fsync(txn: &mut TxnManager, pipeline: &FsyncPipeline) -> Vec<axiomdb_core::TxnId> {
    txn.wal_flush_and_fsync().expect("wal_flush_and_fsync");
    let flushed = txn.wal_current_lsn();
    pipeline.release_ok(flushed)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Single deferred commit: acquire → Acquired, fsync, advance_committed.
/// Row invisible before advance, visible after.
#[test]
fn test_pipeline_single_commit_visibility() {
    let env = TestEnv::new();
    let (mut storage, mut txn) = env.create();
    txn.set_deferred_commit_mode(true);
    let pipeline = FsyncPipeline::new(txn.wal_current_lsn());

    txn.begin().expect("begin");
    let (page_id, slot_id) = do_insert(&mut storage, &mut txn, b"pipeline_row");
    txn.commit().expect("commit");
    let txn_id = txn.take_pending_deferred_commit().expect("pending txn");
    let commit_lsn = txn.wal_current_lsn();

    let snap_before = txn.snapshot();
    assert!(
        !slot_visible(&storage, page_id, slot_id, snap_before),
        "row must be invisible before advance_committed"
    );

    assert!(
        matches!(
            pipeline.acquire(commit_lsn, txn_id),
            AcquireResult::Acquired
        ),
        "first acquire must return Acquired"
    );
    let woken = leader_fsync(&mut txn, &pipeline);
    assert!(woken.is_empty(), "no followers queued");
    txn.advance_committed_single(txn_id);

    let snap_after = txn.snapshot();
    assert!(
        slot_visible(&storage, page_id, slot_id, snap_after),
        "row must be visible after advance_committed"
    );
}

/// Two sequential deferred commits each act as independent leaders (no concurrent
/// pipelining in sequential code). Both rows end up committed.
#[test]
fn test_pipeline_two_sequential_leaders() {
    let env = TestEnv::new();
    let (mut storage, mut txn) = env.create();
    txn.set_deferred_commit_mode(true);
    let pipeline = FsyncPipeline::new(txn.wal_current_lsn());

    // First commit: becomes leader, fsyncs.
    txn.begin().expect("begin");
    let (page_id1, slot_id1) = do_insert(&mut storage, &mut txn, b"row1");
    txn.commit().expect("commit");
    let txn_id1 = txn.take_pending_deferred_commit().expect("pending");
    let lsn1 = txn.wal_current_lsn();
    assert!(matches!(
        pipeline.acquire(lsn1, txn_id1),
        AcquireResult::Acquired
    ));
    let woken = leader_fsync(&mut txn, &pipeline);
    assert!(woken.is_empty());
    txn.advance_committed_single(txn_id1);

    // Second commit: also becomes leader (previous leader released the flag).
    txn.begin().expect("begin");
    let (page_id2, slot_id2) = do_insert(&mut storage, &mut txn, b"row2");
    txn.commit().expect("commit");
    let txn_id2 = txn.take_pending_deferred_commit().expect("pending");
    let lsn2 = txn.wal_current_lsn();
    // lsn2 > flushed_lsn (new WAL entries appended after previous fsync)
    // → Acquired (new leader), NOT Expired.
    assert!(
        matches!(pipeline.acquire(lsn2, txn_id2), AcquireResult::Acquired),
        "second sequential commit must become a new leader"
    );
    let woken = leader_fsync(&mut txn, &pipeline);
    assert!(woken.is_empty());
    txn.advance_committed_single(txn_id2);

    let snap = txn.snapshot();
    assert!(
        slot_visible(&storage, page_id1, slot_id1, snap),
        "row1 must be visible"
    );
    assert!(
        slot_visible(&storage, page_id2, slot_id2, snap),
        "row2 must be visible"
    );
}

/// Follower piggybacks on the leader's in-flight fsync.
#[tokio::test]
async fn test_pipeline_follower_piggybacks_on_leader() {
    let env = TestEnv::new();
    let (mut storage, mut txn) = env.create();
    txn.set_deferred_commit_mode(true);
    let pipeline = FsyncPipeline::new(txn.wal_current_lsn());

    // First commit → leader (no fsync yet).
    txn.begin().expect("begin");
    do_insert(&mut storage, &mut txn, b"leader_row");
    txn.commit().expect("commit");
    let txn_id1 = txn.take_pending_deferred_commit().expect("pending");
    let lsn1 = txn.wal_current_lsn();
    assert!(matches!(
        pipeline.acquire(lsn1, txn_id1),
        AcquireResult::Acquired
    ));

    // Second commit → queued while leader is active.
    txn.begin().expect("begin");
    do_insert(&mut storage, &mut txn, b"follower_row");
    txn.commit().expect("commit");
    let txn_id2 = txn.take_pending_deferred_commit().expect("pending");
    let lsn2 = txn.wal_current_lsn();
    let rx = match pipeline.acquire(lsn2, txn_id2) {
        AcquireResult::Queued(rx) => rx,
        AcquireResult::Acquired => panic!("expected Queued, got Acquired"),
        AcquireResult::Expired => panic!("expected Queued, got Expired"),
    };

    // Leader fsyncs → wakes follower.
    let woken = leader_fsync(&mut txn, &pipeline);
    txn.advance_committed_single(txn_id1);
    txn.advance_committed(&woken);

    let result = rx.await.expect("channel must not be dropped");
    assert!(
        result.is_ok(),
        "follower must receive Ok from leader's release"
    );

    let snap = txn.snapshot();
    assert!(snap.snapshot_id > txn_id2, "both txns committed");
}

/// Data committed through the pipeline survives a crash and is recovered.
#[test]
fn test_pipeline_commit_survives_crash() {
    let env = TestEnv::new();
    let page_id;
    let slot_id;

    {
        let (mut storage, mut txn) = env.create();
        txn.set_deferred_commit_mode(true);
        let pipeline = FsyncPipeline::new(txn.wal_current_lsn());

        txn.begin().expect("begin");
        let ids = do_insert(&mut storage, &mut txn, b"durable_data");
        page_id = ids.0;
        slot_id = ids.1;
        txn.commit().expect("commit");
        let txn_id = txn.take_pending_deferred_commit().expect("pending");
        let lsn = txn.wal_current_lsn();

        assert!(matches!(
            pipeline.acquire(lsn, txn_id),
            AcquireResult::Acquired
        ));
        txn.wal_flush_and_fsync().expect("fsync");
        let flushed = txn.wal_current_lsn();
        pipeline.release_ok(flushed);
        txn.advance_committed_single(txn_id);
        // Drop without graceful shutdown → crash.
    }

    // Crash recovery.
    let mut storage = MmapStorage::open(&env.db).expect("open db");
    let (txn, result) = TxnManager::open_with_recovery(&mut storage, &env.wal).expect("recovery");

    assert_eq!(result.undone_txns, 0, "no in-progress txns to undo");
    assert!(
        result.max_committed >= 1,
        "recovery must restore max_committed"
    );

    // Row must be physically present.
    let page = storage.read_page(page_id).expect("read page");
    assert!(
        read_tuple(&page, slot_id).expect("read_tuple").is_some(),
        "pipeline-committed row must survive crash + recovery"
    );

    // Row must be MVCC-visible after recovery.
    let snap = txn.snapshot();
    assert!(
        slot_visible(&storage, page_id, slot_id, snap),
        "pipeline-committed row must be MVCC-visible after recovery"
    );
}

/// Three followers queued behind one leader: all three resolved by one release_ok.
#[tokio::test]
async fn test_pipeline_batch_wakeup_three_followers() {
    let env = TestEnv::new();
    let (mut storage, mut txn) = env.create();
    txn.set_deferred_commit_mode(true);
    let pipeline = FsyncPipeline::new(txn.wal_current_lsn());

    // Leader acquires.
    txn.begin().expect("begin");
    do_insert(&mut storage, &mut txn, b"leader");
    txn.commit().expect("commit");
    let txn_id0 = txn.take_pending_deferred_commit().expect("pending");
    let lsn0 = txn.wal_current_lsn();
    assert!(matches!(
        pipeline.acquire(lsn0, txn_id0),
        AcquireResult::Acquired
    ));

    // Three followers queue.
    let mut rxs = Vec::new();
    let mut txn_ids = Vec::new();
    for i in 0u8..3 {
        txn.begin().expect("begin");
        do_insert(&mut storage, &mut txn, &[i]);
        txn.commit().expect("commit");
        let txn_id = txn.take_pending_deferred_commit().expect("pending");
        let lsn = txn.wal_current_lsn();
        let rx = match pipeline.acquire(lsn, txn_id) {
            AcquireResult::Queued(rx) => rx,
            AcquireResult::Acquired => panic!("follower {i} must be Queued"),
            AcquireResult::Expired => panic!("follower {i} must be Queued"),
        };
        rxs.push(rx);
        txn_ids.push(txn_id);
    }

    // Leader fsyncs.
    let woken = leader_fsync(&mut txn, &pipeline);
    assert_eq!(woken.len(), 3, "all three followers must be woken");
    for tid in &txn_ids {
        assert!(woken.contains(tid), "txn_id {tid} must be in woken list");
    }

    for (i, rx) in rxs.into_iter().enumerate() {
        let res = rx.await.expect("channel alive");
        assert!(res.is_ok(), "follower {i} must receive Ok");
    }
}

/// MVCC invariant: snapshot before advance sees row as invisible; after sees it.
#[test]
fn test_pipeline_mvcc_visibility_invariant() {
    let env = TestEnv::new();
    let (mut storage, mut txn) = env.create();
    txn.set_deferred_commit_mode(true);
    let pipeline = FsyncPipeline::new(txn.wal_current_lsn());

    txn.begin().expect("begin");
    let (page_id, slot_id) = do_insert(&mut storage, &mut txn, b"mvcc_test");
    txn.commit().expect("commit");
    let txn_id = txn.take_pending_deferred_commit().expect("pending");
    let lsn = txn.wal_current_lsn();

    let snap_before = txn.snapshot();

    assert!(matches!(
        pipeline.acquire(lsn, txn_id),
        AcquireResult::Acquired
    ));
    txn.wal_flush_and_fsync().expect("fsync");
    let flushed = txn.wal_current_lsn();
    pipeline.release_ok(flushed);
    txn.advance_committed_single(txn_id);

    let snap_after = txn.snapshot();

    assert!(
        !slot_visible(&storage, page_id, slot_id, snap_before),
        "pre-advance snapshot must not see the row"
    );
    assert!(
        slot_visible(&storage, page_id, slot_id, snap_after),
        "post-advance snapshot must see the row"
    );
}
