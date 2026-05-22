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

// ── T2 — recovery is re-runnable (crash again after recovery → no-op) ────────

/// After recovery flushes the redone state durable, a second power loss + a
/// second recovery is a no-op (pageLSN idempotence) and the data is intact.
#[test]
fn t2_recovery_is_rerunnable_after_second_crash() {
    let dir = tempdir().expect("tempdir");
    let wal_path = dir.path().join("t2.wal");

    let mut storage = FaultInjectionStorage::new();
    storage.enable_redo_log(&dir.path().join("t2.wf")).unwrap();

    let key = b"k-rerun";
    let data = b"survives-two-crashes".to_vec();
    {
        let txn = TxnManager::create(&wal_path).unwrap();
        let mut conn = txn.begin().unwrap();
        storage.set_current_txn(conn.txn_id);
        let root =
            clustered_tree::insert(&storage, None, key, &row_header(conn.txn_id), &data).unwrap();
        let image = ClusteredRowImage::new(root, row_header(conn.txn_id), &data);
        txn.record_clustered_insert(&mut conn, TABLE_ID, key, &image)
            .unwrap();
        txn.commit_durable(conn, &storage).unwrap();
        storage.set_current_txn(0);
    }

    storage.simulate_power_loss();
    {
        let (_mgr, r1) = TxnManager::open_with_recovery(&mut storage, &wal_path).unwrap();
        assert!(
            r1.redone_pages >= 1,
            "first recovery redoes the committed page"
        );
    }

    // Recovery flushed the redone pages durable → a second crash reverts nothing.
    storage.simulate_power_loss();
    let (mgr, r2) = TxnManager::open_with_recovery(&mut storage, &wal_path).unwrap();
    assert_eq!(
        r2.redone_pages, 0,
        "re-recovery after a second crash must be a no-op (pageLSN guard)"
    );
    let root = mgr.clustered_root(TABLE_ID).expect("root tracked");
    let row = clustered_tree::lookup_physical(&storage, Some(root), key)
        .unwrap()
        .expect("row must remain after two crashes + two recoveries");
    assert_eq!(row.row_data, data);
}

// ── T3 — recovery after a checkpoint combines main (checkpointed) + frames ───

/// A checkpoint applies committed frames to the main file + recycles the log.
/// A later committed txn writes fresh frames; after a crash, recovery must
/// combine the checkpointed data (from main) with the post-checkpoint data
/// (redone from the recycled log's frames).
#[test]
fn t3_recovery_after_checkpoint_combines_main_and_frames() {
    let dir = tempdir().expect("tempdir");
    let wal_path = dir.path().join("t3.wal");

    let mut storage = FaultInjectionStorage::new();
    storage.enable_redo_log(&dir.path().join("t3.wf")).unwrap();

    let txn = TxnManager::create(&wal_path).unwrap();

    // txn A — committed, then checkpointed into main + the frame log recycled.
    let key_a = b"k-checkpointed";
    let data_a = b"in-main-after-checkpoint".to_vec();
    let txn_a;
    {
        let mut conn = txn.begin().unwrap();
        txn_a = conn.txn_id;
        storage.set_current_txn(txn_a);
        let root =
            clustered_tree::insert(&storage, None, key_a, &row_header(txn_a), &data_a).unwrap();
        let image = ClusteredRowImage::new(root, row_header(txn_a), &data_a);
        txn.record_clustered_insert(&mut conn, TABLE_ID, key_a, &image)
            .unwrap();
        txn.commit_durable(conn, &storage).unwrap();
        storage.set_current_txn(0);
    }
    // Under dual-write the main file already holds A's content (same pageLSN), so
    // the apply is a no-op (the strict-`>` guard skips); the checkpoint's job here
    // is to fsync main durable + recycle the frame log. The applied count is an
    // impl detail — what matters is that A survives the later crash from main.
    let _ = storage.checkpoint_frames(&|t| t == txn_a).unwrap();

    // txn B — committed AFTER the checkpoint (frames live on the recycled log).
    let key_b = b"k-post-checkpoint";
    let data_b = b"redone-from-frames".to_vec();
    {
        let mut conn = txn.begin().unwrap();
        let txn_b = conn.txn_id;
        storage.set_current_txn(txn_b);
        let root_before = txn.clustered_root(TABLE_ID);
        let new_root =
            clustered_tree::insert(&storage, root_before, key_b, &row_header(txn_b), &data_b)
                .unwrap();
        let image = ClusteredRowImage::new(new_root, row_header(txn_b), &data_b);
        txn.record_clustered_insert(&mut conn, TABLE_ID, key_b, &image)
            .unwrap();
        txn.commit_durable(conn, &storage).unwrap();
        storage.set_current_txn(0);
    }

    storage.simulate_power_loss();

    let (mgr, _result) = TxnManager::open_with_recovery(&mut storage, &wal_path).unwrap();
    let root = mgr.clustered_root(TABLE_ID);
    let ra = clustered_tree::lookup_physical(&storage, root, key_a)
        .unwrap()
        .expect("checkpointed row A must survive (from the main file)");
    assert_eq!(ra.row_data, data_a);
    let rb = clustered_tree::lookup_physical(&storage, root, key_b)
        .unwrap()
        .expect("post-checkpoint row B must survive (redone from frames)");
    assert_eq!(rb.row_data, data_b);
}

