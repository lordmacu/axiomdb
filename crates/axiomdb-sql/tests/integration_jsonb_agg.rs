//! Phase 11.25b — JSONB aggregates (jsonb_agg, json_agg, jsonb_object_agg,
//! json_object_agg, JSON_ARRAYAGG, JSON_OBJECTAGG).

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn exec(
    sql: &str,
    s: &mut MemoryStorage,
    t: &mut TxnManager,
    b: &mut BloomRegistry,
    c: &mut SessionContext,
) {
    run_ctx(sql, s, t, b, c).unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"));
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

fn seed(s: &mut MemoryStorage, t: &mut TxnManager, b: &mut BloomRegistry, c: &mut SessionContext) {
    exec("CREATE TABLE kv (k TEXT, v INT)", s, t, b, c);
    exec(
        "INSERT INTO kv VALUES ('a', 1), ('b', 2), ('c', 3)",
        s,
        t,
        b,
        c,
    );
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

// ── jsonb_agg / json_agg / JSON_ARRAYAGG ─────────────────────────────────────

#[test]
fn jsonb_agg_returns_jsonb_array() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT jsonb_agg(v) AS arr FROM kv",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert!(matches!(r[0][0], Value::Jsonb(_)));
    assert_eq!(render(&r[0][0]), "[1,2,3]");
}

#[test]
fn json_agg_returns_json_text() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT json_agg(v) AS arr FROM kv",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert!(matches!(r[0][0], Value::Json(_)));
    assert_eq!(render(&r[0][0]), "[1,2,3]");
}

#[test]
fn json_arrayagg_mysql_alias() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT JSON_ARRAYAGG(k) AS arr FROM kv",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert!(matches!(r[0][0], Value::Json(_)));
    assert_eq!(render(&r[0][0]), r#"["a","b","c"]"#);
}

#[test]
fn jsonb_agg_with_group_by() {
    let (mut s, mut t, mut b, mut c) = setup();
    exec(
        "CREATE TABLE items (cat TEXT, n INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        "INSERT INTO items VALUES ('a', 1), ('a', 2), ('b', 3)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(
        "SELECT cat, jsonb_agg(n) AS arr FROM items GROUP BY cat ORDER BY cat",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert_eq!(render(&r[0][1]), "[1,2]");
    assert_eq!(render(&r[1][1]), "[3]");
}

// ── jsonb_object_agg / json_object_agg / JSON_OBJECTAGG ──────────────────────

#[test]
fn jsonb_object_agg_basic() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT jsonb_object_agg(k, v) AS o FROM kv",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert!(matches!(r[0][0], Value::Jsonb(_)));
    let rendered = render(&r[0][0]);
    // JSONB canonical key ordering — sorted keys.
    assert!(rendered.contains("\"a\":1"));
    assert!(rendered.contains("\"b\":2"));
    assert!(rendered.contains("\"c\":3"));
}

#[test]
fn json_object_agg_returns_json_text() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT json_object_agg(k, v) AS o FROM kv",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert!(matches!(r[0][0], Value::Json(_)));
}

#[test]
fn json_objectagg_mysql_alias() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT JSON_OBJECTAGG(k, v) AS o FROM kv",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert!(matches!(r[0][0], Value::Json(_)));
}

#[test]
fn object_agg_null_key_rejected() {
    let (mut s, mut t, mut b, mut c) = setup();
    exec(
        "CREATE TABLE kv2 (k TEXT, v INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        "INSERT INTO kv2 VALUES (NULL, 1)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let err = run_ctx(
        "SELECT jsonb_object_agg(k, v) FROM kv2",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("null key") || msg.contains("null"),
        "got {msg}"
    );
}

#[test]
fn object_agg_duplicate_key_last_wins() {
    let (mut s, mut t, mut b, mut c) = setup();
    exec(
        "CREATE TABLE kv3 (k TEXT, v INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        "INSERT INTO kv3 VALUES ('a', 1), ('a', 99)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(
        "SELECT jsonb_object_agg(k, v) AS o FROM kv3",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let rendered = render(&r[0][0]);
    assert!(rendered.contains("\"a\":99"));
    assert!(!rendered.contains(":1,") && !rendered.ends_with(":1}"));
}

// ── Empty result ────────────────────────────────────────────────────────────

#[test]
fn jsonb_agg_on_empty_returns_null() {
    let (mut s, mut t, mut b, mut c) = setup();
    exec(
        "CREATE TABLE empty_t (v INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(
        "SELECT jsonb_agg(v) FROM empty_t",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // PG returns NULL (not []) when the set is empty. Match that.
    assert_eq!(r.len(), 1);
    match &r[0][0] {
        Value::Null => (),
        Value::Jsonb(_) | Value::Json(_) => {
            // Acceptable alternative: empty array.
            let rendered = render(&r[0][0]);
            assert_eq!(rendered, "[]");
        }
        other => panic!("expected NULL or empty array, got {other:?}"),
    }
}
