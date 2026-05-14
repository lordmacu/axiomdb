//! Integration tests for `array_agg` aggregate function (Phase 20.4 Step 9).
//!
//! Tests PostgreSQL-compatible `array_agg(expr [ORDER BY ...] [DISTINCT])`:
//! - NULLs are included in the result array
//! - Empty group returns NULL (not empty array)
//! - ORDER BY sorts elements before building final array
//! - DISTINCT removes duplicates before building final array

mod common;

use axiomdb_types::Value;

/// Extract rows from QueryResult.
fn rows(result: axiomdb_sql::QueryResult) -> Vec<Vec<Value>> {
    match result {
        axiomdb_sql::QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

/// Run SQL that returns rows using a single context.
fn sql_with_ctx(sql: &str) -> Vec<Vec<Value>> {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let r = common::run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    rows(r)
}

#[test]
fn array_agg_simple() {
    // SELECT array_agg(x) FROM (VALUES (1),(2),(3)) AS t(x) → {1,2,3}
    let result = sql_with_ctx("SELECT array_agg(x) FROM (VALUES (1),(2),(3)) AS t(x)");
    assert_eq!(result.len(), 1);
    // Result is an array: {1,2,3}
    let val = &result[0][0];
    match val {
        Value::Array(arr) => {
            assert_eq!(arr.len(), 3);
        }
        _ => panic!("expected Array, got: {:?}", val),
    }
}

#[test]
fn array_agg_empty_group() {
    // Empty group should return NULL
    let stmts = &[
        "CREATE TABLE empty_test (x INT)",
        "SELECT array_agg(x) FROM empty_test WHERE FALSE",
    ];
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    // First create the table
    common::run_ctx(stmts[0], &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    // Then query with WHERE FALSE
    let result =
        rows(common::run_ctx(stmts[1], &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap());
    assert_eq!(result.len(), 1);
    let val = &result[0][0];
    assert!(matches!(val, Value::Null), "expected NULL, got: {:?}", val);
}

#[test]
fn array_agg_with_nulls() {
    // SELECT array_agg(x) FROM (VALUES (1),(NULL),(3)) AS t(x) → {1,NULL,3}
    let result = sql_with_ctx("SELECT array_agg(x) FROM (VALUES (1),(NULL),(3)) AS t(x)");
    assert_eq!(result.len(), 1);
    let val = &result[0][0];
    match val {
        Value::Array(arr) => {
            // Should have 3 elements including NULL
            assert_eq!(arr.len(), 3);
            assert!(matches!(&arr[0], Value::Int(1)));
            assert!(matches!(&arr[1], Value::Null));
            assert!(matches!(&arr[2], Value::Int(3)));
        }
        _ => panic!("expected Array, got: {:?}", val),
    }
}

#[test]
fn array_agg_with_order_by() {
    // SELECT array_agg(x ORDER BY x DESC) FROM (VALUES (1),(3),(2)) AS t(x) → {3,2,1}
    let result =
        sql_with_ctx("SELECT array_agg(x ORDER BY x DESC) FROM (VALUES (1),(3),(2)) AS t(x)");
    assert_eq!(result.len(), 1);
    let val = &result[0][0];
    match val {
        Value::Array(arr) => {
            assert_eq!(arr.len(), 3);
            // Should be sorted DESC: 3, 2, 1
            assert!(matches!(&arr[0], Value::Int(3)));
            assert!(matches!(&arr[1], Value::Int(2)));
            assert!(matches!(&arr[2], Value::Int(1)));
        }
        _ => panic!("expected Array, got: {:?}", val),
    }
}

#[test]
fn array_agg_distinct() {
    // SELECT array_agg(DISTINCT x) FROM (VALUES (1),(2),(2),(3)) AS t(x) → {1,2,3}
    let result = sql_with_ctx("SELECT array_agg(DISTINCT x) FROM (VALUES (1),(2),(2),(3)) AS t(x)");
    assert_eq!(result.len(), 1);
    let val = &result[0][0];
    match val {
        Value::Array(arr) => {
            // DISTINCT should remove duplicate 2, leaving 3 elements
            assert_eq!(arr.len(), 3);
        }
        _ => panic!("expected Array, got: {:?}", val),
    }
}

#[test]
fn array_agg_grouped() {
    // SELECT grp, array_agg(val) FROM t GROUP BY grp
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE agg_group_test (grp TEXT, val INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO agg_group_test VALUES ('a', 1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO agg_group_test VALUES ('a', 2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO agg_group_test VALUES ('b', 3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let result = rows(
        common::run_ctx(
            "SELECT grp, array_agg(val) FROM agg_group_test GROUP BY grp ORDER BY grp",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 2);

    // First group 'a' should have {1,2}
    let first = &result[0];
    assert!(matches!(&first[0], Value::Text(t) if t == "a"));
    match &first[1] {
        Value::Array(arr) => assert_eq!(arr.len(), 2),
        _ => panic!("expected Array, got: {:?}", first[1]),
    }

    // Second group 'b' should have {3}
    let second = &result[1];
    assert!(matches!(&second[0], Value::Text(t) if t == "b"));
    match &second[1] {
        Value::Array(arr) => assert_eq!(arr.len(), 1),
        _ => panic!("expected Array, got: {:?}", second[1]),
    }
}

#[test]
fn array_agg_with_where() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE where_test (x INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO where_test VALUES (1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO where_test VALUES (2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO where_test VALUES (3)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO where_test VALUES (-1)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO where_test VALUES (-2)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let result = rows(
        common::run_ctx(
            "SELECT array_agg(x) FROM where_test WHERE x > 0",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(result.len(), 1);
    match &result[0][0] {
        Value::Array(arr) => {
            // Should have 3 positive values
            assert_eq!(arr.len(), 3);
        }
        _ => panic!("expected Array, got: {:?}", result[0][0]),
    }
}

#[test]
fn array_agg_order_by_then_distinct() {
    // SELECT array_agg(DISTINCT x ORDER BY x DESC) FROM ...
    // PostgreSQL allows ORDER BY and DISTINCT together
    let result = sql_with_ctx(
        "SELECT array_agg(DISTINCT x ORDER BY x DESC) FROM (VALUES (1),(2),(3),(2)) AS t(x)",
    );
    assert_eq!(result.len(), 1);
    let val = &result[0][0];
    match val {
        Value::Array(arr) => {
            // DISTINCT removes duplicates, then ORDER BY sorts DESC: {3,2,1}
            assert_eq!(arr.len(), 3);
            assert!(matches!(&arr[0], Value::Int(3)));
            assert!(matches!(&arr[1], Value::Int(2)));
            assert!(matches!(&arr[2], Value::Int(1)));
        }
        _ => panic!("expected Array, got: {:?}", val),
    }
}
