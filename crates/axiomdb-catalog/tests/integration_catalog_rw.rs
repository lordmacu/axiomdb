//! Integration tests for CatalogReader and CatalogWriter (subphase 3.12).
//!
//! Tests cover:
//! - Basic create + read roundtrips
//! - MVCC snapshot isolation (pre-commit vs post-commit visibility)
//! - Rollback semantics (rows invisible after rollback)
//! - Multi-page heap chain growth
//! - Cascade deletion (delete_table removes columns and indexes)
//! - Sequence persistence across MmapStorage reopen

use axiomdb_catalog::{
    bootstrap::CatalogBootstrap,
    reader::CatalogReader,
    schema::{ColumnDef, ColumnType, IndexDef},
    writer::CatalogWriter,
};
use axiomdb_core::TransactionSnapshot;
use axiomdb_storage::{MemoryStorage, MmapStorage, StorageEngine};
use axiomdb_wal::TxnManager;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Creates a MemoryStorage with the catalog bootstrapped and a TxnManager.
fn setup() -> (MemoryStorage, TxnManager) {
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("test.wal");
    let txn = TxnManager::create(&wal_path).unwrap();
    // Leak the tempdir so the WAL file stays alive for the test.
    std::mem::forget(dir);
    (storage, txn)
}

/// Committed snapshot: sees everything committed up to now.
fn committed_snap(txn: &TxnManager) -> TransactionSnapshot {
    txn.snapshot()
}

// ── Basic CRUD ────────────────────────────────────────────────────────────────

#[test]
fn test_create_and_get_table() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let table_id = {
        let mut writer = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        writer.create_table("public", "users").unwrap()
    };
    txn.commit().unwrap();

    // Post-commit snapshot sees the table.
    let snap = committed_snap(&txn);
    let mut reader = CatalogReader::new(&mut storage, snap).unwrap();
    let found = reader.get_table("public", "users").unwrap();
    assert!(found.is_some());
    let def = found.unwrap();
    assert_eq!(def.id, table_id);
    assert_eq!(def.schema_name, "public");
    assert_eq!(def.table_name, "users");
}

#[test]
fn test_create_multiple_tables_distinct_ids() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let (id1, id2) = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        let a = w.create_table("public", "orders").unwrap();
        let b = w.create_table("public", "products").unwrap();
        (a, b)
    };
    txn.commit().unwrap();

    assert_ne!(id1, id2, "each create_table must allocate a distinct ID");

    let snap = committed_snap(&txn);
    let mut reader = CatalogReader::new(&mut storage, snap).unwrap();
    assert!(reader.get_table("public", "orders").unwrap().is_some());
    assert!(reader.get_table("public", "products").unwrap().is_some());
}

#[test]
fn test_get_table_by_id() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let table_id = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.create_table("myschema", "items").unwrap()
    };
    txn.commit().unwrap();

    let snap = committed_snap(&txn);
    let mut reader = CatalogReader::new(&mut storage, snap).unwrap();
    let found = reader.get_table_by_id(table_id).unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().table_name, "items");
}

#[test]
fn test_get_table_not_found_returns_none() {
    let (mut storage, txn) = setup();
    let snap = committed_snap(&txn);
    let mut reader = CatalogReader::new(&mut storage, snap).unwrap();
    assert!(reader.get_table("public", "nonexistent").unwrap().is_none());
    assert!(reader.get_table_by_id(9999).unwrap().is_none());
}

// ── Columns ───────────────────────────────────────────────────────────────────

#[test]
fn test_create_columns_list_ordered_by_col_idx() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let table_id = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        let tid = w.create_table("public", "users").unwrap();
        // Insert in reverse col_idx order to verify sort.
        w.create_column(ColumnDef {
            table_id: tid,
            col_idx: 2,
            name: "email".to_string(),
            col_type: ColumnType::Text,
            nullable: true,
            auto_increment: false,
        })
        .unwrap();
        w.create_column(ColumnDef {
            table_id: tid,
            col_idx: 0,
            name: "id".to_string(),
            col_type: ColumnType::BigInt,
            nullable: false,
            auto_increment: false,
        })
        .unwrap();
        w.create_column(ColumnDef {
            table_id: tid,
            col_idx: 1,
            name: "username".to_string(),
            col_type: ColumnType::Text,
            nullable: false,
            auto_increment: false,
        })
        .unwrap();
        tid
    };
    txn.commit().unwrap();

    let snap = committed_snap(&txn);
    let mut reader = CatalogReader::new(&mut storage, snap).unwrap();
    let cols = reader.list_columns(table_id).unwrap();

    assert_eq!(cols.len(), 3);
    // Must be sorted by col_idx.
    assert_eq!(cols[0].col_idx, 0);
    assert_eq!(cols[0].name, "id");
    assert_eq!(cols[1].col_idx, 1);
    assert_eq!(cols[1].name, "username");
    assert_eq!(cols[2].col_idx, 2);
    assert_eq!(cols[2].name, "email");
    assert!(cols[2].nullable);
}

