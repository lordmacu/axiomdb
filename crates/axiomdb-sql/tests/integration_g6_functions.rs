//! Integration tests for G6 function implementations.
//!
//! Covers:
//!   G6.1 — Math functions: PI, EXP, LOG, trig, GREATEST, LEAST, TRUNCATE, RAND
//!   G6.2 — Date functions: UTC_DATE/TIME/TIMESTAMP, FROM_UNIXTIME, TIMESTAMPDIFF, etc.
//!   G6.3 — String functions: SOUNDEX, FORMAT, BIN, OCT, HEX, UNHEX, FIELD, ELT, ORD
//!   G6.4 — System: FOUND_ROWS(), LAST_INSERT_ID(expr) 1-arg form
//!   G6.5 — SQL_CALC_FOUND_ROWS + FOUND_ROWS() pagination
//!   G6.6 — CONVERT(expr, type) MySQL two-arg syntax
//!   G6.7 — SELECT optimizer hint modifiers (HIGH_PRIORITY, STRAIGHT_JOIN, etc.)

mod common;
use axiomdb_types::Value;

fn run(
    sql: &str,
    s: &mut axiomdb_storage::MemoryStorage,
    t: &mut axiomdb_wal::TxnManager,
) -> axiomdb_sql::QueryResult {
    common::run(sql, s, t)
}

fn scalar(
    sql: &str,
    s: &mut axiomdb_storage::MemoryStorage,
    t: &mut axiomdb_wal::TxnManager,
) -> Value {
    let r = common::rows(run(sql, s, t));
    r.into_iter().next().unwrap().into_iter().next().unwrap()
}

// ── G6.1 Math functions ───────────────────────────────────────────────────────

#[test]
fn test_pi() {
    let (mut s, mut t) = common::setup();
    match scalar("SELECT PI()", &mut s, &mut t) {
        Value::Real(f) => assert!((f - std::f64::consts::PI).abs() < 1e-10),
        other => panic!("expected Real, got {other:?}"),
    }
}

#[test]
fn test_exp_log() {
    let (mut s, mut t) = common::setup();
    // EXP(1) ≈ e
    match scalar("SELECT EXP(1)", &mut s, &mut t) {
        Value::Real(f) => assert!((f - std::f64::consts::E).abs() < 1e-10),
        other => panic!("expected Real, got {other:?}"),
    }
    // LOG(EXP(1)) = 1
    match scalar("SELECT LOG(EXP(1))", &mut s, &mut t) {
        Value::Real(f) => assert!((f - 1.0).abs() < 1e-10),
        other => panic!("expected Real, got {other:?}"),
    }
}

#[test]
fn test_log2_log10() {
    let (mut s, mut t) = common::setup();
    match scalar("SELECT LOG2(8)", &mut s, &mut t) {
        Value::Real(f) => assert!((f - 3.0).abs() < 1e-10),
        other => panic!("expected Real, got {other:?}"),
    }
    match scalar("SELECT LOG10(1000)", &mut s, &mut t) {
        Value::Real(f) => assert!((f - 3.0).abs() < 1e-10),
        other => panic!("expected Real, got {other:?}"),
    }
}

#[test]
fn test_trig() {
    let (mut s, mut t) = common::setup();
    match scalar("SELECT SIN(0)", &mut s, &mut t) {
        Value::Real(f) => assert!(f.abs() < 1e-10),
        other => panic!("expected Real, got {other:?}"),
    }
    match scalar("SELECT COS(0)", &mut s, &mut t) {
        Value::Real(f) => assert!((f - 1.0).abs() < 1e-10),
        other => panic!("expected Real, got {other:?}"),
    }
}

#[test]
fn test_radians_degrees() {
    let (mut s, mut t) = common::setup();
    match scalar("SELECT DEGREES(PI())", &mut s, &mut t) {
        Value::Real(f) => assert!((f - 180.0).abs() < 1e-10),
        other => panic!("expected Real, got {other:?}"),
    }
    match scalar("SELECT RADIANS(180)", &mut s, &mut t) {
        Value::Real(f) => assert!((f - std::f64::consts::PI).abs() < 1e-10),
        other => panic!("expected Real, got {other:?}"),
    }
}

#[test]
fn test_greatest_least() {
    let (mut s, mut t) = common::setup();
    assert_eq!(
        scalar("SELECT GREATEST(3, 1, 4, 1, 5)", &mut s, &mut t),
        Value::Int(5)
    );
    assert_eq!(
        scalar("SELECT LEAST(3, 1, 4, 1, 5)", &mut s, &mut t),
        Value::Int(1)
    );
    assert_eq!(
        scalar("SELECT GREATEST(3, NULL, 4)", &mut s, &mut t),
        Value::Null
    );
}

