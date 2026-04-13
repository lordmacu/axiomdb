//! Phase 11.25c — PG construction helpers:
//! `jsonb_build_object`, `jsonb_build_array`, `to_json` (plus aliasing
//! coverage for the existing `json_object`, `json_array`, `to_jsonb`).

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

// ── jsonb_build_array / jsonb_build_object ──────────────────────────────────

#[test]
fn jsonb_build_array_basic() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT jsonb_build_array(1, 'x', TRUE) AS a",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(r[0][0], Value::Jsonb(_)));
    assert_eq!(render(&r[0][0]), r#"[1,"x",true]"#);
}

#[test]
fn jsonb_build_object_basic() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT jsonb_build_object('a', 1, 'b', 'hi') AS o",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(r[0][0], Value::Jsonb(_)));
    let rendered = render(&r[0][0]);
    assert!(rendered.contains("\"a\":1"));
    assert!(rendered.contains("\"b\":\"hi\""));
}

#[test]
fn jsonb_build_object_odd_args_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT jsonb_build_object('a', 1, 'b')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("even arg") || msg.contains("key/value"),
        "got {msg}"
    );
}

#[test]
fn jsonb_build_object_null_key_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT jsonb_build_object(NULL, 1)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("NULL") || msg.contains("null"), "got {msg}");
}

// ── to_json vs to_jsonb ─────────────────────────────────────────────────────

#[test]
fn to_json_returns_text() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT to_json(42) AS a, to_json('hello') AS b, to_json(TRUE) AS c",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(r[0][0], Value::Json(_)));
    assert_eq!(render(&r[0][0]), "42");
    assert_eq!(render(&r[0][1]), r#""hello""#);
    assert_eq!(render(&r[0][2]), "true");
}

#[test]
fn to_jsonb_returns_binary() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows("SELECT to_jsonb(42) AS v", &mut s, &mut t, &mut b, &mut c);
    assert!(matches!(r[0][0], Value::Jsonb(_)));
}

#[test]
fn nested_build_calls() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT jsonb_build_object('items', jsonb_build_array(1, 2, 3), 'n', 3) AS doc",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let rendered = render(&r[0][0]);
    assert!(rendered.contains("\"items\":[1,2,3]"));
    assert!(rendered.contains("\"n\":3"));
}

// ── Existing alias coverage ─────────────────────────────────────────────────

#[test]
fn json_object_still_returns_json_text() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT json_object('a', 1) AS o",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(matches!(r[0][0], Value::Json(_)));
}
