//! Integration tests for column-level `ON UPDATE expr` auto-refresh —
//! MySQL's ubiquitous `updated_at TIMESTAMP ON UPDATE CURRENT_TIMESTAMP`
//! audit pattern.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn value_of(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Value {
    let QueryResult::Rows { rows, .. } = run_ctx(sql, storage, txn, bloom, ctx).unwrap() else {
        panic!("expected rows");
    };
    rows[0][0].clone()
}

#[test]
fn on_update_current_timestamp_refreshes_column_on_update() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, note TEXT, updated_at TIMESTAMP ON UPDATE CURRENT_TIMESTAMP)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 'first', NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let before = value_of(
        "SELECT updated_at FROM t WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(matches!(before, Value::Null));

    // Update a different column — updated_at must auto-refresh to now().
    run_ctx(
        "UPDATE t SET note = 'second' WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let after = value_of(
        "SELECT updated_at FROM t WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(after, Value::Timestamp(_)),
        "updated_at must be auto-refreshed to a timestamp, got {after:?}",
    );
}

#[test]
fn on_update_is_suppressed_when_column_is_explicitly_assigned() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, v INT ON UPDATE 42)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 0)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Explicit assignment wins over ON UPDATE expression.
    run_ctx(
        "UPDATE t SET v = 99 WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let v = value_of(
        "SELECT v FROM t WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(v, Value::Int(99) | Value::BigInt(99)),
        "explicit assignment must win, got {v:?}",
    );
}

#[test]
fn column_without_on_update_keeps_old_value() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, a INT, b INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10, 20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "UPDATE t SET a = 11 WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let b = value_of(
        "SELECT b FROM t WHERE id = 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(matches!(b, Value::Int(20) | Value::BigInt(20)));
}
