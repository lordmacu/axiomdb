//! Project B — frame-only redo crash suite (T1–T7), driven by
//! `FaultInjectionStorage` (power-loss simulation: only `flush`'d data survives
//! `simulate_power_loss`; the fsync'd page-frame log survives). T0 lives in
//! `integration_redo_recovery.rs`; this file is the gate that lets frame-only
//! redo become the embedded default (spec/plan: `specs/fase-redo-recovery/
//! {spec,plan}-redo-default-on.md`).
//!
//! Crash pattern (from T0): enable redo log → durable baseline (write+flush) →
//! write + `commit_durable` (data page stays volatile, frame log fsync'd) →
//! `simulate_power_loss` → `open_with_recovery` REDOes committed frames →
//! assert restored → second `recover` is a no-op (pageLSN idempotence).

use axiomdb_storage::{
    clustered_tree, fault_injection::FaultInjectionStorage, RowHeader, StorageEngine,
};
use axiomdb_wal::{ClusteredRowImage, CrashRecovery, TxnManager};
use tempfile::tempdir;

const TABLE_ID: u32 = 41;

fn row_header(txn_id: u64) -> RowHeader {
    RowHeader {
        txn_id_created: txn_id,
        txn_id_deleted: 0,
        row_version: 0,
        _flags: 0,
    }
}

// ── T1 — redo of each clustered page type ───────────────────────────────────

/// A committed clustered-leaf insert (single-leaf root) survives power loss.
#[test]
fn t1_clustered_leaf_insert_survives_power_loss() {
    let dir = tempdir().expect("tempdir");
    let wal_path = dir.path().join("t1_leaf.wal");

    let mut storage = FaultInjectionStorage::new();
    storage
        .enable_redo_log(&dir.path().join("t1_leaf.wf"))
        .unwrap();

    let key = b"k-leaf";
    let data = b"clustered-leaf-row-survives".to_vec();
    let txn_id;
    {
        let txn = TxnManager::create(&wal_path).unwrap();
        let mut conn = txn.begin().unwrap();
        txn_id = conn.txn_id;
        storage.set_current_txn(conn.txn_id);

        let root = clustered_tree::insert(&storage, None, key, &row_header(txn_id), &data).unwrap();
        let image = ClusteredRowImage::new(root, row_header(txn_id), &data);
        txn.record_clustered_insert(&mut conn, TABLE_ID, key, &image)
            .unwrap();
        txn.commit_durable(conn, &storage).unwrap();
        storage.set_current_txn(0);
    }

    storage.simulate_power_loss();

    let (mgr, result) = TxnManager::open_with_recovery(&mut storage, &wal_path).unwrap();
    assert!(
        result.redone_pages >= 1,
        "recovery must redo the committed clustered leaf page(s)"
    );
    let root = mgr
        .clustered_root(TABLE_ID)
        .expect("clustered root must be tracked after recovery");
    let row = clustered_tree::lookup_physical(&storage, Some(root), key)
        .unwrap()
        .expect("committed clustered-leaf row must survive power loss");
    assert_eq!(row.row_data, data);

    // Idempotence: a second recovery re-applies nothing (pageLSN guard).
    let r2 = CrashRecovery::recover(&mut storage, &wal_path).unwrap();
    assert_eq!(r2.redone_pages, 0, "second recovery must be a no-op");
}

/// Many committed clustered inserts that grow the tree to an internal root all
/// survive power loss — exercises clustered-internal frames + multi-leaf redo.
#[test]
fn t1_clustered_internal_split_survives_power_loss() {
    let dir = tempdir().expect("tempdir");
    let wal_path = dir.path().join("t1_internal.wal");

    let mut storage = FaultInjectionStorage::new();
    storage
        .enable_redo_log(&dir.path().join("t1_internal.wf"))
        .unwrap();

    // 300-byte rows ⇒ ~47 per 16 KiB leaf ⇒ 300 rows force several splits ⇒
    // an internal root + a multi-leaf chain, all committed in one txn.
    let n: u32 = 300;
    let txn_id;
    {
        let txn = TxnManager::create(&wal_path).unwrap();
        let mut conn = txn.begin().unwrap();
        txn_id = conn.txn_id;
        storage.set_current_txn(conn.txn_id);

        let mut root: Option<u64> = None;
        for k in 0..n {
            let key = k.to_be_bytes();
            let data = vec![k as u8; 300];
            let new_root =
                clustered_tree::insert(&storage, root, &key, &row_header(txn_id), &data).unwrap();
            root = Some(new_root);
            let image = ClusteredRowImage::new(new_root, row_header(txn_id), &data);
            txn.record_clustered_insert(&mut conn, TABLE_ID, &key, &image)
                .unwrap();
        }
        txn.commit_durable(conn, &storage).unwrap();
        storage.set_current_txn(0);
    }

    storage.simulate_power_loss();

    let (mgr, result) = TxnManager::open_with_recovery(&mut storage, &wal_path).unwrap();
    assert!(
        result.redone_pages >= 2,
        "a split tree must redo multiple pages (leaves + internal), got {}",
        result.redone_pages
    );
    let root = mgr
        .clustered_root(TABLE_ID)
        .expect("clustered root tracked after recovery");

    // Every committed row must be present after redo (probe across leaves).
    for k in [0u32, 1, 47, 150, n - 1] {
        let row = clustered_tree::lookup_physical(&storage, Some(root), &k.to_be_bytes())
            .unwrap()
            .unwrap_or_else(|| panic!("row {k} must survive power loss"));
        assert_eq!(row.row_data, vec![k as u8; 300]);
    }

    let r2 = CrashRecovery::recover(&mut storage, &wal_path).unwrap();
    assert_eq!(r2.redone_pages, 0, "second recovery must be a no-op");
}