#[test]
fn test_truncate() {
    let (mut s, mut t) = common::setup();
    match scalar("SELECT TRUNCATE(3.9, 0)", &mut s, &mut t) {
        Value::Real(f) => assert_eq!(f, 3.0),
        other => panic!("expected Real, got {other:?}"),
    }
    match scalar("SELECT TRUNCATE(3.456, 2)", &mut s, &mut t) {
        Value::Real(f) => assert!((f - 3.45).abs() < 1e-10),
        other => panic!("expected Real, got {other:?}"),
    }
}

#[test]
fn test_rand() {
    let (mut s, mut t) = common::setup();
    match scalar("SELECT RAND()", &mut s, &mut t) {
        Value::Real(f) => assert!((0.0..=1.0).contains(&f)),
        other => panic!("expected Real, got {other:?}"),
    }
}

// ── G6.2 Date functions ───────────────────────────────────────────────────────

#[test]
fn test_utc_date() {
    let (mut s, mut t) = common::setup();
    // UTC_DATE() should return a date (days since epoch, ≥ 2024 days)
    match scalar("SELECT UTC_DATE()", &mut s, &mut t) {
        Value::Date(d) => assert!(d > 19000), // 2022-01-01 = 18993 days since epoch
        other => panic!("expected Date, got {other:?}"),
    }
}

#[test]
fn test_utc_timestamp() {
    let (mut s, mut t) = common::setup();
    match scalar("SELECT UTC_TIMESTAMP()", &mut s, &mut t) {
        Value::Timestamp(micros) => assert!(micros > 1_700_000_000_000_000),
        other => panic!("expected Timestamp, got {other:?}"),
    }
}

#[test]
fn test_from_unixtime() {
    let (mut s, mut t) = common::setup();
    match scalar("SELECT FROM_UNIXTIME(0)", &mut s, &mut t) {
        Value::Timestamp(micros) => assert_eq!(micros, 0),
        other => panic!("expected Timestamp, got {other:?}"),
    }
}

#[test]
fn test_timestampdiff() {
    let (mut s, mut t) = common::setup();
    // TIMESTAMPDIFF(DAY, '2024-01-01', '2024-01-11') = 10
    match scalar(
        "SELECT TIMESTAMPDIFF(DAY, '2024-01-01', '2024-01-11')",
        &mut s,
        &mut t,
    ) {
        Value::Int(n) => assert_eq!(n, 10),
        Value::BigInt(n) => assert_eq!(n, 10),
        other => panic!("expected Int/BigInt 10, got {other:?}"),
    }
}

