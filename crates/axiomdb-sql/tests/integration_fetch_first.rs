//! Phase 21.19 — `FETCH FIRST` / `OFFSET n ROWS` SQL:2008 row window.

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

fn seed(s: &mut MemoryStorage, t: &mut TxnManager, b: &mut BloomRegistry, c: &mut SessionContext) {
    run_ctx("CREATE TABLE t (id INT)", s, t, b, c).unwrap();
    run_ctx(
        "INSERT INTO t VALUES (1),(2),(3),(4),(5),(6),(7),(8),(9),(10)",
        s,
        t,
        b,
        c,
    )
    .unwrap();
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i as i64,
        Value::BigInt(i) => *i,
        other => panic!("expected int, got {other:?}"),
    }
}

// ── Basic forms ─────────────────────────────────────────────────────────────

#[test]
fn fetch_first_n_rows_only_basic() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT id FROM t ORDER BY id FETCH FIRST 3 ROWS ONLY",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 3);
    assert_eq!(as_i64(&r[0][0]), 1);
    assert_eq!(as_i64(&r[2][0]), 3);
}

#[test]
fn fetch_first_row_only_implies_one() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT id FROM t ORDER BY id FETCH FIRST ROW ONLY",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert_eq!(as_i64(&r[0][0]), 1);
}

#[test]
fn fetch_first_rows_only_no_count_implies_one() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT id FROM t ORDER BY id FETCH FIRST ROWS ONLY",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
}

#[test]
fn fetch_next_interchangeable_with_first() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT id FROM t ORDER BY id FETCH NEXT 4 ROWS ONLY",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 4);
}

// ── OFFSET noise words ──────────────────────────────────────────────────────

#[test]
fn offset_rows_noise_word() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT id FROM t ORDER BY id OFFSET 7 ROWS",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 3);
    assert_eq!(as_i64(&r[0][0]), 8);
}

#[test]
fn offset_row_noise_word_singular() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT id FROM t ORDER BY id OFFSET 9 ROW",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 1);
    assert_eq!(as_i64(&r[0][0]), 10);
}

// ── Combined ────────────────────────────────────────────────────────────────

#[test]
fn combined_offset_fetch_first() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT id FROM t ORDER BY id OFFSET 3 ROWS FETCH FIRST 2 ROWS ONLY",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert_eq!(as_i64(&r[0][0]), 4);
    assert_eq!(as_i64(&r[1][0]), 5);
}

// ── Rejection ───────────────────────────────────────────────────────────────

#[test]
fn limit_and_fetch_mixed_rejected() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let err = run_ctx(
        "SELECT id FROM t LIMIT 3 FETCH FIRST 2 ROWS ONLY",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("LIMIT") && msg.contains("FETCH"),
        "expected LIMIT/FETCH mix error, got {msg}"
    );
}

// ── Regression ──────────────────────────────────────────────────────────────

#[test]
fn legacy_limit_offset_still_works() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 4",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert_eq!(as_i64(&r[0][0]), 5);
}

#[test]
fn mysql_comma_limit_still_works() {
    let (mut s, mut t, mut b, mut c) = setup();
    seed(&mut s, &mut t, &mut b, &mut c);
    let r = rows(
        "SELECT id FROM t ORDER BY id LIMIT 4, 2",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(r.len(), 2);
    assert_eq!(as_i64(&r[0][0]), 5);
}
