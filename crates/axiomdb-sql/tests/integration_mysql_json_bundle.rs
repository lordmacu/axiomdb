//! Phase 11.25a — MySQL JSON completion bundle.

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

// ── JSON_QUOTE / JSON_UNQUOTE ──────────────────────────────────────────────-

#[test]
fn json_quote_escapes_and_wraps() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar("SELECT JSON_QUOTE('a\"b')", &mut s, &mut t, &mut b, &mut c);
    assert_eq!(as_text(&v), "\"a\\\"b\"");
}

#[test]
fn json_quote_null_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar("SELECT JSON_QUOTE(NULL)", &mut s, &mut t, &mut b, &mut c);
    assert!(matches!(v, Value::Null));
}

#[test]
fn json_unquote_strips_quotes() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_UNQUOTE('\"hello\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "hello");
}

#[test]
fn json_unquote_non_string_unchanged() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_UNQUOTE('[1,2]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "[1,2]");
}

// ── JSON_LENGTH ────────────────────────────────────────────────────────────-

#[test]
fn json_length_object_keys() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_LENGTH('{\"a\":1,\"b\":2,\"c\":3}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::BigInt(3) | Value::Int(3)));
}

#[test]
fn json_length_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_LENGTH('[1,2,3,4]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::BigInt(4) | Value::Int(4)));
}

#[test]
fn json_length_scalar() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar("SELECT JSON_LENGTH('42')", &mut s, &mut t, &mut b, &mut c);
    assert!(matches!(v, Value::BigInt(1) | Value::Int(1)));
}

#[test]
fn json_length_with_path() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_LENGTH('{\"a\":[1,2,3]}', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::BigInt(3) | Value::Int(3)));
}

#[test]
fn json_length_missing_path_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_LENGTH('{\"a\":1}', '$.z')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

// ── JSON_STORAGE_SIZE ──────────────────────────────────────────────────────-

#[test]
fn json_storage_size_positive() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_STORAGE_SIZE('{\"a\":1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let n = match v {
        Value::BigInt(n) => n,
        Value::Int(n) => n as i64,
        other => panic!("{other:?}"),
    };
    assert!(n > 0);
}

#[test]
fn json_storage_size_null_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_STORAGE_SIZE(NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

// ── JSON_ARRAY_APPEND ──────────────────────────────────────────────────────-

#[test]
fn json_array_append_to_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_ARRAY_APPEND('{\"a\":[1,2]}', '$.a', 3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_text(&v).contains("[1,2,3]"));
}

#[test]
fn json_array_append_wraps_non_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_ARRAY_APPEND('{\"a\":1}', '$.a', 2)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_text(&v).contains("[1,2]"));
}

#[test]
fn json_array_append_multiple_pairs() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_ARRAY_APPEND('{\"a\":[1],\"b\":[2]}', '$.a', 10, '$.b', 20)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("[1,10]"));
    assert!(text.contains("[2,20]"));
}

// ── JSON_ARRAY_INSERT ──────────────────────────────────────────────────────-

#[test]
fn json_array_insert_at_index() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_ARRAY_INSERT('[1,3]', '$[1]', 2)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "[1,2,3]");
}

#[test]
fn json_array_insert_beyond_end_appends() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_ARRAY_INSERT('[1,2]', '$[99]', 9)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "[1,2,9]");
}

#[test]
fn json_array_insert_null_doc() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_ARRAY_INSERT(NULL, '$[0]', 1)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}
