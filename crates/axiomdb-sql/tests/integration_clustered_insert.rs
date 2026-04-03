mod common;

use axiomdb_catalog::{CatalogReader, ColumnDef, IndexDef, TableDef, TableStorageLayout};
use axiomdb_core::{error::DbError, TransactionSnapshot};
use axiomdb_sql::{
    clustered_secondary::ClusteredSecondaryLayout, key_encoding::encode_index_key,
    table::decode_row_from_bytes, QueryResult,
};
use axiomdb_storage::{clustered_tree, PageType, StorageEngine};
use axiomdb_types::Value;
use common::{affected_count, rows, run, run_ctx, run_result, setup, setup_ctx};

fn table_state(
    storage: &impl StorageEngine,
    snapshot: TransactionSnapshot,
    table_name: &str,
) -> (TableDef, Vec<ColumnDef>, Vec<IndexDef>) {
    let mut reader = CatalogReader::new(storage, snapshot).unwrap();
    let table = reader.get_table("public", table_name).unwrap().unwrap();
    let columns = reader.list_columns(table.id).unwrap();
    let indexes = reader.list_indexes(table.id).unwrap();
    (table, columns, indexes)
}

fn primary_index(indexes: &[IndexDef]) -> IndexDef {
    indexes.iter().find(|idx| idx.is_primary).unwrap().clone()
}

fn only_unique_secondary(indexes: &[IndexDef]) -> IndexDef {
    indexes
        .iter()
        .find(|idx| !idx.is_primary && idx.is_unique)
        .unwrap()
        .clone()
}

fn page_type(storage: &impl StorageEngine, page_id: u64) -> PageType {
    let page = storage.read_page(page_id).unwrap();
    PageType::try_from(page.header().page_type).unwrap()
}

fn lookup_clustered_row(
    storage: &impl StorageEngine,
    table: &TableDef,
    columns: &[ColumnDef],
    pk_values: &[Value],
    snapshot: &TransactionSnapshot,
) -> Option<Vec<Value>> {
    let key = encode_index_key(pk_values).unwrap();
    clustered_tree::lookup(storage, Some(table.root_page_id), &key, snapshot)
        .unwrap()
        .map(|row| decode_row_from_bytes(&row.row_data, columns).unwrap())
}

fn scan_secondary_prefix(
    storage: &impl StorageEngine,
    secondary_idx: &IndexDef,
    primary_idx: &IndexDef,
    logical_prefix: &[Value],
) -> Vec<Vec<Value>> {
    let layout = ClusteredSecondaryLayout::derive(secondary_idx, primary_idx).unwrap();
    layout
        .scan_prefix(storage, secondary_idx.root_page_id, logical_prefix)
        .unwrap()
        .into_iter()
        .map(|entry| entry.primary_key)
        .collect()
}

#[test]
fn clustered_insert_persists_base_row_and_unique_secondary_bookmark() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
        &mut storage,
        &mut txn,
    );

    let result = run(
        "INSERT INTO users VALUES (1, 'alice@example.com', 'Alice')",
        &mut storage,
        &mut txn,
    );
    assert_eq!(affected_count(result), 1);

    let (table, columns, indexes) = table_state(&storage, txn.snapshot(), "users");
    assert_eq!(table.storage_layout, TableStorageLayout::Clustered);

    let primary_idx = primary_index(&indexes);
    let secondary_idx = only_unique_secondary(&indexes);
    let row = lookup_clustered_row(
        &storage,
        &table,
        &columns,
        &[Value::Int(1)],
        &txn.snapshot(),
    )
    .expect("inserted clustered row must exist");
    assert_eq!(
        row,
        vec![
            Value::Int(1),
            Value::Text("alice@example.com".into()),
            Value::Text("Alice".into()),
        ]
    );

    let bookmarks = scan_secondary_prefix(
        &storage,
        &secondary_idx,
        &primary_idx,
        &[Value::Text("alice@example.com".into())],
    );
    assert_eq!(bookmarks, vec![vec![Value::Int(1)]]);
}

