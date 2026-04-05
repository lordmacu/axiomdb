mod common;

use axiomdb_catalog::CatalogReader;
use axiomdb_types::Value;
use common::{rows, run_ctx, setup_ctx};

// ── ADD COLUMN ────────────────────────────────────────────────────────────────

#[test]
fn add_column_to_clustered_table_with_default_null() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 'Alice')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (2, 'Bob')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "ALTER TABLE t ADD COLUMN age INT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = rows(
        run_ctx(
            "SELECT id, name, age FROM t ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 2);
    assert_eq!(
        result[0],
        vec![Value::Int(1), Value::Text("Alice".into()), Value::Null]
    );
    assert_eq!(
        result[1],
        vec![Value::Int(2), Value::Text("Bob".into()), Value::Null]
    );
}

#[test]
fn add_column_to_clustered_table_then_insert_uses_new_schema() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, x INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "ALTER TABLE t ADD COLUMN y INT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "INSERT INTO t VALUES (2, 20, 99)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = rows(
        run_ctx(
            "SELECT id, x, y FROM t ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 2);
    // Old row: y = NULL (added with null default)
    assert_eq!(result[0][2], Value::Null);
    // New row: y = 99
    assert_eq!(result[1][2], Value::Int(99));
}

#[test]
fn add_column_preserves_secondary_index_on_clustered_table() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 'a@example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (2, 'b@example.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "ALTER TABLE t ADD COLUMN notes TEXT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Secondary unique index on email must still enforce uniqueness after ADD COLUMN.
    let err = run_ctx(
        "INSERT INTO t VALUES (3, 'a@example.com', 'dup')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        err.is_err(),
        "duplicate email must be rejected after ADD COLUMN"
    );

    // Existing rows still readable with correct columns.
    let result = rows(
        run_ctx(
            "SELECT id, email FROM t WHERE id = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][1], Value::Text("a@example.com".into()));
}

// ── DROP COLUMN ───────────────────────────────────────────────────────────────

#[test]
fn drop_column_from_clustered_table_removes_data() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT, age INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 'Alice', 30)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (2, 'Bob', 25)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "ALTER TABLE t DROP COLUMN age",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = rows(
        run_ctx(
            "SELECT id, name FROM t ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 2);
    assert_eq!(result[0], vec![Value::Int(1), Value::Text("Alice".into())]);
    assert_eq!(result[1], vec![Value::Int(2), Value::Text("Bob".into())]);
}

#[test]
fn drop_column_then_insert_and_select_use_new_schema() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10, 'hello')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "ALTER TABLE t DROP COLUMN a",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "INSERT INTO t VALUES (2, 'world')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = rows(
        run_ctx(
            "SELECT id, b FROM t ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 2);
    assert_eq!(result[0], vec![Value::Int(1), Value::Text("hello".into())]);
    assert_eq!(result[1], vec![Value::Int(2), Value::Text("world".into())]);
}

#[test]
fn drop_indexed_column_on_clustered_table_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = run_ctx(
        "ALTER TABLE t DROP COLUMN email",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        err.is_err(),
        "dropping an indexed column must return an error"
    );
}

#[test]
fn drop_column_if_exists_on_clustered_table_is_noop_for_missing_column() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, x INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // No error for missing column when IF EXISTS.
    run_ctx(
        "ALTER TABLE t DROP COLUMN IF EXISTS nonexistent",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Table still has original schema.
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let table = reader.get_table("public", "t").unwrap().unwrap();
    let cols = reader.list_columns(table.id).unwrap();
    assert_eq!(cols.len(), 2, "schema must be unchanged");
}

// ── ADD + DROP round-trip ─────────────────────────────────────────────────────

#[test]
fn add_then_drop_column_round_trip_on_clustered_table() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, val INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 100)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "ALTER TABLE t ADD COLUMN tmp TEXT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "ALTER TABLE t DROP COLUMN tmp",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = rows(
        run_ctx(
            "SELECT id, val FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], vec![Value::Int(1), Value::Int(100)]);
}

// ── MODIFY COLUMN ─────────────────────────────────────────────────────────────

#[test]
fn modify_column_widen_int_to_bigint_on_clustered_table() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, score INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 100)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (2, 200)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "ALTER TABLE t MODIFY COLUMN score BIGINT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = rows(
        run_ctx(
            "SELECT id, score FROM t ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 2);
    assert_eq!(result[0][1], Value::BigInt(100));
    assert_eq!(result[1][1], Value::BigInt(200));
}

#[test]
fn modify_column_int_to_text_on_clustered_table() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, code INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 42)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // INT → TEXT: always safe, no data loss
    run_ctx(
        "ALTER TABLE t MODIFY COLUMN code TEXT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = rows(
        run_ctx(
            "SELECT id, code FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][1], Value::Text("42".into()));
}

#[test]
fn modify_column_fails_for_indexed_column_on_clustered_table() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Changing type of an indexed column must be rejected.
    let err = run_ctx(
        "ALTER TABLE t MODIFY COLUMN email INT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(err.is_err(), "modifying type of indexed column must fail");
}

#[test]
fn modify_column_change_nullable_on_clustered_table() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, val INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Change to NOT NULL (same type, just nullability constraint).
    run_ctx(
        "ALTER TABLE t MODIFY COLUMN val INT NOT NULL",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Catalog should reflect NOT NULL.
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let table = reader.get_table("public", "t").unwrap().unwrap();
    let cols = reader.list_columns(table.id).unwrap();
    let val_col = cols.iter().find(|c| c.name == "val").unwrap();
    assert!(!val_col.nullable, "val must be NOT NULL after MODIFY");
}

#[test]
fn modify_column_text_to_int_fails_for_non_numeric_data() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, tag TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 'not_a_number')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // TEXT → INT must fail when data is non-numeric.
    let err = run_ctx(
        "ALTER TABLE t MODIFY COLUMN tag INT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(err.is_err(), "TEXT→INT must fail when data is not numeric");
}
