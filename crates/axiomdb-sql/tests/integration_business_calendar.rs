mod common;

use axiomdb_catalog::CatalogReader;
use axiomdb_core::error::DbError;
use axiomdb_sql::QueryResult;
use axiomdb_types::Value;

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn scalar(result: QueryResult) -> Value {
    let mut r = rows(result);
    assert_eq!(r.len(), 1, "expected exactly 1 row");
    assert_eq!(r[0].len(), 1, "expected exactly 1 column");
    r.remove(0).remove(0)
}

fn ok(
    sql: &str,
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
    bloom: &mut axiomdb_sql::bloom::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) -> QueryResult {
    common::run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn err(
    sql: &str,
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
    bloom: &mut axiomdb_sql::bloom::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) -> DbError {
    common::run_ctx(sql, storage, txn, bloom, ctx).expect_err("expected an error")
}

// ── CREATE HOLIDAY CALENDAR ───────────────────────────────────────────────────

#[test]
fn create_holiday_calendar_persists_catalog_entry() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'CO' WITH HOLIDAYS ('2024-01-01', '2024-07-04')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let def = reader
        .get_holiday_calendar("CO")
        .unwrap()
        .expect("calendar must be persisted");
    assert_eq!(def.country_code, "CO");
    assert_eq!(def.holidays.len(), 2);
}

#[test]
fn create_holiday_calendar_normalizes_country_code_to_uppercase() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'us' WITH HOLIDAYS ('2024-07-04')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let def = reader
        .get_holiday_calendar("US")
        .unwrap()
        .expect("should be stored as US");
    assert_eq!(def.country_code, "US");
}

#[test]
fn create_holiday_calendar_deduplicates_and_sorts_holidays() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'AR' WITH HOLIDAYS ('2024-05-25', '2024-01-01', '2024-05-25')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let def = reader.get_holiday_calendar("AR").unwrap().unwrap();
    assert_eq!(def.holidays.len(), 2, "duplicate holiday must be removed");
    assert!(
        def.holidays.windows(2).all(|w| w[0] < w[1]),
        "holidays must be sorted"
    );
}

#[test]
fn create_holiday_calendar_empty_holidays_is_valid() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'XX'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let def = reader.get_holiday_calendar("XX").unwrap().unwrap();
    assert!(def.holidays.is_empty());
}

#[test]
fn create_holiday_calendar_replaces_existing() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'BR' WITH HOLIDAYS ('2024-01-01')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "CREATE HOLIDAY CALENDAR 'BR' WITH HOLIDAYS ('2024-09-07', '2024-11-15')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let def = reader.get_holiday_calendar("BR").unwrap().unwrap();
    assert_eq!(
        def.holidays.len(),
        2,
        "second CREATE must replace, not append"
    );
}

#[test]
fn create_holiday_calendar_invalid_date_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let e = err(
        "CREATE HOLIDAY CALENDAR 'XX' WITH HOLIDAYS ('not-a-date')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(matches!(e, DbError::InvalidValue { .. }), "got: {e:?}");
}

// ── DROP HOLIDAY CALENDAR ─────────────────────────────────────────────────────

#[test]
fn drop_holiday_calendar_removes_entry() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'MX' WITH HOLIDAYS ('2024-09-16')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "DROP HOLIDAY CALENDAR 'MX'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    assert!(reader.get_holiday_calendar("MX").unwrap().is_none());
}

#[test]
fn drop_holiday_calendar_if_exists_is_idempotent() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    // No error when calendar doesn't exist
    ok(
        "DROP HOLIDAY CALENDAR IF EXISTS 'NOPE'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
}

#[test]
fn drop_holiday_calendar_without_if_exists_errors_when_missing() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let e = err(
        "DROP HOLIDAY CALENDAR 'GHOST'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(matches!(e, DbError::InvalidValue { .. }), "got: {e:?}");
}

// ── IS_BUSINESS_DAY ───────────────────────────────────────────────────────────

// Unix epoch 2024-01-01 = days since epoch: 19723 (Monday)
// 2024-01-06 = 19728 (Saturday)
// 2024-01-07 = 19729 (Sunday)

