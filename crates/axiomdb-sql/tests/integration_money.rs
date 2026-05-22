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

// ── CREATE EXCHANGE RATE ──────────────────────────────────────────────────────

#[test]
fn create_exchange_rate_persists_catalog_entry() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE EXCHANGE RATE 'USD' TO 'EUR' RATE 0.92",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let def = reader
        .get_exchange_rate("USD", "EUR")
        .unwrap()
        .expect("rate must be persisted");
    assert_eq!(def.from_currency, "USD");
    assert_eq!(def.to_currency, "EUR");
    assert_eq!(def.mantissa, 92);
    assert_eq!(def.scale, 2);
}

#[test]
fn create_exchange_rate_replaces_existing() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE EXCHANGE RATE 'USD' TO 'EUR' RATE 0.90",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "CREATE EXCHANGE RATE 'USD' TO 'EUR' RATE 0.92",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let def = reader.get_exchange_rate("USD", "EUR").unwrap().unwrap();
    assert_eq!(def.mantissa, 92);
    assert_eq!(def.scale, 2);
}

#[test]
fn create_exchange_rate_normalizes_currency_to_uppercase() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE EXCHANGE RATE 'usd' TO 'eur' RATE 0.92",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let def = reader.get_exchange_rate("USD", "EUR").unwrap();
    assert!(def.is_some(), "must be stored under uppercase keys");
}

#[test]
fn create_exchange_rate_zero_rate_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let e = err(
        "CREATE EXCHANGE RATE 'USD' TO 'EUR' RATE 0",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(e, DbError::InvalidValue { .. }),
        "expected InvalidValue, got {e:?}"
    );
}

#[test]
fn create_exchange_rate_negative_rate_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let e = err(
        "CREATE EXCHANGE RATE 'USD' TO 'EUR' RATE -0.5",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(e, DbError::InvalidValue { .. } | DbError::ParseError { .. }),
        "expected error, got {e:?}"
    );
}

#[test]
fn drop_exchange_rate_removes_entry() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE EXCHANGE RATE 'USD' TO 'EUR' RATE 0.92",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "DROP EXCHANGE RATE 'USD' TO 'EUR'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    assert!(reader.get_exchange_rate("USD", "EUR").unwrap().is_none());
}

#[test]
fn drop_exchange_rate_if_exists_idempotent() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "DROP EXCHANGE RATE IF EXISTS 'USD' TO 'EUR'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
}

#[test]
fn drop_exchange_rate_without_if_exists_errors_when_missing() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let e = err(
        "DROP EXCHANGE RATE 'USD' TO 'EUR'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(e, DbError::InvalidValue { .. }),
        "expected InvalidValue, got {e:?}"
    );
}

// ── MONEY constructor ─────────────────────────────────────────────────────────

#[test]
fn money_constructor_integer_amount() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT MONEY(100, 'USD')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert!(matches!(v, Value::Money(100, 0, _)), "got {v:?}");
}

#[test]
fn money_constructor_decimal_amount() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT MONEY(9.99, 'USD')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    match v {
        Value::Money(m, s, c) => {
            let currency = std::str::from_utf8(&c).unwrap();
            assert_eq!(currency, "USD");
            assert_eq!(m as f64 / 10f64.powi(s as i32), 9.99);
        }
        other => panic!("expected Money, got {other:?}"),
    }
}

#[test]
fn money_constructor_negative_amount() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT MONEY(-5.00, 'EUR')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert!(
        matches!(v, Value::Money(m, _, _) if m < 0),
        "expected negative mantissa, got {v:?}"
    );
}

#[test]
fn money_constructor_empty_currency_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let e = err(
        "SELECT MONEY(10, '')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(e, DbError::InvalidValue { .. }),
        "expected InvalidValue, got {e:?}"
    );
}

#[test]
fn money_constructor_currency_too_long_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let e = err(
        "SELECT MONEY(10, 'USDX')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(e, DbError::InvalidValue { .. }),
        "expected InvalidValue, got {e:?}"
    );
}

// ── CONVERT ───────────────────────────────────────────────────────────────────

#[test]
fn convert_same_currency_returns_unchanged() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT CONVERT(MONEY(100, 'USD'), 'USD')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    match v {
        Value::Money(_, _, c) => assert_eq!(std::str::from_utf8(&c).unwrap(), "USD"),
        other => panic!("expected Money, got {other:?}"),
    }
}

#[test]
fn convert_usd_to_eur_uses_catalog_rate() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE EXCHANGE RATE 'USD' TO 'EUR' RATE 0.92",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let v = scalar(ok(
        "SELECT CONVERT(MONEY(100, 'USD'), 'EUR')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    match v {
        Value::Money(m, s, c) => {
            assert_eq!(std::str::from_utf8(&c).unwrap(), "EUR");
            let amount = m as f64 / 10f64.powi(s as i32);
            assert!(
                (amount - 92.0).abs() < 0.01,
                "expected ~92.00 EUR, got {amount}"
            );
        }
        other => panic!("expected Money, got {other:?}"),
    }
}

#[test]
fn convert_cache_invalidated_after_create() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE EXCHANGE RATE 'USD' TO 'EUR' RATE 0.90",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let _ = scalar(ok(
        "SELECT CONVERT(MONEY(100, 'USD'), 'EUR')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    ok(
        "CREATE EXCHANGE RATE 'USD' TO 'EUR' RATE 0.95",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let v = scalar(ok(
        "SELECT CONVERT(MONEY(100, 'USD'), 'EUR')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    match v {
        Value::Money(m, s, _) => {
            let amount = m as f64 / 10f64.powi(s as i32);
            assert!(
                (amount - 95.0).abs() < 0.01,
                "cache should be invalidated; got {amount}"
            );
        }
        other => panic!("expected Money, got {other:?}"),
    }
}

