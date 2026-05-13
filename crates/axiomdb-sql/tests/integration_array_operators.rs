//! Integration tests for Phase 20.4, Step 5 — Array operators.
//!
//! Tests subscript `arr[n]`, slice `arr[lo:hi]`, equality `=` / `<>`,
//! contains `@>`, contained-by `<@`, overlap `&&`, and concatenation `||`.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;
use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn scalar(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Value {
    let QueryResult::Rows { rows, .. } = run_ctx(sql, storage, txn, bloom, ctx).unwrap() else {
        panic!("expected Rows for {sql:?}");
    };
    rows[0][0].clone()
}

fn is_null(v: &Value) -> bool {
    matches!(v, Value::Null)
}

fn is_true(v: &Value) -> bool {
    matches!(v, Value::Bool(true) | Value::Int(1) | Value::BigInt(1))
}

fn is_false(v: &Value) -> bool {
    matches!(v, Value::Bool(false) | Value::Int(0) | Value::BigInt(0))
}

// ── Subscript ────────────────────────────────────────────────────────────────

#[test]
fn subscript_1d_first_element() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT (ARRAY[10,20,30])[1] → 10
    let v = scalar(
        "SELECT (ARRAY[10,20,30])[1]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Int(10) | Value::BigInt(10)));
}

#[test]
fn subscript_1d_second_element() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT (ARRAY[10,20,30])[2]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Int(20) | Value::BigInt(20)));
}

#[test]
fn subscript_1d_out_of_bounds_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT (ARRAY[10,20,30])[5] → NULL
    let v = scalar(
        "SELECT (ARRAY[10,20,30])[5]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_null(&v), "expected NULL, got {:?}", v);
}

#[test]
fn subscript_negative_index_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT (ARRAY[1,2,3])[-1] → NULL
    let v = scalar("SELECT (ARRAY[1,2,3])[-1]", &mut s, &mut t, &mut b, &mut c);
    assert!(is_null(&v), "expected NULL, got {:?}", v);
}

#[test]
fn subscript_zero_index_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT (ARRAY[1,2,3])[0] → NULL (0 is not valid in 1-indexed)
    let v = scalar("SELECT (ARRAY[1,2,3])[0]", &mut s, &mut t, &mut b, &mut c);
    assert!(is_null(&v), "expected NULL, got {:?}", v);
}

#[test]
fn subscript_1d_slice() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT (ARRAY[10,20,30,40])[2:3] → {20,30}
    let v = scalar(
        "SELECT (ARRAY[10,20,30,40])[2:3]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    match v {
        Value::Array(elems) => {
            assert_eq!(elems.len(), 2);
            assert!(matches!(elems[0], Value::Int(20) | Value::BigInt(20)));
            assert!(matches!(elems[1], Value::Int(30) | Value::BigInt(30)));
        }
        other => panic!("expected Array, got {:?}", other),
    }
}

#[test]
fn subscript_slice_empty_result() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT (ARRAY[1,2,3])[5:6] → empty array (clamped to bounds)
    let v = scalar("SELECT (ARRAY[1,2,3])[5:6]", &mut s, &mut t, &mut b, &mut c);
    match v {
        Value::Array(elems) => {
            assert_eq!(elems.len(), 0, "expected empty array, got {:?}", elems);
        }
        other => panic!("expected Array, got {:?}", other),
    }
}

#[test]
fn subscript_slice_lo_gt_hi_empty() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT (ARRAY[1,2,3])[3:1] → empty array (lo > hi)
    let v = scalar("SELECT (ARRAY[1,2,3])[3:1]", &mut s, &mut t, &mut b, &mut c);
    match v {
        Value::Array(elems) => {
            assert_eq!(elems.len(), 0, "expected empty array, got {:?}", elems);
        }
        other => panic!("expected Array, got {:?}", other),
    }
}

#[test]
fn subscript_2d() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT (ARRAY[ARRAY[1,2],ARRAY[3,4]])[2][1] → 3
    let v = scalar(
        "SELECT (ARRAY[ARRAY[1,2],ARRAY[3,4]])[2][1]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(
        matches!(v, Value::Int(3) | Value::BigInt(3)),
        "expected 3, got {:?}",
        v
    );
}

