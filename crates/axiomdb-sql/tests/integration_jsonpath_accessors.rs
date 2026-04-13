//! Phase 11.21d (partial) — JSONPath `.size()` and `.type()` accessors.

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

// ── .size() ─────────────────────────────────────────────────────────────────

#[test]
fn size_of_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_FIRST('{\"xs\":[10,20,30]}', '$.xs.size()')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Int(3) | Value::BigInt(3)));
}

#[test]
fn size_of_scalar_is_one() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_FIRST('{\"a\":42}', '$.a.size()')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Int(1) | Value::BigInt(1)));
}

#[test]
fn size_via_query_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY('{\"xs\":[1,2,3,4]}', '$.xs.size()')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "[4]");
}

// ── .type() ─────────────────────────────────────────────────────────────────

#[test]
fn type_object() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_FIRST('{\"a\":{\"x\":1}}', '$.a.type()')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "object");
}

#[test]
fn type_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_FIRST('{\"xs\":[1]}', '$.xs.type()')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "array");
}

#[test]
fn type_integer() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_FIRST('{\"a\":7}', '$.a.type()')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "integer");
}

#[test]
fn type_string() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_FIRST('{\"a\":\"hi\"}', '$.a.type()')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "string");
}

#[test]
fn type_boolean() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_FIRST('{\"a\":true}', '$.a.type()')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "boolean");
}

#[test]
fn type_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_FIRST('{\"a\":null}', '$.a.type()')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "null");
}

#[test]
fn type_number_for_floats() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_FIRST('{\"a\":1.5}', '$.a.type()')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "number");
}

#[test]
fn unknown_accessor_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSONB_PATH_QUERY_FIRST('{\"a\":1}', '$.a.bogus()')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("unknown accessor");
    let msg = format!("{err}");
    assert!(msg.contains("bogus"));
}
