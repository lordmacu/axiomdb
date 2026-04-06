mod common;

use axiomdb_types::Value;
use common::{rows, run_ctx, setup_ctx};

// ── INSERT DEFAULT VALUES ─────────────────────────────────────────────────────

#[test]
fn insert_default_values_all_nullable_heap() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (a INT, b TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "INSERT INTO t DEFAULT VALUES",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = rows(
        run_ctx(
            "SELECT a, b FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Null);
    assert_eq!(result[0][1], Value::Null);
}

#[test]
fn insert_default_values_auto_increment_heap() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT AUTO_INCREMENT, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "INSERT INTO t DEFAULT VALUES",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t DEFAULT VALUES",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = rows(
        run_ctx(
            "SELECT id FROM t ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 2);
    assert_eq!(result[0][0], Value::Int(1));
    assert_eq!(result[1][0], Value::Int(2));
}

#[test]
fn insert_default_values_clustered_table() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, val TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // For clustered tables: DEFAULT VALUES → NULL for PK would violate NOT NULL.
    // This should fail.
    let err = run_ctx(
        "INSERT INTO t DEFAULT VALUES",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        err.is_err(),
        "NULL primary key must be rejected on clustered table"
    );
}

#[test]
fn insert_default_values_auto_increment_clustered() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, score INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_ctx(
        "INSERT INTO t DEFAULT VALUES",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t DEFAULT VALUES",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = rows(
        run_ctx(
            "SELECT id FROM t ORDER BY id",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 2);
    assert_eq!(result[0][0], Value::Int(1));
    assert_eq!(result[1][0], Value::Int(2));
}

// ── SHOW INDEX ───────────────────────────────────────────────────────────────

#[test]
fn show_index_no_indexes() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (a INT, b TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = run_ctx(
        "SHOW INDEX FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let r = rows(result);
    assert_eq!(r.len(), 0, "no indexes on plain table");
}

#[test]
fn show_index_primary_key() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let r = rows(
        run_ctx(
            "SHOW INDEX FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    // One row for the PK (single-column).
    assert_eq!(r.len(), 1);
    // Key_name = "PRIMARY"
    assert_eq!(r[0][2], Value::Text("PRIMARY".into()));
    // Non_unique = 0
    assert_eq!(r[0][1], Value::Int(0));
    // Column_name = "id"
    assert_eq!(r[0][4], Value::Text("id".into()));
    // Index_type = "BTREE"
    assert_eq!(r[0][10], Value::Text("BTREE".into()));
}

#[test]
fn show_index_unique_secondary() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let r = rows(
        run_ctx(
            "SHOW INDEXES FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    // Two indexes: PRIMARY + unique on email.
    assert_eq!(r.len(), 2);

    // Find the non-PRIMARY row.
    let email_row = r.iter().find(|row| row[2] != Value::Text("PRIMARY".into()));
    assert!(email_row.is_some(), "unique index on email not found");
    let email_row = email_row.unwrap();
    assert_eq!(
        email_row[1],
        Value::Int(0),
        "unique index: Non_unique must be 0"
    );
    assert_eq!(email_row[4], Value::Text("email".into()));
}

#[test]
fn show_keys_synonym() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT PRIMARY KEY)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // SHOW KEYS is a synonym for SHOW INDEX.
    let r = rows(
        run_ctx(
            "SHOW KEYS FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][2], Value::Text("PRIMARY".into()));
}

#[test]
fn show_index_non_unique() {
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
        "CREATE INDEX idx_score ON t (score)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let r = rows(
        run_ctx(
            "SHOW INDEX FROM t",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );

    let score_row = r
        .iter()
        .find(|row| row[4] == Value::Text("score".into()))
        .expect("idx_score not found");
    assert_eq!(
        score_row[1],
        Value::Int(1),
        "non-unique index: Non_unique must be 1"
    );
}
