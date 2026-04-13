//! Phase 11.21b — `@?` JSONPath-exists binary operator.

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
fn at_question_returns_true_on_match() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) @? '$.a'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn at_question_returns_false_on_miss() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) @? '$.z'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn at_question_null_doc_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST(NULL AS JSONB) @? '$.a'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn at_question_null_path_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) @? CAST(NULL AS TEXT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn at_question_in_where_filters() {
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
        "INSERT INTO t VALUES (1, CAST('{\"k\":1}' AS JSONB)), \
                               (2, CAST('{\"other\":2}' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM t WHERE doc @? '$.k'",
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
fn at_question_on_text_doc() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT '{\"a\":[1,2]}' @? '$.a'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}
