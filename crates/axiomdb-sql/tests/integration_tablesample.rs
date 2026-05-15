mod common;

use axiomdb_core::error::DbError;
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;
use common::{rows, run, run_result, setup};

fn run_err(sql: &str) -> DbError {
    let (mut storage, mut txn) = setup();
    run_result(sql, &mut storage, &mut txn).expect_err("expected error but got Ok")
}

fn insert_n(storage: &mut MemoryStorage, txn: &mut TxnManager, table: &str, n: usize) {
    for i in 0..n {
        run(&format!("INSERT INTO {table} VALUES ({i})"), storage, txn);
    }
}

// ── Parser / error cases ──────────────────────────────────────────────────────

#[test]
fn tablesample_system_parses() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_sys_parse (v INT)", &mut storage, &mut txn);
    // No panic, no error — just an empty table.
    let res = rows(run(
        "SELECT v FROM t_sys_parse TABLESAMPLE SYSTEM(10)",
        &mut storage,
        &mut txn,
    ));
    assert!(res.is_empty());
}

#[test]
fn tablesample_bernoulli_parses() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_bern_parse (v INT)", &mut storage, &mut txn);
    let res = rows(run(
        "SELECT v FROM t_bern_parse TABLESAMPLE BERNOULLI(10)",
        &mut storage,
        &mut txn,
    ));
    assert!(res.is_empty());
}

#[test]
fn tablesample_unknown_method_errors() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_unk (v INT)", &mut storage, &mut txn);
    let err = run_result(
        "SELECT v FROM t_unk TABLESAMPLE RESERVOIR(10)",
        &mut storage,
        &mut txn,
    )
    .expect_err("expected error");
    assert!(
        matches!(err, DbError::ParseError { .. }),
        "unexpected error: {err:?}"
    );
}

#[test]
fn tablesample_negative_pct_errors() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_neg (v INT)", &mut storage, &mut txn);
    let err = run_result(
        "SELECT v FROM t_neg TABLESAMPLE SYSTEM(-1)",
        &mut storage,
        &mut txn,
    )
    .expect_err("expected error");
    assert!(
        matches!(err, DbError::InvalidValue { .. }),
        "unexpected error: {err:?}"
    );
}

#[test]
fn tablesample_pct_over_100_errors() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_over (v INT)", &mut storage, &mut txn);
    let err = run_result(
        "SELECT v FROM t_over TABLESAMPLE SYSTEM(101)",
        &mut storage,
        &mut txn,
    )
    .expect_err("expected error");
    assert!(
        matches!(err, DbError::InvalidValue { .. }),
        "unexpected error: {err:?}"
    );
}

#[test]
fn tablesample_repeatable_errors() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_rep (v INT)", &mut storage, &mut txn);
    let err = run_result(
        "SELECT v FROM t_rep TABLESAMPLE SYSTEM(10) REPEATABLE(42)",
        &mut storage,
        &mut txn,
    )
    .expect_err("expected error");
    assert!(
        matches!(err, DbError::NotImplemented { .. }),
        "unexpected error: {err:?}"
    );
}

// ── SYSTEM(0) / SYSTEM(100) shortcuts ────────────────────────────────────────

#[test]
fn tablesample_system_zero_returns_empty() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_sys0 (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_sys0", 20);
    let res = rows(run(
        "SELECT v FROM t_sys0 TABLESAMPLE SYSTEM(0)",
        &mut storage,
        &mut txn,
    ));
    assert!(
        res.is_empty(),
        "SYSTEM(0) must return no rows, got {}",
        res.len()
    );
}

#[test]
fn tablesample_system_100_returns_all() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_sys100 (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_sys100", 15);
    let res = rows(run(
        "SELECT v FROM t_sys100 TABLESAMPLE SYSTEM(100)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(res.len(), 15, "SYSTEM(100) must return all rows");
}

// ── BERNOULLI(0) / BERNOULLI(100) shortcuts ──────────────────────────────────

#[test]
fn tablesample_bernoulli_zero_returns_empty() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_bern0 (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_bern0", 20);
    let res = rows(run(
        "SELECT v FROM t_bern0 TABLESAMPLE BERNOULLI(0)",
        &mut storage,
        &mut txn,
    ));
    assert!(
        res.is_empty(),
        "BERNOULLI(0) must return no rows, got {}",
        res.len()
    );
}

#[test]
fn tablesample_bernoulli_100_returns_all() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_bern100 (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_bern100", 15);
    let res = rows(run(
        "SELECT v FROM t_bern100 TABLESAMPLE BERNOULLI(100)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(res.len(), 15, "BERNOULLI(100) must return all rows");
}

// ── Empty table ───────────────────────────────────────────────────────────────

#[test]
fn tablesample_empty_table_returns_empty() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_empty_ts (v INT)", &mut storage, &mut txn);
    let res = rows(run(
        "SELECT v FROM t_empty_ts TABLESAMPLE SYSTEM(50)",
        &mut storage,
        &mut txn,
    ));
    assert!(res.is_empty());
}

// ── Sampling returns subset ───────────────────────────────────────────────────

#[test]
fn tablesample_system_returns_valid_subset() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_sys_sub (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_sys_sub", 100);
    let res = rows(run(
        "SELECT v FROM t_sys_sub TABLESAMPLE SYSTEM(50)",
        &mut storage,
        &mut txn,
    ));
    // Non-deterministic: 0 to 100 rows. Just verify every returned value is valid.
    for row in &res {
        match row[0] {
            Value::Int(n) => assert!((0..100).contains(&n), "out of range: {n}"),
            ref other => panic!("unexpected value: {other:?}"),
        }
    }
    // With 100 rows and 50% sampling, getting 0 rows is astronomically unlikely.
    // We don't assert count to keep the test deterministic on single-page tables.
}

#[test]
fn tablesample_bernoulli_returns_valid_subset() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_bern_sub (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_bern_sub", 100);
    let res = rows(run(
        "SELECT v FROM t_bern_sub TABLESAMPLE BERNOULLI(50)",
        &mut storage,
        &mut txn,
    ));
    for row in &res {
        match row[0] {
            Value::Int(n) => assert!((0..100).contains(&n), "out of range: {n}"),
            ref other => panic!("unexpected value: {other:?}"),
        }
    }
}

// ── Alias works ───────────────────────────────────────────────────────────────

#[test]
fn tablesample_with_alias() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_alias_ts (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_alias_ts", 5);
    let res = rows(run(
        "SELECT t.v FROM t_alias_ts AS t TABLESAMPLE SYSTEM(100)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(res.len(), 5);
}

// ── WHERE + LIMIT compose correctly ──────────────────────────────────────────

#[test]
fn tablesample_with_where_and_limit() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_where_ts (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_where_ts", 50);
    let res = rows(run(
        "SELECT v FROM t_where_ts TABLESAMPLE BERNOULLI(100) WHERE v < 25 LIMIT 5",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(res.len(), 5);
    for row in &res {
        match row[0] {
            Value::Int(n) => assert!(n < 25, "where clause violated: {n}"),
            ref other => panic!("unexpected value: {other:?}"),
        }
    }
}

// ── Decimal percent (e.g. 50.5) ──────────────────────────────────────────────

#[test]
fn tablesample_decimal_percent() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t_dec_ts (v INT)", &mut storage, &mut txn);
    insert_n(&mut storage, &mut txn, "t_dec_ts", 10);
    // 100.0 should return all rows
    let res = rows(run(
        "SELECT v FROM t_dec_ts TABLESAMPLE SYSTEM(100.0)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(res.len(), 10);
}
