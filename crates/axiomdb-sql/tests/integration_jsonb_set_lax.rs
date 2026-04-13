//! Integration tests for Phase 11.22b — `jsonb_set_lax` with
//! `null_value_treatment` enum. Mirrors PG `jsonb_set_lax` semantics
//! (src/backend/utils/adt/jsonfuncs.c:4898-4959).

mod common;

use axiomdb_core::error::DbError;
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

#[test]
fn non_null_value_behaves_like_jsonb_set() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET_LAX(CAST('{\"a\":1}' AS JSONB), '$.a', CAST('42' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_json_text(&v).contains("\"a\":42"));
}

#[test]
fn default_treatment_embeds_json_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET_LAX(CAST('{\"a\":1}' AS JSONB), '$.a', NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_json_text(&v).contains("\"a\":null"));
}

#[test]
fn explicit_use_json_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET_LAX(CAST('{\"a\":1}' AS JSONB), '$.a', NULL, true, 'use_json_null')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_json_text(&v).contains("\"a\":null"));
}

#[test]
fn raise_exception_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSONB_SET_LAX(CAST('{\"a\":1}' AS JSONB), '$.a', NULL, true, 'raise_exception')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("raise_exception must error");
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn delete_key_removes_leaf() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET_LAX(CAST('{\"a\":1,\"b\":2}' AS JSONB), '$.a', NULL, true, 'delete_key')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_json_text(&v);
    assert!(!text.contains("\"a\""), "a should be gone: {text}");
    assert!(text.contains("\"b\":2"));
}

#[test]
fn return_target_unchanged() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET_LAX(CAST('{\"a\":1}' AS JSONB), '$.z', NULL, true, 'return_target')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_json_text(&v);
    assert!(text.contains("\"a\":1"));
    assert!(!text.contains("\"z\""));
}

#[test]
fn invalid_treatment_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSONB_SET_LAX(CAST('{\"a\":1}' AS JSONB), '$.a', NULL, true, 'bogus')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("invalid treatment must error");
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn null_target_returns_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET_LAX(NULL, '$.a', CAST('1' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn null_path_returns_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET_LAX(CAST('{\"a\":1}' AS JSONB), NULL, CAST('1' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn null_create_if_missing_returns_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET_LAX(CAST('{\"a\":1}' AS JSONB), '$.a', CAST('2' AS JSONB), NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn null_treatment_arg_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSONB_SET_LAX(CAST('{\"a\":1}' AS JSONB), '$.a', NULL, true, NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("null treatment must error");
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn create_if_missing_false_non_null_value_no_insert() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET_LAX(CAST('{\"a\":1}' AS JSONB), '$.z', CAST('9' AS JSONB), false)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_json_text(&v);
    assert!(!text.contains("\"z\""));
}
