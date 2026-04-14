//! Phase 21.2 — non-recursive Common Table Expressions (`WITH`).

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
    run_ctx(
        "CREATE TABLE users (id INT, name TEXT, status TEXT)",
        s,
        t,
        b,
        c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO users VALUES \
         (1, 'alice', 'active'), (2, 'bob', 'active'), \
         (3, 'carol', 'inactive'), (4, 'dave', 'active')",
        s,
        t,
        b,
        c,
    )
    .unwrap();
}

// ── Basic ────────────────────────────────────────────────────────────────────

#[test]
fn basic_cte_single_with_ref_once() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "WITH active AS (SELECT id, name FROM users WHERE status = 'active') \
         SELECT name FROM active ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 3);
    assert_eq!(as_text(&r[0][0]), "alice");
    assert_eq!(as_text(&r[1][0]), "bob");
    assert_eq!(as_text(&r[2][0]), "dave");
}

#[test]
fn cte_with_column_override() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "WITH t(uid, uname) AS (SELECT id, name FROM users WHERE id = 1) \
         SELECT uid, uname FROM t",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert_eq!(as_i64(&r[0][0]), 1);
    assert_eq!(as_text(&r[0][1]), "alice");
}

#[test]
fn multi_cte_two_bindings() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    // Two independent CTEs referenced separately.
    let r1 = rows(
        "WITH act AS (SELECT id FROM users WHERE status = 'active'), \
              inact AS (SELECT id FROM users WHERE status = 'inactive') \
         SELECT id FROM act ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r1.len(), 3);
    let r2 = rows(
        "WITH act AS (SELECT id FROM users WHERE status = 'active'), \
              inact AS (SELECT id FROM users WHERE status = 'inactive') \
         SELECT id FROM inact ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r2.len(), 1);
}

#[test]
fn cte_referenced_multiple_times() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "WITH a AS (SELECT id FROM users WHERE status = 'active') \
         SELECT a1.id FROM a AS a1 JOIN a AS a2 ON a1.id = a2.id ORDER BY a1.id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 3);
}

#[test]
fn cte_in_join() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "WITH flags AS (SELECT 1 AS id, 'admin' AS tag) \
         SELECT u.name, flags.tag FROM users u JOIN flags ON u.id = flags.id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert_eq!(as_text(&r[0][0]), "alice");
    assert_eq!(as_text(&r[0][1]), "admin");
}

// ── Error cases ─────────────────────────────────────────────────────────────

#[test]
fn with_recursive_no_step_degrades_to_non_recursive() {
    // Phase 21.3b — `WITH RECURSIVE` without a UNION tail is equivalent
    // to a plain CTE (no self-ref body). Validate the keyword is parsed
    // and the query succeeds; full recursive behavior is covered by
    // integration_recursive_cte.rs.
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "WITH RECURSIVE r(n) AS (SELECT 1) SELECT n FROM r",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
}

#[test]
fn column_override_width_mismatch() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "WITH t(a, b) AS (SELECT 1) SELECT * FROM t",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("column") || msg.contains("CTE"), "got {msg}");
}

// ── Regression ──────────────────────────────────────────────────────────────

#[test]
fn select_without_cte_still_works() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT id FROM users WHERE id = 1",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
}
