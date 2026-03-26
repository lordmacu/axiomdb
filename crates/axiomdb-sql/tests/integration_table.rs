//! Integration tests for `TableEngine`.
//!
//! All tests use `MemoryStorage` (no files) and a `TxnManager` with a real WAL
//! file in a tempdir to exercise the full insert → WAL log → scan path.

use axiomdb_catalog::{
    schema::{ColumnDef, ColumnType, TableDef},
    CatalogBootstrap, CatalogWriter,
};
use axiomdb_core::RecordId;
use axiomdb_sql::TableEngine;
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── Test helpers ──────────────────────────────────────────────────────────────

fn col(idx: u16, name: &str, col_type: ColumnType) -> ColumnDef {
    ColumnDef {
        table_id: 0, // overwritten by create_table_helper
        col_idx: idx,
        name: name.to_string(),
        col_type,
        nullable: true,
        auto_increment: false,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn test_table_engine_empty_scan() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("w.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let mut txn = TxnManager::create(&wal_path).unwrap();

    let columns = vec![col(0, "id", ColumnType::Int)];
    let table_def = create_table_helper(&mut storage, &mut txn, "t", &columns);

    let snap = txn.snapshot();
    let rows = TableEngine::scan_table(&mut storage, &table_def, &columns, snap, None).unwrap();
    assert!(rows.is_empty(), "fresh table must have 0 rows");
}

#[test]
fn test_table_engine_insert_and_scan() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("w.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let mut txn = TxnManager::create(&wal_path).unwrap();

    let columns = vec![
        col(0, "id", ColumnType::Int),
        col(1, "name", ColumnType::Text),
    ];
    let table_def = create_table_helper(&mut storage, &mut txn, "users", &columns);

    txn.begin().unwrap();
    TableEngine::insert_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        vec![Value::Int(1), Value::Text("alice".into())],
    )
    .unwrap();
    TableEngine::insert_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        vec![Value::Int(2), Value::Text("bob".into())],
    )
    .unwrap();
    TableEngine::insert_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        vec![Value::Int(3), Value::Text("carol".into())],
    )
    .unwrap();
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let rows = TableEngine::scan_table(&mut storage, &table_def, &columns, snap, None).unwrap();
    assert_eq!(rows.len(), 3);

    let names: Vec<&Value> = rows.iter().map(|(_, v)| &v[1]).collect();
    assert!(names.contains(&&Value::Text("alice".into())));
    assert!(names.contains(&&Value::Text("bob".into())));
    assert!(names.contains(&&Value::Text("carol".into())));
}

#[test]
fn test_table_engine_insert_mvcc_visibility() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("w.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let mut txn = TxnManager::create(&wal_path).unwrap();

    let columns = vec![col(0, "id", ColumnType::Int)];
    let table_def = create_table_helper(&mut storage, &mut txn, "t", &columns);

    // Snapshot taken BEFORE the insert.
    let snap_before = txn.snapshot();

    txn.begin().unwrap();
    TableEngine::insert_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        vec![Value::Int(42)],
    )
    .unwrap();
    txn.commit().unwrap();

    // Old snapshot sees 0 rows.
    let rows_old =
        TableEngine::scan_table(&mut storage, &table_def, &columns, snap_before, None).unwrap();
    assert_eq!(rows_old.len(), 0, "snapshot before insert must see 0 rows");

    // Fresh snapshot sees 1 row.
    let snap_after = txn.snapshot();
    let rows_new =
        TableEngine::scan_table(&mut storage, &table_def, &columns, snap_after, None).unwrap();
    assert_eq!(rows_new.len(), 1);
    assert_eq!(rows_new[0].1[0], Value::Int(42));
}

