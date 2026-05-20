mod common;

use axiomdb_types::{DataType, Value};

// ── DDL parsing ───────────────────────────────────────────────────────────────

#[test]
fn timestamptz_column_parses_as_correct_datatype() {
    let ct = {
        use axiomdb_sql::parser::parse;
        let stmt = parse("CREATE TABLE t (ts TIMESTAMPTZ)", None).unwrap();
        match stmt {
            axiomdb_sql::ast::Stmt::CreateTable(ct) => ct,
            other => panic!("expected CreateTable, got {other:?}"),
        }
    };
    assert_eq!(ct.columns[0].data_type, DataType::TimestampTz);
}

#[test]
fn timestamp_with_time_zone_parses_as_timestamptz() {
    let ct = {
        use axiomdb_sql::parser::parse;
        let stmt = parse("CREATE TABLE t (ts TIMESTAMP WITH TIME ZONE)", None).unwrap();
        match stmt {
            axiomdb_sql::ast::Stmt::CreateTable(ct) => ct,
            other => panic!("expected CreateTable, got {other:?}"),
        }
    };
    assert_eq!(ct.columns[0].data_type, DataType::TimestampTz);
}

// ── Basic INSERT / SELECT roundtrip ──────────────────────────────────────────

#[test]
fn timestamptz_insert_and_select() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, ts TIMESTAMPTZ)",
        &mut s,
        &mut txn,
    );
    // 2024-01-15 12:00:00 UTC = days_to_epoch * 86400 * 1e6 + 12h * 3600 * 1e6
    // days: 2024-01-15 = 19737 days since epoch
    // micros: 19737 * 86400000000 + 12 * 3600000000 = 1705320000000000
    common::run(
        "INSERT INTO t VALUES (1, '2024-01-15 12:00:00')",
        &mut s,
        &mut txn,
    );
    let out = common::rows(common::run("SELECT ts FROM t", &mut s, &mut txn));
    assert_eq!(out.len(), 1);
    match &out[0][0] {
        Value::TimestampTz(micros) => {
            assert!(*micros > 0, "expected positive micros, got {micros}");
        }
        v => panic!("expected TimestampTz, got {v:?}"),
    }
}

#[test]
fn timestamptz_insert_with_utc_offset() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, ts TIMESTAMPTZ)",
        &mut s,
        &mut txn,
    );
    // Insert with explicit +00:00 offset
    common::run(
        "INSERT INTO t VALUES (1, '2024-01-15 12:00:00+00:00')",
        &mut s,
        &mut txn,
    );
    // Insert with no offset (treated as UTC)
    common::run(
        "INSERT INTO t VALUES (2, '2024-01-15 12:00:00')",
        &mut s,
        &mut txn,
    );
    let out = common::rows(common::run("SELECT ts FROM t ORDER BY id", &mut s, &mut txn));
    assert_eq!(out.len(), 2);
    match (&out[0][0], &out[1][0]) {
        (Value::TimestampTz(a), Value::TimestampTz(b)) => {
            assert_eq!(a, b, "explicit +00:00 and no offset should give same µs");
        }
        _ => panic!("expected TimestampTz values"),
    }
}

#[test]
fn timestamptz_insert_with_positive_offset() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, ts TIMESTAMPTZ)",
        &mut s,
        &mut txn,
    );
    // 12:00 +05:30 = 06:30 UTC
    common::run(
        "INSERT INTO t VALUES (1, '2024-01-15 12:00:00+05:30')",
        &mut s,
        &mut txn,
    );
    // 06:30 UTC as explicit
    common::run(
        "INSERT INTO t VALUES (2, '2024-01-15 06:30:00+00:00')",
        &mut s,
        &mut txn,
    );
    let out = common::rows(common::run("SELECT ts FROM t ORDER BY id", &mut s, &mut txn));
    match (&out[0][0], &out[1][0]) {
        (Value::TimestampTz(a), Value::TimestampTz(b)) => {
            assert_eq!(a, b, "+05:30 and UTC equivalent must store same µs");
        }
        _ => panic!("expected TimestampTz values"),
    }
}

// ── NULL handling ─────────────────────────────────────────────────────────────