#[test]
fn test_list_columns_empty_for_unknown_table() {
    let (mut storage, txn) = setup();
    let snap = committed_snap(&txn);
    let mut reader = CatalogReader::new(&mut storage, snap).unwrap();
    let cols = reader.list_columns(9999).unwrap();
    assert!(cols.is_empty());
}

// ── Indexes ───────────────────────────────────────────────────────────────────

#[test]
fn test_create_and_list_index() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let (table_id, index_id) = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        let tid = w.create_table("public", "users").unwrap();
        let iid = w
            .create_index(IndexDef {
                index_id: 0, // ignored — writer allocates
                table_id: tid,
                name: "users_pkey".to_string(),
                root_page_id: 42,
                is_unique: true,
                is_primary: true,
                columns: vec![],
                predicate: None,
                fillfactor: 90,
                is_fk_index: false,
            })
            .unwrap();
        (tid, iid)
    };
    txn.commit().unwrap();

    let snap = committed_snap(&txn);
    let mut reader = CatalogReader::new(&mut storage, snap).unwrap();
    let indexes = reader.list_indexes(table_id).unwrap();
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].index_id, index_id);
    assert_eq!(indexes[0].name, "users_pkey");
    assert!(indexes[0].is_primary);
    assert!(indexes[0].is_unique);
    assert_eq!(indexes[0].root_page_id, 42);
}

#[test]
fn test_index_ids_are_unique_across_tables() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let (iid1, iid2) = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        let t1 = w.create_table("public", "a").unwrap();
        let t2 = w.create_table("public", "b").unwrap();
        let i1 = w
            .create_index(IndexDef {
                index_id: 0,
                table_id: t1,
                name: "idx_a".to_string(),
                root_page_id: 10,
                is_unique: false,
                is_primary: false,
                columns: vec![],
                predicate: None,
                fillfactor: 90,
                is_fk_index: false,
            })
            .unwrap();
        let i2 = w
            .create_index(IndexDef {
                index_id: 0,
                table_id: t2,
                name: "idx_b".to_string(),
                root_page_id: 11,
                is_unique: false,
                is_primary: false,
                columns: vec![],
                predicate: None,
                fillfactor: 90,
                is_fk_index: false,
            })
            .unwrap();
        (i1, i2)
    };
    txn.commit().unwrap();

    assert_ne!(iid1, iid2);
}

// ── Delete operations ─────────────────────────────────────────────────────────

#[test]
fn test_delete_index() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let (table_id, index_id) = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        let tid = w.create_table("public", "t").unwrap();
        let iid = w
            .create_index(IndexDef {
                index_id: 0,
                table_id: tid,
                name: "idx".to_string(),
                root_page_id: 5,
                is_unique: false,
                is_primary: false,
                columns: vec![],
                predicate: None,
                fillfactor: 90,
                is_fk_index: false,
            })
            .unwrap();
        (tid, iid)
    };
    txn.commit().unwrap();

    // Verify index exists.
    let snap1 = committed_snap(&txn);
    let mut reader1 = CatalogReader::new(&mut storage, snap1).unwrap();
    assert_eq!(reader1.list_indexes(table_id).unwrap().len(), 1);

    // Delete the index.
    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.delete_index(index_id).unwrap();
    }
    txn.commit().unwrap();

    // Post-delete snapshot sees no indexes.
    let snap2 = committed_snap(&txn);
    let mut reader2 = CatalogReader::new(&mut storage, snap2).unwrap();
    assert!(reader2.list_indexes(table_id).unwrap().is_empty());
}

