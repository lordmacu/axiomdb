//! Integration tests for Phase 11.22a — JSONB mutation parity.
//!
//! Covers PG `jsonb_set`, `jsonb_insert`, `jsonb_delete_path` and MySQL
//! `JSON_INSERT`, `JSON_REPLACE` (complementing existing `JSON_SET` /
//! `JSON_REMOVE` from Phase 11.4). Asserts the deliberate divergence
//! between PG `jsonb_insert` (raises on existing key) and MySQL
//! `JSON_INSERT` (silent no-op).

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

fn as_json_text(v: &Value) -> String {
    match v {
        Value::Jsonb(b) => axiomdb_types::jsonb::JsonbDecoder::to_string(b.as_ref()).unwrap(),
        Value::Json(s) | Value::Text(s) => s.clone(),
        Value::Null => "NULL".into(),
        other => panic!("unexpected value: {other:?}"),
    }
}

// ── Path normalizer ─────────────────────────────────────────────────────────

#[test]
fn path_string_form_sets() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET(CAST('{\"a\":1}' AS JSONB), '$.b', CAST('2' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_json_text(&v);
    assert!(text.contains("\"a\":1"), "got {text}");
    assert!(text.contains("\"b\":2"), "got {text}");
}

#[test]
fn path_json_array_form_sets() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET(CAST('{\"a\":1}' AS JSONB), '[\"b\"]', CAST('2' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_json_text(&v).contains("\"b\":2"));
}

#[test]
fn path_rejects_wildcard() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSONB_SET(CAST('{\"a\":1}' AS JSONB), '$.*', CAST('2' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("wildcard path must be rejected");
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

// ── JSONB_SET ───────────────────────────────────────────────────────────────

#[test]
fn jsonb_set_updates_existing_leaf() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET(CAST('{\"a\":1}' AS JSONB), '$.a', CAST('99' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_json_text(&v), "{\"a\":99}");
}

#[test]
fn jsonb_set_creates_missing_when_default() {
    let (mut s, mut t, mut b, mut c) = setup();
    // default create_if_missing = true
    let v = scalar(
        "SELECT JSONB_SET(CAST('{}' AS JSONB), '$.x', CAST('5' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_json_text(&v), "{\"x\":5}");
}

#[test]
fn jsonb_set_noop_when_create_false_and_missing() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET(CAST('{\"a\":1}' AS JSONB), '$.z', CAST('9' AS JSONB), FALSE)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // Input should be unchanged
    assert_eq!(as_json_text(&v), "{\"a\":1}");
}

#[test]
fn jsonb_set_negative_array_index() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET(CAST('[1,2,3]' AS JSONB), '$[-1]', CAST('99' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_json_text(&v), "[1,2,99]");
}

#[test]
fn jsonb_set_scalar_root_raises() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSONB_SET(CAST('42' AS JSONB), '$.a', CAST('1' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("set on scalar root must error");
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn jsonb_set_null_target_is_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_SET(CAST(NULL AS JSONB), '$.a', CAST('1' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

// ── JSONB_INSERT ───────────────────────────────────────────────────────────-

#[test]
fn jsonb_insert_array_before() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_INSERT(CAST('[1,2,3]' AS JSONB), '$[1]', CAST('99' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_json_text(&v), "[1,99,2,3]");
}

#[test]
fn jsonb_insert_array_after() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_INSERT(CAST('[1,2,3]' AS JSONB), '$[1]', CAST('99' AS JSONB), TRUE)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_json_text(&v), "[1,2,99,3]");
}

#[test]
fn jsonb_insert_object_missing_key_adds() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_INSERT(CAST('{\"a\":1}' AS JSONB), '$.b', CAST('2' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_json_text(&v);
    assert!(text.contains("\"a\":1"));
    assert!(text.contains("\"b\":2"));
}

#[test]
fn jsonb_insert_object_existing_key_raises() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSONB_INSERT(CAST('{\"a\":1}' AS JSONB), '$.a', CAST('99' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("PG jsonb_insert must raise on existing key");
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

// ── JSONB_DELETE_PATH ──────────────────────────────────────────────────────-

#[test]
fn jsonb_delete_path_removes_object_key() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_DELETE_PATH(CAST('{\"a\":1,\"b\":2}' AS JSONB), '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_json_text(&v), "{\"b\":2}");
}

#[test]
fn jsonb_delete_path_removes_array_element() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSONB_DELETE_PATH(CAST('[1,2,3]' AS JSONB), '$[1]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_json_text(&v), "[1,3]");
}

#[test]
fn jsonb_delete_path_scalar_root_raises() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSONB_DELETE_PATH(CAST('42' AS JSONB), '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("delete path on scalar root must error");
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

// ── JSON_INSERT (MySQL) ────────────────────────────────────────────────────-

#[test]
fn json_insert_missing_key_adds() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_INSERT('{\"a\":1}', '$.b', 2)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_json_text(&v);
    assert!(text.contains("\"a\":1"));
    assert!(text.contains("\"b\":2"));
}

#[test]
fn json_insert_existing_key_silent_noop() {
    let (mut s, mut t, mut b, mut c) = setup();
    // MySQL: JSON_INSERT on existing key is a silent no-op (diverges from PG).
    let v = scalar(
        "SELECT JSON_INSERT('{\"a\":1}', '$.a', 99)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_json_text(&v), "{\"a\":1}");
}

#[test]
fn json_insert_variadic_multiple_pairs() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_INSERT('{\"a\":1}', '$.b', 2, '$.a', 99, '$.c', 3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_json_text(&v);
    // 'a' is NOT overwritten (existing); 'b' and 'c' added.
    assert!(text.contains("\"a\":1"));
    assert!(text.contains("\"b\":2"));
    assert!(text.contains("\"c\":3"));
    assert!(!text.contains("\"a\":99"));
}

// ── JSON_REPLACE (MySQL) ───────────────────────────────────────────────────-

#[test]
fn json_replace_existing_updates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_REPLACE('{\"a\":1}', '$.a', 99)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_json_text(&v), "{\"a\":99}");
}

#[test]
fn json_replace_missing_silent_noop() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_REPLACE('{\"a\":1}', '$.z', 99)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(as_json_text(&v), "{\"a\":1}");
}

#[test]
fn json_replace_variadic_mixed() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT JSON_REPLACE('{\"a\":1,\"b\":2}', '$.a', 99, '$.z', 0)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = as_json_text(&v);
    assert!(text.contains("\"a\":99"));
    assert!(text.contains("\"b\":2"));
    assert!(!text.contains("\"z\""));
}

// ── Cross-cutting ──────────────────────────────────────────────────────────-

#[test]
fn null_target_returns_null_for_every_fn() {
    let (mut s, mut t, mut b, mut c) = setup();
    for sql in [
        "SELECT JSONB_SET(CAST(NULL AS JSONB), '$.a', CAST('1' AS JSONB))",
        "SELECT JSONB_INSERT(CAST(NULL AS JSONB), '$.a', CAST('1' AS JSONB))",
        "SELECT JSONB_DELETE_PATH(CAST(NULL AS JSONB), '$.a')",
        "SELECT JSON_INSERT(NULL, '$.a', 1)",
        "SELECT JSON_REPLACE(NULL, '$.a', 1)",
    ] {
        let v = scalar(sql, &mut s, &mut t, &mut b, &mut c);
        assert!(
            matches!(v, Value::Null),
            "expected NULL for {sql:?}, got {v:?}"
        );
    }
}

#[test]
fn json_insert_odd_arg_count_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT JSON_INSERT('{\"a\":1}', '$.a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("JSON_INSERT with missing value must error");
    assert!(matches!(err, DbError::TypeMismatch { .. }));
}
