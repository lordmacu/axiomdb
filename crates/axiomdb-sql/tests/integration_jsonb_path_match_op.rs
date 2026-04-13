//! Phase 11.21c — `@@` JSONB JSONPath-match binary operator.

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

#[test]
fn at_at_true_on_boolean_result() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":true}' AS JSONB) @@ '$.a'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn at_at_false_on_boolean_result() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":false}' AS JSONB) @@ '$.a'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn at_at_non_boolean_is_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":42}' AS JSONB) @@ '$.a'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn at_at_missing_path_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) @@ '$.z'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn at_at_null_doc_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST(NULL AS JSONB) @@ '$.a'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn at_at_in_where_filters_rows() {
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
            (1, CAST('{\"ok\":true}' AS JSONB)), \
            (2, CAST('{\"ok\":false}' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM t WHERE doc @@ '$.ok'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap() else {
        panic!();
    };
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Int(1) | Value::BigInt(1)));
}

#[test]
fn at_at_on_text_doc() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT '{\"flag\":true}' @@ '$.flag'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}
