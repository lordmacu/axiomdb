//! Phase 11.24d — Oracle `JSON_DATAGUIDE(doc)` schema discovery.

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

fn parse_array(v: &Value) -> Vec<serde_json::Value> {
    let s = match v {
        Value::Json(s) | Value::Text(s) => s.clone(),
        Value::Jsonb(b) => axiomdb_types::jsonb::JsonbDecoder::to_string(b.as_ref()).unwrap(),
        Value::Null => "null".into(),
        other => panic!("{other:?}"),
    };
    serde_json::from_str(&s).unwrap()
}

#[test]
fn dataguide_object_basic() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_DATAGUIDE('{\"a\":1,\"b\":\"x\"}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let arr = parse_array(&v);
    let entries: Vec<(String, String)> = arr
        .iter()
        .map(|e| {
            (
                e["path"].as_str().unwrap().into(),
                e["type"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert!(entries.contains(&("$".into(), "object".into())));
    assert!(entries.contains(&("$.a".into(), "integer".into())));
    assert!(entries.contains(&("$.b".into(), "string".into())));
}

#[test]
fn dataguide_nested_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_DATAGUIDE('{\"xs\":[1,2,3]}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let arr = parse_array(&v);
    let paths: Vec<String> = arr
        .iter()
        .map(|e| e["path"].as_str().unwrap().into())
        .collect();
    assert!(paths.contains(&"$.xs".into()));
    assert!(paths.contains(&"$.xs[0]".into()));
    assert!(paths.contains(&"$.xs[2]".into()));
}

#[test]
fn dataguide_distinguishes_integer_vs_number() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_DATAGUIDE('{\"a\":1,\"b\":1.5}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let arr = parse_array(&v);
    let mut got_int = false;
    let mut got_num = false;
    for e in &arr {
        let p = e["path"].as_str().unwrap();
        let t = e["type"].as_str().unwrap();
        if p == "$.a" && t == "integer" {
            got_int = true;
        }
        if p == "$.b" && t == "number" {
            got_num = true;
        }
    }
    assert!(got_int && got_num);
}

#[test]
fn dataguide_null_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_DATAGUIDE(NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn dataguide_scalar_root() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_DATAGUIDE('42')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let arr = parse_array(&v);
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["path"], "$");
    assert_eq!(arr[0]["type"], "integer");
}

#[test]
fn dataguide_deep_nesting() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_DATAGUIDE('{\"a\":{\"b\":{\"c\":true}}}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let arr = parse_array(&v);
    let paths: Vec<String> = arr
        .iter()
        .map(|e| e["path"].as_str().unwrap().into())
        .collect();
    assert!(paths.contains(&"$.a.b.c".into()));
}