#[test]
fn subscript_null_array_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT NULL[1] → NULL
    let v = scalar("SELECT NULL[1]", &mut s, &mut t, &mut b, &mut c);
    assert!(is_null(&v), "expected NULL, got {:?}", v);
}

#[test]
fn subscript_null_index_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT (ARRAY[1,2,3])[NULL] → NULL
    let v = scalar(
        "SELECT (ARRAY[1,2,3])[NULL]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_null(&v), "expected NULL, got {:?}", v);
}

// ── Equality ──────────────────────────────────────────────────────────────

#[test]
fn equality_same_arrays() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2,3] = ARRAY[1,2,3] → TRUE
    let v = scalar(
        "SELECT ARRAY[1,2,3] = ARRAY[1,2,3]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v), "expected TRUE, got {:?}", v);
}

#[test]
fn equality_different_arrays() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2,3] = ARRAY[1,2,4] → FALSE
    let v = scalar(
        "SELECT ARRAY[1,2,3] = ARRAY[1,2,4]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v), "expected FALSE, got {:?}", v);
}

#[test]
fn equality_different_lengths() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2] = ARRAY[1,2,3] → FALSE
    let v = scalar(
        "SELECT ARRAY[1,2] = ARRAY[1,2,3]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v), "expected FALSE, got {:?}", v);
}

#[test]
fn equality_null_elements_unknown() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,NULL] = ARRAY[1,NULL] → NULL (UNKNOWN)
    let v = scalar(
        "SELECT ARRAY[1,NULL] = ARRAY[1,NULL]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_null(&v), "expected NULL, got {:?}", v);
}

#[test]
fn inequality_same_arrays() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2,3] <> ARRAY[1,2,3] → FALSE
    let v = scalar(
        "SELECT ARRAY[1,2,3] <> ARRAY[1,2,3]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v), "expected FALSE, got {:?}", v);
}

#[test]
fn inequality_different_arrays() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2,3] <> ARRAY[1,2,4] → TRUE
    let v = scalar(
        "SELECT ARRAY[1,2,3] <> ARRAY[1,2,4]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v), "expected TRUE, got {:?}", v);
}

// ── Containment ────────────────────────────────────────────────────────────

#[test]
fn contains_atat() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2,3] @> ARRAY[1,2] → TRUE
    let v = scalar(
        "SELECT ARRAY[1,2,3] @> ARRAY[1,2]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v), "expected TRUE, got {:?}", v);
}

#[test]
fn contains_not_subset() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2] @> ARRAY[1,3] → FALSE
    let v = scalar(
        "SELECT ARRAY[1,2] @> ARRAY[1,3]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v), "expected FALSE, got {:?}", v);
}

#[test]
fn contains_null_in_query_unknown() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2] @> ARRAY[NULL] → NULL
    let v = scalar(
        "SELECT ARRAY[1,2] @> ARRAY[NULL]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_null(&v), "expected NULL, got {:?}", v);
}

#[test]
fn contains_empty_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2,3] @> ARRAY[] → TRUE (empty is subset of anything)
    let v = scalar(
        "SELECT ARRAY[1,2,3] @> ARRAY[]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v), "expected TRUE, got {:?}", v);
}

#[test]
fn contained_by_ltat() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2] <@ ARRAY[1,2,3] → TRUE
    let v = scalar(
        "SELECT ARRAY[1,2] <@ ARRAY[1,2,3]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v), "expected TRUE, got {:?}", v);
}

#[test]
fn contained_by_not_superset() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,3] <@ ARRAY[1,2,3] → TRUE (1 and 3 are both in the superset)
    let v = scalar(
        "SELECT ARRAY[1,3] <@ ARRAY[1,2,3]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v), "expected TRUE, got {:?}", v);
}

// ── Overlap ───────────────────────────────────────────────────────────────

#[test]
fn overlap_andand() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2,3] && ARRAY[3,4,5] → TRUE
    let v = scalar(
        "SELECT ARRAY[1,2,3] && ARRAY[3,4,5]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v), "expected TRUE, got {:?}", v);
}

