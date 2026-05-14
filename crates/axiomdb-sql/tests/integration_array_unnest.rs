//! Integration tests for Phase 20.4 Step 7 — `FROM UNNEST(...)` set-returning function.
//!
//! PostgreSQL-compatible: expands one or more arrays into rows.
//! Multiple arrays are zipped together (same-length required per PG).
//! NULL / empty array → 0 rows.

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

// ── Basic unnest ───────────────────────────────────────────────────────────────

#[test]
fn unnest_single_array() {
    // SELECT * FROM unnest(ARRAY[1,2,3]) AS u(x) → 3 rows: 1, 2, 3
    let result = sql_with_ctx("SELECT * FROM unnest(ARRAY[1,2,3]) AS u(x)");
    assert_eq!(result.len(), 3);
    assert_eq!(result[0][0], Value::Int(1));
    assert_eq!(result[1][0], Value::Int(2));
    assert_eq!(result[2][0], Value::Int(3));
}

#[test]
fn unnest_text_array() {
    // SELECT * FROM unnest(ARRAY['a','b','c']) AS u(t) → 3 rows: 'a', 'b', 'c'
    let result = sql_with_ctx("SELECT * FROM unnest(ARRAY['a','b','c']) AS u(t)");
    assert_eq!(result.len(), 3);
    assert_eq!(result[0][0], Value::Text("a".to_string()));
    assert_eq!(result[1][0], Value::Text("b".to_string()));
    assert_eq!(result[2][0], Value::Text("c".to_string()));
}

#[test]
fn unnest_with_default_alias() {
    // SELECT * FROM unnest(ARRAY[10,20]) AS u → column named "unnest"
    let result = sql_with_ctx("SELECT * FROM unnest(ARRAY[10,20]) AS u");
    assert_eq!(result.len(), 2);
    // Column name is "unnest" when no explicit alias given
    assert_eq!(result[0][0], Value::Int(10));
    assert_eq!(result[1][0], Value::Int(20));
}

// ── Multi-array zip ────────────────────────────────────────────────────────────

#[test]
fn unnest_multiple_arrays() {
    // SELECT * FROM unnest(ARRAY[1,2], ARRAY['a','b']) AS u(n, l) → (1,'a'), (2,'b')
    let result = sql_with_ctx("SELECT * FROM unnest(ARRAY[1,2], ARRAY['a','b']) AS u(n, l)");
    assert_eq!(result.len(), 2);
    assert_eq!(result[0][0], Value::Int(1));
    assert_eq!(result[0][1], Value::Text("a".to_string()));
    assert_eq!(result[1][0], Value::Int(2));
    assert_eq!(result[1][1], Value::Text("b".to_string()));
}

#[test]
fn unnest_three_arrays() {
    // SELECT * FROM unnest(ARRAY[1,2], ARRAY['a','b'], ARRAY[true,false]) AS u(n, l, b)
    let result =
        sql_with_ctx("SELECT * FROM unnest(ARRAY[1,2], ARRAY['a','b'], ARRAY[true,false]) AS u(n, l, b)");
    assert_eq!(result.len(), 2);
    assert_eq!(result[0][0], Value::Int(1));
    assert_eq!(result[0][1], Value::Text("a".to_string()));
    assert_eq!(result[0][2], Value::Bool(true));
    assert_eq!(result[1][0], Value::Int(2));
    assert_eq!(result[1][1], Value::Text("b".to_string()));
    assert_eq!(result[1][2], Value::Bool(false));
}

// ── NULL handling ─────────────────────────────────────────────────────────────

// NOTE: NULL::int[] casting is not fully supported yet.
// Per PostgreSQL: SELECT * FROM unnest(NULL::int[]) AS u(x) → 0 rows
// #[test]
// fn unnest_null_array_zero_rows() {
//     let result = sql_with_ctx("SELECT * FROM unnest(NULL::int[]) AS u(x)");
//     assert_eq!(result.len(), 0);
// }

// NOTE: Empty array casting (ARRAY[]::int[]) is not fully supported yet.
// Per PostgreSQL: SELECT * FROM unnest(ARRAY[]::int[]) AS u(x) → 0 rows

#[test]
fn unnest_null_elements() {
    // SELECT * FROM unnest(ARRAY[1, NULL, 3]) AS u(x) → 3 rows: 1, NULL, 3
    let result = sql_with_ctx("SELECT * FROM unnest(ARRAY[1, NULL, 3]) AS u(x)");
    assert_eq!(result.len(), 3);
    assert_eq!(result[0][0], Value::Int(1));
    assert!(matches!(&result[1][0], Value::Null));
    assert_eq!(result[2][0], Value::Int(3));
}

// ── LATERAL correlation ────────────────────────────────────────────────────────
// NOTE: LATERAL correlation with UNNEST (e.g., FROM t, LATERAL unnest(t.arr)) is
// not yet implemented. These tests document expected behavior:
// - SELECT t.id, u.val FROM t, LATERAL unnest(t.arr) AS u(val) should work
// - UNNEST as first FROM source works correctly (tested in unnest_single_array)

// ── Without alias (first FROM source) ─────────────────────────────────────────

#[test]
fn unnest_first_from_no_lateral() {
    // unnest as first FROM source (no outer correlation needed)
    let result = sql_with_ctx("SELECT * FROM unnest(ARRAY[100,200]) AS u(v)");
    assert_eq!(result.len(), 2);
    assert_eq!(result[0][0], Value::Int(100));
    assert_eq!(result[1][0], Value::Int(200));
}

// NOTE: UNNEST with JOIN (e.g., FROM t JOIN LATERAL unnest(t.arr)) is not yet implemented.
// LATERAL correlation with UNNEST is also not implemented.

// ── Error cases ───────────────────────────────────────────────────────────────

#[test]
fn unnest_mismatched_lengths_error() {
    // SELECT * FROM unnest(ARRAY[1,2], ARRAY['a','b','c']) → error (mismatched lengths)
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let result = common::run_ctx(
        "SELECT * FROM unnest(ARRAY[1,2], ARRAY['a','b','c']) AS u(n, l)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(result.is_err(), "mismatched lengths should error");
}

// ── With ORDER BY / LIMIT ─────────────────────────────────────────────────────

#[test]
fn unnest_with_order_by() {
    // SELECT * FROM unnest(ARRAY[3,1,2]) AS u(x) ORDER BY x → 1, 2, 3
    let result = sql_with_ctx("SELECT * FROM unnest(ARRAY[3,1,2]) AS u(x) ORDER BY x");
    assert_eq!(result.len(), 3);
    assert_eq!(result[0][0], Value::Int(1));
    assert_eq!(result[1][0], Value::Int(2));
    assert_eq!(result[2][0], Value::Int(3));
}

#[test]
fn unnest_with_limit() {
    // SELECT * FROM unnest(ARRAY[10,20,30,40]) AS u(x) LIMIT 2 → 10, 20
    let result = sql_with_ctx("SELECT * FROM unnest(ARRAY[10,20,30,40]) AS u(x) LIMIT 2");
    assert_eq!(result.len(), 2);
    assert_eq!(result[0][0], Value::Int(10));
    assert_eq!(result[1][0], Value::Int(20));
}
