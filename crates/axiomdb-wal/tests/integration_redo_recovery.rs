//! Project B (REDO recovery) crash tests, driven by `FaultInjectionStorage`
//! (power-loss simulation: only `flush`'d data survives a `simulate_power_loss`).
//!
//! **T0 — committed insert survives power loss (the REDO proof).** A COMMITTED heap
//! insert whose data page was never flushed is restored on recovery: the data-page
//! write stays volatile (lost on the crash) while the page-frame redo log is fsync'd
//! at commit (`commit_durable`), so `recover` REDOes the committed frame. This was the
//! latent durability hole back when recovery was UNDO-only; subphase 5 (REDO replay)
//! closes it, so T0 is GREEN.

use axiomdb_storage::{
    fault_injection::FaultInjectionStorage,
    heap::{insert_tuple, read_tuple, read_tuple_image},
    Page, PageType, StorageEngine,
};
use axiomdb_wal::{CrashRecovery, TxnManager};
use tempfile::tempdir;

const TABLE_ID: u32 = 7;

#[test]
fn t0_committed_heap_insert_survives_power_loss() {
    let dir = tempdir().expect("tempdir");
    let wal_path = dir.path().join("t0.wal");

    let mut storage = FaultInjectionStorage::new();
    // Enable the durable page-frame redo log BEFORE any write (project B subphase 5).
    storage.enable_redo_log(&dir.path().join("t0.wf")).unwrap();

    // Durable baseline: an empty heap page that has reached "disk".
    let page_id = storage.alloc_page(PageType::Data).unwrap();
    storage
        .write_page(page_id, &Page::new(PageType::Data, page_id))
        .unwrap();
    storage.flush().unwrap();

    // Insert a row + commit. The data-page write stays VOLATILE (no flush after it);
    // only the redo log + WAL are fsync'd at commit. Models a non-root-changing insert.
    let row = b"committed-row-survives".to_vec();
    let slot_id;
    {
        let txn = TxnManager::create(&wal_path).unwrap();
        let mut conn = txn.begin().unwrap();
        // Stamp the writing txn into the page frame so REDO knows it committed.
        storage.set_current_txn(conn.txn_id);

        let mut page = Page::from_bytes(*storage.read_page(page_id).unwrap().as_bytes()).unwrap();
        slot_id = insert_tuple(&mut page, &row, conn.txn_id).unwrap();
        page.update_checksum();
        storage.write_page(page_id, &page).unwrap();

        // Heap Insert carries the row bytes + (page_id, slot_id) — the redo data.
        txn.record_insert(&mut conn, TABLE_ID, b"k0", &row, page_id, slot_id)
            .unwrap();
        // commit_durable fsyncs the frame log BEFORE the WAL Commit (write-ahead order).
        txn.commit_durable(conn, &storage).unwrap();
        storage.set_current_txn(0);
    }

    // Power loss: the un-flushed data-page write is gone, but the fsync'd frame survives.
    storage.simulate_power_loss();

    // Precondition: the crash really lost the row (the durable page is still empty).
    let crashed = Page::from_bytes(*storage.read_page(page_id).unwrap().as_bytes()).unwrap();
    assert!(
        !matches!(read_tuple_image(&crashed, slot_id), Ok(Some(_))),
        "precondition: the un-flushed committed row must be gone after the crash"
    );

    // Recover: UNDO (nothing uncommitted) then REDO the committed frame.
    {
        let (_txn, result) = TxnManager::open_with_recovery(&mut storage, &wal_path).unwrap();
        assert_eq!(
            result.redone_pages, 1,
            "REDO must restore exactly the committed page"
        );
    }

    // The committed row is restored by REDO (read_tuple returns (header, data)).
    let recovered = Page::from_bytes(*storage.read_page(page_id).unwrap().as_bytes()).unwrap();
    let (_hdr, recovered_row) = read_tuple(&recovered, slot_id)
        .expect("read tuple")
        .expect("committed row must be present after REDO");
    assert_eq!(
        recovered_row, row,
        "committed heap insert must survive a power loss (REDO must restore its bytes)"
    );

    // Idempotence — a crash *during* recovery: a second REDO pass re-applies nothing
    // (the pageLSN guard holds since frame.lsn == page.lsn now).
    let result2 = CrashRecovery::recover(&mut storage, &wal_path).unwrap();
    assert_eq!(
        result2.redone_pages, 0,
        "a second recovery is a no-op (pageLSN guard)"
    );
}