#[test]
fn clustered_insert_persists_catalog_root_after_split() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)",
        &mut storage,
        &mut txn,
    );

    let (table_before, _, _) = table_state(&storage, txn.snapshot(), "docs");
    let initial_root = table_before.root_page_id;
    assert_eq!(page_type(&storage, initial_root), PageType::ClusteredLeaf);

    for id in 0..240 {
        let payload = "x".repeat(280 + (id % 7) as usize * 37);
        run(
            &format!("INSERT INTO docs VALUES ({id}, '{payload}')"),
            &mut storage,
            &mut txn,
        );
    }

    let (table_after, columns, _) = table_state(&storage, txn.snapshot(), "docs");
    assert_ne!(table_after.root_page_id, initial_root);
    assert_eq!(
        page_type(&storage, table_after.root_page_id),
        PageType::ClusteredInternal
    );

    let probe = lookup_clustered_row(
        &storage,
        &table_after,
        &columns,
        &[Value::Int(173)],
        &txn.snapshot(),
    )
    .expect("split-tree lookup must succeed");
    assert_eq!(probe[0], Value::Int(173));
}

#[test]
fn clustered_insert_duplicate_live_primary_key_fails() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'Alice')",
        &mut storage,
        &mut txn,
    );

    let err = run_result(
        "INSERT INTO users VALUES (1, 'Bob')",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "got {err:?}"
    );
}

#[test]
fn clustered_insert_reuses_committed_delete_marked_primary_key() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'Old Name')",
        &mut storage,
        &mut txn,
    );

    let (table, columns, _) = table_state(&storage, txn.snapshot(), "users");
    let delete_txn = txn.max_committed() + 1;
    let key = encode_index_key(&[Value::Int(1)]).unwrap();
    let deleted = clustered_tree::delete_mark(
        &mut storage,
        Some(table.root_page_id),
        &key,
        delete_txn,
        &TransactionSnapshot::active(delete_txn, txn.max_committed()),
    )
    .unwrap();
    assert!(deleted);

    // Advance TxnManager so later executor snapshots treat the synthetic
    // delete-mark as committed state.
    txn.begin().unwrap();
    txn.commit().unwrap();
    assert_eq!(txn.max_committed(), delete_txn);

    run(
        "INSERT INTO users VALUES (1, 'Reused Key')",
        &mut storage,
        &mut txn,
    );

    let (table_after, _, _) = table_state(&storage, txn.snapshot(), "users");
    let row = lookup_clustered_row(
        &storage,
        &table_after,
        &columns,
        &[Value::Int(1)],
        &txn.snapshot(),
    )
    .expect("reused clustered PK must be visible");
    assert_eq!(row, vec![Value::Int(1), Value::Text("Reused Key".into())]);
}

#[test]
fn clustered_insert_null_primary_key_is_rejected() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
    );

    let err = run_result(
        "INSERT INTO users VALUES (NULL, 'Alice')",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            DbError::NotNullViolation { ref table, ref column }
                if table == "users" && column == "id"
        ),
        "got {err:?}"
    );
}

#[test]
fn clustered_insert_auto_increment_bootstraps_from_clustered_rows() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY AUTO_INCREMENT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (7, 'seven')",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (9, 'nine')",
        &mut storage,
        &mut txn,
    );

    let result = run(
        "INSERT INTO users (name) VALUES ('ten')",
        &mut storage,
        &mut txn,
    );
    match result {
        QueryResult::Affected {
            count,
            last_insert_id,
        } => {
            assert_eq!(count, 1);
            assert_eq!(last_insert_id, Some(10));
        }
        other => panic!("expected affected result, got {other:?}"),
    }

    let (table, columns, _) = table_state(&storage, txn.snapshot(), "users");
    let row = lookup_clustered_row(
        &storage,
        &table,
        &columns,
        &[Value::Int(10)],
        &txn.snapshot(),
    )
    .expect("auto-generated clustered row must exist");
    assert_eq!(row, vec![Value::Int(10), Value::Text("ten".into())]);
}

