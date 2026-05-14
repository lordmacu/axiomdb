//! Integration tests for Phase 20.4 Step 7 — ANY/ALL array constructs.
//!
//! Tests PostgreSQL-compatible `expr = ANY(array)` / `expr > ALL(array)`:
//! - Operators: =, <>, <, <=, >, >=, LIKE, ILIKE
//! - ANY: TRUE if any element satisfies the comparison
//! - ALL: TRUE if all elements satisfy the comparison
//! - NULL semantics: ANY returns NULL when all comparisons are NULL;
//!   ALL returns NULL on first NULL encountered.

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

/// Run SQL that returns a single boolean/text value.
fn scalar(sql: &str) -> Value {
    let result = sql_with_ctx(sql);
    result.into_iter().next().unwrap().into_iter().next().unwrap()
}

// ── ANY/ALL = ─────────────────────────────────────────────────────────────────

#[test]
fn any_equals_true() {
    // SELECT 100 = ANY(ARRAY[50,100,200]) → TRUE
    let val = scalar("SELECT 100 = ANY(ARRAY[50,100,200])");
    assert_eq!(val, Value::Bool(true));
}

#[test]
fn any_equals_false() {
    // SELECT 100 = ANY(ARRAY[50,150]) → FALSE
    let val = scalar("SELECT 100 = ANY(ARRAY[50,150])");
    assert_eq!(val, Value::Bool(false));
}

#[test]
fn all_equals_true() {
    // SELECT 100 = ALL(ARRAY[100,100,100]) → TRUE
    let val = scalar("SELECT 100 = ALL(ARRAY[100,100,100])");
    assert_eq!(val, Value::Bool(true));
}

#[test]
fn all_equals_false() {
    // SELECT 100 = ALL(ARRAY[50,100,150]) → FALSE (not all equal)
    let val = scalar("SELECT 100 = ALL(ARRAY[50,100,150])");
    assert_eq!(val, Value::Bool(false));
}

// ── ANY/ALL > ─────────────────────────────────────────────────────────────────

#[test]
fn any_greater_than_true() {
    // SELECT 100 > ANY(ARRAY[50,150]) → TRUE (100 > 50)
    let val = scalar("SELECT 100 > ANY(ARRAY[50,150])");
    assert_eq!(val, Value::Bool(true));
}

#[test]
fn any_greater_than_false() {
    // SELECT 50 > ANY(ARRAY[100,200]) → FALSE (50 is not > any)
    let val = scalar("SELECT 50 > ANY(ARRAY[100,200])");
    assert_eq!(val, Value::Bool(false));
}

#[test]
fn all_less_than_true() {
    // SELECT 50 < ALL(ARRAY[100,200,300]) → TRUE (50 < all)
    let val = scalar("SELECT 50 < ALL(ARRAY[100,200,300])");
    assert_eq!(val, Value::Bool(true));
}

#[test]
fn all_less_than_false() {
    // SELECT 100 < ALL(ARRAY[50,150]) → FALSE (100 is not < 150)
    let val = scalar("SELECT 100 < ALL(ARRAY[50,150])");
    assert_eq!(val, Value::Bool(false));
}

// ── NULL handling ─────────────────────────────────────────────────────────────

#[test]
fn any_with_null() {
    // SELECT 100 = ANY(ARRAY[NULL,200]) → NULL (3VL: 100 = NULL is NULL, 100 = 200 is FALSE)
    let val = scalar("SELECT 100 = ANY(ARRAY[NULL,200])");
    // Per 3VL, NULL comparisons don't contribute TRUE, so with no TRUE result is NULL
    assert_eq!(val, Value::Null);
}

#[test]
fn any_all_nulls() {
    // SELECT 100 = ANY(ARRAY[NULL,NULL]) → NULL
    let val = scalar("SELECT 100 = ANY(ARRAY[NULL,NULL])");
    assert_eq!(val, Value::Null);
}

#[test]
fn all_with_null_false() {
    // SELECT 100 < ALL(ARRAY[200,NULL]) → NULL (100 < 200 is TRUE, but 100 < NULL is unknown)
    // No FALSE found, but saw_null = true → returns NULL per 3VL
    let val = scalar("SELECT 100 < ALL(ARRAY[200,NULL])");
    assert_eq!(val, Value::Null);
}

