//! Phase 11.23a — `JSON_SCHEMA_VALID(schema, doc)` Draft-07 subset.

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
fn type_string_ok() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"string\"}', '\"hi\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn type_string_fails_on_number() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"string\"}', '42')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn type_integer_accepts_whole_float() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"integer\"}', '3.0')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn type_array_union() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":[\"integer\",\"null\"]}', 'null')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn required_ok() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"object\",\"required\":[\"a\"]}', '{\"a\":1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn required_missing_fails() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"object\",\"required\":[\"a\"]}', '{\"b\":1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn properties_recurse() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            '{\"properties\":{\"a\":{\"type\":\"integer\",\"minimum\":0}}}', \
            '{\"a\":-1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn additional_properties_false_rejects_extra() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            '{\"properties\":{\"a\":{\"type\":\"integer\"}},\"additionalProperties\":false}', \
            '{\"a\":1,\"b\":2}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn minimum_maximum() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"minimum\":0,\"maximum\":10}', '5')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"minimum\":0,\"maximum\":10}', '11')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn exclusive_bounds() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"exclusiveMinimum\":0}', '0')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn min_max_length() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"string\",\"minLength\":3}', '\"ab\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn array_items_homogeneous() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"array\",\"items\":{\"type\":\"integer\"}}', '[1,2,3]')",
        &mut s, &mut t, &mut b, &mut c,
    );
    assert!(is_true(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"type\":\"array\",\"items\":{\"type\":\"integer\"}}', '[1,\"x\"]')",
        &mut s, &mut t, &mut b, &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn enum_check() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"enum\":[\"red\",\"green\"]}', '\"blue\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"enum\":[\"red\",\"green\"]}', '\"red\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn const_check() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('{\"const\":42}', '42')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn true_schema_accepts_all() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('true', '{\"x\":1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn false_schema_rejects_all() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID('false', '42')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn null_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(NULL, '42')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn jsonb_input() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALID(\
            CAST('{\"type\":\"object\",\"required\":[\"a\"]}' AS JSONB), \
            CAST('{\"a\":1}' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}
