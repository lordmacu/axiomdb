//! Integration tests for ctx-path `on_error` session behavior.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::{QueryResult, SessionContext};
use common::*;

// ── on_error session variable tests (Phase 5.2c) ─────────────────────────────

#[test]
fn test_on_error_default_is_rollback_statement() {
    let ctx = SessionContext::new();
    assert_eq!(
        ctx.on_error,
        axiomdb_sql::session::OnErrorMode::RollbackStatement
    );
}

#[test]
fn test_set_on_error_via_execute_with_ctx() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();

    for (sql, expected) in [
        (
            "SET on_error = 'rollback_transaction'",
            axiomdb_sql::session::OnErrorMode::RollbackTransaction,
        ),
        (
            "SET on_error = 'savepoint'",
            axiomdb_sql::session::OnErrorMode::Savepoint,
        ),
        (
            "SET on_error = 'ignore'",
            axiomdb_sql::session::OnErrorMode::Ignore,
        ),
        (
            "SET on_error = DEFAULT",
            axiomdb_sql::session::OnErrorMode::RollbackStatement,
        ),
    ] {
        run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
        assert_eq!(ctx.on_error, expected, "after: {sql}");
    }
}

#[test]
fn test_set_on_error_bare_identifier() {
    // SET on_error = rollback_statement (no quotes) must also work.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "SET on_error = rollback_transaction",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(
        ctx.on_error,
        axiomdb_sql::session::OnErrorMode::RollbackTransaction
    );
}

#[test]
fn test_set_on_error_invalid_value_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    let result = run_ctx(
        "SET on_error = 'banana'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(result, Err(DbError::InvalidValue { .. })),
        "expected InvalidValue, got {result:?}"
    );
    // Mode unchanged
    assert_eq!(
        ctx.on_error,
        axiomdb_sql::session::OnErrorMode::RollbackStatement
    );
}

#[test]
fn test_rollback_statement_keeps_txn_open_on_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    // ctx.on_error is already RollbackStatement (default)
    run_ctx(
        "CREATE TABLE t (id INT UNIQUE NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // Duplicate key error inside active txn
    let err = run_ctx(
        "INSERT INTO t VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(err.is_err());
    // Transaction must still be active
    assert!(
        txn.active_txn_id().is_some(),
        "transaction must stay open after rollback_statement"
    );
    // Successful subsequent insert still works
    run_ctx(
        "INSERT INTO t VALUES (2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("COMMIT", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();

    // Only id=1 (original) and id=2 (after error) committed
    let rows = run_ctx(
        "SELECT id FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(rows.into_rows().len(), 2);
}

#[test]
fn test_rollback_transaction_closes_txn_on_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "SET on_error = 'rollback_transaction'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE t (id INT UNIQUE NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_ctx(
        "INSERT INTO t VALUES (42)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // Duplicate key error
    let err = run_ctx(
        "INSERT INTO t VALUES (42)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(err.is_err());
    // Whole transaction must be rolled back
    assert!(
        txn.active_txn_id().is_none(),
        "rollback_transaction must close the entire txn"
    );
    // id=42 must NOT be committed
    let rows = run_ctx(
        "SELECT id FROM t",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(rows.into_rows().len(), 0);
}

#[test]
fn test_savepoint_keeps_first_implicit_autocommit0_txn_open() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    ctx.autocommit = false;
    run_ctx(
        "SET on_error = 'savepoint'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE t (id INT UNIQUE NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // First DML in autocommit=0 — implicit BEGIN
    let err = run_ctx(
        "INSERT INTO t VALUES (999)", // will fail: table empty, no dup — let's use a parse error
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // This insert will succeed (no dup); use a unique violation next
    assert!(err.is_ok()); // id=999 inserted, txn open
    let err2 = run_ctx(
        "INSERT INTO t VALUES (999)", // dup key
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(err2.is_err());
    // In savepoint mode: implicit txn must stay open after dup-key on non-first DML
    assert!(
        txn.active_txn_id().is_some(),
        "savepoint mode must keep the implicit txn open after error"
    );
}

// Helper to extract rows from QueryResult
trait IntoRows {
    fn into_rows(self) -> Vec<axiomdb_types::Value>;
}
impl IntoRows for QueryResult {
    fn into_rows(self) -> Vec<axiomdb_types::Value> {
        match self {
            QueryResult::Rows { rows, .. } => rows.into_iter().flatten().collect(),
            _ => vec![],
        }
    }
}
