//! Phase 20.10 — GENERATE_SERIES executor integration tests.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager) {
    common::setup()
}

fn setup_ctx() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    common::setup_ctx()
}

fn run_rows(sql: &str, s: &mut MemoryStorage, t: &mut TxnManager) -> Vec<Vec<Value>> {
    let res = run_result(sql, s, t).unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e}"));
    rows(res)
}

fn run_rows_ctx(
    sql: &str,
    s: &mut MemoryStorage,
    t: &mut TxnManager,
    b: &mut BloomRegistry,
    c: &mut SessionContext,
) -> Vec<Vec<Value>> {
    let res = run_ctx(sql, s, t, b, c).unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e}"));
    match res {
        QueryResult::Rows { rows, .. } => rows,
        _ => Vec::new(),
    }
}

// ── Integer series ──────────────────────────────────────────────────────────

// 1. Basic two-argument series: 1..5
#[test]
fn test_gs_int_basic() {
    let (mut s, mut t) = setup();
    let rows = run_rows(
        "SELECT * FROM GENERATE_SERIES(1, 5) AS g(n)",
        &mut s,
        &mut t,
    );
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0], vec![Value::Int(1)]);
    assert_eq!(rows[4], vec![Value::Int(5)]);
}

// 2. Explicit step 2: odd numbers
#[test]
fn test_gs_int_step_2() {
    let (mut s, mut t) = setup();
    let rows = run_rows(
        "SELECT * FROM GENERATE_SERIES(1, 9, 2) AS g(n)",
        &mut s,
        &mut t,
    );
    assert_eq!(rows.len(), 5); // 1,3,5,7,9
    assert_eq!(rows[2], vec![Value::Int(5)]);
}

// 3. Descending series
#[test]
fn test_gs_int_descending() {
    let (mut s, mut t) = setup();
    let rows = run_rows(
        "SELECT * FROM GENERATE_SERIES(5, 1, -1) AS g(n)",
        &mut s,
        &mut t,
    );
    assert_eq!(rows.len(), 5); // 5,4,3,2,1
    assert_eq!(rows[0], vec![Value::Int(5)]);
    assert_eq!(rows[4], vec![Value::Int(1)]);
}

// 4. Empty result — wrong direction (start > stop, default step 1)
#[test]
fn test_gs_int_empty_ascending() {
    let (mut s, mut t) = setup();
    let rows = run_rows("SELECT * FROM GENERATE_SERIES(5, 1)", &mut s, &mut t);
    assert!(rows.is_empty());
}

// 5. Empty result — descending step but ascending range
#[test]
fn test_gs_int_empty_descending() {
    let (mut s, mut t) = setup();
    let rows = run_rows(
        "SELECT * FROM GENERATE_SERIES(1, 5, -1) AS g(n)",
        &mut s,
        &mut t,
    );
    assert!(rows.is_empty());
}

// 6. Single row: start = stop
#[test]
fn test_gs_int_single_row() {
    let (mut s, mut t) = setup();
    let rows = run_rows(
        "SELECT * FROM GENERATE_SERIES(7, 7) AS g(n)",
        &mut s,
        &mut t,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], vec![Value::Int(7)]);
}

// 7. step=0 returns error
#[test]
fn test_gs_int_step_zero_error() {
    let (mut s, mut t) = setup();
    let err = run_result(
        "SELECT * FROM GENERATE_SERIES(1, 5, 0) AS g(n)",
        &mut s,
        &mut t,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("step must not be zero"),
        "unexpected error: {err}"
    );
}

// 8. Default column name is "generate_series" when no alias given
#[test]
fn test_gs_default_column_name() {
    let (mut s, mut t) = setup();
    let res = run_result("SELECT * FROM GENERATE_SERIES(1, 3)", &mut s, &mut t).unwrap();
    match res {
        QueryResult::Rows { columns, rows } => {
            assert_eq!(columns[0].name, "generate_series");
            assert_eq!(rows.len(), 3);
        }
        _ => panic!("expected rows"),
    }
}

