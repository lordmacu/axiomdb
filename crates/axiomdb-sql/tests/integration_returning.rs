//! Phase 21.4 — `RETURNING` clause (DELETE only; INSERT/UPDATE deferred to
//! 21.4b with clear `NotImplemented` runtime errors).

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

fn seed(s: &mut MemoryStorage, t: &mut TxnManager, b: &mut BloomRegistry, c: &mut SessionContext) {
    run_ctx("CREATE TABLE t (id INT, name TEXT)", s, t, b, c).unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        s,
        t,
        b,
        c,
    )
    .unwrap();
}

// ── DELETE RETURNING ────────────────────────────────────────────────────────

#[test]
fn delete_returning_star_pre_delete_values() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "DELETE FROM t WHERE id = 2 RETURNING *",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert_eq!(as_i64(&r[0][0]), 2);
    assert_eq!(as_text(&r[0][1]), "b");
    // Verify the row is gone from the table.
    let remaining = rows(
        "SELECT id FROM t ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(remaining.len(), 2);
}

#[test]
fn delete_returning_specific_columns() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "DELETE FROM t WHERE id >= 2 RETURNING id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    // Single-column projection.
    assert_eq!(r[0].len(), 1);
}

#[test]
fn delete_returning_expression_alias() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "DELETE FROM t WHERE id = 1 RETURNING id * 10 AS shifted",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert_eq!(as_i64(&r[0][0]), 10);
}

#[test]
fn delete_returning_empty_when_where_matches_nothing() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "DELETE FROM t WHERE id = 999 RETURNING *",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 0);
}

#[test]
fn delete_without_returning_still_returns_affected() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let res = run_ctx("DELETE FROM t WHERE id = 1", &mut s, &mut t, &mut b, &mut c).unwrap();
    assert!(matches!(res, QueryResult::Affected { .. }));
}

// ── INSERT / UPDATE parser-accepted but runtime-deferred ────────────────────

#[test]
fn insert_returning_runtime_defers_with_clear_error() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let err = run_ctx(
        "INSERT INTO t VALUES (10, 'x') RETURNING id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("21.4b") && msg.contains("RETURNING"),
        "got {msg}"
    );
}

#[test]
fn update_returning_runtime_defers_with_clear_error() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let err = run_ctx(
        "UPDATE t SET name = 'z' WHERE id = 1 RETURNING id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("21.4b") && msg.contains("RETURNING"),
        "got {msg}"
    );
}

// ── Parser shape coverage ───────────────────────────────────────────────────

#[test]
fn delete_returning_star_parses() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    // Just verify parse + execute, no assertion on content.
    run_ctx(
        "DELETE FROM t WHERE id = 3 RETURNING *",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
}