#[test]
fn timestamptz_null() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, ts TIMESTAMPTZ)",
        &mut s,
        &mut txn,
    );
    common::run("INSERT INTO t VALUES (1, NULL)", &mut s, &mut txn);
    let out = common::rows(common::run("SELECT ts FROM t", &mut s, &mut txn));
    assert_eq!(out[0][0], Value::Null);
}

// ── Comparison operators ──────────────────────────────────────────────────────

#[test]
fn timestamptz_comparison_order() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, ts TIMESTAMPTZ)",
        &mut s,
        &mut txn,
    );
    common::run(
        "INSERT INTO t VALUES (1, '2024-01-01 00:00:00'), (2, '2024-06-01 12:00:00'), (3, '2023-12-31 23:59:59')",
        &mut s,
        &mut txn,
    );
    let out = common::rows(common::run(
        "SELECT id FROM t ORDER BY ts ASC",
        &mut s,
        &mut txn,
    ));
    let ids: Vec<i32> = out
        .iter()
        .map(|row| match &row[0] {
            Value::Int(n) => *n,
            v => panic!("expected Int, got {v:?}"),
        })
        .collect();
    assert_eq!(ids, vec![3, 1, 2], "expected ascending order by timestamp");
}

// ── CAST / coerce ─────────────────────────────────────────────────────────────

#[test]
fn timestamptz_cast_from_text() {
    let (mut s, mut txn) = common::setup();
    let out = common::rows(common::run(
        "SELECT CAST('2024-01-15 12:00:00' AS TIMESTAMPTZ)",
        &mut s,
        &mut txn,
    ));
    match &out[0][0] {
        Value::TimestampTz(micros) => assert!(*micros > 0),
        v => panic!("expected TimestampTz, got {v:?}"),
    }
}

#[test]
fn timestamptz_cast_from_timestamp() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, ts TIMESTAMP)",
        &mut s,
        &mut txn,
    );
    common::run(
        "INSERT INTO t VALUES (1, '2024-01-15 12:00:00')",
        &mut s,
        &mut txn,
    );
    let out = common::rows(common::run(
        "SELECT CAST(ts AS TIMESTAMPTZ) FROM t",
        &mut s,
        &mut txn,
    ));
    match &out[0][0] {
        Value::TimestampTz(micros) => assert!(*micros > 0),
        v => panic!("expected TimestampTz, got {v:?}"),
    }
}

// ── AT TIME ZONE ──────────────────────────────────────────────────────────────

#[test]
fn at_time_zone_utc_converts_timestamp_to_timestamptz() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, ts TIMESTAMP)",
        &mut s,
        &mut txn,
    );
    common::run(
        "INSERT INTO t VALUES (1, '2024-01-15 12:00:00')",
        &mut s,
        &mut txn,
    );
    let out = common::rows(common::run(
        "SELECT ts AT TIME ZONE 'UTC' FROM t",
        &mut s,
        &mut txn,
    ));
    match &out[0][0] {
        Value::TimestampTz(micros) => assert!(*micros > 0),
        v => panic!("expected TimestampTz, got {v:?}"),
    }
}

// ── Codec roundtrip (encode/decode) ──────────────────────────────────────────

#[test]
fn timestamptz_codec_roundtrip() {
    use axiomdb_types::codec::{decode_row, encode_row};
    let micros: i64 = 1_705_320_000_000_000; // 2024-01-15 12:00:00 UTC
    let values = vec![Value::TimestampTz(micros)];
    let schema = vec![DataType::TimestampTz];
    let encoded = encode_row(&values, &schema).expect("encode failed");
    let decoded = decode_row(&encoded, &schema).expect("decode failed");
    assert_eq!(decoded, values);
}

// ── SHOW COLUMNS ─────────────────────────────────────────────────────────────

#[test]
fn timestamptz_show_columns_type_name() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, ts TIMESTAMPTZ)",
        &mut s,
        &mut txn,
    );
    let out = common::rows(common::run("SHOW COLUMNS FROM t", &mut s, &mut txn));
    // SHOW COLUMNS returns (Field, Type, ...) — row 1 is ts column
    let ts_type = match &out[1][1] {
        Value::Text(s) => s.clone(),
        v => panic!("expected Text for type, got {v:?}"),
    };
    assert!(
        ts_type.to_lowercase().contains("timestamptz"),
        "expected 'timestamptz' in type name, got '{ts_type}'"
    );
}