#[test]
fn is_business_day_monday_without_holiday_returns_1() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'CO'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // 2024-01-01 is a Monday → business day (no holidays in calendar)
    let v = scalar(ok(
        "SELECT IS_BUSINESS_DAY('2024-01-01', 'CO')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Int(1));
}

#[test]
fn is_business_day_saturday_returns_0() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'CO'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // 2024-01-06 is a Saturday
    let v = scalar(ok(
        "SELECT IS_BUSINESS_DAY('2024-01-06', 'CO')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Int(0));
}

#[test]
fn is_business_day_sunday_returns_0() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'CO'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let v = scalar(ok(
        "SELECT IS_BUSINESS_DAY('2024-01-07', 'CO')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Int(0));
}

#[test]
fn is_business_day_holiday_weekday_returns_0() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    // 2024-01-01 is a Monday — register it as holiday
    ok(
        "CREATE HOLIDAY CALENDAR 'CO' WITH HOLIDAYS ('2024-01-01')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let v = scalar(ok(
        "SELECT IS_BUSINESS_DAY('2024-01-01', 'CO')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Int(0));
}

#[test]
fn is_business_day_unknown_calendar_returns_1_for_weekday() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    // No calendar registered for 'ZZ' → empty holiday set → weekday = business day
    let v = scalar(ok(
        "SELECT IS_BUSINESS_DAY('2024-01-01', 'ZZ')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Int(1));
}

// ── NEXT_BUSINESS_DAY ─────────────────────────────────────────────────────────

#[test]
fn next_business_day_from_friday_skips_weekend() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'CO'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // 2024-01-05 is a Friday → next business day should be 2024-01-08 (Monday)
    let v = scalar(ok(
        "SELECT NEXT_BUSINESS_DAY('2024-01-05', 'CO')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    // 2024-01-08: days since epoch
    let expected = {
        use chrono::NaiveDate;
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let d = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();
        i32::try_from(d.signed_duration_since(epoch).num_days()).unwrap()
    };
    assert_eq!(v, Value::Int(expected));
}

#[test]
fn next_business_day_skips_holiday_on_monday() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    // 2024-01-08 is a Monday — register as holiday
    ok(
        "CREATE HOLIDAY CALENDAR 'CO' WITH HOLIDAYS ('2024-01-08')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // From 2024-01-05 (Friday) → skip weekend + holiday Monday → 2024-01-09 (Tuesday)
    let v = scalar(ok(
        "SELECT NEXT_BUSINESS_DAY('2024-01-05', 'CO')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    let expected = {
        use chrono::NaiveDate;
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let d = NaiveDate::from_ymd_opt(2024, 1, 9).unwrap();
        i32::try_from(d.signed_duration_since(epoch).num_days()).unwrap()
    };
    assert_eq!(v, Value::Int(expected));
}

// ── BUSINESS_DAYS_BETWEEN ─────────────────────────────────────────────────────

#[test]
fn business_days_between_same_date_returns_0() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'CO'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let v = scalar(ok(
        "SELECT BUSINESS_DAYS_BETWEEN('2024-01-01', '2024-01-01', 'CO')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Int(0));
}

#[test]
fn business_days_between_end_before_start_returns_0() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'CO'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let v = scalar(ok(
        "SELECT BUSINESS_DAYS_BETWEEN('2024-01-05', '2024-01-01', 'CO')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Int(0));
}

#[test]
fn business_days_between_one_week_no_holidays() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'CO'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Mon 2024-01-01 to Mon 2024-01-08 = 5 business days (Mon–Fri)
    let v = scalar(ok(
        "SELECT BUSINESS_DAYS_BETWEEN('2024-01-01', '2024-01-08', 'CO')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Int(5));
}

#[test]
fn business_days_between_subtracts_holidays() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    // Register Monday 2024-01-01 as holiday
    ok(
        "CREATE HOLIDAY CALENDAR 'CO' WITH HOLIDAYS ('2024-01-01')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Mon 2024-01-01 (holiday) to Mon 2024-01-08 = 4 business days
    let v = scalar(ok(
        "SELECT BUSINESS_DAYS_BETWEEN('2024-01-01', '2024-01-08', 'CO')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Int(4));
}

#[test]
fn business_days_between_holiday_cache_is_invalidated_after_create() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE HOLIDAY CALENDAR 'CO'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // First query — loads empty calendar into cache
    let v1 = scalar(ok(
        "SELECT BUSINESS_DAYS_BETWEEN('2024-01-01', '2024-01-08', 'CO')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v1, Value::Int(5));

    // Recreate calendar with a holiday on Monday
    ok(
        "CREATE HOLIDAY CALENDAR 'CO' WITH HOLIDAYS ('2024-01-01')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Cache should be cleared after the DDL; now should see the holiday
    let v2 = scalar(ok(
        "SELECT BUSINESS_DAYS_BETWEEN('2024-01-01', '2024-01-08', 'CO')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v2, Value::Int(4));
}
