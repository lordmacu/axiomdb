//! Integration tests for MySQL scalar date functions (subfase 4.19d).
//!
//! Covers: DATE_FORMAT, STR_TO_DATE, FIND_IN_SET, and the fixed
//! year/month/day/hour/minute/second extractors.
//!
//! All tests run through the full SQL pipeline: parse → analyze → execute.

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute, parse, QueryResult};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── helpers ───────────────────────────────────────────────────────────────────

fn setup() -> (MemoryStorage, TxnManager) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.keep().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    (storage, txn)
}

fn run_result(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let analyzed = analyze(stmt, storage, snap)?;
    execute(analyzed, storage, txn)
}

/// Execute a scalar SELECT and return the single value.
fn scalar(sql: &str) -> Value {
    let (mut st, mut txn) = setup();
    let result =
        run_result(sql, &mut st, &mut txn).unwrap_or_else(|e| panic!("SQL failed: {sql}\n{e:?}"));
    match result {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .next()
            .expect("no rows")
            .into_iter()
            .next()
            .expect("no columns"),
        other => panic!("expected Rows, got {other:?}"),
    }
}

// ── DATE_FORMAT ───────────────────────────────────────────────────────────────

#[test]
fn date_format_null_ts_returns_null() {
    assert_eq!(scalar("SELECT DATE_FORMAT(NULL, '%Y-%m-%d')"), Value::Null);
}

#[test]
fn date_format_null_fmt_returns_null() {
    assert_eq!(
        scalar("SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25', '%Y-%m-%d'), NULL)"),
        Value::Null
    );
}

#[test]
fn date_format_date_value_yyyymmdd() {
    // STR_TO_DATE with date-only format → Value::Date → formatted back
    let v = scalar("SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25', '%Y-%m-%d'), '%Y-%m-%d')");
    assert_eq!(v, Value::Text("2025-03-25".into()));
}

#[test]
fn date_format_timestamp_yyyymmdd() {
    let v = scalar(
        "SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25 14:30:45', '%Y-%m-%d %H:%i:%s'), '%Y-%m-%d')",
    );
    assert_eq!(v, Value::Text("2025-03-25".into()));
}

#[test]
fn date_format_time_components() {
    let v = scalar(
        "SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25 14:30:45', '%Y-%m-%d %H:%i:%s'), '%H:%i:%s')",
    );
    assert_eq!(v, Value::Text("14:30:45".into()));
}

#[test]
fn date_format_dmyyy_slash_separator() {
    let v = scalar("SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25', '%Y-%m-%d'), '%d/%m/%Y')");
    assert_eq!(v, Value::Text("25/03/2025".into()));
}

#[test]
fn date_format_unknown_specifier_passthrough() {
    // %X is unknown → literal %X
    let v = scalar("SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25', '%Y-%m-%d'), '%Y-%X-%d')");
    assert_eq!(v, Value::Text("2025-%X-25".into()));
}

#[test]
fn date_format_month_name() {
    let v = scalar("SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25', '%Y-%m-%d'), '%M')");
    assert_eq!(v, Value::Text("March".into()));
}

#[test]
fn date_format_month_abbr() {
    let v = scalar("SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25', '%Y-%m-%d'), '%b')");
    assert_eq!(v, Value::Text("Mar".into()));
}

#[test]
fn date_format_percent_literal() {
    let v = scalar("SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25', '%Y-%m-%d'), '%%')");
    assert_eq!(v, Value::Text("%".into()));
}

#[test]
fn date_format_t_specifier() {
    // %T = HH:MM:SS
    let v =
        scalar("SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25 14:30:45', '%Y-%m-%d %H:%i:%s'), '%T')");
    assert_eq!(v, Value::Text("14:30:45".into()));
}

// ── STR_TO_DATE ───────────────────────────────────────────────────────────────

#[test]
fn str_to_date_date_only_returns_date_value() {
    // Date-only format → Value::Date (not Timestamp)
    let v = scalar("SELECT STR_TO_DATE('2025-03-25', '%Y-%m-%d')");
    assert!(
        matches!(v, Value::Date(d) if d > 0),
        "expected Value::Date, got {v:?}"
    );
}

#[test]
fn str_to_date_datetime_returns_timestamp() {
    let v = scalar("SELECT STR_TO_DATE('2025-03-25 14:30:00', '%Y-%m-%d %H:%i:%s')");
    assert!(
        matches!(v, Value::Timestamp(_)),
        "expected Value::Timestamp, got {v:?}"
    );
}

#[test]
fn str_to_date_roundtrip_via_date_format() {
    // STR_TO_DATE → DATE_FORMAT should recover the original date string
    let v = scalar("SELECT DATE_FORMAT(STR_TO_DATE('2025-03-25', '%Y-%m-%d'), '%Y-%m-%d')");
    assert_eq!(v, Value::Text("2025-03-25".into()));
}

#[test]
fn str_to_date_bad_input_returns_null() {
    assert_eq!(
        scalar("SELECT STR_TO_DATE('not-a-date', '%Y-%m-%d')"),
        Value::Null
    );
}

#[test]
fn str_to_date_null_str_returns_null() {
    assert_eq!(scalar("SELECT STR_TO_DATE(NULL, '%Y-%m-%d')"), Value::Null);
}

