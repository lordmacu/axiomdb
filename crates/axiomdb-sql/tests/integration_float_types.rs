mod common;

use axiomdb_types::{DataType, Value};

// ── REAL (f32) ────────────────────────────────────────────────────────────────

#[test]
fn real_insert_and_select() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, val REAL)",
        &mut s,
        &mut txn,
    );
    common::run("INSERT INTO t VALUES (1, 3.14)", &mut s, &mut txn);
    common::run("INSERT INTO t VALUES (2, -1.5)", &mut s, &mut txn);
    let out = common::rows(common::run(
        "SELECT val FROM t ORDER BY id",
        &mut s,
        &mut txn,
    ));
    // Values round-trip through f32 encoding — precision is f32.
    assert_eq!(out.len(), 2);
    match &out[0][0] {
        Value::Real(f) => assert!(
            (*f - 3.14f32 as f64).abs() < 1e-6,
            "expected ~3.14, got {f}"
        ),
        v => panic!("expected Real, got {v:?}"),
    }
    match &out[1][0] {
        Value::Real(f) => assert!((*f - (-1.5f64)).abs() < 1e-7, "expected -1.5, got {f}"),
        v => panic!("expected Real, got {v:?}"),
    }
}

#[test]
fn real_column_has_float_datatype() {
    let ct = {
        use axiomdb_sql::parser::parse;
        let stmt = parse("CREATE TABLE t (val REAL)", None).unwrap();
        match stmt {
            axiomdb_sql::ast::Stmt::CreateTable(ct) => ct,
            other => panic!("expected CreateTable, got {other:?}"),
        }
    };
    assert_eq!(ct.columns[0].data_type, DataType::Float);
}

// ── DOUBLE (f64) ──────────────────────────────────────────────────────────────

#[test]
fn double_insert_and_select() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, val DOUBLE)",
        &mut s,
        &mut txn,
    );
    common::run(
        "INSERT INTO t VALUES (1, 3.141592653589793)",
        &mut s,
        &mut txn,
    );
    let out = common::rows(common::run(
        "SELECT val FROM t ORDER BY id",
        &mut s,
        &mut txn,
    ));
    assert_eq!(out.len(), 1);
    match &out[0][0] {
        Value::Real(f) => assert!(
            (*f - std::f64::consts::PI).abs() < 1e-15,
            "expected π, got {f}"
        ),
        v => panic!("expected Real, got {v:?}"),
    }
}

#[test]
fn double_column_has_real_datatype() {
    let ct = {
        use axiomdb_sql::parser::parse;
        let stmt = parse("CREATE TABLE t (val DOUBLE)", None).unwrap();
        match stmt {
            axiomdb_sql::ast::Stmt::CreateTable(ct) => ct,
            other => panic!("expected CreateTable, got {other:?}"),
        }
    };
    assert_eq!(ct.columns[0].data_type, DataType::Real);
}

// ── FLOAT4 / FLOAT8 aliases ───────────────────────────────────────────────────

#[test]
fn float4_parses_to_float_datatype() {
    let ct = {
        use axiomdb_sql::parser::parse;
        let stmt = parse("CREATE TABLE t (val FLOAT4)", None).unwrap();
        match stmt {
            axiomdb_sql::ast::Stmt::CreateTable(ct) => ct,
            other => panic!("expected CreateTable, got {other:?}"),
        }
    };
    assert_eq!(ct.columns[0].data_type, DataType::Float);
}

#[test]
fn float8_parses_to_real_datatype() {
    let ct = {
        use axiomdb_sql::parser::parse;
        let stmt = parse("CREATE TABLE t (val FLOAT8)", None).unwrap();
        match stmt {
            axiomdb_sql::ast::Stmt::CreateTable(ct) => ct,
            other => panic!("expected CreateTable, got {other:?}"),
        }
    };
    assert_eq!(ct.columns[0].data_type, DataType::Real);
}

// ── DOUBLE PRECISION alias ────────────────────────────────────────────────────

#[test]
fn double_precision_parses_to_real_datatype() {
    let ct = {
        use axiomdb_sql::parser::parse;
        let stmt = parse("CREATE TABLE t (val DOUBLE PRECISION)", None).unwrap();
        match stmt {
            axiomdb_sql::ast::Stmt::CreateTable(ct) => ct,
            other => panic!("expected CreateTable, got {other:?}"),
        }
    };
    assert_eq!(ct.columns[0].data_type, DataType::Real);
}

// ── f32 precision truncation ──────────────────────────────────────────────────

#[test]
fn real_truncates_to_f32_precision() {
    let (mut s, mut txn) = common::setup();
    common::run(
        "CREATE TABLE t (id INT PRIMARY KEY, val REAL)",
        &mut s,
        &mut txn,
    );
    // Pi at f64 precision; stored as REAL (f32), so only 7 significant digits survive.
    common::run(
        "INSERT INTO t VALUES (1, 3.141592653589793)",
        &mut s,
        &mut txn,
    );
    let out = common::rows(common::run("SELECT val FROM t", &mut s, &mut txn));
    match &out[0][0] {
        Value::Real(f) => {
            // After f32 round-trip, should differ from f64 Pi.
            let pi_f64 = std::f64::consts::PI;
            let pi_f32 = std::f32::consts::PI as f64;
            let diff_from_f32 = (f - pi_f32).abs();
            let diff_from_f64 = (f - pi_f64).abs();
            // Closer to f32 Pi than to f64 Pi.
            assert!(
                diff_from_f32 < diff_from_f64,
                "expected f32 precision, got {f}"
            );
        }
        v => panic!("expected Real, got {v:?}"),
    }
}
