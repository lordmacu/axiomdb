//! Integration tests for `SET TRANSACTION ISOLATION LEVEL` (SQL-std form).
//!
//! Complements the MySQL-compat `SET transaction_isolation = '...'` form
//! already covered in the 28.1 test suite.

mod common;

use axiomdb_sql::parse;

fn parses_to_set_transaction_isolation(sql: &str, expected_level: &str) {
    let stmt = parse(sql, None).unwrap_or_else(|e| panic!("parse failed for {sql:?}: {e}"));
    let set = match stmt {
        axiomdb_sql::ast::Stmt::Set(s) => s,
        other => panic!("expected Set, got {other:?}"),
    };
    assert_eq!(
        set.variable.to_ascii_lowercase(),
        "transaction_isolation",
        "variable mismatch for {sql:?}",
    );
    let got_level = match set.value {
        axiomdb_sql::ast::SetValue::Expr(axiomdb_sql::expr::Expr::Literal(
            axiomdb_types::Value::Text(t),
        )) => t,
        other => panic!("expected Text literal, got {other:?}"),
    };
    assert_eq!(got_level.to_ascii_lowercase(), expected_level);
}

#[test]
fn parses_read_committed() {
    parses_to_set_transaction_isolation(
        "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
        "read committed",
    );
}

#[test]
fn parses_read_uncommitted() {
    parses_to_set_transaction_isolation(
        "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED",
        "read uncommitted",
    );
}

#[test]
fn parses_repeatable_read() {
    parses_to_set_transaction_isolation(
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "repeatable read",
    );
}

#[test]
fn parses_serializable() {
    parses_to_set_transaction_isolation(
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "serializable",
    );
}

#[test]
fn parses_with_session_prefix() {
    parses_to_set_transaction_isolation(
        "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
        "read committed",
    );
}

#[test]
fn parses_read_only_write_transaction() {
    let stmt = parse("SET TRANSACTION READ ONLY", None).unwrap();
    let set = match stmt {
        axiomdb_sql::ast::Stmt::Set(s) => s,
        _ => panic!(),
    };
    assert_eq!(set.variable, "transaction_read_only");

    let stmt = parse("SET TRANSACTION READ WRITE", None).unwrap();
    let set = match stmt {
        axiomdb_sql::ast::Stmt::Set(s) => s,
        _ => panic!(),
    };
    assert_eq!(set.variable, "transaction_read_only");
}
