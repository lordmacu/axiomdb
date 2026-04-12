//! Integration tests for `ALTER TABLE ... AUTO_INCREMENT = N`.
//!
//! Covers GAP-C.6: MySQL-compatible semantics — N is honored when greater
//! than the current max on the AUTO_INCREMENT column; otherwise silently
//! ignored.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn next_auto_id(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> i64 {
    let result = run_ctx(sql, storage, txn, bloom, ctx).unwrap();
    let QueryResult::Affected { last_insert_id, .. } = result else {
        panic!("expected Affected, got {result:?}");
    };
    last_insert_id.expect("AUTO_INCREMENT must produce last_insert_id") as i64
}

#[test]
fn alter_auto_increment_advances_counter_on_empty_table() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "ALTER TABLE t AUTO_INCREMENT = 100",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let id = next_auto_id(
        "INSERT INTO t (name) VALUES ('a')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(id, 100, "next insert must start at N=100");
}

#[test]
fn alter_auto_increment_ignored_when_below_current_max() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t (id, name) VALUES (50, 'a')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // N=10 is below max=50 → MySQL silently ignores, next id = max+1 = 51.
    run_ctx(
        "ALTER TABLE t AUTO_INCREMENT = 10",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let id = next_auto_id(
        "INSERT INTO t (name) VALUES ('b')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(id, 51, "N below current max must be ignored");
}

#[test]
fn alter_auto_increment_honors_when_above_max() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t (id, name) VALUES (5, 'a')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "ALTER TABLE t AUTO_INCREMENT = 1000",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let id = next_auto_id(
        "INSERT INTO t (name) VALUES ('b')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(id, 1000);

    // Subsequent inserts continue from there.
    let id2 = next_auto_id(
        "INSERT INTO t (name) VALUES ('c')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(id2, 1001);

    // Sanity check: row with id=1000 actually inserted.
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM t WHERE name = 'b'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap() else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Int(1000) | Value::BigInt(1000)));
}
