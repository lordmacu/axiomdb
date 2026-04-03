mod common;

use axiomdb_catalog::{CatalogReader, IndexDef};
use axiomdb_core::error::DbError;
use axiomdb_sql::clustered_secondary::ClusteredSecondaryLayout;
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;
use common::{affected_count, rows, run, run_ctx, setup, setup_ctx};

fn setup_users(
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut axiomdb_sql::bloom::BloomRegistry,
    ctx: &mut axiomdb_sql::session::SessionContext,
) {
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'Alice', 30)",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (2, 'Bob', 25)",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (3, 'Charlie', 35)",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (4, 'Diana', 28)",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (5, 'Eve', 22)",
        storage,
        txn,
        bloom,
        ctx,
    )
    .unwrap();
}

fn table_indexes(
    storage: &impl axiomdb_storage::StorageEngine,
    txn: &TxnManager,
    table_name: &str,
) -> Vec<IndexDef> {
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let mut reader = CatalogReader::new(storage, snap).unwrap();
    let table = reader.get_table("public", table_name).unwrap().unwrap();
    reader.list_indexes(table.id).unwrap()
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

fn scan_secondary_prefix(
    storage: &impl axiomdb_storage::StorageEngine,
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
fn clustered_delete_with_pk_where() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "DELETE FROM users WHERE id = 3",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 1);

    // Verify row gone.
    let r = rows(
        run_ctx(
            "SELECT * FROM users WHERE id = 3",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(r.is_empty());

    // Other rows still present.
    let r = rows(
        run_ctx(
            "SELECT COUNT(*) FROM users",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::BigInt(4));
}

#[test]
fn clustered_delete_all_rows() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "DELETE FROM users",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 5);

    let r = rows(
        run_ctx(
            "SELECT COUNT(*) FROM users",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::BigInt(0));
}

#[test]
fn clustered_delete_with_non_pk_where() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "DELETE FROM users WHERE age > 27",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 3); // Alice(30), Charlie(35), Diana(28)

    let r = rows(
        run_ctx(
            "SELECT id FROM users ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(2)); // Bob(25)
    assert_eq!(r[1][0], Value::Int(5)); // Eve(22)
}

#[test]
fn clustered_delete_empty_result() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "DELETE FROM users WHERE id = 999",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 0);

    // All rows still present.
    let r = rows(
        run_ctx(
            "SELECT COUNT(*) FROM users",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::BigInt(5));
}

#[test]
fn clustered_delete_then_insert_same_pk() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    // Delete id=3.
    run_ctx(
        "DELETE FROM users WHERE id = 3",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Re-insert with same PK.
    run_ctx(
        "INSERT INTO users VALUES (3, 'New Charlie', 40)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let r = rows(
        run_ctx(
            "SELECT name, age FROM users WHERE id = 3",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("New Charlie".into()));
    assert_eq!(r[0][1], Value::Int(40));
}

#[test]
fn clustered_delete_via_secondary_predicate_keeps_secondary_bookmark() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'alice@example.com', 'Alice')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "DELETE FROM users WHERE email = 'alice@example.com'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 1);

    let r = rows(
        run_ctx(
            "SELECT * FROM users WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(r.is_empty());

    let indexes = table_indexes(&storage, &txn, "users");
    let primary_idx = primary_index(&indexes);
    let secondary_idx = only_unique_secondary(&indexes);
    assert_eq!(
        scan_secondary_prefix(
            &storage,
            &secondary_idx,
            &primary_idx,
            &[Value::Text("alice@example.com".into())],
        ),
        vec![vec![Value::Int(1)]]
    );
}

#[test]
fn clustered_delete_rollback_restores_row() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    let result = run_ctx(
        "DELETE FROM users WHERE id = 4",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 1);

    let during_txn = rows(
        run_ctx(
            "SELECT * FROM users WHERE id = 4",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(during_txn.is_empty());

    run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let after_rollback = rows(
        run_ctx(
            "SELECT name, age FROM users WHERE id = 4",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        after_rollback,
        vec![vec![Value::Text("Diana".into()), Value::Int(28)]]
    );
}

#[test]
fn clustered_delete_parent_fk_restrict_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE orders (id INT, user_id INT REFERENCES users(id) ON DELETE RESTRICT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'Alice')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO orders VALUES (10, 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = run_ctx(
        "DELETE FROM users WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::ForeignKeyParentViolation { .. }),
        "expected ForeignKeyParentViolation, got: {err}"
    );

    let parent_rows = rows(
        run_ctx(
            "SELECT name FROM users WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(parent_rows, vec![vec![Value::Text("Alice".into())]]);
}

#[test]
fn clustered_delete_non_ctx_path_is_supported() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'Alice', 30)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (2, 'Bob', 25)",
        &mut storage,
        &mut txn,
    );

    let result = run("DELETE FROM users WHERE id = 2", &mut storage, &mut txn);
    assert_eq!(affected_count(result), 1);

    let remaining = rows(run(
        "SELECT id, name FROM users ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(
        remaining,
        vec![vec![Value::Int(1), Value::Text("Alice".into())]]
    );
}

#[test]
fn clustered_delete_from_empty_table() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE empty_t (id INT PRIMARY KEY, val TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "DELETE FROM empty_t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 0);
}
