//! Phase 7.13 / 40.10 — isolation tests on the shared database handle.
//!
//! These tests verify MVCC isolation semantics using the production
//! `SharedDatabase` handle. Under the current single-TxnManager model, each
//! test validates snapshot behavior within a single session — the same
//! guarantees users experience through one MySQL connection.
//!
//! True cross-session isolation (two connections seeing different snapshots
//! simultaneously) requires multiple TxnManagers, which is a future
//! architecture change (Phase 13.7+). The lower-level `integration_isolation.rs`
//! tests in `axiomdb-sql` already validate the snapshot logic.

use axiomdb_network::mysql::SharedDatabase;
use axiomdb_sql::{SchemaCache, SessionContext};
use axiomdb_types::Value;

fn open_test_db() -> (tempfile::TempDir, SharedDatabase) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = SharedDatabase::open(dir.path()).expect("open test db");
    (dir, db)
}

fn exec_q(
    db: &SharedDatabase,
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

#[test]
fn test_rr_frozen_snapshot_across_statements() {
    let (_dir, db) = open_test_db();

    let mut s = SessionContext::new();
    exec_q(&db, "CREATE TABLE rr_frozen (id INT, val INT)", &mut s);
    exec_q(&db, "INSERT INTO rr_frozen VALUES (1, 100)", &mut s);

    // RR transaction: snapshot captured at BEGIN.
    exec_q(&db, "SET transaction_isolation = 'REPEATABLE-READ'", &mut s);
    exec_q(&db, "BEGIN", &mut s);

    let r1 = exec_q(&db, "SELECT val FROM rr_frozen WHERE id = 1", &mut s);
    assert_eq!(first_int(r1), 100);

    // Own UPDATE within the same txn.
    exec_q(&db, "UPDATE rr_frozen SET val = 200 WHERE id = 1", &mut s);

    // Read-your-own-writes: see the update.
    let r2 = exec_q(&db, "SELECT val FROM rr_frozen WHERE id = 1", &mut s);
    assert_eq!(first_int(r2), 200, "must see own writes in RR");

    exec_q(&db, "COMMIT", &mut s);
}

// ── Read committed: each statement sees latest committed ─────────────────────

#[test]
fn test_rc_fresh_snapshot_per_statement() {
    let (_dir, db) = open_test_db();

    let mut s = SessionContext::new();
    exec_q(&db, "CREATE TABLE rc_fresh (id INT, val INT)", &mut s);
    exec_q(&db, "INSERT INTO rc_fresh VALUES (1, 100)", &mut s);

    exec_q(&db, "SET transaction_isolation = 'READ-COMMITTED'", &mut s);
    exec_q(&db, "BEGIN", &mut s);

    let r1 = exec_q(&db, "SELECT val FROM rc_fresh WHERE id = 1", &mut s);
    assert_eq!(first_int(r1), 100);

    // Update within the same transaction.
    exec_q(&db, "UPDATE rc_fresh SET val = 200 WHERE id = 1", &mut s);

    let r2 = exec_q(&db, "SELECT val FROM rc_fresh WHERE id = 1", &mut s);
    assert_eq!(first_int(r2), 200);

    exec_q(&db, "COMMIT", &mut s);
}

// ── Rollback hides all modifications ─────────────────────────────────────────

#[test]
fn test_rollback_hides_insert_update_delete() {
    let (_dir, db) = open_test_db();

    let mut s = SessionContext::new();
    exec_q(&db, "CREATE TABLE rb_all (id INT, val INT)", &mut s);
    exec_q(&db, "INSERT INTO rb_all VALUES (1, 100)", &mut s);
    exec_q(&db, "INSERT INTO rb_all VALUES (2, 200)", &mut s);

    // Transaction: insert, update, delete — then rollback.
    exec_q(&db, "BEGIN", &mut s);
    exec_q(&db, "INSERT INTO rb_all VALUES (3, 300)", &mut s);
    exec_q(&db, "UPDATE rb_all SET val = 999 WHERE id = 1", &mut s);
    exec_q(&db, "DELETE FROM rb_all WHERE id = 2", &mut s);

    // Within txn: see all changes.
    let r1 = exec_q(&db, "SELECT id, val FROM rb_all", &mut s);
    assert_eq!(row_count(r1), 2); // row 2 deleted, row 3 inserted

    exec_q(&db, "ROLLBACK", &mut s);

    // After rollback: original state restored.
    let r2 = exec_q(&db, "SELECT id, val FROM rb_all", &mut s);
    assert_eq!(row_count(r2), 2);

    let r3 = exec_q(&db, "SELECT val FROM rb_all WHERE id = 1", &mut s);
    assert_eq!(first_int(r3), 100, "UPDATE must be rolled back");

    let r4 = exec_q(&db, "SELECT val FROM rb_all WHERE id = 2", &mut s);
    assert_eq!(first_int(r4), 200, "DELETE must be rolled back");
}

// ── Savepoint partial rollback preserves earlier work ────────────────────────

#[test]
fn test_savepoint_partial_rollback() {
    let (_dir, db) = open_test_db();

    let mut s = SessionContext::new();
    exec_q(&db, "CREATE TABLE sp_iso (id INT)", &mut s);

    exec_q(&db, "BEGIN", &mut s);
    exec_q(&db, "INSERT INTO sp_iso VALUES (1)", &mut s);
    exec_q(&db, "SAVEPOINT sp1", &mut s);
    exec_q(&db, "INSERT INTO sp_iso VALUES (2)", &mut s);
    exec_q(&db, "INSERT INTO sp_iso VALUES (3)", &mut s);

    // 3 rows visible within txn.
    let r1 = exec_q(&db, "SELECT id FROM sp_iso", &mut s);
    assert_eq!(row_count(r1), 3);

    // Rollback to sp1 — rows 2 and 3 undone.
    exec_q(&db, "ROLLBACK TO sp1", &mut s);
    let r2 = exec_q(&db, "SELECT id FROM sp_iso", &mut s);
    assert_eq!(row_count(r2), 1, "only row 1 survives savepoint rollback");

    // Can continue working after savepoint rollback.
    exec_q(&db, "INSERT INTO sp_iso VALUES (4)", &mut s);
    exec_q(&db, "COMMIT", &mut s);

    let r3 = exec_q(&db, "SELECT id FROM sp_iso", &mut s);
    assert_eq!(row_count(r3), 2, "rows 1 and 4 should be committed");
}

// ── Nested savepoints ────────────────────────────────────────────────────────

#[test]
fn test_nested_savepoints() {
    let (_dir, db) = open_test_db();

    let mut s = SessionContext::new();
    exec_q(&db, "CREATE TABLE nested_sp (id INT)", &mut s);

    exec_q(&db, "BEGIN", &mut s);
    exec_q(&db, "INSERT INTO nested_sp VALUES (1)", &mut s);
    exec_q(&db, "SAVEPOINT a", &mut s);
    exec_q(&db, "INSERT INTO nested_sp VALUES (2)", &mut s);
    exec_q(&db, "SAVEPOINT b", &mut s);
    exec_q(&db, "INSERT INTO nested_sp VALUES (3)", &mut s);

    // Rollback to b — only row 3 undone.
    exec_q(&db, "ROLLBACK TO b", &mut s);
    let r1 = exec_q(&db, "SELECT id FROM nested_sp", &mut s);
    assert_eq!(row_count(r1), 2, "only row 3 should be undone");

    // Rollback to a — row 2 also undone.
    exec_q(&db, "ROLLBACK TO a", &mut s);
    let r2 = exec_q(&db, "SELECT id FROM nested_sp", &mut s);
    assert_eq!(row_count(r2), 1, "rows 2 and 3 should be undone");

    exec_q(&db, "COMMIT", &mut s);
}

// ── RELEASE savepoint ────────────────────────────────────────────────────────

#[test]
fn test_release_savepoint() {
    let (_dir, db) = open_test_db();

    let mut s = SessionContext::new();
    exec_q(&db, "CREATE TABLE rel_sp (id INT)", &mut s);

    exec_q(&db, "BEGIN", &mut s);
    exec_q(&db, "INSERT INTO rel_sp VALUES (1)", &mut s);
    exec_q(&db, "SAVEPOINT sp1", &mut s);
    exec_q(&db, "INSERT INTO rel_sp VALUES (2)", &mut s);

    // Release sp1 — changes persist, savepoint destroyed.
    exec_q(&db, "RELEASE sp1", &mut s);

    // ROLLBACK TO sp1 should fail — savepoint no longer exists.
    let mut cache = SchemaCache::new();
    let result = db.execute_query("ROLLBACK TO sp1", &mut s, &mut cache);
    assert!(result.is_err(), "ROLLBACK TO released savepoint must fail");

    exec_q(&db, "COMMIT", &mut s);

    // Both rows committed.
    let r = exec_q(&db, "SELECT id FROM rel_sp", &mut s);
    assert_eq!(row_count(r), 2);
}

// ── Autocommit: each statement is its own transaction ────────────────────────

#[test]
fn test_autocommit_isolation() {
    let (_dir, db) = open_test_db();

    let mut s = SessionContext::new();
    exec_q(&db, "CREATE TABLE ac_iso (id INT, val INT)", &mut s);
    exec_q(&db, "INSERT INTO ac_iso VALUES (1, 100)", &mut s);

    // Autocommit UPDATE — immediately visible to next statement.
    exec_q(&db, "UPDATE ac_iso SET val = 200 WHERE id = 1", &mut s);
    let r = exec_q(&db, "SELECT val FROM ac_iso WHERE id = 1", &mut s);
    assert_eq!(first_int(r), 200, "autocommit update immediately visible");
}

// ── DELETE visibility: deleted rows invisible after commit ───────────────────

#[test]
fn test_delete_visibility_after_commit() {
    let (_dir, db) = open_test_db();

    let mut s = SessionContext::new();
    exec_q(&db, "CREATE TABLE dv (id INT)", &mut s);
    for i in 1..=10 {
        exec_q(&db, &format!("INSERT INTO dv VALUES ({i})"), &mut s);
    }

    exec_q(&db, "BEGIN", &mut s);
    exec_q(&db, "DELETE FROM dv WHERE id <= 5", &mut s);

    // Within txn: 5 rows visible.
    let r1 = exec_q(&db, "SELECT id FROM dv", &mut s);
    assert_eq!(row_count(r1), 5);

    exec_q(&db, "COMMIT", &mut s);

    // After commit: still 5.
    let r2 = exec_q(&db, "SELECT id FROM dv", &mut s);
    assert_eq!(row_count(r2), 5);
}

// ── Index-based query respects isolation ──────────────────────────────────────

#[test]
fn test_index_query_isolation() {
    let (_dir, db) = open_test_db();

    let mut s = SessionContext::new();
    exec_q(&db, "CREATE TABLE idx_iso (id INT, status TEXT)", &mut s);
    exec_q(&db, "CREATE INDEX idx_s ON idx_iso (status)", &mut s);
    for i in 1..=5 {
        exec_q(
            &db,
            &format!("INSERT INTO idx_iso VALUES ({i}, 'active')"),
            &mut s,
        );
    }

    // Delete via index path, then verify via index path.
    exec_q(
        &db,
        "DELETE FROM idx_iso WHERE status = 'active' AND id <= 2",
        &mut s,
    );

    let r = exec_q(
        &db,
        "SELECT id FROM idx_iso WHERE status = 'active'",
        &mut s,
    );
    assert_eq!(row_count(r), 3, "index scan must respect delete visibility");
}
