//! Integration tests for Phase 11.21a — PG `jsonb_path_*` family.

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

fn as_json_text(v: &Value) -> String {
    match v {
        Value::Jsonb(b) => axiomdb_types::jsonb::JsonbDecoder::to_string(b.as_ref()).unwrap(),
        Value::Json(s) | Value::Text(s) => s.clone(),
        Value::Null => "NULL".into(),
        other => panic!("unexpected value: {other:?}"),
    }
}

fn is_true(v: &Value) -> bool {
    matches!(v, Value::Bool(true) | Value::Int(1) | Value::BigInt(1))
}
fn is_false(v: &Value) -> bool {
    matches!(v, Value::Bool(false) | Value::Int(0) | Value::BigInt(0))
}

#[test]
fn path_exists_match() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_EXISTS('{\"a\":1}', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn path_exists_miss() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_EXISTS('{\"a\":1}', '$.z')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn path_exists_null_doc() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_EXISTS(NULL, '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn path_query_returns_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY('[1,2,3]', '$[*]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_json_text(&v);
    assert!(text.contains('1') && text.contains('2') && text.contains('3'));
}

#[test]
fn path_query_first_match() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_FIRST('[10,20,30]', '$[*]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Int(10) | Value::BigInt(10)));
}

#[test]
fn path_query_first_miss() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_FIRST('{\"a\":1}', '$.z')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn path_query_array_packs_matches() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY('[1,2,3]', '$[*]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_json_text(&v), "[1,2,3]");
}

#[test]
fn path_query_array_empty_on_miss() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_QUERY_ARRAY('{}', '$.z')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_json_text(&v), "[]");
}

#[test]
fn path_match_true() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_MATCH('{\"a\":true}', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn path_match_false() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_MATCH('{\"a\":false}', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn path_match_non_boolean_is_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_MATCH('{\"a\":1}', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn path_match_no_result_is_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_MATCH('{}', '$.z')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn path_match_null_doc() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_PATH_MATCH(NULL, '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}
