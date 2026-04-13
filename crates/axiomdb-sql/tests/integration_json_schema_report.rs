//! Phase 11.23b — `JSON_SCHEMA_VALIDATION_REPORT` detailed error report.

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

fn parse_report(v: &Value) -> Vec<serde_json::Value> {
    let s = match v {
        Value::Json(s) | Value::Text(s) => s.clone(),
        Value::Jsonb(b) => axiomdb_types::jsonb::JsonbDecoder::to_string(b.as_ref()).unwrap(),
        Value::Null => "null".into(),
        other => panic!("unexpected: {other:?}"),
    };
    serde_json::from_str(&s).unwrap()
}

#[test]
fn valid_doc_empty_report() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALIDATION_REPORT('{\"type\":\"integer\"}', '42')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(parse_report(&v).len(), 0);
}

#[test]
fn type_mismatch_reported() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALIDATION_REPORT('{\"type\":\"integer\"}', '\"not-int\"')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let errs = parse_report(&v);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["keyword"], "type");
    assert_eq!(errs[0]["path"], "#");
}

#[test]
fn missing_required_reported() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALIDATION_REPORT(\
            '{\"type\":\"object\",\"required\":[\"a\",\"b\"]}', \
            '{\"a\":1}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let errs = parse_report(&v);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["keyword"], "required");
    assert!(errs[0]["message"].as_str().unwrap().contains("`b`"));
}

#[test]
fn nested_property_path_in_error() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALIDATION_REPORT(\
            '{\"properties\":{\"name\":{\"type\":\"string\",\"minLength\":3}}}', \
            '{\"name\":\"ab\"}')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let errs = parse_report(&v);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["path"], "#/name");
    assert_eq!(errs[0]["keyword"], "minLength");
}

#[test]
fn array_item_index_in_path() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALIDATION_REPORT(\
            '{\"type\":\"array\",\"items\":{\"type\":\"integer\"}}', \
            '[1,\"x\",3]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let errs = parse_report(&v);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["path"], "#/1");
    assert_eq!(errs[0]["keyword"], "type");
}

#[test]
fn multiple_failures_accumulate() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALIDATION_REPORT(\
            '{\"minimum\":0,\"maximum\":10}', \
            '100')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let errs = parse_report(&v);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["keyword"], "maximum");
}

#[test]
fn null_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALIDATION_REPORT(NULL, '42')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn ref_path_reported() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALIDATION_REPORT(\
            '{\"definitions\":{\"pos\":{\"type\":\"integer\",\"minimum\":0}},\
              \"$ref\":\"#/definitions/pos\"}', \
            '-5')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let errs = parse_report(&v);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["keyword"], "minimum");
}

#[test]
fn unresolved_ref_reported() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_SCHEMA_VALIDATION_REPORT('{\"$ref\":\"#/nope\"}', '1')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let errs = parse_report(&v);
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["keyword"], "$ref");
}