#[test]
fn test_delete_table_cascades_columns_and_indexes() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let table_id = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        let tid = w.create_table("public", "target").unwrap();
        w.create_column(ColumnDef {
            table_id: tid,
            col_idx: 0,
            name: "id".to_string(),
            col_type: ColumnType::Int,
            nullable: false,
            auto_increment: false,
        })
        .unwrap();
        w.create_index(IndexDef {
            index_id: 0,
            table_id: tid,
            name: "target_pkey".to_string(),
            root_page_id: 99,
            is_unique: true,
            is_primary: true,
            columns: vec![],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
        })
        .unwrap();
        tid
    };
    txn.commit().unwrap();

    // Confirm everything exists.
    let snap1 = committed_snap(&txn);
    let mut r1 = CatalogReader::new(&mut storage, snap1).unwrap();
    assert!(r1.get_table_by_id(table_id).unwrap().is_some());
    assert_eq!(r1.list_columns(table_id).unwrap().len(), 1);
    assert_eq!(r1.list_indexes(table_id).unwrap().len(), 1);

    // Drop the table.
    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.delete_table(table_id).unwrap();
    }
    txn.commit().unwrap();

    // Nothing visible after drop.
    let snap2 = committed_snap(&txn);
    let mut r2 = CatalogReader::new(&mut storage, snap2).unwrap();
    assert!(r2.get_table_by_id(table_id).unwrap().is_none());
    assert!(r2.list_columns(table_id).unwrap().is_empty());
    assert!(r2.list_indexes(table_id).unwrap().is_empty());
}

#[test]
fn test_delete_nonexistent_index_returns_error() {
    let (mut storage, mut txn) = setup();
    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        let err = w.delete_index(9999).unwrap_err();
        assert!(
            matches!(
                err,
                axiomdb_core::DbError::CatalogIndexNotFound { index_id: 9999 }
            ),
            "expected CatalogIndexNotFound, got: {err}"
        );
    }
    txn.rollback(&mut storage).unwrap();
}

// ── MVCC snapshot isolation ───────────────────────────────────────────────────

#[test]
fn test_snapshot_before_commit_does_not_see_new_table() {
    let (mut storage, mut txn) = setup();

    // Capture a snapshot BEFORE the transaction commits.
    let snap_before = committed_snap(&txn);

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.create_table("public", "invisible").unwrap();
    }
    txn.commit().unwrap();

    // Old snapshot must not see it.
    let mut reader_before = CatalogReader::new(&mut storage, snap_before).unwrap();
    assert!(reader_before
        .get_table("public", "invisible")
        .unwrap()
        .is_none());

    // New snapshot sees it.
    let snap_after = committed_snap(&txn);
    let mut reader_after = CatalogReader::new(&mut storage, snap_after).unwrap();
    assert!(reader_after
        .get_table("public", "invisible")
        .unwrap()
        .is_some());
}

#[test]
fn test_snapshot_before_delete_still_sees_row() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let table_id = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.create_table("public", "mortal").unwrap()
    };
    txn.commit().unwrap();

    // Capture snapshot after creation, before deletion.
    let snap_before_delete = committed_snap(&txn);

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.delete_table(table_id).unwrap();
    }
    txn.commit().unwrap();

    // Old snapshot still sees the table.
    let mut reader_old = CatalogReader::new(&mut storage, snap_before_delete).unwrap();
    assert!(reader_old.get_table_by_id(table_id).unwrap().is_some());

    // New snapshot does not.
    let snap_after_delete = committed_snap(&txn);
    let mut reader_new = CatalogReader::new(&mut storage, snap_after_delete).unwrap();
    assert!(reader_new.get_table_by_id(table_id).unwrap().is_none());
}

// ── Rollback ──────────────────────────────────────────────────────────────────

#[test]
fn test_rollback_create_table_row_invisible() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.create_table("public", "ghost").unwrap();
    }
    txn.rollback(&mut storage).unwrap();

    // After rollback, the row must be invisible to any committed snapshot.
    let snap = committed_snap(&txn);
    let mut reader = CatalogReader::new(&mut storage, snap).unwrap();
    assert!(reader.get_table("public", "ghost").unwrap().is_none());
}

