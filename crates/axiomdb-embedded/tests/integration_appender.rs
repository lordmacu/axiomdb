//! Integration tests for the embedded Appender fast-path INSERT API
//! (Attack 7, perf-sqlite-gap).
//!
//! Spec: `specs/fase-perf-sqlite-gap/spec-embedded-appender.md`
//! Plan: `specs/fase-perf-sqlite-gap/plan-embedded-appender.md`
//!
//! Step 1 here: the open path (`Db::appender(table)`) and its guards.
//! Subsequent steps add append/flush/finish/Drop and the edge-case
//! suite.

use axiomdb_core::error::DbError;
use axiomdb_embedded::Db;
use axiomdb_types::Value;
use tempfile::TempDir;

fn open_db() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().join("test.db")).unwrap();
    (dir, db)
}

// ── Step 1 — Open path ────────────────────────────────────────────────────────

#[test]
fn appender_opens_on_heap_table() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let app = db.appender("t").unwrap();
    assert_eq!(
        app.pending(),
        0,
        "freshly-opened appender has empty buffer"
    );
}

#[test]
fn appender_open_on_missing_table_returns_table_not_found() {
    let (_dir, mut db) = open_db();
    let err = db.appender("ghost").unwrap_err();
    assert!(
        matches!(err, DbError::TableNotFound { .. }),
        "expected TableNotFound, got {err:?}"
    );
}

#[test]
fn appender_open_while_user_txn_open_returns_already_active() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    db.run("BEGIN").unwrap();
    let err = db.appender("t").unwrap_err();
    assert!(
        matches!(err, DbError::TransactionAlreadyActive { .. }),
        "expected TransactionAlreadyActive, got {err:?}"
    );
}

// ── Step 2 — append_row ───────────────────────────────────────────────────────

#[test]
fn append_row_accumulates_in_buffer() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Text("a".into())])
        .unwrap();
    app.append_row(&[Value::Int(2), Value::Text("b".into())])
        .unwrap();
    assert_eq!(app.pending(), 2);
}

#[test]
fn append_row_wrong_arity_returns_type_mismatch() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    let err = app.append_row(&[Value::Int(1)]).unwrap_err();
    assert!(
        matches!(err, DbError::TypeMismatch { .. }),
        "expected TypeMismatch, got {err:?}"
    );
    // Appender remains usable after the rejected row.
    app.append_row(&[Value::Int(1), Value::Text("a".into())])
        .unwrap();
    assert_eq!(app.pending(), 1);
}

#[test]
fn append_row_not_null_violation_returns_error() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT NOT NULL, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    let err = app
        .append_row(&[Value::Null, Value::Text("a".into())])
        .unwrap_err();
    assert!(
        matches!(err, DbError::NotNullViolation { .. }),
        "expected NotNullViolation, got {err:?}"
    );
    assert_eq!(app.pending(), 0, "rejected row not added to buffer");
}

#[test]
fn append_row_owned_consumes_vec() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    let row = vec![Value::Int(1), Value::Text("a".into())];
    app.append_row_owned(row).unwrap();
    assert_eq!(app.pending(), 1);
}

#[test]
fn append_row_type_coercion_succeeds_in_permissive() {
    // strict_mode=ON (default) — coercion of a BigInt into Int succeeds
    // because the value fits (1 fits in i32). The point is to confirm
    // coerce_values_with_ctx is invoked.
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::BigInt(7)]).unwrap();
    assert_eq!(app.pending(), 1);
}