#[test]
fn test_dayofweek() {
    let (mut s, mut t) = common::setup();
    // DAYOFWEEK: 1=Sunday, 2=Monday, ..., 7=Saturday
    // 2024-01-01 was a Monday → 2
    match scalar("SELECT DAYOFWEEK('2024-01-01')", &mut s, &mut t) {
        Value::Int(n) => assert_eq!(n, 2),
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn test_quarter() {
    let (mut s, mut t) = common::setup();
    assert_eq!(
        scalar("SELECT QUARTER('2024-04-01')", &mut s, &mut t),
        Value::Int(2)
    );
    assert_eq!(
        scalar("SELECT QUARTER('2024-12-31')", &mut s, &mut t),
        Value::Int(4)
    );
}

// ── G6.3 String functions ─────────────────────────────────────────────────────

#[test]
fn test_soundex() {
    let (mut s, mut t) = common::setup();
    // Classic Soundex: Robert → R163
    assert_eq!(
        scalar("SELECT SOUNDEX('Robert')", &mut s, &mut t),
        Value::Text("R163".into())
    );
    assert_eq!(
        scalar("SELECT SOUNDEX('Rupert')", &mut s, &mut t),
        Value::Text("R163".into())
    );
}

#[test]
fn test_format() {
    let (mut s, mut t) = common::setup();
    assert_eq!(
        scalar("SELECT FORMAT(1234567.891, 2)", &mut s, &mut t),
        Value::Text("1,234,567.89".into())
    );
}

#[test]
fn test_hex_unhex() {
    let (mut s, mut t) = common::setup();
    assert_eq!(
        scalar("SELECT HEX(255)", &mut s, &mut t),
        Value::Text("FF".into())
    );
    assert_eq!(
        scalar("SELECT HEX(0)", &mut s, &mut t),
        Value::Text("0".into())
    );
    // UNHEX round-trip: HEX(255) = 'FF', UNHEX('FF') decodes to bytes
    match scalar("SELECT UNHEX('41')", &mut s, &mut t) {
        Value::Bytes(b) => assert_eq!(b, vec![0x41]),
        other => panic!("expected Bytes, got {other:?}"),
    }
}

#[test]
fn test_bin_oct() {
    let (mut s, mut t) = common::setup();
    assert_eq!(
        scalar("SELECT BIN(10)", &mut s, &mut t),
        Value::Text("1010".into())
    );
    assert_eq!(
        scalar("SELECT OCT(8)", &mut s, &mut t),
        Value::Text("10".into())
    );
}

#[test]
fn test_field() {
    let (mut s, mut t) = common::setup();
    // FIELD returns 1-based position
    assert_eq!(
        scalar("SELECT FIELD('b', 'a', 'b', 'c')", &mut s, &mut t),
        Value::Int(2)
    );
    // Not found → 0
    assert_eq!(
        scalar("SELECT FIELD('z', 'a', 'b', 'c')", &mut s, &mut t),
        Value::Int(0)
    );
}

#[test]
fn test_elt() {
    let (mut s, mut t) = common::setup();
    assert_eq!(
        scalar("SELECT ELT(2, 'a', 'b', 'c')", &mut s, &mut t),
        Value::Text("b".into())
    );
    assert_eq!(
        scalar("SELECT ELT(5, 'a', 'b', 'c')", &mut s, &mut t),
        Value::Null
    );
}

#[test]
fn test_ord() {
    let (mut s, mut t) = common::setup();
    assert_eq!(scalar("SELECT ORD('A')", &mut s, &mut t), Value::Int(65));
    assert_eq!(scalar("SELECT ORD('')", &mut s, &mut t), Value::Int(0));
}

#[test]
fn test_insert_string() {
    let (mut s, mut t) = common::setup();
    // INSERT('Hello World', 7, 5, 'MySQL')
    assert_eq!(
        scalar(
            "SELECT INSERT('Hello World', 7, 5, 'MySQL')",
            &mut s,
            &mut t
        ),
        Value::Text("Hello MySQL".into())
    );
}

// ── G6.4 System functions ─────────────────────────────────────────────────────

#[test]
fn test_last_insert_id_1arg() {
    let (mut s, mut t) = common::setup();
    // LAST_INSERT_ID(42) stores 42 and returns it
    assert_eq!(
        scalar("SELECT LAST_INSERT_ID(42)", &mut s, &mut t),
        Value::BigInt(42)
    );
    // LAST_INSERT_ID() without args returns the stored value
    assert_eq!(
        scalar("SELECT LAST_INSERT_ID()", &mut s, &mut t),
        Value::BigInt(42)
    );
}

// ── G6.5 SQL_CALC_FOUND_ROWS + FOUND_ROWS() ──────────────────────────────────

#[test]
fn test_sql_calc_found_rows() {
    let (mut s, mut t) = common::setup();
    run("CREATE TABLE nums (n INT)", &mut s, &mut t);
    for i in 1..=10 {
        run(&format!("INSERT INTO nums VALUES ({i})"), &mut s, &mut t);
    }

    // SELECT SQL_CALC_FOUND_ROWS with LIMIT 3 — returns 3 rows but stores 10
    let result = common::rows(run(
        "SELECT SQL_CALC_FOUND_ROWS n FROM nums ORDER BY n LIMIT 3",
        &mut s,
        &mut t,
    ));
    assert_eq!(result.len(), 3);

    // FOUND_ROWS() should return 10
    assert_eq!(
        scalar("SELECT FOUND_ROWS()", &mut s, &mut t),
        Value::BigInt(10)
    );
}

// ── G6.6 CONVERT(expr, type) ─────────────────────────────────────────────────

#[test]
fn test_convert_signed() {
    let (mut s, mut t) = common::setup();
    assert_eq!(
        scalar("SELECT CONVERT('42', SIGNED)", &mut s, &mut t),
        Value::BigInt(42)
    );
    assert_eq!(
        scalar("SELECT CONVERT('42', UNSIGNED INTEGER)", &mut s, &mut t),
        Value::BigInt(42)
    );
}

#[test]
fn test_convert_char() {
    let (mut s, mut t) = common::setup();
    match scalar("SELECT CONVERT(42, CHAR)", &mut s, &mut t) {
        Value::Text(s) => assert_eq!(s, "42"),
        other => panic!("expected Text '42', got {other:?}"),
    }
}

#[test]
fn test_convert_using() {
    let (mut s, mut t) = common::setup();
    // CONVERT(str USING charset) — charset conversion not implemented,
    // but the parse + return-value must succeed without error.
    match scalar("SELECT CONVERT('hello' USING utf8mb4)", &mut s, &mut t) {
        Value::Text(s) => assert_eq!(s, "hello"),
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── G6.7 SELECT optimizer hint modifiers ─────────────────────────────────────

#[test]
fn test_select_high_priority() {
    let (mut s, mut t) = common::setup();
    // These modifiers are consumed and discarded — query must succeed.
    let r = common::rows(run("SELECT HIGH_PRIORITY 1+1", &mut s, &mut t));
    assert_eq!(r[0][0], Value::Int(2));
}

#[test]
fn test_select_straight_join() {
    let (mut s, mut t) = common::setup();
    let r = common::rows(run("SELECT STRAIGHT_JOIN 42", &mut s, &mut t));
    assert_eq!(r[0][0], Value::Int(42));
}

#[test]
fn test_select_sql_big_result() {
    let (mut s, mut t) = common::setup();
    let r = common::rows(run("SELECT SQL_BIG_RESULT 1", &mut s, &mut t));
    assert_eq!(r[0][0], Value::Int(1));
}
