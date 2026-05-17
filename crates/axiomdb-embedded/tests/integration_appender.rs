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

// ── Step 3 — flush (visibility tests deferred to Step 4 / finish) ─────────────

#[test]
fn flush_empty_buffer_is_noop() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.flush().unwrap();
    app.flush().unwrap();
    assert_eq!(app.pending(), 0);
}

#[test]
fn flush_drains_buffer_and_keeps_appender_usable() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Text("a".into())])
        .unwrap();
    app.append_row(&[Value::Int(2), Value::Text("b".into())])
        .unwrap();
    assert_eq!(app.pending(), 2);
    app.flush().unwrap();
    assert_eq!(app.pending(), 0, "buffer drained after flush");
    // Appender remains usable for more appends.
    app.append_row(&[Value::Int(3), Value::Text("c".into())])
        .unwrap();
    assert_eq!(app.pending(), 1);
}

#[test]
fn appender_on_table_with_check_returns_unsupported() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, age INT CHECK (age >= 0))")
        .unwrap();
    let err = db.appender("t").unwrap_err();
    assert!(
        matches!(err, DbError::NotImplemented { .. }),
        "expected NotImplemented, got {err:?}"
    );
}

#[test]
fn appender_on_table_with_auto_increment_returns_unsupported() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT AUTO_INCREMENT, v TEXT)")
        .unwrap();
    let err = db.appender("t").unwrap_err();
    assert!(
        matches!(err, DbError::NotImplemented { .. }),
        "expected NotImplemented, got {err:?}"
    );
}

// (Step 3's "flush on indexed table → NotImplemented" test was removed
// once Step 5 wired index maintenance. Positive index tests live below.)

// ── Step 4 — finish + Drop rollback ───────────────────────────────────────────

#[test]
fn finish_commits_and_returns_count() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    for i in 0..5 {
        app.append_row(&[Value::Int(i), Value::Text(format!("row{i}"))])
            .unwrap();
    }
    let n = app.finish().unwrap();
    assert_eq!(n, 5);
    // Rows are visible to subsequent queries on the same Db.
    let rows = db.query("SELECT id, v FROM t ORDER BY id").unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0][0], Value::Int(0));
    assert_eq!(rows[4][1], Value::Text("row4".into()));
}

#[test]
fn finish_flushes_remaining_buffered_rows() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    // No explicit flush — finish() must flush before committing.
    app.append_row(&[Value::Int(42)]).unwrap();
    let n = app.finish().unwrap();
    assert_eq!(n, 1);
    let rows = db.query("SELECT id FROM t").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(42));
}

#[test]
fn finish_with_no_appends_commits_empty_txn() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let app = db.appender("t").unwrap();
    let n = app.finish().unwrap();
    assert_eq!(n, 0);
    assert_eq!(db.query("SELECT COUNT(*) FROM t").unwrap()[0][0], Value::BigInt(0));
}

#[test]
fn drop_without_finish_rolls_back() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    {
        let mut app = db.appender("t").unwrap();
        app.append_row(&[Value::Int(1)]).unwrap();
        app.append_row(&[Value::Int(2)]).unwrap();
        // Drop without finish — txn rolled back; rows must NOT persist.
    }
    let count = db.query("SELECT COUNT(*) FROM t").unwrap()[0][0].clone();
    assert_eq!(count, Value::BigInt(0));
    // Subsequent appender works (no leaked txn state).
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(7)]).unwrap();
    app.finish().unwrap();
    let rows = db.query("SELECT id FROM t").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(7));
}

#[test]
fn drop_after_partial_flush_rolls_back_remaining_buffer() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    {
        let mut app = db.appender("t").unwrap();
        app.append_row(&[Value::Int(1)]).unwrap();
        app.flush().unwrap(); // row 1 written to heap, txn still open
        app.append_row(&[Value::Int(2)]).unwrap(); // buffered, not flushed
        // Drop here — rollback should undo BOTH the flushed row and the
        // buffered row (the whole txn aborts).
    }
    let count = db.query("SELECT COUNT(*) FROM t").unwrap()[0][0].clone();
    assert_eq!(count, Value::BigInt(0), "ALL rows including flushed ones roll back");
}

// ── Step 5 — Secondary index maintenance ──────────────────────────────────────

#[test]
fn appender_maintains_secondary_btree_index() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    db.run("CREATE INDEX idx_v ON t (v)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Text("alpha".into())])
        .unwrap();
    app.append_row(&[Value::Int(2), Value::Text("beta".into())])
        .unwrap();
    app.append_row(&[Value::Int(3), Value::Text("gamma".into())])
        .unwrap();
    app.finish().unwrap();
    let rows = db.query("SELECT id FROM t WHERE v = 'beta'").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(2));
}

#[test]
fn appender_unique_index_violation_rolls_back_batch() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    db.run("CREATE UNIQUE INDEX idx_v ON t (v)").unwrap();
    db.run("INSERT INTO t VALUES (1, 'dup')").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(2), Value::Text("ok".into())])
        .unwrap();
    app.append_row(&[Value::Int(3), Value::Text("dup".into())])
        .unwrap();
    let err = app.finish().unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "expected UniqueViolation, got {err:?}"
    );
    let n = db.query("SELECT COUNT(*) FROM t").unwrap()[0][0].clone();
    assert_eq!(n, Value::BigInt(1));
}

#[test]
fn appender_supports_table_with_multiple_indexes() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, age INT, name TEXT)").unwrap();
    db.run("CREATE INDEX idx_age ON t (age)").unwrap();
    db.run("CREATE INDEX idx_name ON t (name)").unwrap();
    let mut app = db.appender("t").unwrap();
    for i in 0..50 {
        app.append_row(&[
            Value::Int(i),
            Value::Int(20 + i % 30),
            Value::Text(format!("user_{i}")),
        ])
        .unwrap();
    }
    app.finish().unwrap();
    let by_age = db.query("SELECT COUNT(*) FROM t WHERE age = 25").unwrap();
    assert!(matches!(by_age[0][0], Value::BigInt(n) if n > 0));
    let by_name = db.query("SELECT id FROM t WHERE name = 'user_42'").unwrap();
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0][0], Value::Int(42));
}
