//! Phase 11.25a — JSONB set-returning functions (jsonb_each, etc.)

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

fn as_text(v: &Value) -> &str {
    match v {
        Value::Text(s) => s.as_str(),
        other => panic!("expected text, got {other:?}"),
    }
}

// ── Basic ────────────────────────────────────────────────────────────────────

#[test]
fn jsonb_each_basic() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        r#"SELECT key, value FROM jsonb_each('{"a":1,"b":2}') ORDER BY key"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert_eq!(as_text(&r[0][0]), "a");
    assert_eq!(as_text(&r[1][0]), "b");
}

#[test]
fn jsonb_each_text_strips_quotes() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        r#"SELECT key, value FROM jsonb_each_text('{"a":"hello","b":42}') ORDER BY key"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert_eq!(as_text(&r[0][0]), "a");
    assert_eq!(as_text(&r[0][1]), "hello"); // no quotes
    assert_eq!(as_text(&r[1][0]), "b");
    assert_eq!(as_text(&r[1][1]), "42");
}

#[test]
fn jsonb_object_keys_basic() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        r#"SELECT key FROM jsonb_object_keys('{"a":1,"b":2,"c":3}') ORDER BY key"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 3);
    assert_eq!(as_text(&r[0][0]), "a");
    assert_eq!(as_text(&r[1][0]), "b");
    assert_eq!(as_text(&r[2][0]), "c");
}

#[test]
fn jsonb_array_elements_basic() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        r#"SELECT value FROM jsonb_array_elements('[1, 2, 3]')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 3);
}

#[test]
fn jsonb_array_elements_text_unquoted() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        r#"SELECT value FROM jsonb_array_elements_text('["x","y","z"]')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 3);
    assert_eq!(as_text(&r[0][0]), "x");
    assert_eq!(as_text(&r[1][0]), "y");
    assert_eq!(as_text(&r[2][0]), "z");
}

// ── Errors ──────────────────────────────────────────────────────────────────

#[test]
fn jsonb_each_on_array_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        r#"SELECT * FROM jsonb_each('[1,2]')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("jsonb_each") || msg.contains("object"),
        "got {msg}"
    );
}

#[test]
fn jsonb_array_elements_on_object_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        r#"SELECT * FROM jsonb_array_elements('{"a":1}')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("jsonb_array_elements") || msg.contains("array"),
        "got {msg}"
    );
}

#[test]
fn jsonb_each_null_doc_zero_rows() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        r#"SELECT * FROM jsonb_each(NULL)"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 0);
}

// ── Joins ────────────────────────────────────────────────────────────────────

#[test]
fn srf_join_with_real_table() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE t (k TEXT, v INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES ('a', 1), ('b', 2)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let r = rows(
        r#"SELECT t.k, t.v
             FROM jsonb_object_keys('{"a":1,"b":2,"c":3}') AS j
             JOIN t ON t.k = j.key
             ORDER BY t.k"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
}

#[test]
fn srf_cross_apply_correlated() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE u (id INT, tags TEXT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        r#"INSERT INTO u VALUES (1, '["x","y"]'), (2, '["z"]')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let r = rows(
        r#"SELECT u.id, j.value
             FROM u
             CROSS APPLY jsonb_array_elements_text(u.tags) AS j
             ORDER BY u.id, j.value"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 3);
    assert_eq!(as_text(&r[0][1]), "x");
    assert_eq!(as_text(&r[1][1]), "y");
    assert_eq!(as_text(&r[2][1]), "z");
}

#[test]
fn srf_outer_apply_empty_preserves_left() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE u (id INT, tags TEXT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        r#"INSERT INTO u VALUES (1, '[]'), (2, '["z"]')"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let r = rows(
        r#"SELECT u.id, j.value
             FROM u
             OUTER APPLY jsonb_array_elements_text(u.tags) AS j
             ORDER BY u.id"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert!(matches!(r[0][1], Value::Null));
}

#[test]
fn srf_in_update_join() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE t (k TEXT, v INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES ('a', 0), ('b', 0)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        r#"UPDATE t
             JOIN jsonb_each_text('{"a":"10","b":"20"}') AS j ON t.k = j.key
             SET t.v = CAST(j.value AS INT)"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let r = rows(
        "SELECT k, v FROM t ORDER BY k",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    // Both rows updated from the SRF.
    match &r[0][1] {
        Value::Int(n) => assert_eq!(*n, 10),
        Value::BigInt(n) => assert_eq!(*n, 10),
        other => panic!("expected int, got {other:?}"),
    }
}
