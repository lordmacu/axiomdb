//! Phase 11.19c — `PASSING expr AS name` clause on JSON_VALUE/QUERY/EXISTS.

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

#[test]
fn passing_clause_parses_unused_binding() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_VALUE('{\"a\":42}', '$.a' PASSING 5 AS threshold)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(axiomdb_types::Value::Text("42".into()), v);
}

#[test]
fn passing_multiple_bindings() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_VALUE('{\"a\":1}', '$.a' PASSING 5 AS x, 'str' AS y)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(axiomdb_types::Value::Text("1".into()), v);
}

#[test]
fn passing_before_returning_is_required_order() {
    let (mut s, mut t, mut b, mut c) = setup();
    // PASSING must come before RETURNING per SQL:2016.
    let v = scalar(
        "SELECT JSON_VALUE('{\"a\":1}', '$.a' PASSING 5 AS z RETURNING INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Int(1) | Value::BigInt(1)));
}

#[test]
fn passing_expression_evaluated() {
    let (mut s, mut t, mut b, mut c) = setup();
    // The binding is computed from an expression: 2 + 3 = 5.
    // Binding is unused in the path but must successfully evaluate without
    // producing any parse/exec error.
    let v = scalar(
        "SELECT JSON_VALUE('{\"a\":7}', '$.a' PASSING (2 + 3) AS sum)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(axiomdb_types::Value::Text("7".into()), v);
}

#[test]
fn passing_on_json_query_with_wrapper() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":1}', '$.a' \
            PASSING 9 AS k RETURNING TEXT WITH ARRAY WRAPPER)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(v, axiomdb_types::Value::Text("[1]".into()));
}

#[test]
fn passing_on_json_exists() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_EXISTS('{\"a\":1}', '$.a' PASSING 0 AS z)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(
        v,
        Value::Bool(true) | Value::Int(1) | Value::BigInt(1)
    ));
}

#[test]
fn passing_missing_as_is_parse_error() {
    let err = axiomdb_sql::parse("SELECT JSON_VALUE('{}', '$.a' PASSING 5 threshold)", None)
        .expect_err("missing AS after PASSING expr");
    let msg = format!("{err}");
    assert!(msg.to_uppercase().contains("AS"));
}
