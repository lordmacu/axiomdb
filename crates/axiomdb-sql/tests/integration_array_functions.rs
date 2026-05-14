//! Integration tests for Phase 20.4, Step 6 — Array functions.
//!
//! Tests all 17 array functions:
//! - Metadata: array_length, array_lower, array_upper, array_ndims, array_dims, cardinality
//! - Mutation: array_append, array_prepend, array_cat, array_remove, array_replace
//! - Search: array_position
//! - Conversion: array_to_string, string_to_array

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

fn expect_array(v: &Value) -> &[Value] {
    match v {
        Value::Array(elems) => elems,
        other => panic!("expected Array, got {:?}", other),
    }
}

// ── array_length ──────────────────────────────────────────────────────────────

#[test]
fn array_length_1d() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_length(ARRAY[1,2,3], 1) → 3
    let v = scalar(
        "SELECT array_length(ARRAY[1,2,3], 1)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Int(3));
}

#[test]
fn array_length_nonexistent_dim() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_length(ARRAY[1,2,3], 2) → NULL (dim 2 does not exist for 1D array)
    let v = scalar(
        "SELECT array_length(ARRAY[1,2,3], 2)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_null(&v), "expected NULL, got {:?}", v);
}

#[test]
fn array_length_null_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_length(NULL, 1) → NULL
    let v = scalar(
        "SELECT array_length(NULL, 1)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_null(&v), "expected NULL, got {:?}", v);
}

// ── array_lower / array_upper ─────────────────────────────────────────────────

#[test]
fn array_lower() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_lower(ARRAY[1,2,3], 1) → 1 (default lbound is 1)
    let v = scalar(
        "SELECT array_lower(ARRAY[1,2,3], 1)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Int(1));
}

#[test]
fn array_upper() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_upper(ARRAY[1,2,3], 1) → 3
    let v = scalar(
        "SELECT array_upper(ARRAY[1,2,3], 1)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Int(3));
}

// ── array_ndims ───────────────────────────────────────────────────────────────

#[test]
fn array_ndims_1d() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_ndims(ARRAY[1,2,3]) → 1
    let v = scalar(
        "SELECT array_ndims(ARRAY[1,2,3])",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Int(1));
}

#[test]
fn array_ndims_empty() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_ndims(ARRAY[]) → 0
    let v = scalar(
        "SELECT array_ndims(ARRAY[])",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Int(0));
}

// ── array_dims ────────────────────────────────────────────────────────────────

#[test]
fn array_dims() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_dims(ARRAY[1,2,3]) → '[1:3]'
    let v = scalar(
        "SELECT array_dims(ARRAY[1,2,3])",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Text("[1:3]".to_string()));
}

#[test]
fn array_dims_2d() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_dims(ARRAY[ARRAY[1,2],ARRAY[3,4]]) → '[1:2][1:2]'
    let v = scalar(
        "SELECT array_dims(ARRAY[ARRAY[1,2],ARRAY[3,4]])",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Text("[1:2][1:2]".to_string()));
}

// ── cardinality ────────────────────────────────────────────────────────────────

#[test]
fn cardinality_1d() {
    let (mut s, mut t, mut b, mut c) = setup();
    // cardinality(ARRAY[1,2,3]) → 3
    let v = scalar(
        "SELECT cardinality(ARRAY[1,2,3])",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::BigInt(3));
}

#[test]
fn cardinality_2d() {
    let (mut s, mut t, mut b, mut c) = setup();
    // cardinality(ARRAY[ARRAY[1,2],ARRAY[3,4]]) → 4 (2x2)
    let v = scalar(
        "SELECT cardinality(ARRAY[ARRAY[1,2],ARRAY[3,4]])",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::BigInt(4));
}

// ── array_append ──────────────────────────────────────────────────────────────

#[test]
fn array_append() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_append(ARRAY[1,2], 3) → {1,2,3}
    let v = scalar(
        "SELECT array_append(ARRAY[1,2], 3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let elems = expect_array(&v);
    assert_eq!(elems.len(), 3);
    assert_eq!(elems[0], Value::Int(1));
    assert_eq!(elems[1], Value::Int(2));
    assert_eq!(elems[2], Value::Int(3));
}

#[test]
fn array_append_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_append(ARRAY[1,2], NULL) → {1,2,NULL}
    let v = scalar(
        "SELECT array_append(ARRAY[1,2], NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let elems = expect_array(&v);
    assert_eq!(elems.len(), 3);
    assert_eq!(elems[2], Value::Null);
}

// ── array_prepend ─────────────────────────────────────────────────────────────

#[test]
fn array_prepend() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_prepend(0, ARRAY[1,2]) → {0,1,2}
    let v = scalar(
        "SELECT array_prepend(0, ARRAY[1,2])",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let elems = expect_array(&v);
    assert_eq!(elems.len(), 3);
    assert_eq!(elems[0], Value::Int(0));
    assert_eq!(elems[1], Value::Int(1));
    assert_eq!(elems[2], Value::Int(2));
}

// ── array_cat ─────────────────────────────────────────────────────────────────

