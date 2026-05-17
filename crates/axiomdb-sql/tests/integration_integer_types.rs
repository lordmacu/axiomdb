mod common;

use axiomdb_types::Value;

// ── TINYINT ───────────────────────────────────────────────────────────────────

#[test]
fn tinyint_insert_and_select() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, val TINYINT)",
        &mut s,
        &mut txn,
    );
    common::run("INSERT INTO t VALUES (1, 42)", &mut s, &mut txn);
    common::run("INSERT INTO t VALUES (2, -10)", &mut s, &mut txn);
    let out = common::rows(common::run(
        "SELECT val FROM t ORDER BY id",
        &mut s,
        &mut txn,
    ));
    assert_eq!(out, vec![vec![Value::Int(42)], vec![Value::Int(-10)]]);
}

#[test]
fn tinyint_boundary_values_accepted() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, val TINYINT)",
        &mut s,
        &mut txn,
    );
    common::run("INSERT INTO t VALUES (1, -128)", &mut s, &mut txn);
    common::run("INSERT INTO t VALUES (2, 127)", &mut s, &mut txn);
    let out = common::rows(common::run(
        "SELECT val FROM t ORDER BY id",
        &mut s,
        &mut txn,
    ));
    assert_eq!(out, vec![vec![Value::Int(-128)], vec![Value::Int(127)]]);
}

#[test]
fn tinyint_overflow_high_returns_error() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, val TINYINT)",
        &mut s,
        &mut txn,
    );
    let err = common::run_result("INSERT INTO t VALUES (1, 128)", &mut s, &mut txn).unwrap_err();
    match err {
        axiomdb_core::error::DbError::InvalidValue { reason } => {
            assert!(reason.contains("128"), "expected 128 in: {reason}");
            assert!(reason.contains("TINYINT"), "expected TINYINT in: {reason}");
        }
        other => panic!("expected InvalidValue, got {other:?}"),
    }
}

#[test]
fn tinyint_overflow_low_returns_error() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, val TINYINT)",
        &mut s,
        &mut txn,
    );
    let err = common::run_result("INSERT INTO t VALUES (1, -129)", &mut s, &mut txn).unwrap_err();
    match err {
        axiomdb_core::error::DbError::InvalidValue { reason } => {
            assert!(reason.contains("-129"), "expected -129 in: {reason}");
        }
        other => panic!("expected InvalidValue, got {other:?}"),
    }
}

#[test]
fn tinyint_null_in_nullable_column() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, val TINYINT)",
        &mut s,
        &mut txn,
    );
    common::run("INSERT INTO t VALUES (1, NULL)", &mut s, &mut txn);
    let out = common::rows(common::run("SELECT val FROM t", &mut s, &mut txn));
    assert_eq!(out, vec![vec![Value::Null]]);
}

// ── SMALLINT ──────────────────────────────────────────────────────────────────

#[test]
fn smallint_insert_and_select() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, val SMALLINT)",
        &mut s,
        &mut txn,
    );
    common::run("INSERT INTO t VALUES (1, 1000)", &mut s, &mut txn);
    common::run("INSERT INTO t VALUES (2, -500)", &mut s, &mut txn);
    let out = common::rows(common::run(
        "SELECT val FROM t ORDER BY id",
        &mut s,
        &mut txn,
    ));
    assert_eq!(out, vec![vec![Value::Int(1000)], vec![Value::Int(-500)]]);
}

#[test]
fn smallint_boundary_values_accepted() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, val SMALLINT)",
        &mut s,
        &mut txn,
    );
    common::run("INSERT INTO t VALUES (1, -32768)", &mut s, &mut txn);
    common::run("INSERT INTO t VALUES (2, 32767)", &mut s, &mut txn);
    let out = common::rows(common::run(
        "SELECT val FROM t ORDER BY id",
        &mut s,
        &mut txn,
    ));
    assert_eq!(out, vec![vec![Value::Int(-32768)], vec![Value::Int(32767)]]);
}

#[test]
fn smallint_overflow_returns_error() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, val SMALLINT)",
        &mut s,
        &mut txn,
    );
    let err = common::run_result("INSERT INTO t VALUES (1, 40000)", &mut s, &mut txn).unwrap_err();
    match err {
        axiomdb_core::error::DbError::InvalidValue { reason } => {
            assert!(reason.contains("40000"), "expected 40000 in: {reason}");
            assert!(
                reason.contains("SMALLINT"),
                "expected SMALLINT in: {reason}"
            );
        }
        other => panic!("expected InvalidValue, got {other:?}"),
    }
}

