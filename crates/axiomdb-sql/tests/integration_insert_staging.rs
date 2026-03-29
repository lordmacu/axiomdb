//! Integration tests for transactional INSERT staging (Phase 5.21).

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::QueryResult;
use axiomdb_types::Value;

use common::*;

#[test]
fn test_5_21_staged_rows_committed() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE staged_t (id INT, val INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    for i in 1..=100 {
        ok_ctx_5_21(
            &format!("INSERT INTO staged_t VALUES ({i}, {i})"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }
    ok_ctx_5_21("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = ok_ctx_5_21(
        "SELECT COUNT(*) FROM staged_t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows[0][0], Value::BigInt(100));
    } else {
        panic!("expected Rows");
    }
}

/// SELECT inside an explicit transaction sees rows inserted before it (barrier flush).
#[test]
fn test_5_21_read_your_own_writes() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE rowt (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok_ctx_5_21(
        "INSERT INTO rowt VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // SELECT triggers barrier flush — the inserted row must be visible.
    let r = ok_ctx_5_21(
        "SELECT v FROM rowt WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int(10));
    } else {
        panic!("expected Rows");
    }
    ok_ctx_5_21("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);
}

/// Table switch triggers a flush of the first table before buffering the second.
#[test]
fn test_5_21_table_switch_flushes() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE t1 (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21(
        "CREATE TABLE t2 (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok_ctx_5_21(
        "INSERT INTO t1 VALUES (1, 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Insert into different table — triggers flush of t1.
    ok_ctx_5_21(
        "INSERT INTO t2 VALUES (2, 2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);

    // Both tables must have their rows.
    let r1 = ok_ctx_5_21(
        "SELECT COUNT(*) FROM t1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r2 = ok_ctx_5_21(
        "SELECT COUNT(*) FROM t2",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let (QueryResult::Rows { rows: r1, .. }, QueryResult::Rows { rows: r2, .. }) = (r1, r2) {
        assert_eq!(r1[0][0], Value::BigInt(1));
        assert_eq!(r2[0][0], Value::BigInt(1));
    }
}

/// A table-switch flush must survive a later failing statement.
#[test]
fn test_5_21_table_switch_flush_survives_later_statement_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE first_t (id INT PRIMARY KEY, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21(
        "CREATE TABLE second_t (id INT PRIMARY KEY, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    ok_ctx_5_21("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok_ctx_5_21(
        "INSERT INTO first_t VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21(
        "INSERT INTO second_t VALUES (1, 20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    let err = run_ctx_5_21(
        "INSERT INTO second_t VALUES (1, 99)",
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

    ok_ctx_5_21(
        "INSERT INTO first_t VALUES (2, 30)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);

    let r1 = ok_ctx_5_21(
        "SELECT id FROM first_t ORDER BY id ASC",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r1 {
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    } else {
        panic!("expected Rows");
    }

    let r2 = ok_ctx_5_21(
        "SELECT id, v FROM second_t ORDER BY id ASC",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r2 {
        assert_eq!(rows, vec![vec![Value::Int(1), Value::Int(20)]]);
    } else {
        panic!("expected Rows");
    }
}

/// ROLLBACK discards unflushed staged rows without touching heap or WAL.
#[test]
fn test_5_21_rollback_discards_staged_rows() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE rbt (id INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok_ctx_5_21(
        "INSERT INTO rbt VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21(
        "INSERT INTO rbt VALUES (2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // ROLLBACK must discard buffer without heap writes.
    ok_ctx_5_21("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = ok_ctx_5_21(
        "SELECT COUNT(*) FROM rbt",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows[0][0], Value::BigInt(0));
    } else {
        panic!("expected Rows");
    }
}

/// UNIQUE violations across staged rows are detected before heap mutation.
#[test]
fn test_5_21_unique_violation_in_buffer_detected_early() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE uvt (id INT UNIQUE, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok_ctx_5_21(
        "INSERT INTO uvt VALUES (1, 10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Duplicate key within the buffer — must fail immediately.
    let e = run_ctx_5_21(
        "INSERT INTO uvt VALUES (1, 20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .expect_err("expected UniqueViolation");
    assert!(
        matches!(e, DbError::UniqueViolation { .. }),
        "expected UniqueViolation, got {e:?}"
    );

    // After the error, the transaction should still be able to commit cleanly
    // with only the first row (the duplicate was never staged).
    ok_ctx_5_21("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);
    let r = ok_ctx_5_21(
        "SELECT COUNT(*) FROM uvt",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows[0][0], Value::BigInt(1));
    }
}

/// Autocommit single-row INSERT still works correctly (not staged).
#[test]
fn test_5_21_autocommit_insert_not_staged() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok_ctx_5_21(
        "CREATE TABLE acsingle (id INT, v INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Three autocommit inserts — no explicit BEGIN.
    ok_ctx_5_21(
        "INSERT INTO acsingle VALUES (1, 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21(
        "INSERT INTO acsingle VALUES (2, 2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok_ctx_5_21(
        "INSERT INTO acsingle VALUES (3, 3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    let r = ok_ctx_5_21(
        "SELECT COUNT(*) FROM acsingle",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    if let QueryResult::Rows { rows, .. } = r {
        assert_eq!(rows[0][0], Value::BigInt(3));
    }
}