#[test]
fn overlap_disjoint() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2] && ARRAY[3,4] → FALSE
    let v = scalar(
        "SELECT ARRAY[1,2] && ARRAY[3,4]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v), "expected FALSE, got {:?}", v);
}

#[test]
fn overlap_null_array_unknown() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2] && NULL → NULL
    let v = scalar("SELECT ARRAY[1,2] && NULL", &mut s, &mut t, &mut b, &mut c);
    assert!(is_null(&v), "expected NULL, got {:?}", v);
}

// ── Concatenation ────────────────────────────────────────────────────────

#[test]
fn concatenation_pipepipe() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2] || ARRAY[3,4] → {1,2,3,4}
    let v = scalar(
        "SELECT ARRAY[1,2] || ARRAY[3,4]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    match v {
        Value::Array(elems) => {
            assert_eq!(elems.len(), 4);
            assert!(matches!(elems[0], Value::Int(1) | Value::BigInt(1)));
            assert!(matches!(elems[1], Value::Int(2) | Value::BigInt(2)));
            assert!(matches!(elems[2], Value::Int(3) | Value::BigInt(3)));
            assert!(matches!(elems[3], Value::Int(4) | Value::BigInt(4)));
        }
        other => panic!("expected Array, got {:?}", other),
    }
}

#[test]
fn concat_element_to_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[2,3] || 1 → {2,3,1} (element appended to front)
    // Actually PG: 1 || ARRAY[2,3] → {1,2,3}
    let v = scalar("SELECT 1 || ARRAY[2,3]", &mut s, &mut t, &mut b, &mut c);
    match v {
        Value::Array(elems) => {
            assert_eq!(elems.len(), 3);
            assert!(matches!(elems[0], Value::Int(1) | Value::BigInt(1)));
            assert!(matches!(elems[1], Value::Int(2) | Value::BigInt(2)));
            assert!(matches!(elems[2], Value::Int(3) | Value::BigInt(3)));
        }
        other => panic!("expected Array, got {:?}", other),
    }
}

#[test]
fn concat_array_empty() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[1,2] || ARRAY[] → {1,2}
    let v = scalar(
        "SELECT ARRAY[1,2] || ARRAY[]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    match v {
        Value::Array(elems) => {
            assert_eq!(elems.len(), 2);
        }
        other => panic!("expected Array, got {:?}", other),
    }
}

// ── Polymorphic dispatch ──────────────────────────────────────────────────

#[test]
fn jsonb_atat_still_works() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT CAST('{"a":1}' AS JSONB) @> CAST('{"a":1}' AS JSONB) → TRUE
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) @> CAST('{\"a\":1}' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v), "expected TRUE, got {:?}", v);
}

#[test]
fn jsonb_concat_still_works() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT CAST('{"a":1}' AS JSONB) || CAST('{"b":2}' AS JSONB) → combined
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) || CAST('{\"b\":2}' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // Should be JSONB, not an error
    assert!(matches!(v, Value::Jsonb(_)), "expected Jsonb, got {:?}", v);
}

// ── 2D array equality ────────────────────────────────────────────────────

#[test]
fn array_dimensional_equality() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[ARRAY[1,2],ARRAY[3,4]] = ARRAY[ARRAY[1,2],ARRAY[3,4]] → TRUE
    let v = scalar(
        "SELECT ARRAY[ARRAY[1,2],ARRAY[3,4]] = ARRAY[ARRAY[1,2],ARRAY[3,4]]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v), "expected TRUE, got {:?}", v);
}

#[test]
fn array_dimensional_inequality() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SELECT ARRAY[ARRAY[1,2],ARRAY[3,4]] = ARRAY[ARRAY[1,2],ARRAY[3,5]] → FALSE
    let v = scalar(
        "SELECT ARRAY[ARRAY[1,2],ARRAY[3,4]] = ARRAY[ARRAY[1,2],ARRAY[3,5]]",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v), "expected FALSE, got {:?}", v);
}