#[test]
fn clustered_insert_select_into_clustered_table_succeeds() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE src (id INT, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO src VALUES (1, 'alice')",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO src VALUES (2, 'bob')", &mut storage, &mut txn);
    run(
        "CREATE TABLE dst (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
    );

    let result = run(
        "INSERT INTO dst SELECT id, name FROM src",
        &mut storage,
        &mut txn,
    );
    assert_eq!(affected_count(result), 2);

    let (table, columns, _) = table_state(&storage, txn.snapshot(), "dst");
    let first = lookup_clustered_row(
        &storage,
        &table,
        &columns,
        &[Value::Int(1)],
        &txn.snapshot(),
    )
    .expect("first INSERT SELECT row must exist");
    let second = lookup_clustered_row(
        &storage,
        &table,
        &columns,
        &[Value::Int(2)],
        &txn.snapshot(),
    )
    .expect("second INSERT SELECT row must exist");
    assert_eq!(first, vec![Value::Int(1), Value::Text("alice".into())]);
    assert_eq!(second, vec![Value::Int(2), Value::Text("bob".into())]);
}

#[test]
fn clustered_insert_rollback_removes_base_and_secondary_rows() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'alice@example.com', 'Alice')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let (table, columns, indexes) = table_state(&storage, txn.snapshot(), "users");
    let primary_idx = primary_index(&indexes);
    let secondary_idx = only_unique_secondary(&indexes);

    assert!(lookup_clustered_row(
        &storage,
        &table,
        &columns,
        &[Value::Int(1)],
        &txn.snapshot(),
    )
    .is_none());
    assert!(scan_secondary_prefix(
        &storage,
        &secondary_idx,
        &primary_idx,
        &[Value::Text("alice@example.com".into())],
    )
    .is_empty());

    run_ctx(
        "INSERT INTO users VALUES (1, 'alice@example.com', 'Alice')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
}

#[test]
fn clustered_insert_savepoint_rollback_keeps_pre_savepoint_rows() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'alice@example.com', 'Alice')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("SAVEPOINT s1", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "INSERT INTO users VALUES (2, 'bob@example.com', 'Bob')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "ROLLBACK TO SAVEPOINT s1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let (table, columns, indexes) = table_state(&storage, txn.snapshot(), "users");
    let primary_idx = primary_index(&indexes);
    let secondary_idx = only_unique_secondary(&indexes);

    assert_eq!(
        lookup_clustered_row(
            &storage,
            &table,
            &columns,
            &[Value::Int(1)],
            &txn.snapshot(),
        )
        .unwrap(),
        vec![
            Value::Int(1),
            Value::Text("alice@example.com".into()),
            Value::Text("Alice".into()),
        ]
    );
    assert!(lookup_clustered_row(
        &storage,
        &table,
        &columns,
        &[Value::Int(2)],
        &txn.snapshot(),
    )
    .is_none());
    assert_eq!(
        scan_secondary_prefix(
            &storage,
            &secondary_idx,
            &primary_idx,
            &[Value::Text("alice@example.com".into())],
        ),
        vec![vec![Value::Int(1)]]
    );
    assert!(scan_secondary_prefix(
        &storage,
        &secondary_idx,
        &primary_idx,
        &[Value::Text("bob@example.com".into())],
    )
    .is_empty());
}

#[test]
fn clustered_insert_statement_rollback_preserves_flushed_heap_batch() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE logs (id INT, note TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "INSERT INTO logs VALUES (1, 'buffered')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = run_ctx(
        "INSERT INTO users VALUES (1, 'Alice'), (1, 'Duplicate')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "got {err:?}"
    );

    run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let log_rows = rows(
        run_ctx(
            "SELECT * FROM logs",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        log_rows,
        vec![vec![Value::Int(1), Value::Text("buffered".into())]]
    );

    let (table, columns, _) = table_state(&storage, txn.snapshot(), "users");
    assert!(lookup_clustered_row(
        &storage,
        &table,
        &columns,
        &[Value::Int(1)],
        &txn.snapshot(),
    )
    .is_none());
}
