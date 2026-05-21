//! Project B (REDO recovery) crash tests, driven by `FaultInjectionStorage`
//! (power-loss simulation: only `flush`'d data survives a `simulate_power_loss`).
//!
//! **T0 proves the latent durability hole.** A COMMITTED heap insert whose data
//! page was not flushed is LOST after a power loss, because crash recovery is
//! UNDO-only today (it rolls back uncommitted txns; it never REDOes committed ones).
//! T0 is `#[ignore]`d because it is **RED until the REDO replay lands (subphase 4)**,
//! at which point it must pass. Run it explicitly to confirm the hole:
//!
//! ```text
//! cargo nextest run -p axiomdb-wal --run-ignored all t0_committed_heap_insert_survives_power_loss
//! ```

use axiomdb_storage::{
    fault_injection::FaultInjectionStorage,
    heap::{insert_tuple, read_tuple_image},
    Page, PageType, StorageEngine,
};
use axiomdb_wal::TxnManager;
use tempfile::tempdir;

const TABLE_ID: u32 = 7;

#[test]
#[ignore = "RED until REDO replay (project B subphase 4) — proves the latent durability hole"]
fn t0_committed_heap_insert_survives_power_loss() {
    let dir = tempdir().expect("tempdir");
    let wal_path = dir.path().join("t0.wal");

    let mut storage = FaultInjectionStorage::new();

    // Durable baseline: an empty heap page that has reached "disk".
    let page_id = storage.alloc_page(PageType::Data).unwrap();
    storage
        .write_page(page_id, &Page::new(PageType::Data, page_id))
        .unwrap();
    storage.flush().unwrap();

    // Insert a row + commit. The page write stays VOLATILE (no flush after it);
    // only the WAL is fsync'd at commit. Models a non-root-changing insert.
    let row = b"committed-row-survives".to_vec();
    let slot_id;
    {
        let txn = TxnManager::create(&wal_path).unwrap();
        let mut conn = txn.begin().unwrap();

        let mut page = Page::from_bytes(*storage.read_page(page_id).unwrap().as_bytes()).unwrap();
        slot_id = insert_tuple(&mut page, &row, conn.txn_id).unwrap();
        page.update_checksum();
        storage.write_page(page_id, &page).unwrap();

        // Heap Insert carries the row bytes + (page_id, slot_id) — the redo data.
        txn.record_insert(&mut conn, TABLE_ID, b"k0", &row, page_id, slot_id)
            .unwrap();
        txn.commit(conn).unwrap(); // WAL fsync — the commit is durable in the WAL.
    }

    // Power loss: the un-flushed page write (the committed row) is gone.
    storage.simulate_power_loss();

    // Precondition: the crash really lost the row (durable page is still empty).
    let crashed = Page::from_bytes(*storage.read_page(page_id).unwrap().as_bytes()).unwrap();
    assert!(
        !matches!(read_tuple_image(&crashed, slot_id), Ok(Some(_))),
        "precondition: the un-flushed committed row must be gone after the crash"
    );

    // Recover from the (durable) WAL.
    let (_txn, _result) = TxnManager::open_with_recovery(&mut storage, &wal_path).unwrap();

    // The committed row MUST be restored by REDO. Today recovery is UNDO-only, so the
    // page is still empty (read returns Err(InvalidSlot)/None) → this FAILS, proving
    // the hole. Subphase 4 (REDO replay) makes it pass.
    let recovered = Page::from_bytes(*storage.read_page(page_id).unwrap().as_bytes()).unwrap();
    let recovered_row = read_tuple_image(&recovered, slot_id).ok().flatten();
    assert_eq!(
        recovered_row.as_deref(),
        Some(&row[..]),
        "committed heap insert must survive a power loss (REDO must restore it); \
         recovery returned {recovered_row:?} — UNDO-only recovery loses committed-but-unflushed data"
    );
}