// ── T7 — randomized soak: committed survives, uncommitted is undone ──────────

/// Per seed: a committed batch of random-key clustered inserts (arbitrary tree
/// shapes / 50-50 splits) plus an uncommitted batch of distinct keys, then a
/// power loss. Recovery must restore exactly the committed batch (vs an oracle)
/// and undo the uncommitted batch. Bounded + deterministic per seed.
#[test]
fn t7_randomized_insert_soak_committed_survives_uncommitted_undone() {
    for seed in 0u64..40 {
        let dir = tempdir().expect("tempdir");
        let wal_path = dir.path().join("t7.wal");
        let mut storage = FaultInjectionStorage::new();
        storage.enable_redo_log(&dir.path().join("t7.wf")).unwrap();

        // Deterministic PRNG (LCG) seeded per iteration.
        let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as u32) % 100_000
        };

        let txn = TxnManager::create(&wal_path).unwrap();
        let mut oracle: std::collections::BTreeMap<u32, Vec<u8>> =
            std::collections::BTreeMap::new();

        // Committed batch.
        {
            let mut conn = txn.begin().unwrap();
            storage.set_current_txn(conn.txn_id);
            let mut root: Option<u64> = None;
            let m = 20 + (seed % 60) as usize;
            for _ in 0..m {
                let key = next();
                if oracle.contains_key(&key) {
                    continue; // distinct keys only (a dup would be UniqueViolation)
                }
                let kb = key.to_be_bytes();
                let data = vec![(key % 251) as u8; 64 + (key % 200) as usize];
                let new_root =
                    clustered_tree::insert(&storage, root, &kb, &row_header(conn.txn_id), &data)
                        .unwrap();
                root = Some(new_root);
                let image = ClusteredRowImage::new(new_root, row_header(conn.txn_id), &data);
                txn.record_clustered_insert(&mut conn, TABLE_ID, &kb, &image)
                    .unwrap();
                oracle.insert(key, data);
            }
            txn.commit_durable(conn, &storage).unwrap();
            storage.set_current_txn(0);
        }

        // Uncommitted batch — distinct keys, never committed.
        let mut uncommitted: Vec<u32> = Vec::new();
        {
            let mut conn = txn.begin().unwrap();
            storage.set_current_txn(conn.txn_id);
            let mut root = txn.clustered_root(TABLE_ID);
            for _ in 0..(5 + (seed % 15) as usize) {
                let key = next();
                if oracle.contains_key(&key) || uncommitted.contains(&key) {
                    continue;
                }
                let kb = key.to_be_bytes();
                let data = vec![0xEEu8; 80];
                let new_root =
                    clustered_tree::insert(&storage, root, &kb, &row_header(conn.txn_id), &data)
                        .unwrap();
                root = Some(new_root);
                let image = ClusteredRowImage::new(new_root, row_header(conn.txn_id), &data);
                txn.record_clustered_insert(&mut conn, TABLE_ID, &kb, &image)
                    .unwrap();
                uncommitted.push(key);
            }
            // NO commit_durable.
            storage.set_current_txn(0);
            drop(conn);
        }

        storage.simulate_power_loss();

        let (mgr, _result) = TxnManager::open_with_recovery(&mut storage, &wal_path).unwrap();
        let root = mgr.clustered_root(TABLE_ID);

        for (k, v) in &oracle {
            let row = clustered_tree::lookup_physical(&storage, root, &k.to_be_bytes())
                .unwrap()
                .unwrap_or_else(|| panic!("seed {seed}: committed key {k} lost after recovery"));
            assert_eq!(
                &row.row_data, v,
                "seed {seed}: committed key {k} value mismatch"
            );
        }
        for k in &uncommitted {
            let found = matches!(
                clustered_tree::lookup_physical(&storage, root, &k.to_be_bytes()),
                Ok(Some(_))
            );
            assert!(!found, "seed {seed}: uncommitted key {k} survived recovery");
        }
    }
}