#[test]
fn test_table_engine_delete_row() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("w.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let mut txn = TxnManager::create(&wal_path).unwrap();

    let columns = vec![col(0, "id", ColumnType::Int)];
    let table_def = create_table_helper(&mut storage, &mut txn, "t", &columns);

    // Insert 2 rows.
    txn.begin().unwrap();
    let rid1 = TableEngine::insert_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        vec![Value::Int(1)],
    )
    .unwrap();
    TableEngine::insert_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        vec![Value::Int(2)],
    )
    .unwrap();
    txn.commit().unwrap();

    // Delete row 1.
    txn.begin().unwrap();
    TableEngine::delete_row(&mut storage, &mut txn, &table_def, rid1).unwrap();
    txn.commit().unwrap();

    // Scan sees only row 2.
    let snap = txn.snapshot();
    let rows = TableEngine::scan_table(&mut storage, &table_def, &columns, snap, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1[0], Value::Int(2));
}

#[test]
fn test_table_engine_delete_invalid_slot_error() {
    // Deleting with an out-of-range slot_id must return InvalidSlot.
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("w.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let mut txn = TxnManager::create(&wal_path).unwrap();

    let columns = vec![col(0, "id", ColumnType::Int)];
    let table_def = create_table_helper(&mut storage, &mut txn, "t", &columns);

    txn.begin().unwrap();
    let rid = TableEngine::insert_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        vec![Value::Int(1)],
    )
    .unwrap();
    txn.commit().unwrap();

    // Use a slot_id that is guaranteed to be out of range.
    let bad_rid = RecordId {
        page_id: rid.page_id,
        slot_id: 9999,
    };
    txn.begin().unwrap();
    let err = TableEngine::delete_row(&mut storage, &mut txn, &table_def, bad_rid).unwrap_err();
    txn.rollback(&mut storage).unwrap();
    assert!(
        matches!(err, axiomdb_core::error::DbError::InvalidSlot { .. }),
        "expected InvalidSlot for out-of-range slot, got {err:?}"
    );
}

#[test]
fn test_table_engine_update_row() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("w.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let mut txn = TxnManager::create(&wal_path).unwrap();

    let columns = vec![
        col(0, "id", ColumnType::Int),
        col(1, "age", ColumnType::Int),
    ];
    let table_def = create_table_helper(&mut storage, &mut txn, "t", &columns);

    txn.begin().unwrap();
    let old_rid = TableEngine::insert_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        vec![Value::Int(1), Value::Int(30)],
    )
    .unwrap();
    txn.commit().unwrap();

    // Update age from 30 to 31.
    txn.begin().unwrap();
    let new_rid = TableEngine::update_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        old_rid,
        vec![Value::Int(1), Value::Int(31)],
    )
    .unwrap();
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let rows = TableEngine::scan_table(&mut storage, &table_def, &columns, snap, None).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "should be exactly 1 visible row after update"
    );
    assert_eq!(rows[0].1[1], Value::Int(31), "age must be 31 after update");
    assert_eq!(rows[0].0, new_rid, "RecordId must match new location");
    assert_ne!(
        old_rid, new_rid,
        "update must produce a new RecordId (old slot is dead)"
    );
}

#[test]
fn test_table_engine_update_changes_record_id() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("w.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let mut txn = TxnManager::create(&wal_path).unwrap();

    let columns = vec![col(0, "v", ColumnType::Int)];
    let table_def = create_table_helper(&mut storage, &mut txn, "t", &columns);

    txn.begin().unwrap();
    let old_rid = TableEngine::insert_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        vec![Value::Int(10)],
    )
    .unwrap();
    txn.commit().unwrap();

    txn.begin().unwrap();
    let new_rid = TableEngine::update_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        old_rid,
        vec![Value::Int(20)],
    )
    .unwrap();
    txn.commit().unwrap();

    // The new RecordId is valid and the value was updated.
    let snap = txn.snapshot();
    let rows = TableEngine::scan_table(&mut storage, &table_def, &columns, snap, None).unwrap();
    let rids: Vec<RecordId> = rows.iter().map(|(r, _)| *r).collect();
    assert!(
        rids.contains(&new_rid),
        "new RecordId must be in the scan result"
    );
    assert!(
        !rids.contains(&old_rid),
        "old RecordId must be invisible after update"
    );
}

