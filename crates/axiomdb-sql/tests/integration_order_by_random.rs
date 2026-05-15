mod common;

use axiomdb_core::error::DbError;
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;
use common::{rows, run, run_result, setup};

fn eval(sql: &str) -> Value {
    let (mut storage, mut txn) = setup();
    let result = run(&format!("SELECT {sql}"), &mut storage, &mut txn);
    rows(result)
        .into_iter()
        .next()
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
}

fn eval_err(sql: &str) -> DbError {
    let (mut storage, mut txn) = setup();
    run_result(&format!("SELECT {sql}"), &mut storage, &mut txn)
        .expect_err("expected error but got Ok")
}

fn insert_n(storage: &mut MemoryStorage, txn: &mut TxnManager, table: &str, n: usize) {
    for i in 0..n {
        run(&format!("INSERT INTO {table} VALUES ({i})"), storage, txn);
    }
}

// ── RAND() / RANDOM() scalar function ────────────────────────────────────────

#[test]
fn rand_returns_real_in_range() {
    let v = eval("RAND()");
    match v {
        Value::Real(f) => assert!((0.0..1.0).contains(&f), "out of range: {f}"),
        other => panic!("expected Real, got {other:?}"),
    }
}

#[test]
fn random_returns_real_in_range() {
    let v = eval("RANDOM()");
    match v {
        Value::Real(f) => assert!((0.0..1.0).contains(&f), "out of range: {f}"),
        other => panic!("expected Real, got {other:?}"),
    }
}

#[test]
fn rand_wrong_arity_errors() {
    let err = eval_err("RAND(1)");
    assert!(
        matches!(err, DbError::InvalidValue { ref reason } if reason.contains("no arguments")),
        "unexpected error: {err:?}"
    );
}

#[test]
fn rand_returns_different_values_across_queries() {
    // Two sequential RAND() calls should (almost certainly) differ.
    // Collision probability ≈ 2^-53.
    let (mut storage, mut txn) = setup();
    let r1 = rows(run("SELECT RAND()", &mut storage, &mut txn));
    let r2 = rows(run("SELECT RAND()", &mut storage, &mut txn));
    assert_ne!(r1[0][0], r2[0][0], "RAND() returned the same value twice");
}

// ── ORDER BY RANDOM() ─────────────────────────────────────────────────────────

#[test]
fn order_by_random_empty_table() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_empty (v INT)", &mut storage, &mut txn);
    let res = rows(run(
        "SELECT v FROM t_empty ORDER BY RANDOM()",
        &mut storage,
        &mut txn,
    ));
    assert!(res.is_empty());
}

#[test]
fn order_by_random_single_row() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_one (v INT)", &mut storage, &mut txn);
    run("INSERT INTO t_one VALUES (42)", &mut storage, &mut txn);
    let res = rows(run(
        "SELECT v FROM t_one ORDER BY RANDOM()",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(res.len(), 1);
    assert_eq!(res[0][0], Value::Int(42));
}

#[test]
fn order_by_random_is_permutation() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_perm (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_perm", 10);
    let res = rows(run(
        "SELECT v FROM t_perm ORDER BY RANDOM()",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(res.len(), 10);
    let mut vals: Vec<i32> = res
        .iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            _ => panic!("unexpected value"),
        })
        .collect();
    vals.sort_unstable();
    assert_eq!(vals, (0..10i32).collect::<Vec<_>>());
}

#[test]
fn order_by_random_limit() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_lim (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_lim", 20);
    let res = rows(run(
        "SELECT v FROM t_lim ORDER BY RANDOM() LIMIT 5",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(res.len(), 5);
    // All returned values must be distinct.
    let mut vals: Vec<i32> = res
        .iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            _ => panic!("unexpected value"),
        })
        .collect();
    vals.sort_unstable();
    vals.dedup();
    assert_eq!(vals.len(), 5, "duplicate rows returned");
}

#[test]
fn order_by_random_limit_zero() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_lim0 (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_lim0", 5);
    let res = rows(run(
        "SELECT v FROM t_lim0 ORDER BY RANDOM() LIMIT 0",
        &mut storage,
        &mut txn,
    ));
    assert!(res.is_empty());
}

#[test]
fn order_by_random_limit_exceeds_rows() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_exc (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_exc", 5);
    let res = rows(run(
        "SELECT v FROM t_exc ORDER BY RANDOM() LIMIT 100",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(res.len(), 5);
}

#[test]
fn order_by_random_offset_limit() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_off (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_off", 10);
    let res = rows(run(
        "SELECT v FROM t_off ORDER BY RANDOM() LIMIT 3 OFFSET 2",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(res.len(), 3);
}

// ── Mixed ORDER BY col, RANDOM() ─────────────────────────────────────────────

#[test]
fn order_by_col_then_random_respects_primary_sort() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t_mixed (grp INT, val INT)",
        &mut storage,
        &mut txn,
    );
    for g in [1i32, 2] {
        for v in 0..5i32 {
            run(
                &format!("INSERT INTO t_mixed VALUES ({g}, {v})"),
                &mut storage,
                &mut txn,
            );
        }
    }
    let res = rows(run(
        "SELECT grp, val FROM t_mixed ORDER BY grp ASC, RANDOM()",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(res.len(), 10);
    for r in &res[..5] {
        assert_eq!(r[0], Value::Int(1), "first 5 rows must have grp=1");
    }
    for r in &res[5..] {
        assert_eq!(r[0], Value::Int(2), "last 5 rows must have grp=2");
    }
}
