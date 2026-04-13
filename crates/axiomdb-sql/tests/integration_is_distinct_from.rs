//! Phase 21.17 — `IS [NOT] DISTINCT FROM` NULL-safe comparison.

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

fn as_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        other => panic!("expected bool, got {other:?}"),
    }
}

// ── Truth table ─────────────────────────────────────────────────────────────

#[test]
fn distinct_both_non_null_equal_is_false() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT 1 IS DISTINCT FROM 1",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(!as_bool(&r[0][0]));
}

#[test]
fn distinct_both_non_null_different_is_true() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT 1 IS DISTINCT FROM 2",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_bool(&r[0][0]));
}

#[test]
fn distinct_one_null_is_true() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT 1 IS DISTINCT FROM NULL",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_bool(&r[0][0]));
}

#[test]
fn distinct_both_null_is_false() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT NULL IS DISTINCT FROM NULL",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(!as_bool(&r[0][0]));
}

#[test]
fn not_distinct_both_null_is_true() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT NULL IS NOT DISTINCT FROM NULL",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(as_bool(&r[0][0]));
}

#[test]
fn not_distinct_mixed_null_is_false() {
    let (mut s, mut t, mut b, mut c) = setup();
    let r = rows(
        "SELECT 1 IS NOT DISTINCT FROM NULL",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(!as_bool(&r[0][0]));
}

// ── WHERE clause usage ──────────────────────────────────────────────────────

#[test]
fn is_distinct_from_in_where_clause_filters_rows() {
    let (mut s, mut t, mut b, mut c) = setup();
    run_ctx(
        "CREATE TABLE t (a INT, b INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1, 1), (1, 2), (1, NULL), (NULL, NULL)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    // IS DISTINCT FROM matches: (1,2), (1,NULL). 2 rows.
    let r = rows(
        "SELECT COUNT(*) FROM t WHERE a IS DISTINCT FROM b",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let n = match &r[0][0] {
        Value::BigInt(n) => *n,
        Value::Int(n) => *n as i64,
        other => panic!("{other:?}"),
    };
    assert_eq!(n, 2);
}
