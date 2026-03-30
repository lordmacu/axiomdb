//! Phase 7.13 — Isolation level tests at the Database layer.
//!
//! These tests verify MVCC isolation semantics using the production `Database`
//! type. Under the current single-TxnManager model, each test validates
//! snapshot behavior within a single session — the same guarantees users
//! experience through one MySQL connection.
//!
//! True cross-session isolation (two connections seeing different snapshots
//! simultaneously) requires multiple TxnManagers, which is a future
//! architecture change (Phase 13.7+). The lower-level `integration_isolation.rs`
//! tests in `axiomdb-sql` already validate the snapshot logic.

use std::sync::Arc;
use tokio::sync::RwLock;

use axiomdb_network::mysql::Database;
use axiomdb_sql::{SchemaCache, SessionContext};
use axiomdb_types::Value;

fn open_test_db() -> (tempfile::TempDir, Arc<RwLock<Database>>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Database::open(dir.path()).expect("open test db");
    (dir, Arc::new(RwLock::new(db)))
}

fn exec_q(
    db: &mut Database,
    sql: &str,
    session: &mut SessionContext,
) -> axiomdb_sql::result::QueryResult {
    let mut cache = SchemaCache::new();
    let (result, _) = db
        .execute_query(sql, session, &mut cache)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}: {e}"));
    result
}

fn row_count(result: axiomdb_sql::result::QueryResult) -> usize {
    match result {
        axiomdb_sql::result::QueryResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    }
}

fn first_int(result: axiomdb_sql::result::QueryResult) -> i32 {
    match result {
        axiomdb_sql::result::QueryResult::Rows { rows, .. } => match &rows[0][0] {
            Value::Int(v) => *v,
            Value::BigInt(v) => *v as i32,
            other => panic!("expected int, got {other:?}"),
        },
        _ => panic!("expected rows"),
    }
}

// ── Repeatable read: snapshot frozen at BEGIN ─────────────────────────────────

#[tokio::test]
async fn test_rr_frozen_snapshot_across_statements() {
    let (_dir, db) = open_test_db();
    let mut guard = db.write().await;

    let mut s = SessionContext::new();
    exec_q(
        &mut guard,
        "CREATE TABLE rr_frozen (id INT, val INT)",
        &mut s,
    );
    exec_q(&mut guard, "INSERT INTO rr_frozen VALUES (1, 100)", &mut s);

    // RR transaction: snapshot captured at BEGIN.
    exec_q(
        &mut guard,
        "SET transaction_isolation = 'REPEATABLE-READ'",
        &mut s,
    );
    exec_q(&mut guard, "BEGIN", &mut s);

    let r1 = exec_q(&mut guard, "SELECT val FROM rr_frozen WHERE id = 1", &mut s);
    assert_eq!(first_int(r1), 100);

    // Own UPDATE within the same txn.
    exec_q(
        &mut guard,
        "UPDATE rr_frozen SET val = 200 WHERE id = 1",
        &mut s,
    );

    // Read-your-own-writes: see the update.
    let r2 = exec_q(&mut guard, "SELECT val FROM rr_frozen WHERE id = 1", &mut s);
    assert_eq!(first_int(r2), 200, "must see own writes in RR");

    exec_q(&mut guard, "COMMIT", &mut s);
}

// ── Read committed: each statement sees latest committed ─────────────────────

#[tokio::test]
async fn test_rc_fresh_snapshot_per_statement() {
    let (_dir, db) = open_test_db();
    let mut guard = db.write().await;

    let mut s = SessionContext::new();
    exec_q(
        &mut guard,
        "CREATE TABLE rc_fresh (id INT, val INT)",
        &mut s,
    );
    exec_q(&mut guard, "INSERT INTO rc_fresh VALUES (1, 100)", &mut s);

    exec_q(
        &mut guard,
        "SET transaction_isolation = 'READ-COMMITTED'",
        &mut s,
    );
    exec_q(&mut guard, "BEGIN", &mut s);

    let r1 = exec_q(&mut guard, "SELECT val FROM rc_fresh WHERE id = 1", &mut s);
    assert_eq!(first_int(r1), 100);

    // Update within the same transaction.
    exec_q(
        &mut guard,
        "UPDATE rc_fresh SET val = 200 WHERE id = 1",
        &mut s,
    );

    let r2 = exec_q(&mut guard, "SELECT val FROM rc_fresh WHERE id = 1", &mut s);
    assert_eq!(first_int(r2), 200);

    exec_q(&mut guard, "COMMIT", &mut s);
}

// ── Rollback hides all modifications ─────────────────────────────────────────

