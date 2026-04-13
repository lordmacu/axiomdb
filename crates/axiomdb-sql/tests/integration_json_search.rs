//! Phase 11.25c — MySQL `JSON_SEARCH(doc, one|all, pattern)`.

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

#[test]
fn search_one_finds_first() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SEARCH('{\"a\":\"hello\",\"b\":\"world\"}', 'one', 'hello')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "$.a");
}

#[test]
fn search_all_returns_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SEARCH('[\"x\",\"y\",\"x\"]', 'all', 'x')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("$[0]"));
    assert!(text.contains("$[2]"));
}

#[test]
fn search_no_match_returns_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SEARCH('{\"a\":\"x\"}', 'one', 'z')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn search_like_percent_wildcard() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SEARCH('{\"a\":\"hello\"}', 'one', 'he%')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "$.a");
}

#[test]
fn search_like_underscore_wildcard() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SEARCH('{\"a\":\"hi\"}', 'one', 'h_')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "$.a");
}

#[test]
fn search_nested() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SEARCH('{\"top\":{\"inner\":\"target\"}}', 'one', 'target')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "$.top.inner");
}

#[test]
fn search_ignores_non_strings() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SEARCH('{\"a\":42,\"b\":\"42\"}', 'one', '42')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "$.b");
}

#[test]
fn search_bad_mode_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSON_SEARCH('{}', 'bogus', 'x')",
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
fn search_null_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SEARCH(NULL, 'one', 'x')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}