#[test]
fn str_to_date_null_fmt_returns_null() {
    assert_eq!(
        scalar("SELECT STR_TO_DATE('2025-03-25', NULL)"),
        Value::Null
    );
}

#[test]
fn str_to_date_slash_separator() {
    let v = scalar("SELECT DATE_FORMAT(STR_TO_DATE('25/03/2025', '%d/%m/%Y'), '%Y-%m-%d')");
    assert_eq!(v, Value::Text("2025-03-25".into()));
}

#[test]
fn str_to_date_2digit_year_below70() {
    // %y: 00-69 → 2000-2069
    let v = scalar("SELECT DATE_FORMAT(STR_TO_DATE('25-03-25', '%y-%m-%d'), '%Y-%m-%d')");
    assert_eq!(v, Value::Text("2025-03-25".into()));
}

#[test]
fn str_to_date_2digit_year_above70() {
    // %y: 70-99 → 1970-1999
    let v = scalar("SELECT DATE_FORMAT(STR_TO_DATE('85-06-15', '%y-%m-%d'), '%Y-%m-%d')");
    assert_eq!(v, Value::Text("1985-06-15".into()));
}

#[test]
fn str_to_date_invalid_day_returns_null() {
    // Feb 30 is invalid → NULL
    assert_eq!(
        scalar("SELECT STR_TO_DATE('2025-02-30', '%Y-%m-%d')"),
        Value::Null
    );
}

// ── FIND_IN_SET ───────────────────────────────────────────────────────────────

#[test]
fn find_in_set_found_second_position() {
    assert_eq!(scalar("SELECT FIND_IN_SET('b', 'a,b,c')"), Value::Int(2));
}

#[test]
fn find_in_set_found_first_position() {
    assert_eq!(scalar("SELECT FIND_IN_SET('a', 'a,b,c')"), Value::Int(1));
}

#[test]
fn find_in_set_found_last_position() {
    assert_eq!(scalar("SELECT FIND_IN_SET('c', 'a,b,c')"), Value::Int(3));
}

#[test]
fn find_in_set_not_found_returns_zero() {
    assert_eq!(scalar("SELECT FIND_IN_SET('z', 'a,b,c')"), Value::Int(0));
}

#[test]
fn find_in_set_case_insensitive() {
    assert_eq!(scalar("SELECT FIND_IN_SET('B', 'a,b,c')"), Value::Int(2));
}

#[test]
fn find_in_set_null_needle_returns_null() {
    assert_eq!(scalar("SELECT FIND_IN_SET(NULL, 'a,b,c')"), Value::Null);
}

#[test]
fn find_in_set_null_list_returns_null() {
    assert_eq!(scalar("SELECT FIND_IN_SET('a', NULL)"), Value::Null);
}

#[test]
fn find_in_set_empty_list_returns_zero() {
    assert_eq!(scalar("SELECT FIND_IN_SET('a', '')"), Value::Int(0));
}

#[test]
fn find_in_set_empty_needle_returns_zero() {
    assert_eq!(scalar("SELECT FIND_IN_SET('', 'a,b,c')"), Value::Int(0));
}

// ── year / month / day / hour / minute / second (fixed) ──────────────────────

/// Build a sub-expression for a known timestamp: 2025-03-25 14:30:45 UTC.
fn known_ts() -> &'static str {
    "STR_TO_DATE('2025-03-25 14:30:45', '%Y-%m-%d %H:%i:%s')"
}

fn known_date() -> &'static str {
    "STR_TO_DATE('2025-03-25', '%Y-%m-%d')"
}

#[test]
fn year_from_timestamp() {
    assert_eq!(
        scalar(&format!("SELECT year({known})", known = known_ts())),
        Value::Int(2025)
    );
}

#[test]
fn month_from_timestamp() {
    assert_eq!(
        scalar(&format!("SELECT month({known})", known = known_ts())),
        Value::Int(3)
    );
}

#[test]
fn day_from_timestamp() {
    assert_eq!(
        scalar(&format!("SELECT day({known})", known = known_ts())),
        Value::Int(25)
    );
}

#[test]
fn hour_from_timestamp() {
    assert_eq!(
        scalar(&format!("SELECT hour({known})", known = known_ts())),
        Value::Int(14)
    );
}

#[test]
fn minute_from_timestamp() {
    assert_eq!(
        scalar(&format!("SELECT minute({known})", known = known_ts())),
        Value::Int(30)
    );
}

#[test]
fn second_from_timestamp() {
    assert_eq!(
        scalar(&format!("SELECT second({known})", known = known_ts())),
        Value::Int(45)
    );
}

#[test]
fn year_from_date() {
    assert_eq!(
        scalar(&format!("SELECT year({known})", known = known_date())),
        Value::Int(2025)
    );
}

#[test]
fn month_from_date() {
    assert_eq!(
        scalar(&format!("SELECT month({known})", known = known_date())),
        Value::Int(3)
    );
}

#[test]
fn day_from_date() {
    assert_eq!(
        scalar(&format!("SELECT day({known})", known = known_date())),
        Value::Int(25)
    );
}

#[test]
fn year_null_returns_null() {
    assert_eq!(scalar("SELECT year(NULL)"), Value::Null);
}

#[test]
fn month_null_returns_null() {
    assert_eq!(scalar("SELECT month(NULL)"), Value::Null);
}