/// A committed overflow-backed clustered row (logical row > inline budget, so it
/// spills into an overflow-page chain) survives power loss with its full bytes.
#[test]
fn t1_overflow_row_survives_power_loss() {
    let dir = tempdir().expect("tempdir");
    let wal_path = dir.path().join("t1_overflow.wal");

    let mut storage = FaultInjectionStorage::new();
    storage
        .enable_redo_log(&dir.path().join("t1_overflow.wf"))
        .unwrap();

    let key = b"k-overflow";
    // 6000 bytes exceeds the ~page/4 inline budget ⇒ an overflow-page chain.
    let data: Vec<u8> = (0..6000u32).map(|i| (i % 251) as u8).collect();
    let txn_id;
    {
        let txn = TxnManager::create(&wal_path).unwrap();
        let mut conn = txn.begin().unwrap();
        txn_id = conn.txn_id;
        storage.set_current_txn(conn.txn_id);

        let root = clustered_tree::insert(&storage, None, key, &row_header(txn_id), &data).unwrap();
        let image = ClusteredRowImage::new(root, row_header(txn_id), &data);
        txn.record_clustered_insert(&mut conn, TABLE_ID, key, &image)
            .unwrap();
        txn.commit_durable(conn, &storage).unwrap();
        storage.set_current_txn(0);
    }

    storage.simulate_power_loss();

    let (mgr, result) = TxnManager::open_with_recovery(&mut storage, &wal_path).unwrap();
    assert!(
        result.redone_pages >= 2,
        "overflow row must redo the leaf + overflow page(s), got {}",
        result.redone_pages
    );
    let root = mgr.clustered_root(TABLE_ID).expect("root tracked");
    let row = clustered_tree::lookup_physical(&storage, Some(root), key)
        .unwrap()
        .expect("committed overflow row must survive power loss");
    assert_eq!(
        row.row_data, data,
        "full overflow-backed bytes must be intact"
    );

    let r2 = CrashRecovery::recover(&mut storage, &wal_path).unwrap();
    assert_eq!(r2.redone_pages, 0, "second recovery must be a no-op");
}

// ── T6 — uncommitted txn is still UNDONE (redo coexists with logical undo) ───

/// A clustered insert that never reaches `commit_durable` is uncommitted at the
/// crash: its frames were never fsync'd, so recovery must REDO nothing and the
/// row must be absent.
#[test]
fn t6_uncommitted_clustered_insert_is_undone_after_power_loss() {
    let dir = tempdir().expect("tempdir");
    let wal_path = dir.path().join("t6.wal");

    let mut storage = FaultInjectionStorage::new();
    storage.enable_redo_log(&dir.path().join("t6.wf")).unwrap();

    let key = b"k-uncommitted";
    let data = b"never-committed".to_vec();
    {
        let txn = TxnManager::create(&wal_path).unwrap();
        let mut conn = txn.begin().unwrap();
        storage.set_current_txn(conn.txn_id);

        let root =
            clustered_tree::insert(&storage, None, key, &row_header(conn.txn_id), &data).unwrap();
        let image = ClusteredRowImage::new(root, row_header(conn.txn_id), &data);
        txn.record_clustered_insert(&mut conn, TABLE_ID, key, &image)
            .unwrap();
        // NO commit_durable → the frames are never fsync'd.
        storage.set_current_txn(0);
        drop(conn);
    }

    storage.simulate_power_loss();

    let (mgr, result) = TxnManager::open_with_recovery(&mut storage, &wal_path).unwrap();
    assert_eq!(
        result.redone_pages, 0,
        "uncommitted frames must NOT be redone"
    );
    // The committed-only root hint must not surface the uncommitted row: either
    // there is no committed root (None) or the lost page is unreadable (Err) —
    // both mean the uncommitted row did not survive.
    if let Some(root) = mgr.clustered_root(TABLE_ID) {
        let readable = matches!(
            clustered_tree::lookup_physical(&storage, Some(root), key),
            Ok(Some(_))
        );
        assert!(
            !readable,
            "uncommitted row must not be readable after recovery"
        );
    }
}
