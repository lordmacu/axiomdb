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