#[test]
fn array_cat() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_cat(ARRAY[1,2], ARRAY[3,4]) → {1,2,3,4}
    let v = scalar(
        "SELECT array_cat(ARRAY[1,2], ARRAY[3,4])",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let elems = expect_array(&v);
    assert_eq!(elems.len(), 4);
    assert_eq!(elems[0], Value::Int(1));
    assert_eq!(elems[1], Value::Int(2));
    assert_eq!(elems[2], Value::Int(3));
    assert_eq!(elems[3], Value::Int(4));
}

// ── array_remove ──────────────────────────────────────────────────────────────

#[test]
fn array_remove() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_remove(ARRAY[1,2,3,2], 2) → {1,3}
    let v = scalar(
        "SELECT array_remove(ARRAY[1,2,3,2], 2)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let elems = expect_array(&v);
    assert_eq!(elems.len(), 2);
    assert_eq!(elems[0], Value::Int(1));
    assert_eq!(elems[1], Value::Int(3));
}

#[test]
fn array_remove_nulls() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_remove(ARRAY[1,NULL,3,NULL], NULL) → {1,3}
    let v = scalar(
        "SELECT array_remove(ARRAY[1,NULL,3,NULL], NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let elems = expect_array(&v);
    assert_eq!(elems.len(), 2);
    assert_eq!(elems[0], Value::Int(1));
    assert_eq!(elems[1], Value::Int(3));
}

// ── array_replace ─────────────────────────────────────────────────────────────

#[test]
fn array_replace() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_replace(ARRAY[1,2,3], 2, 5) → {1,5,3}
    let v = scalar(
        "SELECT array_replace(ARRAY[1,2,3], 2, 5)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let elems = expect_array(&v);
    assert_eq!(elems.len(), 3);
    assert_eq!(elems[0], Value::Int(1));
    assert_eq!(elems[1], Value::Int(5));
    assert_eq!(elems[2], Value::Int(3));
}

// ── array_position ────────────────────────────────────────────────────────────

#[test]
fn array_position_found() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_position(ARRAY[10,20,30], 20) → 2
    let v = scalar(
        "SELECT array_position(ARRAY[10,20,30], 20)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Int(2));
}

#[test]
fn array_position_not_found() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_position(ARRAY[1,2,3], 5) → 0
    let v = scalar(
        "SELECT array_position(ARRAY[1,2,3], 5)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Int(0));
}

#[test]
fn array_position_start() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_position(ARRAY[1,2,3,2], 2, 3) → 4 (starts searching from index 3)
    let v = scalar(
        "SELECT array_position(ARRAY[1,2,3,2], 2, 3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Int(4));
}

// ── array_to_string ──────────────────────────────────────────────────────────

#[test]
fn array_to_string_simple() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_to_string(ARRAY[1,2,3], ',') → '1,2,3'
    let v = scalar(
        "SELECT array_to_string(ARRAY[1,2,3], ',')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Text("1,2,3".to_string()));
}

#[test]
fn array_to_string_null_skip() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_to_string(ARRAY[1,NULL,3], ',') → '1,3' (NULL skipped)
    let v = scalar(
        "SELECT array_to_string(ARRAY[1,NULL,3], ',')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Text("1,3".to_string()));
}

#[test]
fn array_to_string_null_replace() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_to_string(ARRAY[1,NULL,3], ',', 'X') → '1,X,3'
    let v = scalar(
        "SELECT array_to_string(ARRAY[1,NULL,3], ',', 'X')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Text("1,X,3".to_string()));
}

#[test]
fn array_to_string_text() {
    let (mut s, mut t, mut b, mut c) = setup();
    // array_to_string(ARRAY['a','b','c'], ',') → 'a,b,c'
    let v = scalar(
        "SELECT array_to_string(ARRAY['a','b','c'], ',')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, Value::Text("a,b,c".to_string()));
}

// ── string_to_array ──────────────────────────────────────────────────────────

#[test]
fn string_to_array_simple() {
    let (mut s, mut t, mut b, mut c) = setup();
    // string_to_array('a,b,c', ',') → {a,b,c}
    let v = scalar(
        "SELECT string_to_array('a,b,c', ',')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let elems = expect_array(&v);
    assert_eq!(elems.len(), 3);
    assert_eq!(elems[0], Value::Text("a".to_string()));
    assert_eq!(elems[1], Value::Text("b".to_string()));
    assert_eq!(elems[2], Value::Text("c".to_string()));
}

#[test]
fn string_to_array_null_str() {
    let (mut s, mut t, mut b, mut c) = setup();
    // string_to_array('a,X,c', ',', 'X') → {a,NULL,c}
    let v = scalar(
        "SELECT string_to_array('a,X,c', ',', 'X')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let elems = expect_array(&v);
    assert_eq!(elems.len(), 3);
    assert_eq!(elems[0], Value::Text("a".to_string()));
    assert!(is_null(&elems[1]), "expected NULL, got {:?}", elems[1]);
    assert_eq!(elems[2], Value::Text("c".to_string()));
}

// NOTE: unnest() is a FROM-clause SRF only — not a scalar function.
// Proper UNNEST tests live in `integration_array_unnest.rs`.