#[tokio::test]
async fn test_rollback_hides_insert_update_delete() {
    let (_dir, db) = open_test_db();
    let mut guard = db.write().await;

    let mut s = SessionContext::new();
    exec_q(&mut guard, "CREATE TABLE rb_all (id INT, val INT)", &mut s);
    exec_q(&mut guard, "INSERT INTO rb_all VALUES (1, 100)", &mut s);
    exec_q(&mut guard, "INSERT INTO rb_all VALUES (2, 200)", &mut s);

    // Transaction: insert, update, delete — then rollback.
    exec_q(&mut guard, "BEGIN", &mut s);
    exec_q(&mut guard, "INSERT INTO rb_all VALUES (3, 300)", &mut s);
    exec_q(
        &mut guard,
        "UPDATE rb_all SET val = 999 WHERE id = 1",
        &mut s,
    );
    exec_q(&mut guard, "DELETE FROM rb_all WHERE id = 2", &mut s);

    // Within txn: see all changes.
    let r1 = exec_q(&mut guard, "SELECT id, val FROM rb_all", &mut s);
    assert_eq!(row_count(r1), 2); // row 2 deleted, row 3 inserted

    exec_q(&mut guard, "ROLLBACK", &mut s);

    // After rollback: original state restored.
    let r2 = exec_q(&mut guard, "SELECT id, val FROM rb_all", &mut s);
    assert_eq!(row_count(r2), 2);

    let r3 = exec_q(&mut guard, "SELECT val FROM rb_all WHERE id = 1", &mut s);
    assert_eq!(first_int(r3), 100, "UPDATE must be rolled back");

    let r4 = exec_q(&mut guard, "SELECT val FROM rb_all WHERE id = 2", &mut s);
    assert_eq!(first_int(r4), 200, "DELETE must be rolled back");
}

// ── Savepoint partial rollback preserves earlier work ────────────────────────

#[tokio::test]
async fn test_savepoint_partial_rollback() {
    let (_dir, db) = open_test_db();
    let mut guard = db.write().await;

    let mut s = SessionContext::new();
    exec_q(&mut guard, "CREATE TABLE sp_iso (id INT)", &mut s);

    exec_q(&mut guard, "BEGIN", &mut s);
    exec_q(&mut guard, "INSERT INTO sp_iso VALUES (1)", &mut s);
    exec_q(&mut guard, "SAVEPOINT sp1", &mut s);
    exec_q(&mut guard, "INSERT INTO sp_iso VALUES (2)", &mut s);
    exec_q(&mut guard, "INSERT INTO sp_iso VALUES (3)", &mut s);

    // 3 rows visible within txn.
    let r1 = exec_q(&mut guard, "SELECT id FROM sp_iso", &mut s);
    assert_eq!(row_count(r1), 3);

    // Rollback to sp1 — rows 2 and 3 undone.
    exec_q(&mut guard, "ROLLBACK TO sp1", &mut s);
    let r2 = exec_q(&mut guard, "SELECT id FROM sp_iso", &mut s);
    assert_eq!(row_count(r2), 1, "only row 1 survives savepoint rollback");

    // Can continue working after savepoint rollback.
    exec_q(&mut guard, "INSERT INTO sp_iso VALUES (4)", &mut s);
    exec_q(&mut guard, "COMMIT", &mut s);

    let r3 = exec_q(&mut guard, "SELECT id FROM sp_iso", &mut s);
    assert_eq!(row_count(r3), 2, "rows 1 and 4 should be committed");
}

// ── Nested savepoints ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_nested_savepoints() {
    let (_dir, db) = open_test_db();
    let mut guard = db.write().await;

    let mut s = SessionContext::new();
    exec_q(&mut guard, "CREATE TABLE nested_sp (id INT)", &mut s);

    exec_q(&mut guard, "BEGIN", &mut s);
    exec_q(&mut guard, "INSERT INTO nested_sp VALUES (1)", &mut s);
    exec_q(&mut guard, "SAVEPOINT a", &mut s);
    exec_q(&mut guard, "INSERT INTO nested_sp VALUES (2)", &mut s);
    exec_q(&mut guard, "SAVEPOINT b", &mut s);
    exec_q(&mut guard, "INSERT INTO nested_sp VALUES (3)", &mut s);

    // Rollback to b — only row 3 undone.
    exec_q(&mut guard, "ROLLBACK TO b", &mut s);
    let r1 = exec_q(&mut guard, "SELECT id FROM nested_sp", &mut s);
    assert_eq!(row_count(r1), 2, "only row 3 should be undone");

    // Rollback to a — row 2 also undone.
    exec_q(&mut guard, "ROLLBACK TO a", &mut s);
    let r2 = exec_q(&mut guard, "SELECT id FROM nested_sp", &mut s);
    assert_eq!(row_count(r2), 1, "rows 2 and 3 should be undone");

    exec_q(&mut guard, "COMMIT", &mut s);
}