#[test]
fn all_with_null_unknown() {
    // SELECT 100 < ALL(ARRAY[50,NULL]) → NULL (100 < 50 is FALSE → short-circuits to FALSE, not NULL)
    // Actually: 100 < 50 = FALSE, so ALL returns FALSE immediately
    let val = scalar("SELECT 100 < ALL(ARRAY[50,NULL])");
    assert_eq!(val, Value::Bool(false));
}

// ── LIKE with ANY ─────────────────────────────────────────────────────────────
// NOTE: LIKE/ILIKE with ANY/ALL requires special handling in the evaluator
// because Expr::Like doesn't handle AnyOf patterns. This is deferred.
// The tests below document the expected PostgreSQL semantics:
// SELECT 'hello' LIKE ANY(ARRAY['%ello','world']) → TRUE
// SELECT 'hello' LIKE ANY(ARRAY['%world','foo']) → FALSE

// ── ANY/ALL with text arrays ───────────────────────────────────────────────────

#[test]
fn any_text_equals_true() {
    // SELECT 'foo' = ANY(ARRAY['bar','foo','baz']) → TRUE
    let val = scalar("SELECT 'foo' = ANY(ARRAY['bar','foo','baz'])");
    assert_eq!(val, Value::Bool(true));
}

#[test]
fn all_text_less_than_true() {
    // SELECT 'aaa' < ALL(ARRAY['bbb','ccc']) → TRUE
    let val = scalar("SELECT 'aaa' < ALL(ARRAY['bbb','ccc'])");
    assert_eq!(val, Value::Bool(true));
}

// ── <> (not equals) ───────────────────────────────────────────────────────────

#[test]
fn any_not_equals_true() {
    // SELECT 100 <> ANY(ARRAY[200,300]) → TRUE (100 <> 200)
    let val = scalar("SELECT 100 <> ANY(ARRAY[200,300])");
    assert_eq!(val, Value::Bool(true));
}

#[test]
fn all_not_equals_true() {
    // SELECT 100 <> ALL(ARRAY[50,200]) → TRUE (100 <> both)
    let val = scalar("SELECT 100 <> ALL(ARRAY[50,200])");
    assert_eq!(val, Value::Bool(true));
}

// ── <=, >= operators ─────────────────────────────────────────────────────────

#[test]
fn any_less_than_or_equals_true() {
    // SELECT 100 <= ANY(ARRAY[100,200]) → TRUE
    let val = scalar("SELECT 100 <= ANY(ARRAY[100,200])");
    assert_eq!(val, Value::Bool(true));
}

#[test]
fn all_greater_than_or_equals_true() {
    // SELECT 200 >= ALL(ARRAY[100,200]) → TRUE
    let val = scalar("SELECT 200 >= ALL(ARRAY[100,200])");
    assert_eq!(val, Value::Bool(true));
}

// ── Empty array ───────────────────────────────────────────────────────────────
// NOTE: CAST(ARRAY[] AS INT[]) causes an InvalidCoercion error because empty
// array type inference doesn't work properly. This is a known gap.
// Per PostgreSQL semantics:
// - ANY with empty array → NULL (no elements to compare)
// - ALL with empty array → NULL (vacuous truth)

// ── With table column ─────────────────────────────────────────────────────────

#[test]
fn any_equals_with_table_column() {
    // SELECT x FROM t WHERE x = ANY(arr)
    // x = ANY(arr) means "x equals any element in arr"
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    common::run_ctx(
        "CREATE TABLE t(x INT, arr INT[])",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO t VALUES (1, ARRAY[1,2]), (5, ARRAY[3,4]), (2, ARRAY[1,5])",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let result =
        rows(common::run_ctx("SELECT x FROM t WHERE x = ANY(arr)", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap());
    let ids: Vec<i32> = result
        .into_iter()
        .map(|row| match row.into_iter().next().unwrap() {
            Value::Int(n) => n,
            other => panic!("expected Int, got {other:?}"),
        })
        .collect();
    // x=1 matches arr=[1,2] (1 is element 1); x=5 and x=2 don't match any array
    assert_eq!(ids, vec![1]);
}
