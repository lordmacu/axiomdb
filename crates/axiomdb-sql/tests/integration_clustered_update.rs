mod common;

use axiomdb_catalog::{CatalogReader, IndexDef};
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
fn clustered_update_non_key_in_place() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "UPDATE users SET name = 'Alicia' WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 1);

    // Verify updated value.
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
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][1], Value::Text("Alicia".into()));
    assert_eq!(r[0][2], Value::Int(30)); // age unchanged
}

#[test]
fn clustered_update_all_rows() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "UPDATE users SET age = age + 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 5);

    let r = rows(
        run_ctx(
            "SELECT age FROM users ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Int(31));
    assert_eq!(r[1][0], Value::Int(26));
    assert_eq!(r[2][0], Value::Int(36));
    assert_eq!(r[3][0], Value::Int(29));
    assert_eq!(r[4][0], Value::Int(23));
}

#[test]
fn clustered_update_with_pk_range_where() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "UPDATE users SET name = 'Updated' WHERE id >= 2 AND id < 5",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 3);

    let r = rows(
        run_ctx(
            "SELECT name FROM users ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("Alice".into())); // id=1 unchanged
    assert_eq!(r[1][0], Value::Text("Updated".into())); // id=2
    assert_eq!(r[2][0], Value::Text("Updated".into())); // id=3
    assert_eq!(r[3][0], Value::Text("Updated".into())); // id=4
    assert_eq!(r[4][0], Value::Text("Eve".into())); // id=5 unchanged
}

#[test]
fn clustered_update_noop_when_values_unchanged() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "UPDATE users SET name = name WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // MySQL semantics: matched_count = 1, but no physical change.
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
    assert_eq!(r[0][1], Value::Text("Alice".into()));
}

#[test]
fn clustered_update_rollback_restores_original() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "UPDATE users SET name = 'CHANGED' WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let r = rows(
        run_ctx(
            "SELECT name FROM users WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("Alice".into()));
}

#[test]
fn clustered_update_pk_change_rewrites_primary_and_secondary_bookmarks() {
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
        "UPDATE users SET id = 7 WHERE email = 'alice@example.com'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 1);

    let old_pk = rows(
        run_ctx(
            "SELECT name FROM users WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(old_pk.is_empty());
    let new_pk = rows(
        run_ctx(
            "SELECT name FROM users WHERE id = 7",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(new_pk[0][0], Value::Text("Alice".into()));

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
        vec![vec![Value::Int(7)]]
    );
}

#[test]
fn clustered_update_secondary_key_change_updates_bookmark_and_rolls_back_cleanly() {
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

    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "UPDATE users SET email = 'alice+new@example.com' WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let indexes = table_indexes(&storage, &txn, "users");
    let primary_idx = primary_index(&indexes);
    let secondary_idx = only_unique_secondary(&indexes);
    assert_eq!(
        scan_secondary_prefix(
            &storage,
            &secondary_idx,
            &primary_idx,
            &[Value::Text("alice+new@example.com".into())],
        ),
        vec![vec![Value::Int(1)]]
    );
    assert!(scan_secondary_prefix(
        &storage,
        &secondary_idx,
        &primary_idx,
        &[Value::Text("alice@example.com".into())],
    )
    .is_empty());

    run_ctx("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    let restored = rows(
        run_ctx(
            "SELECT email FROM users WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(restored[0][0], Value::Text("alice@example.com".into()));

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
    assert!(scan_secondary_prefix(
        &storage,
        &secondary_idx,
        &primary_idx,
        &[Value::Text("alice+new@example.com".into())],
    )
    .is_empty());
}

#[test]
fn clustered_update_empty_result_set() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_users(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = run_ctx(
        "UPDATE users SET name = 'Nobody' WHERE id = 999",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 0);
}

#[test]
fn clustered_update_after_splits() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for id in 0..200 {
        let body = "x".repeat(200 + (id % 5) * 40);
        run_ctx(
            &format!("INSERT INTO docs VALUES ({id}, '{body}')"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    // Update a row in the middle of the tree.
    let result = run_ctx(
        "UPDATE docs SET body = 'UPDATED' WHERE id = 100",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 1);

    let r = rows(
        run_ctx(
            "SELECT body FROM docs WHERE id = 100",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("UPDATED".into()));

    // Verify other rows unaffected.
    let r = rows(
        run_ctx(
            "SELECT COUNT(*) FROM docs",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::BigInt(200));
}

#[test]
fn clustered_update_relocation_preserves_row_visibility() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE docs (id INT PRIMARY KEY, body TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    for id in 0..96 {
        run_ctx(
            &format!("INSERT INTO docs VALUES ({id}, '{}')", "x".repeat(120)),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap();
    }

    let result = run_ctx(
        &format!(
            "UPDATE docs SET body = '{}' WHERE id = 42",
            "y".repeat(4000)
        ),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(affected_count(result), 1);

    let r = rows(
        run_ctx(
            "SELECT body FROM docs WHERE id = 42",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r[0][0], Value::Text("y".repeat(4000)));
}

#[test]
fn clustered_update_non_ctx_path_is_supported() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT UNIQUE, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO users VALUES (1, 'alice@example.com', 'Alice')",
        &mut storage,
        &mut txn,
    );

    let result = run(
        "UPDATE users SET email = 'alice+nonctx@example.com' WHERE id = 1",
        &mut storage,
        &mut txn,
    );
    assert_eq!(affected_count(result), 1);

    let rows = rows(run(
        "SELECT email FROM users WHERE id = 1",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(rows[0][0], Value::Text("alice+nonctx@example.com".into()));

    let indexes = table_indexes(&storage, &txn, "users");
    let primary_idx = primary_index(&indexes);
    let secondary_idx = only_unique_secondary(&indexes);
    assert_eq!(
        scan_secondary_prefix(
            &storage,
            &secondary_idx,
            &primary_idx,
            &[Value::Text("alice+nonctx@example.com".into())],
        ),
        vec![vec![Value::Int(1)]]
    );
}
