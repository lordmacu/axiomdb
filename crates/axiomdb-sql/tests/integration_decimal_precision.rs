mod common;

use axiomdb_types::Value;

// ── Step 1: parser captures DECIMAL(p,s) → type_len ─────────────────────────

#[test]
fn decimal_column_params_stored_and_shown() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (price DECIMAL(10,2), qty DECIMAL(5,0), bare DECIMAL)",
        &mut storage,
        &mut txn,
    );
    let cols = common::rows(common::run("SHOW COLUMNS FROM t", &mut storage, &mut txn));
    // SHOW COLUMNS: Field, Type, Null, Key, Default, Extra
    let types: Vec<String> = cols.iter().map(|r| r[1].to_string()).collect();
    assert_eq!(types[0], "decimal(10,2)");
    assert_eq!(types[1], "decimal(5,0)");
    assert_eq!(types[2], "decimal(10,0)"); // bare DECIMAL → (10,0)
}

#[test]
fn decimal_params_invalid_precision_zero_rejected() {
    let (mut storage, mut txn) = common::setup();
    let r = common::run_result(
        "CREATE TABLE t (x DECIMAL(0,0))",
        &mut storage,
        &mut txn,
    );
    assert!(r.is_err(), "precision 0 should be rejected");
}

#[test]
fn decimal_params_invalid_precision_too_large_rejected() {
    let (mut storage, mut txn) = common::setup();
    let r = common::run_result(
        "CREATE TABLE t (x DECIMAL(39,0))",
        &mut storage,
        &mut txn,
    );
    assert!(r.is_err(), "precision > 38 should be rejected");
}

#[test]
fn decimal_params_scale_exceeds_precision_rejected() {
    let (mut storage, mut txn) = common::setup();
    let r = common::run_result(
        "CREATE TABLE t (x DECIMAL(5,6))",
        &mut storage,
        &mut txn,
    );
    assert!(r.is_err(), "scale > precision should be rejected");
}

#[test]
fn numeric_alias_same_as_decimal() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (x NUMERIC(8,3))",
        &mut storage,
        &mut txn,
    );
    let cols = common::rows(common::run("SHOW COLUMNS FROM t", &mut storage, &mut txn));
    let type_str = cols[0][1].to_string();
    assert_eq!(type_str, "decimal(8,3)");
}

// ── Step 2: enforcement at insert ────────────────────────────────────────────

#[test]
fn decimal_insert_rounds_to_scale() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (x DECIMAL(10,2))", &mut storage, &mut txn);
    common::run("INSERT INTO t VALUES ('123.456')", &mut storage, &mut txn);
    let out = common::rows(common::run("SELECT x FROM t", &mut storage, &mut txn));
    // ROUND_HALF_UP: .456 rounds to .46 → mantissa 12346, scale 2
    assert_eq!(out, vec![vec![Value::Decimal(12346, 2)]]);
}

#[test]
fn decimal_insert_rounds_half_up() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (x DECIMAL(10,2))", &mut storage, &mut txn);
    common::run("INSERT INTO t VALUES ('1.235')", &mut storage, &mut txn);
    let out = common::rows(common::run("SELECT x FROM t", &mut storage, &mut txn));
    // 1.235 → 1.24 (ROUND_HALF_UP)
    assert_eq!(out, vec![vec![Value::Decimal(124, 2)]]);
}

#[test]
fn decimal_insert_overflow_precision_rejected() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (x DECIMAL(5,2))", &mut storage, &mut txn);
    // 1234.56 has 4 integer digits but max integer digits = 5-2 = 3 → rejected
    let r = common::run_result("INSERT INTO t VALUES ('1234.56')", &mut storage, &mut txn);
    assert!(r.is_err(), "overflow should be rejected");
}

#[test]
fn decimal_insert_null_passes() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (x DECIMAL(5,2))", &mut storage, &mut txn);
    common::run("INSERT INTO t VALUES (NULL)", &mut storage, &mut txn);
    let out = common::rows(common::run("SELECT x FROM t", &mut storage, &mut txn));
    assert_eq!(out, vec![vec![Value::Null]]);
}

#[test]
fn decimal_bare_rounds_to_zero_scale() {
    let (mut storage, mut txn) = common::setup();
    // Bare DECIMAL = DECIMAL(10,0)
    common::run("CREATE TABLE t (x DECIMAL)", &mut storage, &mut txn);
    common::run("INSERT INTO t VALUES ('1.5')", &mut storage, &mut txn);
    let out = common::rows(common::run("SELECT x FROM t", &mut storage, &mut txn));
    // 1.5 → 2 (ROUND_HALF_UP to 0 decimal places)
    assert_eq!(out, vec![vec![Value::Decimal(2, 0)]]);
}

