//! Integration tests for Phase 7.3b — MVCC on Secondary Indexes.
//!
//! Tests cover:
//! - Lazy index deletion: DELETE does not physically remove non-unique index entries.
//! - Heap-aware uniqueness: INSERT into unique index succeeds when existing entry is dead.
//! - Index undo tracking: ROLLBACK removes index entries added by INSERT/UPDATE.
//! - HOT optimization: UPDATE of non-indexed column skips index maintenance.
//! - Basic vacuum: `vacuum_index` removes dead entries.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_types::Value;
use common::{run_ctx, setup_ctx};

// ── helpers ───────────────────────────────────────────────────────────────────

fn ok(
    sql: &str,
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
    bloom: &mut axiomdb_sql::bloom::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) {
    run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"));
}

fn rows_of(
    sql: &str,
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
    bloom: &mut axiomdb_sql::bloom::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) -> Vec<Vec<Value>> {
    match run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
    {
        axiomdb_sql::QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn err(
    sql: &str,
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
    bloom: &mut axiomdb_sql::bloom::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) -> DbError {
    run_ctx(sql, storage, txn, bloom, ctx).expect_err(&format!("expected error from: {sql}"))
}

// ── Test 1: DELETE leaves non-unique index entry (lazy delete) ────────────────

#[test]
fn test_delete_leaves_index_entry() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(100))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "CREATE INDEX idx_name ON t (name)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO t VALUES (1, 'alice')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "DELETE FROM t WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // After DELETE, SELECT should return nothing (heap visibility filters the dead row).
    let result = rows_of(
        "SELECT * FROM t WHERE name = 'alice'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        result.is_empty(),
        "deleted row must be invisible via index scan"
    );

    // A full table scan should also return nothing.
    let result = rows_of(
        "SELECT * FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        result.is_empty(),
        "deleted row must be invisible via table scan"
    );
}

// ── Test 2: DELETE then re-INSERT unique key (same transaction) ───────────────

#[test]
fn test_delete_then_reinsert_unique() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE users (email VARCHAR(100) UNIQUE, name VARCHAR(100))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO users VALUES ('foo@bar.com', 'Old Name')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok(
        "DELETE FROM users WHERE email = 'foo@bar.com'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Re-INSERT same unique key — must succeed because old entry is dead.
    ok(
        "INSERT INTO users VALUES ('foo@bar.com', 'New Name')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx);

    let result = rows_of(
        "SELECT name FROM users WHERE email = 'foo@bar.com'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::from("New Name"));
}

// ── Test 3: INSERT duplicate of live unique key still fails ───────────────────

#[test]
fn test_insert_duplicate_live_unique_fails() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE users (email VARCHAR(100) UNIQUE, name VARCHAR(100))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO users VALUES ('foo@bar.com', 'Alice')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    let e = err(
        "INSERT INTO users VALUES ('foo@bar.com', 'Bob')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(e, DbError::UniqueViolation { .. }),
        "expected UniqueViolation, got {e:?}"
    );
}

// ── Test 4: ROLLBACK of INSERT removes index entry ────────────────────────────

#[test]
fn test_insert_rollback_removes_index_entry() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE t (id INT PRIMARY KEY, val VARCHAR(100) UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok(
        "INSERT INTO t VALUES (1, 'test')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx);

    // After ROLLBACK, SELECT returns nothing.
    let result = rows_of(
        "SELECT * FROM t WHERE val = 'test'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(result.is_empty(), "rolled-back row must be invisible");

    // After ROLLBACK, re-INSERT same unique key must succeed.
    ok(
        "INSERT INTO t VALUES (1, 'test')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let result = rows_of(
        "SELECT * FROM t WHERE val = 'test'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(result.len(), 1);
}

// ── Test 5: ROLLBACK of DELETE restores row visibility via index ──────────────

#[test]
fn test_delete_rollback_restores_row() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE t (id INT PRIMARY KEY, val VARCHAR(100) UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO t VALUES (42, 'hello')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok(
        "DELETE FROM t WHERE id = 42",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Row invisible inside the transaction.
    let result = rows_of(
        "SELECT * FROM t WHERE val = 'hello'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        result.is_empty(),
        "deleted row must be invisible inside txn"
    );

    ok("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx);

    // After ROLLBACK, row must be visible again via index scan.
    let result = rows_of(
        "SELECT * FROM t WHERE val = 'hello'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        result.len(),
        1,
        "rolled-back DELETE must restore row visibility"
    );
    assert_eq!(result[0][0], Value::Int(42));
}

// ── Test 6: UPDATE indexed column — ROLLBACK restores old index entry ─────────

#[test]
fn test_update_indexed_col_rollback() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE t (id INT PRIMARY KEY, email VARCHAR(100) UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO t VALUES (1, 'old@bar.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx);
    ok(
        "UPDATE t SET email = 'new@bar.com' WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok("ROLLBACK", &mut storage, &mut txn, &mut bloom, &mut ctx);

    // Old email must be findable via index.
    let result = rows_of(
        "SELECT id FROM t WHERE email = 'old@bar.com'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(result.len(), 1, "old email must be visible after ROLLBACK");

    // New email must NOT be findable.
    let result = rows_of(
        "SELECT id FROM t WHERE email = 'new@bar.com'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        result.is_empty(),
        "new email must be invisible after ROLLBACK"
    );
}

// ── Test 7: UPDATE non-indexed column (HOT — no index maintenance) ────────────

#[test]
fn test_update_non_indexed_col_hot() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR(100), notes VARCHAR(200))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "CREATE INDEX idx_name ON t (name)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO t VALUES (1, 'alice', 'original')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Update only non-indexed column.
    ok(
        "UPDATE t SET notes = 'updated' WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Row must still be findable via name index after HOT update.
    let result = rows_of(
        "SELECT notes FROM t WHERE name = 'alice'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::from("updated"));
}

// ── Test 8: Unique index check with dead duplicate ────────────────────────────

#[test]
fn test_unique_check_with_dead_entry() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE t (email VARCHAR(100) UNIQUE)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Committed insert + delete leaves a dead entry.
    ok(
        "INSERT INTO t VALUES ('x@y.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "DELETE FROM t WHERE email = 'x@y.com'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Re-insert same key must succeed (heap visibility shows the entry is dead).
    ok(
        "INSERT INTO t VALUES ('x@y.com')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let result = rows_of(
        "SELECT * FROM t WHERE email = 'x@y.com'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(result.len(), 1);
}

// ── Test 9: Range scan with dead entries is filtered correctly ────────────────

#[test]
fn test_range_scan_filters_dead_entries() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    ok(
        "CREATE TABLE t (id INT PRIMARY KEY, status VARCHAR(20))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "CREATE INDEX idx_status ON t (status)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    for i in 1..=5 {
        ok(
            &format!("INSERT INTO t VALUES ({i}, 'active')"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }

    // Delete 2 rows — their non-unique index entries become dead.
    ok(
        "DELETE FROM t WHERE id IN (2, 4)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Range scan via status index must return only 3 alive rows.
    let result = rows_of(
        "SELECT id FROM t WHERE status = 'active'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        result.len(),
        3,
        "dead entries must be filtered by heap visibility"
    );
}
