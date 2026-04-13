//! Phase 11.19b — `WITH [CONDITIONAL|UNCONDITIONAL] ARRAY WRAPPER` and
//! `KEEP|OMIT QUOTES` clauses on `JSON_QUERY`.

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
fn without_wrapper_single_item_ok() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":{\"b\":1}}', '$.a' RETURNING TEXT WITHOUT ARRAY WRAPPER)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "{\"b\":1}");
}

#[test]
fn with_unconditional_wrapper_wraps_single_item() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":1}', '$.a' RETURNING TEXT WITH UNCONDITIONAL ARRAY WRAPPER)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "[1]");
}

#[test]
fn with_wrapper_defaults_to_unconditional() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":1}', '$.a' RETURNING TEXT WITH ARRAY WRAPPER)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "[1]");
}

#[test]
fn with_conditional_wrapper_skips_array_results() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":[1,2,3]}', '$.a' RETURNING TEXT WITH CONDITIONAL ARRAY WRAPPER)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "[1,2,3]");
}

#[test]
fn with_conditional_wrapper_wraps_non_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":\"str\"}', '$.a' RETURNING TEXT WITH CONDITIONAL ARRAY WRAPPER)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "[\"str\"]");
}

#[test]
fn omit_quotes_scalar_string_strips_quotes() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":\"hello\"}', '$.a' OMIT QUOTES)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "hello");
}

#[test]
fn omit_quotes_on_scalar_string_suffix_parses() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":\"hi\"}', '$.a' OMIT QUOTES ON SCALAR STRING)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "hi");
}

#[test]
fn keep_quotes_default_preserves_quotes() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":\"hello\"}', '$.a' RETURNING TEXT KEEP QUOTES)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "\"hello\"");
}

#[test]
fn omit_quotes_on_non_string_has_no_effect() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":{\"b\":1}}', '$.a' RETURNING TEXT OMIT QUOTES)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "{\"b\":1}");
}

#[test]
fn wrapper_rejected_on_json_value() {
    let err = axiomdb_sql::parse(
        "SELECT JSON_VALUE('{\"a\":1}', '$.a' WITH ARRAY WRAPPER)",
        None,
    )
    .expect_err("WRAPPER invalid on JSON_VALUE");
    let msg = format!("{err}");
    assert!(msg.to_ascii_uppercase().contains("JSON_QUERY"));
}

#[test]
fn quotes_rejected_on_json_value() {
    let err = axiomdb_sql::parse(
        "SELECT JSON_VALUE('{\"a\":\"hi\"}', '$.a' OMIT QUOTES)",
        None,
    )
    .expect_err("OMIT QUOTES invalid on JSON_VALUE");
    let msg = format!("{err}");
    assert!(msg.to_ascii_uppercase().contains("JSON_QUERY"));
}

#[test]
fn wrapper_rejected_on_json_exists() {
    let err = axiomdb_sql::parse(
        "SELECT JSON_EXISTS('{\"a\":1}', '$.a' WITH ARRAY WRAPPER)",
        None,
    )
    .expect_err("WRAPPER invalid on JSON_EXISTS");
    let msg = format!("{err}");
    assert!(msg.to_ascii_uppercase().contains("JSON_QUERY"));
}
