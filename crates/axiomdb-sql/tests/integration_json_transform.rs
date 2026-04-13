//! Phase 11.24b — `JSON_TRANSFORM` variadic multi-op mutator.

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
fn set_op() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_TRANSFORM('{\"a\":1}', 'SET', '$.b', 2)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(text.contains("\"a\":1"));
    assert!(text.contains("\"b\":2"));
}

#[test]
fn remove_op() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_TRANSFORM('{\"a\":1,\"b\":2}', 'REMOVE', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(!text.contains("\"a\""));
    assert!(text.contains("\"b\":2"));
}

#[test]
fn rename_op() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_TRANSFORM('{\"old\":1}', 'RENAME', '$.old', 'new_key')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(!text.contains("\"old\""));
    assert!(text.contains("\"new_key\":1"));
}

#[test]
fn append_op() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_TRANSFORM('{\"xs\":[1,2]}', 'APPEND', '$.xs', 3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_text(&v).contains("[1,2,3]"));
}

#[test]
fn insert_op_creates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_TRANSFORM('{\"a\":1}', 'INSERT', '$.b', 2)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_text(&v).contains("\"b\":2"));
}

#[test]
fn insert_op_noop_on_existing() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_TRANSFORM('{\"a\":1}', 'INSERT', '$.a', 99)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_text(&v).contains("\"a\":1"));
    assert!(!as_text(&v).contains("99"));
}

#[test]
fn replace_op_only_if_exists() {
    let (mut s, mut t, mut b, mut c) = setup();
    // REPLACE with create_if_missing=false → no change on missing key.
    let v = scalar(
        "SELECT JSON_TRANSFORM('{\"a\":1}', 'REPLACE', '$.z', 99)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(!as_text(&v).contains("\"z\""));
    let v = scalar(
        "SELECT JSON_TRANSFORM('{\"a\":1}', 'REPLACE', '$.a', 99)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_text(&v).contains("\"a\":99"));
}

#[test]
fn multi_op_sequential() {
    let (mut s, mut t, mut b, mut c) = setup();
    // SET b=2, REMOVE a, RENAME b→bb.
    let v = scalar(
        "SELECT JSON_TRANSFORM('{\"a\":1}', \
            'SET', '$.b', 2, \
            'REMOVE', '$.a', \
            'RENAME', '$.b', 'bb')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_text(&v);
    assert!(!text.contains("\"a\""));
    assert!(!text.contains("\"b\":"));
    assert!(text.contains("\"bb\":2"));
}

#[test]
fn unknown_op_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSON_TRANSFORM('{}', 'BOGUS', '$.a', 1)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("unknown op");
    let msg = format!("{err}");
    assert!(msg.to_ascii_uppercase().contains("BOGUS"));
}

#[test]
fn null_doc_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_TRANSFORM(NULL, 'SET', '$.a', 1)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn missing_op_args_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSON_TRANSFORM('{}', 'SET', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("SET needs path + value");
    let msg = format!("{err}");
    assert!(msg.to_uppercase().contains("SET"));
}