#[test]
fn decimal_exact_boundary_accepted() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (x DECIMAL(5,2))", &mut storage, &mut txn);
    // 999.99 has 3 integer digits, max = 5-2 = 3 → exactly fits
    common::run("INSERT INTO t VALUES ('999.99')", &mut storage, &mut txn);
    let out = common::rows(common::run("SELECT x FROM t", &mut storage, &mut txn));
    assert_eq!(out, vec![vec![Value::Decimal(99999, 2)]]);
}

// ── Step 3: division precision + ROUND/TRUNC ─────────────────────────────────
// Decimal literals like 1.2345 parse as Real, so we must load values through
// DECIMAL columns to exercise the Decimal arms of these functions.

#[test]
fn decimal_division_produces_fractional_digits() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (a DECIMAL(10,2), b DECIMAL(10,2))",
        &mut storage,
        &mut txn,
    );
    common::run("INSERT INTO t VALUES ('10.00', '3.00')", &mut storage, &mut txn);
    let out = common::rows(common::run("SELECT a / b FROM t", &mut storage, &mut txn));
    match &out[0][0] {
        Value::Decimal(m, s) => {
            let f = *m as f64 / 10f64.powi(*s as i32);
            assert!(
                (f - 3.333333).abs() < 1e-4,
                "expected ~3.333333, got {m}×10^-{s} = {f}"
            );
        }
        other => panic!("expected Decimal, got {other:?}"),
    }
}

#[test]
fn decimal_round_half_up_function() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (a DECIMAL(10,4), b DECIMAL(10,4), c DECIMAL(10,4))",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO t VALUES ('1.2345', '1.2350', '1.2349')",
        &mut storage,
        &mut txn,
    );
    let out = common::rows(common::run(
        "SELECT ROUND(a, 2), ROUND(b, 2), ROUND(c, 2) FROM t",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(out[0][0], Value::Decimal(123, 2));  // 1.23
    assert_eq!(out[0][1], Value::Decimal(124, 2));  // 1.24 (HALF_UP)
    assert_eq!(out[0][2], Value::Decimal(123, 2));  // 1.23
}

#[test]
fn decimal_round_no_scale_arg() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (a DECIMAL(10,1), b DECIMAL(10,1), c DECIMAL(10,1))",
        &mut storage,
        &mut txn,
    );
    common::run("INSERT INTO t VALUES ('1.6', '1.4', '1.5')", &mut storage, &mut txn);
    let out = common::rows(common::run(
        "SELECT ROUND(a), ROUND(b), ROUND(c) FROM t",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(out[0][0], Value::Decimal(2, 0));
    assert_eq!(out[0][1], Value::Decimal(1, 0));
    assert_eq!(out[0][2], Value::Decimal(2, 0)); // HALF_UP
}

#[test]
fn decimal_trunc_function() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (a DECIMAL(10,3), b DECIMAL(10,3))",
        &mut storage,
        &mut txn,
    );
    common::run("INSERT INTO t VALUES ('1.999', '-1.999')", &mut storage, &mut txn);
    let out = common::rows(common::run(
        "SELECT TRUNC(a, 1), TRUNC(a, 2), TRUNC(b, 1) FROM t",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(out[0][0], Value::Decimal(19, 1));   // 1.9
    assert_eq!(out[0][1], Value::Decimal(199, 2));  // 1.99
    assert_eq!(out[0][2], Value::Decimal(-19, 1));  // -1.9
}

#[test]
fn decimal_truncate_alias() {
    let (mut storage, mut txn) = common::setup();
    common::run("CREATE TABLE t (x DECIMAL(10,3))", &mut storage, &mut txn);
    common::run("INSERT INTO t VALUES ('1.999')", &mut storage, &mut txn);
    let out = common::rows(common::run(
        "SELECT TRUNCATE(x, 2) FROM t",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(out[0][0], Value::Decimal(199, 2));
}

#[test]
fn decimal_round_null_returns_null() {
    let (mut storage, mut txn) = common::setup();
    let out = common::rows(common::run(
        "SELECT ROUND(NULL, 2)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(out[0][0], Value::Null);
}
