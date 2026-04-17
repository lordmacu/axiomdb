//! Integration tests for `SELECT DISTINCT ON (...)` (Phase 21.12).
//!
//! Covers: classic latest-per-group, multiple key columns, expr not in SELECT,
//! NULL keys, WITH LIMIT, WITH WHERE, positional, parse error, regression.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    // orders(customer_id, order_id, order_date, amount)
    run_ctx(
        "CREATE TABLE orders (customer_id INT, order_id INT, order_date INT, amount INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // customer 1: orders 10 (date=20230101, amt=100), 11 (date=20230201, amt=200)
    // customer 2: orders 20 (date=20230301, amt=50)
    // customer 3: orders 30 (date=20230101, amt=10), 31 (date=20230201, amt=15), 32 (date=20230301, amt=20)
    run_ctx(
        "INSERT INTO orders VALUES \
         (1,10,20230101,100),(1,11,20230201,200), \
         (2,20,20230301,50), \
         (3,30,20230101,10),(3,31,20230201,15),(3,32,20230301,20)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    (storage, txn, bloom, ctx)
}

fn ok(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn err(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> DbError {
    run_ctx(sql, storage, txn, bloom, ctx).expect_err("expected SQL to fail")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Classic "latest order per customer": DISTINCT ON (customer_id), ORDER BY customer_id, order_date DESC.
/// For customer 1 → order 11 (most recent), customer 2 → order 20, customer 3 → order 32.
#[test]
fn test_distinct_on_latest_per_group() {
    let (mut s, mut t, mut b, mut c) = setup();
    let res = ok(
        "SELECT DISTINCT ON (customer_id) customer_id, order_id, amount \
         FROM orders ORDER BY customer_id ASC, order_date DESC",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(res);
    assert_eq!(r.len(), 3, "one row per customer");
    // customer 1 → order 11 (amt=200)
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[0][1], Value::Int(11));
    assert_eq!(r[0][2], Value::Int(200));
    // customer 2 → order 20 (amt=50)
    assert_eq!(r[1][0], Value::Int(2));
    assert_eq!(r[1][1], Value::Int(20));
    // customer 3 → order 32 (amt=20)
    assert_eq!(r[2][0], Value::Int(3));
    assert_eq!(r[2][1], Value::Int(32));
}

/// DISTINCT ON with no ORDER BY — one row per group, order within group is scan order.
/// Just verify we get exactly 3 rows (one per customer) and no duplicates.
#[test]
fn test_distinct_on_no_order_by() {
    let (mut s, mut t, mut b, mut c) = setup();
    let res = ok(
        "SELECT DISTINCT ON (customer_id) customer_id FROM orders",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(res);
    assert_eq!(r.len(), 3, "one row per distinct customer_id");
    // customer IDs should be 1, 2, 3 (in some order due to no ORDER BY)
    let mut ids: Vec<i32> = r
        .iter()
        .map(|row| match &row[0] {
            Value::Int(v) => *v,
            other => panic!("unexpected value: {other:?}"),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, vec![1, 2, 3]);
}

/// DISTINCT ON with multiple key columns: (customer_id, order_date).
/// Each (customer, date) is unique in the data — all 6 rows should be returned.
#[test]
fn test_distinct_on_multiple_key_cols() {
    let (mut s, mut t, mut b, mut c) = setup();
    let res = ok(
        "SELECT DISTINCT ON (customer_id, order_date) customer_id, order_date \
         FROM orders ORDER BY customer_id, order_date",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(res);
    assert_eq!(
        r.len(),
        6,
        "all 6 (customer_id, order_date) pairs are unique"
    );
}

/// DISTINCT ON expression not in SELECT list: DISTINCT ON (order_date * 10).
/// The expression is evaluated on pre-projection rows, so it should work
/// even though the SELECT doesn't include order_date.
#[test]
fn test_distinct_on_expr_not_in_select() {
    let (mut s, mut t, mut b, mut c) = setup();
    // Distinct on order_date values: 20230101, 20230201, 20230301 → 3 groups.
    let res = ok(
        "SELECT DISTINCT ON (order_date) customer_id, amount \
         FROM orders ORDER BY order_date ASC",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(res);
    assert_eq!(r.len(), 3, "3 distinct order_date values");
}

/// NULL values in DISTINCT ON key — two NULL keys treated as equal → one row.
#[test]
fn test_distinct_on_null_keys_treated_equal() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE null_test (grp INT, val INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO null_test VALUES (NULL, 1), (NULL, 2), (1, 10)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    // Two NULL keys → one row. grp=1 → one row. Total: 2 rows.
    let res = ok(
        "SELECT DISTINCT ON (grp) grp, val FROM null_test ORDER BY grp ASC NULLS LAST, val ASC",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(res);
    assert_eq!(r.len(), 2, "NULL keys collapse to one group");
}

/// DISTINCT ON with LIMIT — LIMIT applied after dedup.
#[test]
fn test_distinct_on_with_limit() {
    let (mut s, mut t, mut b, mut c) = setup();
    let res = ok(
        "SELECT DISTINCT ON (customer_id) customer_id \
         FROM orders ORDER BY customer_id LIMIT 2",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(res);
    assert_eq!(r.len(), 2, "LIMIT 2 after DISTINCT ON dedup");
}

/// DISTINCT ON with WHERE clause — WHERE filters before DISTINCT ON.
#[test]
fn test_distinct_on_with_where() {
    let (mut s, mut t, mut b, mut c) = setup();
    // Only customer_id = 1 or 3
    let res = ok(
        "SELECT DISTINCT ON (customer_id) customer_id \
         FROM orders WHERE customer_id != 2 ORDER BY customer_id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(res);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(3));
}

/// DISTINCT ON (1) — positional reference resolves to first SELECT column.
#[test]
fn test_distinct_on_positional() {
    let (mut s, mut t, mut b, mut c) = setup();
    let res = ok(
        "SELECT DISTINCT ON (1) customer_id, order_id \
         FROM orders ORDER BY customer_id, order_date DESC",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(res);
    assert_eq!(r.len(), 3, "positional (1) = customer_id, 3 groups");
}

/// DISTINCT ON () — parse error: at least one expression required.
#[test]
fn test_distinct_on_empty_parens_error() {
    let (mut s, mut t, mut b, mut c) = setup();
    let e = err(
        "SELECT DISTINCT ON () customer_id FROM orders",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(
        matches!(e, DbError::ParseError { .. }),
        "expected ParseError, got {e:?}"
    );
}

/// Regression: plain `SELECT DISTINCT` still works after DISTINCT ON changes.
#[test]
fn test_plain_distinct_still_works() {
    let (mut s, mut t, mut b, mut c) = setup();
    let res = ok(
        "SELECT DISTINCT customer_id FROM orders ORDER BY customer_id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(res);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(2));
    assert_eq!(r[2][0], Value::Int(3));
}

/// DISTINCT ON on a table where all DISTINCT ON keys are unique — all rows returned.
#[test]
fn test_distinct_on_single_col_all_unique() {
    let (mut s, mut t, mut b, mut c) = setup();
    // order_id is unique for each row (10,11,20,30,31,32)
    let res = ok(
        "SELECT DISTINCT ON (order_id) order_id FROM orders ORDER BY order_id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(res);
    assert_eq!(r.len(), 6, "all 6 rows have unique order_id");
}

/// DISTINCT ON inside a subquery.
#[test]
fn test_distinct_on_subquery() {
    let (mut s, mut t, mut b, mut c) = setup();
    // subquery finds latest order per customer, outer query sums amounts
    let res = ok(
        "SELECT customer_id, amount \
         FROM (SELECT DISTINCT ON (customer_id) customer_id, amount \
               FROM orders ORDER BY customer_id, order_date DESC) AS latest \
         ORDER BY customer_id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(res);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0], vec![Value::Int(1), Value::Int(200)]);
    assert_eq!(r[1], vec![Value::Int(2), Value::Int(50)]);
    assert_eq!(r[2], vec![Value::Int(3), Value::Int(20)]);
}

/// DISTINCT ON with ORDER BY DESC on the distinct key.
/// Ensures the "first" row is the one with the highest order_date (DESC).
#[test]
fn test_distinct_on_order_desc() {
    let (mut s, mut t, mut b, mut c) = setup();
    // Customer 1: orders at 20230101 (amt=100) and 20230201 (amt=200).
    // ORDER BY order_date DESC → most recent first → first=order_11 (amt=200).
    let res = ok(
        "SELECT DISTINCT ON (customer_id) customer_id, amount \
         FROM orders WHERE customer_id = 1 ORDER BY customer_id, order_date DESC",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(res);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][1], Value::Int(200));
}
