//! Phase 11.24a — Oracle JSON surface: JSON_EQUAL, JSON_SCALAR, JSON_SERIALIZE.

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

fn as_text(v: &Value) -> String {
    match v {
        Value::Jsonb(b) => axiomdb_types::jsonb::JsonbDecoder::to_string(b.as_ref()).unwrap(),
        Value::Text(s) | Value::Json(s) => s.clone(),
        Value::Null => "NULL".into(),
        other => format!("{other:?}"),
    }
}

fn is_true(v: &Value) -> bool {
    matches!(v, Value::Bool(true) | Value::Int(1) | Value::BigInt(1))
}
fn is_false(v: &Value) -> bool {
    matches!(v, Value::Bool(false) | Value::Int(0) | Value::BigInt(0))
}

// ── JSON_EQUAL ─────────────────────────────────────────────────────────────-

#[test]
fn json_equal_true_identical() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_EQUAL('{\"a\":1}', '{\"a\":1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn json_equal_true_key_order_insensitive() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_EQUAL('{\"a\":1,\"b\":2}', '{\"b\":2,\"a\":1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn json_equal_false_different_values() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_EQUAL('{\"a\":1}', '{\"a\":2}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn json_equal_null_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_EQUAL(NULL, '{\"a\":1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn json_equal_deep_nesting() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_EQUAL('{\"a\":[1,{\"x\":2}]}', '{\"a\":[1,{\"x\":2}]}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn json_equal_jsonb_vs_text() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_EQUAL(CAST('{\"a\":1}' AS JSONB), '{\"a\":1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

// ── JSON_SCALAR ────────────────────────────────────────────────────────────-

#[test]
fn json_scalar_int() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar("SELECT JSON_SCALAR(42)", &mut s, &mut t, &mut b, &mut c);
    assert_eq!(as_text(&v), "42");
}

#[test]
fn json_scalar_text() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar("SELECT JSON_SCALAR('hi')", &mut s, &mut t, &mut b, &mut c);
    assert_eq!(as_text(&v), "\"hi\"");
}

#[test]
fn json_scalar_null_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar("SELECT JSON_SCALAR(NULL)", &mut s, &mut t, &mut b, &mut c);
    assert!(matches!(v, Value::Null));
}

// ── JSON_SERIALIZE ─────────────────────────────────────────────────────────-

#[test]
fn json_serialize_object() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SERIALIZE(CAST('{\"a\":1}' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Text(_)));
    assert_eq!(as_text(&v), "{\"a\":1}");
}

#[test]
fn json_serialize_from_text_normalizes() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SERIALIZE('[1, 2 , 3]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Text(_)));
    assert_eq!(as_text(&v), "[1,2,3]");
}

#[test]
fn json_serialize_null_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SERIALIZE(NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}
