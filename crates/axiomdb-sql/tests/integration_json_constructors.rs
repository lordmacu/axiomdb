//! Phase 11.25b — JSON constructors + merge_preserve + contains_path.

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

// ── JSON_ARRAY ──────────────────────────────────────────────────────────────

#[test]
fn json_array_empty() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar("SELECT JSON_ARRAY()", &mut s, &mut t, &mut b, &mut c);
    assert_eq!(as_text(&v), "[]");
}

#[test]
fn json_array_mixed_types() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_ARRAY(1, 'two', true, NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.starts_with('['));
    assert!(text.contains('1'));
    assert!(text.contains("\"two\""));
    assert!(text.contains("null"));
}

// ── JSON_OBJECT ─────────────────────────────────────────────────────────────

#[test]
fn json_object_basic() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_OBJECT('a', 1, 'b', 'x')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"a\":1"));
    assert!(text.contains("\"b\":\"x\""));
}

#[test]
fn json_object_odd_args_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSON_OBJECT('a', 1, 'b')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("odd arg count");
    let msg = format!("{err}");
    assert!(msg.to_ascii_lowercase().contains("even"));
}

// ── JSON_MERGE_PRESERVE ─────────────────────────────────────────────────────

#[test]
fn merge_preserve_arrays_concat() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_MERGE_PRESERVE('[1,2]', '[3,4]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "[1,2,3,4]");
}

#[test]
fn merge_preserve_objects_key_conflict_wraps_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_MERGE_PRESERVE('{\"a\":1}', '{\"a\":2}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_text(&v).contains("\"a\":[1,2]"));
}

#[test]
fn merge_preserve_object_plus_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_MERGE_PRESERVE('{\"a\":1}', '[2,3]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.starts_with('['));
    assert!(text.contains("{\"a\":1}"));
}

#[test]
fn merge_preserve_null_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_MERGE_PRESERVE('[1]', NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn json_merge_alias() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_MERGE('[1]', '[2]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "[1,2]");
}

// ── JSON_CONTAINS_PATH ──────────────────────────────────────────────────────

#[test]
fn contains_path_one_hit() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_CONTAINS_PATH('{\"a\":1,\"b\":2}', 'one', '$.z', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn contains_path_one_miss() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_CONTAINS_PATH('{\"a\":1}', 'one', '$.x', '$.y')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn contains_path_all_ok() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_CONTAINS_PATH('{\"a\":1,\"b\":2}', 'all', '$.a', '$.b')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn contains_path_all_miss() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_CONTAINS_PATH('{\"a\":1}', 'all', '$.a', '$.z')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn contains_path_bad_mode_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSON_CONTAINS_PATH('{}', 'bogus', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("bad mode");
    let msg = format!("{err}");
    assert!(msg.contains("one") || msg.contains("all"));
}

#[test]
fn contains_path_null_doc() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_CONTAINS_PATH(NULL, 'one', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}
