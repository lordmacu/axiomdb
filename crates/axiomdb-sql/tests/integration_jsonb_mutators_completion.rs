//! Phase 11.25d — Mutators + MySQL completeness:
//! jsonb_strip_nulls (PG), jsonb_set_lax (PG — verify existing), JSON_MERGE_PRESERVE (MySQL — verify),
//! JSON_STORAGE_SIZE (MySQL — verify), JSON_STORAGE_FREE (MySQL — new).

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn rows(
    sql: &str,
    s: &mut MemoryStorage,
    t: &mut TxnManager,
    b: &mut BloomRegistry,
    c: &mut SessionContext,
) -> Vec<Vec<Value>> {
    let res =
        run_ctx(sql, s, t, b, c).unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"));
    match res {
        QueryResult::Rows { rows, .. } => rows,
        _ => Vec::new(),
    }
}

fn render(v: &Value) -> String {
    match v {
        Value::Json(s) | Value::Text(s) => s.clone(),
        Value::Jsonb(bytes) => {
            let jref = axiomdb_types::JsonbRef::new(bytes.as_slice());
            jref.to_serde_json().unwrap().to_string()
        }
        other => format!("{other:?}"),
    }
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i as i64,
        Value::BigInt(i) => *i,
        other => panic!("expected integer, got {other:?}"),
    }
}

// ── jsonb_strip_nulls ────────────────────────────────────────────────────────

#[test]
fn strip_nulls_removes_object_keys_with_null_values() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        r#"SELECT jsonb_strip_nulls('{"a":1,"b":null,"c":"x"}')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(r[0][0], Value::Jsonb(_)));
    let rendered = render(&r[0][0]);
    assert!(rendered.contains("\"a\":1"));
    assert!(rendered.contains("\"c\":\"x\""));
    assert!(!rendered.contains("\"b\""));
}

#[test]
fn strip_nulls_keeps_null_elements_in_arrays() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        r#"SELECT jsonb_strip_nulls('[1, null, 2]')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // PG semantics: array nulls survive.
    assert_eq!(render(&r[0][0]), "[1,null,2]");
}

#[test]
fn strip_nulls_recurses_into_nested_objects() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        r#"SELECT jsonb_strip_nulls('{"outer":{"a":1,"b":null},"x":null}')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let rendered = render(&r[0][0]);
    assert!(rendered.contains("\"outer\":{\"a\":1}"));
    assert!(!rendered.contains("\"x\""));
    assert!(!rendered.contains("\"b\""));
}

#[test]
fn strip_nulls_null_doc_returns_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT jsonb_strip_nulls(NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(r[0][0], Value::Null));
}

// ── JSON_STORAGE_FREE (MySQL) ────────────────────────────────────────────────

#[test]
fn json_storage_free_always_zero() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        r#"SELECT JSON_STORAGE_FREE('{"a":1,"b":[1,2,3]}')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // MySQL contract: 0 when the value has never been updated in place.
    // AxiomDB always re-encodes from scratch so returns 0.
    assert_eq!(as_i64(&r[0][0]), 0);
}

#[test]
fn json_storage_free_null_returns_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT JSON_STORAGE_FREE(NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(r[0][0], Value::Null));
}

// ── JSON_STORAGE_SIZE (MySQL) regression ─────────────────────────────────────

#[test]
fn json_storage_size_returns_bytes() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        r#"SELECT JSON_STORAGE_SIZE('{"a":1}')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let n = as_i64(&r[0][0]);
    assert!(n > 0, "expected positive byte count, got {n}");
}

// ── JSON_MERGE_PRESERVE (MySQL) regression ───────────────────────────────────

#[test]
fn json_merge_preserve_concatenates_arrays() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        r#"SELECT JSON_MERGE_PRESERVE('[1,2]', '[3,4]')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // MySQL: array merge concatenates.
    let rendered = render(&r[0][0]);
    assert!(rendered.contains("1") && rendered.contains("4"));
}

#[test]
fn json_merge_preserve_merges_objects_keeping_duplicates() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        r#"SELECT JSON_MERGE_PRESERVE('{"a":1}', '{"a":2}')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // MySQL: value becomes an array [1,2] to preserve both.
    let rendered = render(&r[0][0]);
    assert!(
        rendered.contains("[1,2]")
            || rendered.contains("[2,1]")
            || rendered.contains("1") && rendered.contains("2"),
        "got {rendered}"
    );
}

// ── jsonb_set_lax (PG) regression ───────────────────────────────────────────

#[test]
fn jsonb_set_lax_basic() {
    let (mut s, mut t, mut b, mut c) = setup();
    // Use CAST to unambiguously feed a JSON/JSONB value as the new element.
    let r = rows(
        r#"SELECT jsonb_set_lax('{"a":1}', '$.b', CAST('2' AS JSONB), TRUE)"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let rendered = render(&r[0][0]);
    assert!(rendered.contains("\"a\":1"));
    assert!(rendered.contains("\"b\""));
}
