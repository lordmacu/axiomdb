//! Phase 21.22 — inline `VALUES` as FROM source.

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

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i as i64,
        Value::BigInt(i) => *i,
        other => panic!("expected int, got {other:?}"),
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
fn values_basic_two_rows() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT id, name FROM (VALUES (1, 'a'), (2, 'b')) AS t(id, name) ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert_eq!(as_i64(&r[0][0]), 1);
    assert_eq!(as_text(&r[0][1]), "a");
    assert_eq!(as_i64(&r[1][0]), 2);
    assert_eq!(as_text(&r[1][1]), "b");
}

#[test]
fn values_with_column_names() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT t.num FROM (VALUES (42)) AS t(num)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert_eq!(as_i64(&r[0][0]), 42);
}

#[test]
fn values_default_column_names() {
    let (mut s, mut t, mut b, mut c) = setup();
    // No column list — default to column1, column2.
    let r = rows(
        "SELECT t.column1, t.column2 FROM (VALUES (10, 20)) AS t",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert_eq!(as_i64(&r[0][0]), 10);
    assert_eq!(as_i64(&r[0][1]), 20);
}

#[test]
fn values_single_row() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT * FROM (VALUES (1)) AS t(x)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
}

// ── JOIN ────────────────────────────────────────────────────────────────────

#[test]
fn values_join_with_real_table() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE users (id INT, name TEXT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let r = rows(
        "SELECT u.name, r.tag FROM users u
         JOIN (VALUES (1, 'admin'), (3, 'viewer')) AS r(id, tag)
           ON r.id = u.id
         ORDER BY u.id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert_eq!(as_text(&r[0][0]), "alice");
    assert_eq!(as_text(&r[0][1]), "admin");
    assert_eq!(as_text(&r[1][0]), "carol");
    assert_eq!(as_text(&r[1][1]), "viewer");
}

#[test]
fn values_from_join_drives_where() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE t (id INT, v INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30), (4, 40)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let r = rows(
        "SELECT t.v FROM t
         JOIN (VALUES (1), (3)) AS keep(id) ON keep.id = t.id
         ORDER BY t.id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert_eq!(as_i64(&r[0][0]), 10);
    assert_eq!(as_i64(&r[1][0]), 30);
}

// ── Rejections ──────────────────────────────────────────────────────────────

#[test]
fn values_inconsistent_width_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT * FROM (VALUES (1, 'a'), (2)) AS t(id, name)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("VALUES") || msg.contains("width"), "got {msg}");
}

#[test]
fn values_in_update_join_rejected_target() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "UPDATE (VALUES (1)) AS t(x) SET x = 2",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("must be a table") || msg.contains("UPDATE"),
        "got {msg}"
    );
}
