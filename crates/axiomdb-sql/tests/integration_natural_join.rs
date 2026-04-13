//! Phase 21.18 — NATURAL JOIN.

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

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i as i64,
        Value::BigInt(i) => *i,
        other => panic!("expected int, got {other:?}"),
    }
}

// ── Basic ────────────────────────────────────────────────────────────────────

#[test]
fn natural_join_one_shared_column() {
    let (mut s, mut t, mut b, mut c) = setup();
    exec(
        "CREATE TABLE a (id INT, x INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        "CREATE TABLE bt (id INT, y INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        "INSERT INTO a VALUES (1, 10), (2, 20)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        "INSERT INTO bt VALUES (1, 100), (3, 300)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // INNER: only id=1 matches.
    let r = rows(
        "SELECT a.x, bt.y FROM a NATURAL JOIN bt ORDER BY a.x",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert_eq!(as_i64(&r[0][0]), 10);
    assert_eq!(as_i64(&r[0][1]), 100);
}

#[test]
fn natural_join_multiple_shared_columns() {
    let (mut s, mut t, mut b, mut c) = setup();
    exec(
        "CREATE TABLE a (p INT, q INT, extra INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        "CREATE TABLE b2 (p INT, q INT, other INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        "INSERT INTO a VALUES (1, 10, 100), (1, 20, 200)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        "INSERT INTO b2 VALUES (1, 10, 7), (1, 20, 8), (1, 99, 0)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(
        "SELECT a.extra, b2.other FROM a NATURAL JOIN b2 ORDER BY a.extra",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert_eq!(as_i64(&r[0][1]), 7);
    assert_eq!(as_i64(&r[1][1]), 8);
}

#[test]
fn natural_left_join_preserves_left_rows() {
    let (mut s, mut t, mut b, mut c) = setup();
    exec(
        "CREATE TABLE a (id INT, x INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        "CREATE TABLE bt (id INT, y INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        "INSERT INTO a VALUES (1, 10), (2, 20)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    exec(
        "INSERT INTO bt VALUES (1, 100)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let r = rows(
        "SELECT a.id, a.x, bt.y FROM a NATURAL LEFT JOIN bt ORDER BY a.id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert!(matches!(r[1][2], Value::Null));
}

// `case_insensitive_match` moved to fase-21/wishlist: requires
// unified case-folding across catalog + resolver + USING downstream,
// a broader change than the NATURAL JOIN feature itself. Left as a
// follow-up when collation + identifier normalization (13.13e)
// lands. Today AxiomDB preserves identifier case in catalog.

// ── Rejections ──────────────────────────────────────────────────────────────

#[test]
fn natural_join_no_shared_columns_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    exec("CREATE TABLE a (x INT)", &mut s, &mut t, &mut b, &mut c);
    exec("CREATE TABLE bt (y INT)", &mut s, &mut t, &mut b, &mut c);
    let err = run_ctx(
        "SELECT * FROM a NATURAL JOIN bt",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("NATURAL JOIN") || msg.contains("shared"),
        "got {msg}"
    );
}

#[test]
fn natural_join_rejects_on_clause() {
    let (mut s, mut t, mut b, mut c) = setup();
    exec("CREATE TABLE a (id INT)", &mut s, &mut t, &mut b, &mut c);
    exec("CREATE TABLE bt (id INT)", &mut s, &mut t, &mut b, &mut c);
    let err = run_ctx(
        "SELECT * FROM a NATURAL JOIN bt ON a.id = bt.id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("NATURAL JOIN") || msg.contains("ON"),
        "got {msg}"
    );
}

#[test]
fn natural_cross_join_rejected() {
    let (mut s, mut t, mut b, mut c) = setup();
    exec("CREATE TABLE a (id INT)", &mut s, &mut t, &mut b, &mut c);
    exec("CREATE TABLE bt (id INT)", &mut s, &mut t, &mut b, &mut c);
    let err = run_ctx(
        "SELECT * FROM a NATURAL CROSS JOIN bt",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("CROSS"), "got {msg}");
}
