//! Phase 11.18b — `?|` and `?&` JSONB any/all-keys exists operators.

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

fn is_true(v: &Value) -> bool {
    matches!(v, Value::Bool(true) | Value::Int(1) | Value::BigInt(1))
}
fn is_false(v: &Value) -> bool {
    matches!(v, Value::Bool(false) | Value::Int(0) | Value::BigInt(0))
}

// ── ?| (any-of) ─────────────────────────────────────────────────────────────

#[test]
fn any_of_finds_one() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1,\"b\":2}' AS JSONB) ?| CAST('[\"z\",\"a\"]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn any_of_no_match() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) ?| CAST('[\"x\",\"y\"]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn any_of_array_doc() {
    let (mut s, mut t, mut b, mut c) = setup();
    // String element in array satisfies any-of.
    let v = scalar(
        "SELECT CAST('[\"x\",\"y\"]' AS JSONB) ?| CAST('[\"q\",\"y\"]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

// ── ?& (all-of) ─────────────────────────────────────────────────────────────

#[test]
fn all_of_ok() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1,\"b\":2,\"c\":3}' AS JSONB) ?& CAST('[\"a\",\"b\"]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn all_of_one_missing_fails() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) ?& CAST('[\"a\",\"z\"]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn all_of_empty_array_is_true() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) ?& CAST('[]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // Vacuously true.
    assert!(is_true(&v));
}

// ── NULL propagation ────────────────────────────────────────────────────────

#[test]
fn null_lhs_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST(NULL AS JSONB) ?| CAST('[\"a\"]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn null_rhs_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) ?& CAST(NULL AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

// ── WHERE filter ────────────────────────────────────────────────────────────

#[test]
fn any_of_in_where_filters_rows() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, doc JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES \
            (1, CAST('{\"red\":1}' AS JSONB)), \
            (2, CAST('{\"blue\":1}' AS JSONB)), \
            (3, CAST('{\"green\":1}' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM t WHERE doc ?| CAST('[\"red\",\"blue\"]' AS JSONB) ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap() else {
        panic!();
    };
    assert_eq!(rows.len(), 2);
}
