//! 6f synchronous back-pressure: a frame-only commit keeps the frame log bounded
//! even with NO background checkpointer (the robustness net for Lever 2 / Task 1).

use axiomdb_storage::{MmapStorage, Page, PageType, StorageEngine};
use axiomdb_wal::TxnManager;

#[test]
fn commit_back_pressure_bounds_the_frame_log() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bp.db");
    let wal = dir.path().join("bp.wal");

    let mut storage = MmapStorage::create(&db).unwrap();
    storage.enable_frame_only_redo(&db).unwrap();
    let hard: u64 = 64 * 1024; // ~4 16-KB frames
    storage.set_checkpoint_hard_bytes(hard);

    let txn = TxnManager::create(&wal).unwrap();
    let pid = storage.alloc_page(PageType::Data).unwrap();

    // 30 committed writes to the same page. Without back-pressure the frame log
    // would grow to ~30 frames (~480 KB); with it, every commit that crosses the
    // hard cap checkpoints + recycles inline, so the log stays bounded.
    for i in 0..30u8 {
        let conn = txn.begin().unwrap();
        storage.set_current_txn(conn.txn_id);
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = i;
        p.update_checksum();
        storage.write_page(pid, &p).unwrap();
        storage.set_current_txn(0);
        txn.commit_durable(conn, &storage).unwrap();

        // After every commit the log is bounded: it either never reached the cap,
        // or this commit's back-pressure checkpointed + recycled it.
        assert!(
            storage.frame_log_durable_len() <= hard,
            "iter {i}: frame log {} exceeded hard cap {hard} — back-pressure did not fire",
            storage.frame_log_durable_len()
        );
    }

    // The latest committed value survives (served from the frame, or from the main
    // file after a back-pressure checkpoint recycled the log).
    assert_eq!(storage.read_page(pid).unwrap().body()[0], 29);
}

#[test]
fn no_back_pressure_when_hard_cap_unset() {
    // Default hard cap is u64::MAX → back-pressure never fires; the log grows.
    // (Confirms the trigger is opt-in: nothing changes until the open path sets it.)
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("nb.db");
    let wal = dir.path().join("nb.wal");

    let mut storage = MmapStorage::create(&db).unwrap();
    storage.enable_frame_only_redo(&db).unwrap();
    // No set_checkpoint_hard_bytes → stays u64::MAX.
    let txn = TxnManager::create(&wal).unwrap();
    let pid = storage.alloc_page(PageType::Data).unwrap();

    let mut last = 0;
    for i in 0..8u8 {
        let conn = txn.begin().unwrap();
        storage.set_current_txn(conn.txn_id);
        let mut p = Page::new(PageType::Data, pid);
        p.body_mut()[0] = i;
        p.update_checksum();
        storage.write_page(pid, &p).unwrap();
        storage.set_current_txn(0);
        txn.commit_durable(conn, &storage).unwrap();
        last = storage.frame_log_durable_len();
    }
    // Without a cap the log kept growing well past a single frame.
    assert!(last > 64 * 1024, "log should grow unbounded with no cap, got {last}");
}