#[test]
fn test_rollback_create_does_not_consume_id_permanently() {
    // After rollback, the sequence counter still advanced (no retry mechanism).
    // The next successful create_table must get the ID after the rolled-back one.
    let (mut storage, mut txn) = setup();

    // First transaction — rolled back.
    txn.begin().unwrap();
    let rolled_back_id = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.create_table("public", "ghost").unwrap()
    };
    txn.rollback(&mut storage).unwrap();

    // Second transaction — committed.
    txn.begin().unwrap();
    let committed_id = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.create_table("public", "real").unwrap()
    };
    txn.commit().unwrap();

    // IDs must be distinct (rollback does not rewind the sequence).
    assert_ne!(rolled_back_id, committed_id);
    // The committed one is visible.
    let snap = committed_snap(&txn);
    let mut reader = CatalogReader::new(&mut storage, snap).unwrap();
    assert!(reader.get_table("public", "real").unwrap().is_some());
}

// ── Multi-page heap chain ─────────────────────────────────────────────────────

#[test]
fn test_multi_page_chain_insert_and_scan() {
    let (mut storage, mut txn) = setup();

    // Insert enough tables to overflow the root page.
    // Each TableRow is roughly 4 + 1 + 6 + 1 + ~10 = ~22 bytes of data.
    // With RowHeader (24B) + SlotEntry (4B) = 50B per row.
    // A 16KB page body (16320B) fits ~326 such rows. We insert 400 to force overflow.
    let count = 400usize;
    let mut expected_ids = Vec::with_capacity(count);

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        for i in 0..count {
            let name = format!("table_{i:04}");
            let id = w.create_table("stress", &name).unwrap();
            expected_ids.push(id);
        }
    }
    txn.commit().unwrap();

    // All rows must be visible.
    let snap = committed_snap(&txn);
    let mut reader = CatalogReader::new(&mut storage, snap).unwrap();
    let tables = reader.list_tables("stress").unwrap();
    assert_eq!(
        tables.len(),
        count,
        "all rows must be visible across chain pages"
    );

    // IDs must all be distinct and match what was returned at insert time.
    let seen_ids: std::collections::HashSet<u32> = tables.iter().map(|t| t.id).collect();
    assert_eq!(seen_ids.len(), count, "all IDs must be distinct");
    for id in &expected_ids {
        assert!(seen_ids.contains(id), "ID {id} missing from scan result");
    }
}

// ── Sequence persistence (MmapStorage) ───────────────────────────────────────

#[test]
fn test_sequence_persistence_across_reopen() {
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("catalog.db");
    let wal_path = db_dir.path().join("catalog.wal");

    // Session 1: create a table, commit, get its ID.
    let id_session1 = {
        let mut storage = MmapStorage::create(&db_path).unwrap();
        CatalogBootstrap::init(&mut storage).unwrap();
        let mut txn = TxnManager::create(&wal_path).unwrap();
        txn.begin().unwrap();
        let id = {
            let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
            w.create_table("public", "session1_table").unwrap()
        };
        txn.commit().unwrap();
        storage.flush().unwrap();
        id
    };

    // Session 2: reopen, create another table — ID must be > session1's ID.
    let id_session2 = {
        let mut storage = MmapStorage::open(&db_path).unwrap();
        let mut txn = TxnManager::open(&wal_path).unwrap();
        txn.begin().unwrap();
        let id = {
            let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
            w.create_table("public", "session2_table").unwrap()
        };
        txn.commit().unwrap();
        storage.flush().unwrap();
        id
    };

    assert!(
        id_session2 > id_session1,
        "session 2 ID {id_session2} must be greater than session 1 ID {id_session1}"
    );

    // Session 3: read both tables.
    {
        let mut storage = MmapStorage::open(&db_path).unwrap();
        let txn = TxnManager::open(&wal_path).unwrap();
        let snap = txn.snapshot();
        let mut reader = CatalogReader::new(&mut storage, snap).unwrap();
        assert!(reader
            .get_table("public", "session1_table")
            .unwrap()
            .is_some());
        assert!(reader
            .get_table("public", "session2_table")
            .unwrap()
            .is_some());
    }
}

#[test]
fn test_catalog_not_initialized_returns_error() {
    let mut storage = MemoryStorage::new();
    // No CatalogBootstrap::init() called.

    let wal_dir = tempfile::tempdir().unwrap();
    let wal_path = wal_dir.path().join("test.wal");
    let mut txn = TxnManager::create(&wal_path).unwrap();
    txn.begin().unwrap();

    let err = CatalogWriter::new(&mut storage, &mut txn)
        .err()
        .expect("expected CatalogNotInitialized error");
    assert!(
        matches!(err, axiomdb_core::DbError::CatalogNotInitialized),
        "expected CatalogNotInitialized, got: {err}"
    );

    txn.rollback(&mut storage).unwrap();
}