#[test]
fn smallint_underflow_returns_error() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, val SMALLINT)",
        &mut s,
        &mut txn,
    );
    let err = common::run_result("INSERT INTO t VALUES (1, -40000)", &mut s, &mut txn).unwrap_err();
    match err {
        axiomdb_core::error::DbError::InvalidValue { reason } => {
            assert!(reason.contains("-40000"), "expected -40000 in: {reason}");
        }
        other => panic!("expected InvalidValue, got {other:?}"),
    }
}

// ── BIGSERIAL ─────────────────────────────────────────────────────────────────

#[test]
fn bigserial_auto_increments() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id BIGSERIAL PRIMARY KEY, name TEXT)",
        &mut s,
        &mut txn,
    );
    common::run("INSERT INTO t (name) VALUES ('alice')", &mut s, &mut txn);
    common::run("INSERT INTO t (name) VALUES ('bob')", &mut s, &mut txn);
    common::run("INSERT INTO t (name) VALUES ('carol')", &mut s, &mut txn);
    let out = common::rows(common::run(
        "SELECT id FROM t ORDER BY id",
        &mut s,
        &mut txn,
    ));
    assert_eq!(
        out,
        vec![
            vec![Value::BigInt(1)],
            vec![Value::BigInt(2)],
            vec![Value::BigInt(3)],
        ]
    );
}

// ── SHOW COLUMNS type names ───────────────────────────────────────────────────

#[test]
fn show_columns_reports_tinyint_and_smallint() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, ti TINYINT, si SMALLINT)",
        &mut s,
        &mut txn,
    );
    let result = common::run("SHOW COLUMNS FROM t", &mut s, &mut txn);
    let rows = common::rows(result);
    // The Type column (index 1 in SHOW COLUMNS: Field, Type, Null, Key, Default, Extra)
    let types: Vec<String> = rows
        .iter()
        .map(|row| match &row[1] {
            Value::Text(s) => s.to_lowercase(),
            other => format!("{other:?}"),
        })
        .collect();
    assert!(
        types.iter().any(|t| t == "tinyint"),
        "expected tinyint in types, got {types:?}"
    );
    assert!(
        types.iter().any(|t| t == "smallint"),
        "expected smallint in types, got {types:?}"
    );
}

// ── SERIAL / SMALLSERIAL ──────────────────────────────────────────────────────

#[test]
fn serial_auto_increments() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id SERIAL PRIMARY KEY, name TEXT)",
        &mut s,
        &mut txn,
    );
    common::run("INSERT INTO t (name) VALUES ('a')", &mut s, &mut txn);
    common::run("INSERT INTO t (name) VALUES ('b')", &mut s, &mut txn);
    let out = common::rows(common::run(
        "SELECT id FROM t ORDER BY id",
        &mut s,
        &mut txn,
    ));
    assert_eq!(out, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}

#[test]
fn smallserial_auto_increments() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id SMALLSERIAL PRIMARY KEY, name TEXT)",
        &mut s,
        &mut txn,
    );
    common::run("INSERT INTO t (name) VALUES ('x')", &mut s, &mut txn);
    common::run("INSERT INTO t (name) VALUES ('y')", &mut s, &mut txn);
    let out = common::rows(common::run(
        "SELECT id FROM t ORDER BY id",
        &mut s,
        &mut txn,
    ));
    assert_eq!(out, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}

#[test]
fn show_columns_reports_serial_types() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (a SERIAL PRIMARY KEY, b SMALLSERIAL, c BIGSERIAL)",
        &mut s,
        &mut txn,
    );
    let rows = common::rows(common::run("SHOW COLUMNS FROM t", &mut s, &mut txn));
    let types: Vec<String> = rows
        .iter()
        .map(|row| match &row[1] {
            Value::Text(s) => s.to_lowercase(),
            other => format!("{other:?}"),
        })
        .collect();
    assert!(
        types.iter().any(|t| t == "int"),
        "expected int, got {types:?}"
    );
    assert!(
        types.iter().any(|t| t == "smallint"),
        "expected smallint, got {types:?}"
    );
    assert!(
        types.iter().any(|t| t == "bigint"),
        "expected bigint, got {types:?}"
    );
}

#[test]
fn serial_trailing_form_regression() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT SERIAL PRIMARY KEY, val TEXT)",
        &mut s,
        &mut txn,
    );
    common::run("INSERT INTO t (val) VALUES ('r')", &mut s, &mut txn);
    let out = common::rows(common::run("SELECT id FROM t", &mut s, &mut txn));
    assert_eq!(out, vec![vec![Value::Int(1)]]);
}