#[test]
fn convert_missing_rate_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let e = err(
        "SELECT CONVERT(MONEY(100, 'USD'), 'JPY')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(e, DbError::InvalidValue { .. }),
        "expected InvalidValue, got {e:?}"
    );
}

#[test]
fn convert_null_returns_null() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT CONVERT(NULL, 'USD')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Null);
}

// ── CURRENCY_OF / AMOUNT_OF ───────────────────────────────────────────────────

#[test]
fn currency_of_returns_currency_code() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT CURRENCY_OF(MONEY(100, 'eur'))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Text("EUR".into()));
}

#[test]
fn amount_of_returns_decimal() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT AMOUNT_OF(MONEY(9.99, 'USD'))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    match v {
        Value::Decimal(m, s) => {
            let f = m as f64 / 10f64.powi(s as i32);
            assert!((f - 9.99).abs() < 0.001, "expected 9.99, got {f}");
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
}

#[test]
fn currency_of_null_returns_null() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT CURRENCY_OF(NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Null);
}

#[test]
fn amount_of_null_returns_null() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT AMOUNT_OF(NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Null);
}

// ── Money arithmetic ──────────────────────────────────────────────────────────

#[test]
fn money_add_same_currency() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT MONEY(10.00, 'USD') + MONEY(5.50, 'USD')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    match v {
        Value::Money(m, s, c) => {
            assert_eq!(std::str::from_utf8(&c).unwrap(), "USD");
            let amount = m as f64 / 10f64.powi(s as i32);
            assert!(
                (amount - 15.50).abs() < 0.001,
                "expected 15.50, got {amount}"
            );
        }
        other => panic!("expected Money, got {other:?}"),
    }
}

#[test]
fn money_sub_same_currency() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT MONEY(10.00, 'USD') - MONEY(3.00, 'USD')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    match v {
        Value::Money(m, s, _) => {
            let amount = m as f64 / 10f64.powi(s as i32);
            assert!((amount - 7.00).abs() < 0.001, "expected 7.00, got {amount}");
        }
        other => panic!("expected Money, got {other:?}"),
    }
}

#[test]
fn money_add_different_currencies_returns_error() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let e = err(
        "SELECT MONEY(10, 'USD') + MONEY(10, 'EUR')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert!(
        matches!(e, DbError::InvalidValue { .. }),
        "expected InvalidValue, got {e:?}"
    );
}

#[test]
fn money_mul_by_integer() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT MONEY(10, 'USD') * 3",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    match v {
        Value::Money(m, s, c) => {
            assert_eq!(std::str::from_utf8(&c).unwrap(), "USD");
            let amount = m as f64 / 10f64.powi(s as i32);
            assert!(
                (amount - 30.0).abs() < 0.001,
                "expected 30.00, got {amount}"
            );
        }
        other => panic!("expected Money, got {other:?}"),
    }
}

#[test]
fn money_eq_same_currency_and_amount() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT MONEY(10, 'USD') = MONEY(10, 'USD')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn money_lt_same_currency() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    let v = scalar(ok(
        "SELECT MONEY(5, 'USD') < MONEY(10, 'USD')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Bool(true));
}

// ── CREATE TABLE with MONEY column ────────────────────────────────────────────

#[test]
fn create_table_with_money_column_and_insert_select() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE TABLE prices (id INT, price MONEY NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO prices VALUES (1, MONEY(9.99, 'USD'))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO prices VALUES (2, MONEY(19.99, 'EUR'))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(ok(
        "SELECT id, price FROM prices ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(r.len(), 2);
    assert!(
        matches!(r[0][1], Value::Money(_, _, _)),
        "row 0 price should be Money"
    );
    assert!(
        matches!(r[1][1], Value::Money(_, _, _)),
        "row 1 price should be Money"
    );
    if let Value::Money(_, _, c) = &r[0][1] {
        assert_eq!(std::str::from_utf8(c).unwrap(), "USD");
    }
    if let Value::Money(_, _, c) = &r[1][1] {
        assert_eq!(std::str::from_utf8(c).unwrap(), "EUR");
    }
}

#[test]
fn money_currency_of_on_table_column() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE TABLE wallet (amount MONEY NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO wallet VALUES (MONEY(50, 'GBP'))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let v = scalar(ok(
        "SELECT CURRENCY_OF(amount) FROM wallet",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(v, Value::Text("GBP".into()));
}

#[test]
fn money_where_filter_by_currency() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    ok(
        "CREATE TABLE prices (id INT, price MONEY NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO prices VALUES (1, MONEY(10, 'USD'))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "INSERT INTO prices VALUES (2, MONEY(20, 'EUR'))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let r = rows(ok(
        "SELECT id FROM prices WHERE CURRENCY_OF(price) = 'USD'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn money_display_format() {
    let v = Value::Money(10050, 2, *b"USD");
    assert_eq!(v.to_string(), "100.50 USD");
    let v2 = Value::Money(1234, 0, *b"JPY");
    assert_eq!(v2.to_string(), "1234 JPY");
    let v3 = Value::Money(-500, 2, *b"EUR");
    assert_eq!(v3.to_string(), "-5.00 EUR");
}
