//! Integration tests for Phase 11.19a — SQL:2016 JSON query functions.
//!
//! Covers `JSON_VALUE`, `JSON_QUERY`, `JSON_EXISTS` with:
//!   - strict vs lax path mode (default strict, SQL:2016 + PG default),
//!   - RETURNING type coercion,
//!   - ON EMPTY / ON ERROR dispatch (ERROR / NULL / DEFAULT expr),
//!   - JSON_EXISTS-only ON ERROR { TRUE | FALSE | UNKNOWN }.

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

fn is_true(v: &Value) -> bool {
    matches!(v, Value::Bool(true) | Value::Int(1) | Value::BigInt(1))
}

fn is_false(v: &Value) -> bool {
    matches!(v, Value::Bool(false) | Value::Int(0) | Value::BigInt(0))
}

fn as_text(v: &Value) -> String {
    match v {
        Value::Text(s) | Value::Json(s) => s.clone(),
        Value::Jsonb(b) => axiomdb_types::jsonb::JsonbDecoder::to_string(b.as_ref()).unwrap(),
        Value::Null => "NULL".into(),
        other => format!("{other:?}"),
    }
}

// ── JSON_VALUE ──────────────────────────────────────────────────────────────

#[test]
fn value_extracts_scalar_as_text_by_default() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_VALUE('{\"a\":42}', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "42");
}

#[test]
fn value_returning_int_coerces() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_VALUE('{\"a\":42}', '$.a' RETURNING INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Int(42) | Value::BigInt(42)));
}

#[test]
fn value_returning_bool_coerces() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_VALUE('{\"ok\":true}', '$.ok' RETURNING BOOL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn value_on_missing_key_strict_routes_to_on_error() {
    let (mut s, mut t, mut b, mut c) = setup();
    // Default path mode = strict. Missing key → ON ERROR. Default ON ERROR = NULL.
    let v = scalar(
        "SELECT JSON_VALUE('{\"a\":1}', '$.z')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn value_on_missing_key_explicit_strict_mode() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_VALUE('{\"a\":1}', 'strict $.z' RETURNING INT ON ERROR DEFAULT '99')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Int(99) | Value::BigInt(99)));
}

#[test]
fn value_on_missing_key_lax_mode_returns_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    // lax: missing key = empty result → ON EMPTY (default NULL).
    let v = scalar(
        "SELECT JSON_VALUE('{\"a\":1}', 'lax $.z')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn value_on_empty_default_expression_runs() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_VALUE('{\"a\":1}', 'lax $.z' ON EMPTY DEFAULT 'fallback')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "fallback");
}

#[test]
fn value_array_match_routes_to_on_error() {
    let (mut s, mut t, mut b, mut c) = setup();
    // Match is an array → JSON_VALUE rejects (scalar-only).
    let v = scalar(
        "SELECT JSON_VALUE('{\"a\":[1,2]}', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn value_null_doc_returns_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_VALUE(NULL, '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn value_on_error_error_raises() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSON_VALUE('{\"a\":1}', 'strict $.z' ON ERROR ERROR)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("ON ERROR ERROR must raise");
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn value_coercion_failure_routes_to_on_error() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_VALUE('{\"a\":\"not-a-number\"}', '$.a' RETURNING INT \
            ON ERROR DEFAULT -1)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Int(-1) | Value::BigInt(-1)));
}

// ── JSON_QUERY ──────────────────────────────────────────────────────────────

#[test]
fn query_returns_jsonb_by_default() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":[1,2,3]}', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // Either Jsonb or Json surface depending on encoding choice.
    assert_eq!(as_text(&v), "[1,2,3]");
}

#[test]
fn query_returning_text_renders_json_text() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":{\"b\":1}}', '$.a' RETURNING TEXT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "{\"b\":1}");
}

#[test]
fn query_on_empty_null_default() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":1}', 'lax $.z')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn query_on_empty_default_expr() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_QUERY('{\"a\":1}', 'lax $.z' RETURNING TEXT \
            ON EMPTY DEFAULT '[]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "[]");
}

// ── JSON_EXISTS ─────────────────────────────────────────────────────────────

#[test]
fn exists_true_on_match() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_EXISTS('{\"a\":1}', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn exists_false_on_miss_lax() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_EXISTS('{\"a\":1}', 'lax $.z')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn exists_strict_miss_routes_via_on_error() {
    let (mut s, mut t, mut b, mut c) = setup();
    // Default ON ERROR for JSON_EXISTS = FALSE.
    let v = scalar(
        "SELECT JSON_EXISTS('{\"a\":1}', 'strict $.z')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn exists_on_error_true_literal() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_EXISTS('{\"a\":1}', 'strict $.z' ON ERROR TRUE)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn exists_on_error_unknown_returns_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_EXISTS('{\"a\":1}', 'strict $.z' ON ERROR UNKNOWN)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

#[test]
fn exists_on_error_error_raises() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSON_EXISTS('{\"a\":1}', 'strict $.z' ON ERROR ERROR)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("strict miss + ON ERROR ERROR must raise");
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

// ── Row-level / column-level ────────────────────────────────────────────────

#[test]
fn operators_work_on_column_values() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, doc JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, CAST('{\"tier\":3}' AS JSONB)), \
                               (2, CAST('{\"tier\":1}' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM t WHERE JSON_VALUE(doc, '$.tier' RETURNING INT) >= 3",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap() else {
        panic!();
    };
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Int(1) | Value::BigInt(1)));
}

// ── Wildcard rejection ──────────────────────────────────────────────────────

#[test]
fn wildcard_path_is_rejected() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_VALUE('{\"a\":1}', '$.*' ON ERROR DEFAULT 'rejected')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_text(&v), "rejected");
}

// ── Clause ordering errors ──────────────────────────────────────────────────

#[test]
fn clause_wrong_ordering_is_parse_error() {
    let err = axiomdb_sql::parse(
        "SELECT JSON_VALUE('{}', '$.a' ON EMPTY NULL RETURNING INT)",
        None,
    )
    .expect_err("RETURNING after ON EMPTY must fail to parse");
    let msg = format!("{err}");
    assert!(
        msg.to_ascii_lowercase().contains("expected")
            || msg.contains("EMPTY")
            || msg.contains("ERROR"),
        "unexpected error: {msg}",
    );
}

#[test]
fn exists_rejects_returning_clause() {
    let err = axiomdb_sql::parse("SELECT JSON_EXISTS('{}', '$.a' RETURNING INT)", None)
        .expect_err("RETURNING is not valid on JSON_EXISTS");
    let _ = err;
}
