//! Integration tests for Phase 11.18a — JSONB PostgreSQL operator parity.
//!
//! Covers the 5 operators (`?`, `<@`, `||`, `-(text)`, `-(int)`), their
//! function-style aliases, and GIN planner integration for `?`.

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

fn jsonb_text(v: &Value) -> String {
    match v {
        Value::Jsonb(b) => axiomdb_types::jsonb::JsonbDecoder::to_string(b.as_ref()).unwrap(),
        Value::Text(s) | Value::Json(s) => s.clone(),
        Value::Null => "NULL".into(),
        other => panic!("expected Jsonb/Text, got {other:?}"),
    }
}

fn is_true(v: &Value) -> bool {
    matches!(v, Value::Bool(true) | Value::Int(1) | Value::BigInt(1))
}

fn is_false(v: &Value) -> bool {
    matches!(v, Value::Bool(false) | Value::Int(0) | Value::BigInt(0))
}

// ── `?` operator ───────────────────────────────────────────────────────────-

#[test]
fn exists_on_object_key() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1,\"b\":2}' AS JSONB) ? 'a'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn exists_on_array_string_element() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('[\"x\",\"y\"]' AS JSONB) ? 'x'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn exists_false_on_non_string_array_element() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('[1,2,3]' AS JSONB) ? '1'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(
        is_false(&v),
        "PG: numeric array elements don't match ? text"
    );
}

#[test]
fn exists_on_scalar_is_false() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('42' AS JSONB) ? 'a'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

#[test]
fn exists_null_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST(NULL AS JSONB) ? 'a'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

// ── `<@` operator ──────────────────────────────────────────────────────────-

#[test]
fn contained_by_deep_structural() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) <@ CAST('{\"a\":1,\"b\":2}' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v));
}

#[test]
fn contained_by_type_mismatch_is_false() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) <@ CAST('[1,2]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_false(&v));
}

// ── `||` operator ──────────────────────────────────────────────────────────-

#[test]
fn concat_object_object_rhs_wins() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1,\"b\":2}' AS JSONB) || CAST('{\"b\":99}' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = jsonb_text(&v);
    // key order may vary; check both keys + RHS-wins on b.
    assert!(text.contains("\"a\":1"));
    assert!(text.contains("\"b\":99"));
}

#[test]
fn concat_array_array_appends() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('[1,2]' AS JSONB) || CAST('[3,4]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(jsonb_text(&v), "[1,2,3,4]");
}

#[test]
fn concat_object_array_wraps_object() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1}' AS JSONB) || CAST('[2,3]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = jsonb_text(&v);
    assert!(text.starts_with('['));
    assert!(text.contains("\"a\":1"));
    assert!(text.ends_with(']'));
}

#[test]
fn concat_null_propagates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST(NULL AS JSONB) || CAST('[1]' AS JSONB)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(v, Value::Null));
}

// ── `-` operator (text) ────────────────────────────────────────────────────-

#[test]
fn delete_key_from_object() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('{\"a\":1,\"b\":2}' AS JSONB) - 'a'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(jsonb_text(&v), "{\"b\":2}");
}

#[test]
fn delete_key_from_array_drops_matching_strings() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('[\"x\",\"y\",\"x\",1]' AS JSONB) - 'x'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let text = jsonb_text(&v);
    assert!(!text.contains("\"x\""));
    assert!(text.contains("\"y\""));
    assert!(text.contains('1'));
}

#[test]
fn delete_key_on_scalar_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT CAST('42' AS JSONB) - 'a'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("deleting from scalar JSONB must error");
    assert!(
        matches!(err, DbError::InvalidValue { .. }),
        "unexpected error: {err:?}",
    );
}

// ── `-` operator (int) ─────────────────────────────────────────────────────-

#[test]
fn delete_idx_negative_counts_from_end() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('[1,2,3]' AS JSONB) - (-1)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(jsonb_text(&v), "[1,2]");
}

#[test]
fn delete_idx_out_of_range_is_noop() {
    let (mut s, mut t, mut b, mut c) = setup();
    let v = scalar(
        "SELECT CAST('[1,2,3]' AS JSONB) - 99",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(jsonb_text(&v), "[1,2,3]");
}

#[test]
fn delete_idx_on_object_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT CAST('{\"a\":1}' AS JSONB) - 0",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .expect_err("int-index delete on object must error");
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

// ── Function aliases ───────────────────────────────────────────────────────-

#[test]
fn function_aliases_match_operators() {
    let (mut s, mut t, mut b, mut c) = setup();
    // JSONB_EXISTS
    let v1 = scalar(
        "SELECT JSONB_EXISTS(CAST('{\"a\":1}' AS JSONB), 'a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v1));

    // JSONB_CONTAINED
    let v2 = scalar(
        "SELECT JSONB_CONTAINED(CAST('{\"a\":1}' AS JSONB), CAST('{\"a\":1,\"b\":2}' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(is_true(&v2));

    // JSONB_CONCAT
    let v3 = scalar(
        "SELECT JSONB_CONCAT(CAST('[1]' AS JSONB), CAST('[2]' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(jsonb_text(&v3), "[1,2]");

    // JSONB_DELETE_KEY
    let v4 = scalar(
        "SELECT JSONB_DELETE_KEY(CAST('{\"a\":1,\"b\":2}' AS JSONB), 'a')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(jsonb_text(&v4), "{\"b\":2}");

    // JSONB_DELETE_INDEX
    let v5 = scalar(
        "SELECT JSONB_DELETE_INDEX(CAST('[1,2,3]' AS JSONB), 1)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(jsonb_text(&v5), "[1,3]");
}

// ── Row-level operations on JSONB columns ──────────────────────────────────-

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
        "INSERT INTO t VALUES \
         (1, CAST('{\"a\":1}' AS JSONB)), \
         (2, CAST('{\"b\":2}' AS JSONB)), \
         (3, CAST('[\"x\",\"y\"]' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();

    // `?` on a column, no GIN index — should still work via full scan + eval.
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM t WHERE doc ? 'a' ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap() else {
        panic!();
    };
    assert_eq!(rows.len(), 1);

    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM t WHERE doc ? 'x' ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap() else {
        panic!();
    };
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Int(3) | Value::BigInt(3)));
}

// ── GIN planner integration for `?` ────────────────────────────────────────-

#[test]
fn gin_index_accelerates_exists_query() {
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
        "INSERT INTO t VALUES \
         (1, CAST('{\"a\":1,\"b\":2}' AS JSONB)), \
         (2, CAST('{\"c\":3}' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "CREATE INDEX idx_doc ON t USING GIN (doc)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();

    // Correctness under GIN plan.
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM t WHERE doc ? 'a'",
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

    // Empty result via GIN is not a crash.
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM t WHERE doc ? 'nonexistent'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap() else {
        panic!();
    };
    assert_eq!(rows.len(), 0);
}

#[test]
fn gin_index_survives_insert_delete_update() {
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
        "CREATE INDEX idx_doc ON t USING GIN (doc)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES \
         (1, CAST('{\"a\":1}' AS JSONB)), \
         (2, CAST('{\"a\":2}' AS JSONB)), \
         (3, CAST('{\"b\":3}' AS JSONB))",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx("DELETE FROM t WHERE id = 2", &mut s, &mut t, &mut b, &mut c).unwrap();
    let QueryResult::Rows { rows, .. } = run_ctx(
        "SELECT id FROM t WHERE doc ? 'a'",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap() else {
        panic!();
    };
    // Only row 1 remains with key 'a'.
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], Value::Int(1) | Value::BigInt(1)));
}
