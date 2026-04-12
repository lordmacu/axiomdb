//! Integration tests for correlated subqueries at nesting depth > 1.
//!
//! Covers GAP-C.8.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t1 (id INT, code INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE t2 (id INT, t1_id INT, flag INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE TABLE t3 (id INT, t2_id INT, code INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t1 VALUES (1, 100), (2, 200)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t2 VALUES (10, 1, 1), (11, 1, 0), (20, 2, 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // t3 rows where code matches t1.code for matching t2
    run_ctx(
        "INSERT INTO t3 VALUES (100, 10, 100), (101, 10, 999), (200, 20, 200)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    (storage, txn, bloom, ctx)
}

fn rows_of(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Vec<Vec<Value>> {
    let QueryResult::Rows { rows, .. } = run_ctx(sql, storage, txn, bloom, ctx).unwrap() else {
        panic!("expected rows");
    };
    rows
}

fn as_int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n as i64,
        Value::BigInt(n) => *n,
        other => panic!("not int: {other:?}"),
    }
}

#[test]
fn depth_2_correlation_via_nested_exists() {
    // For each t1, EXISTS a t2 with t1_id = t1.id AND EXISTS a t3 with
    // t2_id = t2.id AND code = t1.code (outermost reference at depth 1).
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();

    let rows = rows_of(
        "SELECT id FROM t1 \
         WHERE EXISTS (\
             SELECT 1 FROM t2 WHERE t2.t1_id = t1.id AND EXISTS (\
                 SELECT 1 FROM t3 WHERE t3.t2_id = t2.id AND t3.code = t1.code\
             )\
         ) \
         ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // t1.id=1, code=100: needs t2 with t1_id=1 that has t3 with code=100.
    //   t2.id=10 → t3 has (100,10,100)✓ → pass
    // t1.id=2, code=200: needs t2 with t1_id=2 (id=20) and t3.code=200, t2_id=20
    //   t3 has (200,20,200)✓ → pass
    // Both rows should come back.
    assert_eq!(rows.len(), 2);
    assert_eq!(as_int(&rows[0][0]), 1);
    assert_eq!(as_int(&rows[1][0]), 2);
}

#[test]
fn depth_2_correlation_negative_case() {
    // Same shape, but change t1 so that one row has no matching deep t3.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    run_ctx(
        "INSERT INTO t1 VALUES (3, 999)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let rows = rows_of(
        "SELECT id FROM t1 \
         WHERE EXISTS (\
             SELECT 1 FROM t2 WHERE t2.t1_id = t1.id AND EXISTS (\
                 SELECT 1 FROM t3 WHERE t3.t2_id = t2.id AND t3.code = t1.code\
             )\
         ) \
         ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // t1.id=3 has no matching t2 with a deep t3 of code=999 → excluded.
    assert_eq!(rows.len(), 2, "t1.id=3 must be excluded, got {rows:?}");
}

#[test]
fn depth_2_correlation_in_in_subquery() {
    // Depth-2 correlation via nested IN subqueries.
    // Innermost `t3.code = t1.code` is a depth-1 reference (from inner's POV,
    // t1 is the grandparent). Without depth tracking, substitute_outer would
    // apply the outer t1 row's value at the wrong nesting level.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    let rows = rows_of(
        "SELECT id FROM t1 WHERE t1.id IN (\
             SELECT t1_id FROM t2 WHERE t2.id IN (\
                 SELECT t2_id FROM t3 WHERE t3.code = t1.code\
             )\
         ) ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // t1.id=1,code=100 → t3 has (100,10,100) → t2.id=10 in t2 → t1_id=1 ✓
    // t1.id=2,code=200 → t3 has (200,20,200) → t2.id=20 → t1_id=2 ✓
    assert_eq!(rows.len(), 2);
    assert_eq!(as_int(&rows[0][0]), 1);
    assert_eq!(as_int(&rows[1][0]), 2);
}