// ── RELEASE savepoint ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_release_savepoint() {
    let (_dir, db) = open_test_db();
    let mut guard = db.write().await;

    let mut s = SessionContext::new();
    exec_q(&mut guard, "CREATE TABLE rel_sp (id INT)", &mut s);

    exec_q(&mut guard, "BEGIN", &mut s);
    exec_q(&mut guard, "INSERT INTO rel_sp VALUES (1)", &mut s);
    exec_q(&mut guard, "SAVEPOINT sp1", &mut s);
    exec_q(&mut guard, "INSERT INTO rel_sp VALUES (2)", &mut s);

    // Release sp1 — changes persist, savepoint destroyed.
    exec_q(&mut guard, "RELEASE sp1", &mut s);

    // ROLLBACK TO sp1 should fail — savepoint no longer exists.
    let mut cache = SchemaCache::new();
    let result = guard.execute_query("ROLLBACK TO sp1", &mut s, &mut cache);
    assert!(result.is_err(), "ROLLBACK TO released savepoint must fail");

    exec_q(&mut guard, "COMMIT", &mut s);

    // Both rows committed.
    let r = exec_q(&mut guard, "SELECT id FROM rel_sp", &mut s);
    assert_eq!(row_count(r), 2);
}

// ── Autocommit: each statement is its own transaction ────────────────────────

#[tokio::test]
async fn test_autocommit_isolation() {
    let (_dir, db) = open_test_db();
    let mut guard = db.write().await;

    let mut s = SessionContext::new();
    exec_q(&mut guard, "CREATE TABLE ac_iso (id INT, val INT)", &mut s);
    exec_q(&mut guard, "INSERT INTO ac_iso VALUES (1, 100)", &mut s);

    // Autocommit UPDATE — immediately visible to next statement.
    exec_q(
        &mut guard,
        "UPDATE ac_iso SET val = 200 WHERE id = 1",
        &mut s,
    );
    let r = exec_q(&mut guard, "SELECT val FROM ac_iso WHERE id = 1", &mut s);
    assert_eq!(first_int(r), 200, "autocommit update immediately visible");
}

// ── DELETE visibility: deleted rows invisible after commit ───────────────────

#[tokio::test]
async fn test_delete_visibility_after_commit() {
    let (_dir, db) = open_test_db();
    let mut guard = db.write().await;

    let mut s = SessionContext::new();
    exec_q(&mut guard, "CREATE TABLE dv (id INT)", &mut s);
    for i in 1..=10 {
        exec_q(&mut guard, &format!("INSERT INTO dv VALUES ({i})"), &mut s);
    }

    exec_q(&mut guard, "BEGIN", &mut s);
    exec_q(&mut guard, "DELETE FROM dv WHERE id <= 5", &mut s);

    // Within txn: 5 rows visible.
    let r1 = exec_q(&mut guard, "SELECT id FROM dv", &mut s);
    assert_eq!(row_count(r1), 5);

    exec_q(&mut guard, "COMMIT", &mut s);

    // After commit: still 5.
    let r2 = exec_q(&mut guard, "SELECT id FROM dv", &mut s);
    assert_eq!(row_count(r2), 5);
}

// ── Index-based query respects isolation ──────────────────────────────────────

#[tokio::test]
async fn test_index_query_isolation() {
    let (_dir, db) = open_test_db();
    let mut guard = db.write().await;

    let mut s = SessionContext::new();
    exec_q(
        &mut guard,
        "CREATE TABLE idx_iso (id INT, status TEXT)",
        &mut s,
    );
    exec_q(&mut guard, "CREATE INDEX idx_s ON idx_iso (status)", &mut s);
    for i in 1..=5 {
        exec_q(
            &mut guard,
            &format!("INSERT INTO idx_iso VALUES ({i}, 'active')"),
            &mut s,
        );
    }

    // Delete via index path, then verify via index path.
    exec_q(
        &mut guard,
        "DELETE FROM idx_iso WHERE status = 'active' AND id <= 2",
        &mut s,
    );

    let r = exec_q(
        &mut guard,
        "SELECT id FROM idx_iso WHERE status = 'active'",
        &mut s,
    );
    assert_eq!(row_count(r), 3, "index scan must respect delete visibility");
}