// 9. WITH (CTE) wrapping generate_series
#[test]
fn test_gs_in_cte() {
    let (mut s, mut t) = setup();
    let rows = run_rows(
        "WITH nums AS (SELECT * FROM GENERATE_SERIES(1, 5) AS g(n)) SELECT SUM(n) FROM nums",
        &mut s,
        &mut t,
    );
    assert_eq!(rows.len(), 1);
    // SUM(1..5) = 15 (type may be Int or BigInt depending on input)
    let v = &rows[0][0];
    let n = match v {
        Value::Int(n) => *n as i64,
        Value::BigInt(n) => *n,
        other => panic!("expected numeric sum, got {other:?}"),
    };
    assert_eq!(n, 15);
}

// 10. WHERE filter inside generate_series query
#[test]
fn test_gs_with_where_filter() {
    let (mut s, mut t) = setup();
    let rows = run_rows(
        "SELECT * FROM GENERATE_SERIES(1, 10) AS g(n) WHERE n > 7",
        &mut s,
        &mut t,
    );
    assert_eq!(rows.len(), 3); // 8,9,10
    assert_eq!(rows[0], vec![Value::Int(8)]);
}

// 11. ORDER BY + LIMIT
#[test]
fn test_gs_order_by_limit() {
    let (mut s, mut t) = setup();
    let rows = run_rows(
        "SELECT * FROM GENERATE_SERIES(1, 10, 2) AS g(n) ORDER BY n DESC LIMIT 3",
        &mut s,
        &mut t,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![Value::Int(9)]);
}

// 12. JOIN generate_series with a real table (ctx path)
#[test]
fn test_gs_join_with_table() {
    let (mut s, mut t, mut b, mut c) = setup_ctx();
    run_ctx(
        "CREATE TABLE nums_test (id INT PRIMARY KEY)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    run_ctx(
        "INSERT INTO nums_test VALUES (1), (3), (5)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    )
    .unwrap();
    let rows = run_rows_ctx(
        "SELECT t.id FROM nums_test t JOIN GENERATE_SERIES(1, 5) AS g(n) ON t.id = g.n ORDER BY t.id",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![Value::Int(1)]);
    assert_eq!(rows[1], vec![Value::Int(3)]);
    assert_eq!(rows[2], vec![Value::Int(5)]);
}

// 13. Date series — monthly (ctx path via execute_select delegated path)
#[test]
fn test_gs_date_monthly() {
    let (mut s, mut t) = setup();
    // 2024-01-01 to 2024-03-01 step '1 month' → 3 rows
    let rows = run_rows(
        "SELECT * FROM GENERATE_SERIES('2024-01-01'::date, '2024-03-01'::date, '1 month') AS g(d)",
        &mut s,
        &mut t,
    );
    assert_eq!(rows.len(), 3);
    // First row should be 2024-01-01 = days since epoch
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let jan1 = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
    let expected_days = jan1.signed_duration_since(epoch).num_days() as i32;
    assert_eq!(rows[0], vec![Value::Date(expected_days)]);
}

// 14. Date series — daily
#[test]
fn test_gs_date_daily() {
    let (mut s, mut t) = setup();
    let rows = run_rows(
        "SELECT * FROM GENERATE_SERIES('2024-01-01'::date, '2024-01-07'::date, '1 day') AS g(d)",
        &mut s,
        &mut t,
    );
    assert_eq!(rows.len(), 7);
}

// 15. Date series — missing step returns error
#[test]
fn test_gs_date_missing_step_error() {
    let (mut s, mut t) = setup();
    let err = run_result(
        "SELECT * FROM GENERATE_SERIES('2024-01-01'::date, '2024-12-31'::date)",
        &mut s,
        &mut t,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("step is required"),
        "unexpected error: {err}"
    );
}
