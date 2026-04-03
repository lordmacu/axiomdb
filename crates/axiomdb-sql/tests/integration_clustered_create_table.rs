mod common;

use axiomdb_catalog::{CatalogReader, TableStorageLayout};
use axiomdb_core::error::DbError;
use axiomdb_sql::verify_and_repair_indexes_on_open;
use axiomdb_storage::{PageType, StorageEngine};

use common::{run, run_ctx, run_result, setup, setup_ctx};

fn page_type(storage: &impl StorageEngine, page_id: u64) -> PageType {
    let page = storage.read_page(page_id).unwrap();
    PageType::try_from(page.header().page_type).unwrap()
}

#[test]
fn create_table_with_inline_primary_key_uses_clustered_root_and_primary_index_metadata() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, email TEXT UNIQUE)",
        &mut storage,
        &mut txn,
    );

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let table = reader.get_table("public", "users").unwrap().unwrap();
    assert_eq!(table.storage_layout, TableStorageLayout::Clustered);
    assert_eq!(
        page_type(&storage, table.root_page_id),
        PageType::ClusteredLeaf
    );

    let indexes = reader.list_indexes(table.id).unwrap();
    let pk = indexes.iter().find(|idx| idx.is_primary).unwrap();
    assert_eq!(pk.root_page_id, table.root_page_id);
    assert!(pk.is_unique);
    assert_eq!(pk.columns.len(), 1);
    assert_eq!(pk.columns[0].col_idx, 0);

    let unique = indexes
        .iter()
        .find(|idx| idx.name == "users_email_unique")
        .unwrap();
    assert!(!unique.is_primary);
    assert_eq!(page_type(&storage, unique.root_page_id), PageType::Index);
}

#[test]
fn create_table_with_table_level_composite_primary_key_preserves_column_order() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE orders (account_id INT, seq INT, payload TEXT, PRIMARY KEY (account_id, seq))",
        &mut storage,
        &mut txn,
    );

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let table = reader.get_table("public", "orders").unwrap().unwrap();
    assert_eq!(table.storage_layout, TableStorageLayout::Clustered);

    let indexes = reader.list_indexes(table.id).unwrap();
    let pk = indexes.iter().find(|idx| idx.is_primary).unwrap();
    let col_order: Vec<u16> = pk.columns.iter().map(|c| c.col_idx).collect();
    assert_eq!(col_order, vec![0, 1]);
    assert_eq!(pk.root_page_id, table.root_page_id);
}

#[test]
fn create_table_without_primary_key_stays_heap() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE logs (ts INT, msg TEXT)",
        &mut storage,
        &mut txn,
    );

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let table = reader.get_table("public", "logs").unwrap().unwrap();
    assert_eq!(table.storage_layout, TableStorageLayout::Heap);
    assert_eq!(page_type(&storage, table.root_page_id), PageType::Data);
}

#[test]
fn insert_into_clustered_table_is_rejected_in_non_ctx_path() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
    );

    let err = run_result(
        "INSERT INTO users VALUES (1, 'alice')",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::NotImplemented { ref feature } if feature.contains("Phase 39.14")),
        "got {err:?}"
    );
}

#[test]
fn select_from_clustered_table_is_rejected_in_ctx_path() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = run_ctx(
        "SELECT * FROM users",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::NotImplemented { ref feature } if feature.contains("Phase 39.15")),
        "got {err:?}"
    );
}

#[test]
fn startup_index_integrity_skips_clustered_tables() {
    let (mut storage, mut txn) = setup();

    run(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE)",
        &mut storage,
        &mut txn,
    );

    let report = verify_and_repair_indexes_on_open(&mut storage, &mut txn).unwrap();
    assert_eq!(report.tables_checked, 0);
    assert_eq!(report.indexes_checked, 0);
    assert!(report.rebuilt_indexes.is_empty());
}
