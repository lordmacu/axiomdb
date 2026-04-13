//! Phase 11.18c — `#>`, `#>>`, `#-` JSONB path operators (JSONB-array RHS).

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

// ── #> path-extract as JSONB ────────────────────────────────────────────────

#[test]
fn extract_object_subtree() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":{\"b\":{\"c\":1}}}' AS JSONB) #> CAST('[\"a\",\"b\"]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "{\"c\":1}");
}

#[test]
fn extract_array_index() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"xs\":[10,20,30]}' AS JSONB) #> CAST('[\"xs\",1]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "20");
}

#[test]
fn extract_missing_path_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) #> CAST('[\"z\"]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

// ── #>> path-extract as TEXT ────────────────────────────────────────────────

#[test]
fn extract_text_strips_quotes() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":\"hello\"}' AS JSONB) #>> CAST('[\"a\"]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Text(_)));
    assert_eq!(as_text(&v), "hello");
}

#[test]
fn extract_text_renders_int() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":42}' AS JSONB) #>> CAST('[\"a\"]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "42");
}

// ── #- path-delete ──────────────────────────────────────────────────────────

#[test]
fn delete_nested_key() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":{\"b\":1,\"c\":2}}' AS JSONB) #- CAST('[\"a\",\"b\"]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"c\":2"));
    assert!(!text.contains("\"b\":1"));
}

#[test]
fn delete_array_index() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"xs\":[10,20,30]}' AS JSONB) #- CAST('[\"xs\",1]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("[10,30]"));
}

#[test]
fn delete_missing_path_unchanged() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) #- CAST('[\"z\"]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "{\"a\":1}");
}

// ── NULL propagation ────────────────────────────────────────────────────────

#[test]
fn null_lhs_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST(NULL AS JSONB) #> CAST('[\"a\"]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn null_rhs_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) #- CAST(NULL AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}