#[test]
fn test_table_engine_coercion_on_insert() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("w.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let mut txn = TxnManager::create(&wal_path).unwrap();

    let columns = vec![col(0, "age", ColumnType::Int)];
    let table_def = create_table_helper(&mut storage, &mut txn, "t", &columns);

    // Insert Text("42") into an INT column — coercion must convert it to Int(42).
    txn.begin().unwrap();
    TableEngine::insert_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        vec![Value::Text("42".into())],
    )
    .unwrap();
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let rows = TableEngine::scan_table(&mut storage, &table_def, &columns, snap, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].1[0],
        Value::Int(42),
        "Text('42') must be coerced to Int(42)"
    );
}

#[test]
fn test_table_engine_insert_outside_txn_error() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("w.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let mut txn = TxnManager::create(&wal_path).unwrap();

    let columns = vec![col(0, "id", ColumnType::Int)];
    let table_def = create_table_helper(&mut storage, &mut txn, "t", &columns);

    // No txn.begin() — must fail with NoActiveTransaction.
    let err = TableEngine::insert_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        vec![Value::Int(1)],
    )
    .unwrap_err();
    assert!(
        matches!(err, axiomdb_core::error::DbError::NoActiveTransaction),
        "expected NoActiveTransaction, got {err:?}"
    );
}

#[test]
fn test_table_engine_scan_respects_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("w.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let mut txn = TxnManager::create(&wal_path).unwrap();

    let columns = vec![col(0, "id", ColumnType::Int)];
    let table_def = create_table_helper(&mut storage, &mut txn, "t", &columns);

    // Insert row A.
    txn.begin().unwrap();
    TableEngine::insert_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        vec![Value::Int(1)],
    )
    .unwrap();
    txn.commit().unwrap();

    // Snapshot after A committed — sees 1 row.
    let snap_after_a = txn.snapshot();

    // Insert row B.
    txn.begin().unwrap();
    TableEngine::insert_row(
        &mut storage,
        &mut txn,
        &table_def,
        &columns,
        vec![Value::Int(2)],
    )
    .unwrap();
    txn.commit().unwrap();

    // snap_after_a must still see only row A.
    let rows =
        TableEngine::scan_table(&mut storage, &table_def, &columns, snap_after_a, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1[0], Value::Int(1));

    // Current snapshot sees both.
    let snap_current = txn.snapshot();
    let rows2 =
        TableEngine::scan_table(&mut storage, &table_def, &columns, snap_current, None).unwrap();
    assert_eq!(rows2.len(), 2);
}

#[test]
fn test_table_engine_chain_growth() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("w.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let mut txn = TxnManager::create(&wal_path).unwrap();

    // Use a BYTES column with large values to force chain growth faster.
    let columns = vec![col(0, "payload", ColumnType::Bytes)];
    let table_def = create_table_helper(&mut storage, &mut txn, "t", &columns);

    // Each row ≈ 4000 bytes of payload. A 16 KB page holds ~3–4 such rows.
    // Insert 20 rows to guarantee at least two pages in the chain.
    let big = vec![0xABu8; 4000];
    let n_rows = 20usize;

    txn.begin().unwrap();
    for _ in 0..n_rows {
        TableEngine::insert_row(
            &mut storage,
            &mut txn,
            &table_def,
            &columns,
            vec![Value::Bytes(big.clone())],
        )
        .unwrap();
    }
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let rows = TableEngine::scan_table(&mut storage, &table_def, &columns, snap, None).unwrap();
    assert_eq!(
        rows.len(),
        n_rows,
        "all {n_rows} rows must be visible after chain growth"
    );
}

// ── Helper used by multiple tests ─────────────────────────────────────────────

fn create_table_helper(
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    name: &str,
    columns: &[ColumnDef],
) -> TableDef {
    txn.begin().unwrap();
    let mut writer = CatalogWriter::new(storage, txn).unwrap();
    let table_id = writer.create_table("public", name).unwrap();
    for (i, col) in columns.iter().enumerate() {
        writer
            .create_column(ColumnDef {
                table_id,
                col_idx: i as u16,
                name: col.name.clone(),
                col_type: col.col_type,
                nullable: col.nullable,
                auto_increment: false,
            })
            .unwrap();
    }
    drop(writer);
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(storage, snap).unwrap();
    reader
        .get_table("public", name)
        .unwrap()
        .unwrap_or_else(|| panic!("table '{name}' not found after creation"))
}
